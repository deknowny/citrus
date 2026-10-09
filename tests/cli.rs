//! Behaviour of the real binary on throwaway Git repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

/// The citrus under test, without the environment of a Citrus that runs these
/// tests: a pool agent sets CITRUS_PROTOCOL, CITRUS_BIN, CITRUS_AGENT_POOL…
fn citrus_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_citrus"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        // A shared target directory would take the fixtures' own builds.
        if (name.starts_with("CITRUS_") && name != "CITRUS_TEST_POOL") || name == "CARGO_TARGET_DIR"
        {
            command.env_remove(&key);
        }
    }
    // Never this machine's pool (~/.config/citrus/pool).
    command.env("CITRUS_POOL", "");
    // State in a test database: each temporary repository gets its own schema.
    command.env("CITRUS_STATE", test_state());
    command
}

/// CITRUS_TEST_STATE, else the local test Postgres (docker run -p 55432:5432 postgres).
fn test_state() -> String {
    std::env::var("CITRUS_TEST_STATE").unwrap_or_else(|_| {
        "postgres://postgres:t@localhost:55432/postgres?sslmode=disable".to_owned()
    })
}

struct Project {
    dir: tempfile::TempDir,
}

const MAKEFILE: &str = "ok:\n\t@echo fine\nplain:\n\t@echo plain\nfail:\n\t@echo building; echo 'AssertionError: broken thing'; exit 1\nslow:\n\t@sleep 30\n";
const BASE: &str = r#"#![citrus(2)]

#[paths("src/*.txt")]
check ok {
    run!("make ok")?;
}

#[paths("other/*")]
check fail {
    run!("make fail")?;
}

#[paths("Makefile")]
#[cache(false)]
check plain {
    run!("make plain")?;
}

#[paths("Makefile")]
#[cache(false)]
check slow {
    run!("make slow")?;
}
"#;

impl Project {
    /// `config`: more of citrus.ci after the base checks.
    fn new(config: &str) -> Project {
        let project = Project {
            dir: tempfile::tempdir().unwrap(),
        };
        project.write("Makefile", MAKEFILE);
        project.write("citrus.ci", &format!("{BASE}\n{config}"));
        project.write("src/a.txt", "one\n");
        project.write("other/x", "x\n");
        project.write(".gitignore", ".scratch/\n");
        project.git(&["init", "-q", "-b", "main"]);
        project.git(&["add", "-A"]);
        project.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ]);
        project
    }

    /// A project whose `citrus.ci` is exactly `config` (language v2).
    fn v2(config: &str) -> Project {
        let project = Project::new("");
        project.write("citrus.ci", config);
        project.git(&["add", "-A"]);
        project.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "v2",
        ]);
        project
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, path: &str, content: &str) {
        let file = self.root().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, content).unwrap();
    }

    fn git(&self, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(self.root())
                .status()
                .unwrap()
                .success()
        );
    }

    fn citrus(&self, args: &[&str]) -> Output {
        citrus_command()
            // The machine's pool (~/.config/citrus/pool) is not the tests'.
            .env("CITRUS_POOL", "")
            .args(args)
            .current_dir(self.root())
            .env_remove("CODEX_THREAD_ID")
            .env_remove("CLAUDECODE")
            .env("CITRUS_AGENT", "test")
            .output()
            .unwrap()
    }

    fn json(&self, args: &[&str]) -> (Value, i32) {
        let output = self.citrus(&[args, &["--json"]].concat());
        let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
            panic!(
                "not JSON: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (value, output.status.code().unwrap_or(-1))
    }

    /// Append to citrus.ci (not committed).
    fn declare(&self, text: &str) {
        let current = fs::read_to_string(self.root().join("citrus.ci")).unwrap();
        self.write("citrus.ci", &format!("{current}\n{text}"));
    }

    fn commit(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            message,
        ]);
    }

    fn receipts(&self) -> PathBuf {
        self.root().join(".git/citrus/receipts")
    }
}

fn target<'a>(run: &'a Value, name: &str) -> &'a Value {
    run["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"] == name)
        .unwrap_or_else(|| panic!("no {name} in {run}"))
}

#[test]
fn declared_target_is_reused_until_its_inputs_change() {
    let project = Project::new("");
    let (first, code) = project.json(&["run", "ok"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(target(&first, "ok")["result"], "passed");
    let (second, _) = project.json(&["run", "ok"]);
    assert_eq!(target(&second, "ok")["result"], "reused");
    assert_eq!(target(&second, "ok")["reason"], "inputs_unchanged");
    assert_eq!(target(&second, "ok")["evidence_run"], first["run"]["id"]);

    project.write("other/x", "unrelated\n");
    assert_eq!(
        target(&project.json(&["run", "ok"]).0, "ok")["result"],
        "reused"
    );

    project.write("src/a.txt", "two\n");
    let (why, _) = project.json(&["why", "ok"]);
    assert_eq!(why["decision"]["reason"], "input_changed");
    assert_eq!(
        why["changed_since_last_pass"],
        serde_json::json!(["src/a.txt"])
    );
    let (third, _) = project.json(&["run", "ok"]);
    assert_eq!(target(&third, "ok")["result"], "passed");
}

#[test]
fn failure_reports_the_first_error_not_the_whole_log() {
    let project = Project::new("");
    let (run, code) = project.json(&["run", "fail"]);
    assert_eq!(code, 1);
    assert_eq!(run["run"]["state"], "failed");
    let row = target(&run, "fail");
    assert_eq!(row["result"], "failed");
    assert!(
        row["first_error"]
            .as_str()
            .unwrap()
            .contains("AssertionError: broken thing")
    );
    assert!(
        run["next"]
            .as_array()
            .unwrap()
            .iter()
            .any(|step| step.as_str().unwrap().starts_with("citrus log"))
    );
    let log = project.citrus(&["log", "last"]);
    assert!(String::from_utf8_lossy(&log.stdout).contains("AssertionError: broken thing"));
    assert!(
        project
            .receipts()
            .read_dir()
            .map(|mut dir| dir.next().is_none())
            .unwrap_or(true)
    );
}

#[test]
fn the_same_key_returns_the_same_run() {
    let project = Project::new("");
    let first = project.json(&["run", "ok", "--key", "task-1"]).0;
    project.write("src/a.txt", "changed\n");
    let second = project.json(&["run", "ok", "--key", "task-1"]).0;
    assert_eq!(first["run"]["id"], second["run"]["id"]);
}

#[test]
fn detached_runs_can_be_followed_cancelled_and_detected_as_lost() {
    let project = Project::new("");
    let (started, _) = project.json(&["run", "slow", "--detach"]);
    let id = started["run"]["id"].as_str().unwrap().to_owned();
    assert!(matches!(
        started["run"]["state"].as_str().unwrap(),
        "queued" | "running"
    ));
    let (joined, _) = project.json(&["run", "slow", "--detach"]);
    assert_eq!(
        joined["run"]["id"], started["run"]["id"],
        "asking again joins the running run"
    );
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["next"][0], format!("citrus wait {id}"));
    let (cancelled, code) = project.json(&["cancel", &id]);
    assert_eq!(cancelled["run"]["state"], "cancelled");
    assert_eq!(code, 3);

    let (lost, _) = project.json(&["run", "slow", "--detach"]);
    let id = lost["run"]["id"].as_str().unwrap().to_owned();
    let pid = wait_for_pid(&project, &id);
    // SAFETY: test-only signal to the worker's process group.
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (shown, _) = project.json(&["show", &id]);
        if shown["run"]["state"] == "unknown" {
            assert_eq!(target(&shown, "slow")["reason"], "process_lost");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "run never became unknown: {shown}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn wait_for_pid(project: &Project, id: &str) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = project.json(&["show", id]).0["run"]["pid"].as_i64() {
            return pid;
        }
        assert!(Instant::now() < deadline, "no worker pid");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn builtin_plan_selects_targets_owning_changed_paths() {
    let project = Project::new("");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["targets"].as_array().unwrap().len(), 0);
    project.write("src/b.txt", "new\n");
    project.write("loose.md", "x");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["plan"]["targets"], serde_json::json!(["ok"]));
    assert_eq!(plan["plan"]["unmapped"], serde_json::json!(["loose.md"]));
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["targets"][0]["result"], "pending");
    assert_eq!(status["next"][0], "citrus run");
}

/// Two checks the runner `remote.sh` runs, both selected by a change in lib/.
const ALPHA_BETA: &str = r#"#![runner(cmd!("sh remote.sh"))]

#[paths("lib/**")]
group lib {
    check alpha {
        run!("make ok")?;
    }

    check beta {
        run!("make ok")?;
    }
}

"#;

#[test]
fn remote_runner_progress_is_recorded_per_target() {
    let project = Project::new(ALPHA_BETA);
    project.write(
        "remote.sh",
        "echo 'CITRUS_WAIT builder'\necho 'CITRUS_TARGET target=lib.alpha status=START'\necho 'CITRUS_TARGET target=lib.alpha status=PASS exit=0 seconds=3'\n\
         echo 'CITRUS_TARGET target=lib.beta status=START'\necho 'error: beta exploded'\necho 'CITRUS_TARGET target=lib.beta status=FAIL exit=2 seconds=4'\nexit 2\n",
    );
    project.commit("runner");
    project.write("lib/x", "changed\n");
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(run["run"]["mode"], "remote");
    assert_eq!(target(&run, "lib.alpha")["result"], "passed");
    assert_eq!(target(&run, "lib.beta")["result"], "failed");
    assert!(
        target(&run, "lib.beta")["first_error"]
            .as_str()
            .unwrap()
            .contains("beta exploded")
    );
    assert_eq!(target(&run, "lib.beta")["seconds"], 4);
    assert!(
        project.json(&["run", "lib.alpha", "--remote"]).0["error"]
            .as_str()
            .unwrap()
            .contains("drop the target names")
    );
}

#[test]
fn remote_details_are_read_from_the_linked_log() {
    let project = Project::new(ALPHA_BETA);
    project.write(
        ".scratch/detail.log",
        "CITRUS_TARGET target=lib.alpha status=START\nCITRUS_TARGET target=lib.alpha status=PASS exit=0\nCITRUS_TARGET target=lib.beta status=START\nerror: beta is broken in detail\nCITRUS_TARGET target=lib.beta status=FAIL exit=1\n",
    );
    project.write(
        "remote.sh",
        "echo 'ERROR remote gate failed'\necho 'CITRUS_LOG .scratch/detail.log'\nexit 2\n",
    );
    project.commit("runner");
    project.write("lib/x", "changed\n");
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(target(&run, "lib.alpha")["result"], "passed");
    assert!(
        target(&run, "lib.beta")["first_error"]
            .as_str()
            .unwrap()
            .contains("beta is broken in detail")
    );
    assert!(
        run["run"]["linked_log"]
            .as_str()
            .unwrap()
            .ends_with(".scratch/detail.log")
    );

    project.write(
        "remote.sh",
        "echo 'FAIL transport: descriptor mismatch for x'\nexit 2\n",
    );
    project.write("lib/x", "changed so the plan runs again\n");
    let (run, _) = project.json(&["run"]);
    assert_eq!(target(&run, "suite")["reason"], "suite_error");
    assert!(
        target(&run, "suite")["first_error"]
            .as_str()
            .unwrap()
            .contains("descriptor mismatch")
    );
}

/// The receipt fingerprint is a stable, documented format (docs/manifest.md):
/// this value was computed by an independent implementation of that spec.
#[test]
fn fingerprint_matches_the_documented_format() {
    let config = r#"#![toolchain(["rust-toolchain.toml", ".tool-versions"])]

"#;
    let project = Project::new(config);
    project.write("rust-toolchain.toml", "[toolchain]\nchannel = \"1\"\n");
    project.write("src/\u{e9}.txt", "unicode path\n");
    project.write("src/run.txt", "#!/bin/sh\n");
    let mut perms = fs::metadata(project.root().join("src/run.txt"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("src/run.txt"), perms).unwrap();
    assert_eq!(project.json(&["run", "ok"]).1, 0);
    let receipt = project
        .receipts()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    assert_eq!(
        receipt.to_string_lossy(),
        "ok-400096b708f41a065d8a51d3a379731e4504ee69d50adf1d9c9f993518257018.pass"
    );
    let (targets, _) = project.json(&["targets", "--fingerprints"]);
    let ok = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"] == "ok")
        .unwrap();
    assert_eq!(
        ok["fingerprint"],
        "400096b708f41a065d8a51d3a379731e4504ee69d50adf1d9c9f993518257018"
    );
}

#[test]
fn stats_count_reuse_and_saved_time() {
    let project = Project::new("");
    project.json(&["run", "ok"]);
    project.json(&["run", "ok"]);
    project.json(&["run", "fail"]);
    let (stats, _) = project.json(&["stats"]);
    let outcomes = stats["stats"]["outcomes"].as_array().unwrap();
    let count = |result: &str| {
        outcomes
            .iter()
            .find(|row| row[0] == result)
            .map_or(0, |row| row[1].as_i64().unwrap())
    };
    assert_eq!(
        (count("passed"), count("reused"), count("failed")),
        (1, 1, 1),
        "{stats}"
    );
    assert!(stats["stats"]["saved_seconds"].as_i64().unwrap() >= 0);
    assert_eq!(stats["stats"]["runs_by_agent"][0][0], "test");
}

#[test]
fn status_shows_resources_without_waiting_for_them() {
    let config = r#"#![runner(cmd!("true"), status = cmd!("sh res.sh"))]

"#;
    let project = Project::new(config);
    project.write("res.sh", "sleep 5\necho 'CITRUS_RESOURCE host=root@b1 state=busy operation=remote-test owner=agent-a elapsed_seconds=90'\necho 'other line'\n");
    let started = Instant::now();
    let (first, _) = project.json(&["status"]);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "status must not wait for the resource command"
    );
    assert_eq!(first["resources"]["refreshing"], true);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (status, _) = project.json(&["status"]);
        if let Some(item) = status["resources"]["items"]
            .as_array()
            .and_then(|items| items.first())
        {
            assert_eq!(item["owner"], "agent-a");
            assert_eq!(status["resources"]["items"].as_array().unwrap().len(), 1);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "resources never refreshed: {status}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn status_warns_when_sources_change_under_a_running_run() {
    let project = Project::new("");
    let (started, _) = project.json(&["run", "slow", "--detach"]);
    let id = started["run"]["id"].as_str().unwrap().to_owned();
    assert!(
        project.json(&["status"]).0["warnings"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    project.write("src/a.txt", "edited mid-run\n");
    let (status, _) = project.json(&["status"]);
    assert!(
        status["warnings"][0].as_str().unwrap().contains(&id),
        "{status}"
    );
    project.json(&["cancel", &id]);
}

#[test]
fn doctor_reports_setup_problems() {
    let project = Project::new("");
    let (healthy, code) = project.json(&["doctor"]);
    assert_eq!(code, 0, "{healthy}");
    assert_eq!(healthy["ok"], true);

    project.declare(
        r#"#![logs("logs")]

#[paths("missing/*")]
check ghost {
    run!("make ok")?;
}

"#,
    );
    let (broken, code) = project.json(&["doctor"]);
    assert_eq!(code, 1);
    let failed: Vec<&str> = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["status"] == "fail")
        .map(|finding| finding["check"].as_str().unwrap())
        .collect();
    assert!(failed.contains(&"check ghost"), "{broken}");
    assert!(failed.contains(&"logs"), "{broken}");
}

#[test]
fn bundled_examples_are_valid_configurations() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in fs::read_dir(&examples).unwrap() {
        let dir = entry.unwrap().path();
        // The example is the whole repository: its paths come from its files.
        let project = Project::new("");
        fs::remove_file(project.root().join("citrus.ci")).unwrap();
        copy_tree(&dir, project.root());
        project.commit("example");
        let (checked, code) = project.json(&["check"]);
        assert_eq!(code, 0, "{}: {checked}", dir.display());
        assert_eq!(
            checked["warnings"],
            serde_json::json!([]),
            "{}: {checked}",
            dir.display()
        );
    }
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

/// A bare "origin" next to the project, with the project's main pushed to it.
fn with_origin(project: &Project) -> PathBuf {
    let origin = project.root().with_extension("origin.git");
    assert!(
        Command::new("git")
            .args(["init", "-q", "--bare", "-b", "main"])
            .arg(&origin)
            .status()
            .unwrap()
            .success()
    );
    project.git(&["remote", "add", "origin", origin.to_str().unwrap()]);
    project.git(&["push", "-q", "-u", "origin", "main"]);
    origin
}

/// Commit `path` on origin/main from a separate clone.
fn commit_upstream(origin: &Path, path: &str, content: &str) {
    let clone = origin.with_extension(format!("clone-{}", path.replace('/', "-")));
    let _ = fs::remove_dir_all(&clone);
    assert!(
        Command::new("git")
            .args(["clone", "-q"])
            .arg(origin)
            .arg(&clone)
            .status()
            .unwrap()
            .success()
    );
    let file = clone.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, content).unwrap();
    let git = |args: &[&str]| {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(&clone)
                .status()
                .unwrap()
                .success()
        )
    };
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=u",
        "-c",
        "user.email=u@u",
        "commit",
        "-qm",
        "upstream",
    ]);
    git(&["push", "-q", "origin", "HEAD:main"]);
}

// Changes under web/ select `web`; it is not cached, so a pass is carried by integrate.
const WEB: &str = r#"#[paths("web/**")]
#[cache(false)]
check web {
    run!("make plain")?;
}

"#;

#[test]
fn integrate_keeps_checks_the_incoming_changes_do_not_touch() {
    let project = Project::new(WEB);
    let origin = with_origin(&project);
    project.git(&["checkout", "-q", "-b", "feature"]);
    project.write("web/page.txt", "feature\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "feature",
    ]);
    assert_eq!(target(&project.json(&["run"]).0, "web")["result"], "passed");

    commit_upstream(&origin, "other/y", "unrelated\n");
    let (merged, code) = project.json(&["integrate", "--no-run"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["integration"]["outcome"], "merged");
    assert_eq!(
        merged["integration"]["carried"],
        serde_json::json!(["web"]),
        "{merged}"
    );
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["targets"][0]["reason"], "carried_over", "{status}");

    commit_upstream(&origin, "web/other.txt", "touches web\n");
    let (merged, _) = project.json(&["integrate", "--no-run"]);
    assert_eq!(
        merged["integration"]["reselected"],
        serde_json::json!(["web"]),
        "{merged}"
    );
    assert_eq!(merged["integration"]["carried"], serde_json::json!([]));

    let (pushed, code) = project.json(&["integrate", "--push"]);
    assert_eq!(code, 0, "{pushed}");
    assert_eq!(pushed["pushed"], true);
    let remote_head = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&origin)
        .output()
        .unwrap()
        .stdout;
    let local_head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(project.root())
        .output()
        .unwrap()
        .stdout;
    assert_eq!(remote_head, local_head);
}

#[test]
fn integrate_reports_conflicts_and_needs_a_clean_tree() {
    let project = Project::new("");
    let origin = with_origin(&project);
    project.write("src/a.txt", "local\n");
    assert!(
        project.json(&["integrate"]).0["error"]
            .as_str()
            .unwrap()
            .contains("commit or stash")
    );
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "local",
    ]);
    commit_upstream(&origin, "src/a.txt", "upstream\n");
    let (conflict, code) = project.json(&["integrate"]);
    assert_eq!(code, 1);
    assert_eq!(conflict["integration"]["outcome"], "conflict");
    assert_eq!(
        conflict["integration"]["conflicts"],
        serde_json::json!(["src/a.txt"])
    );
}

#[test]
fn tasks_say_what_other_worktrees_do_and_need() {
    let project = Project::new("");
    let other = project.root().with_extension("second");
    project.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "second",
        other.to_str().unwrap(),
    ]);
    let citrus = |args: &[&str]| {
        citrus_command()
            // The machine's pool (~/.config/citrus/pool) is not the tests'.
            .env("CITRUS_POOL", "")
            .args(args)
            .arg("--json")
            .current_dir(&other)
            .env("CITRUS_AGENT", "agent-b")
            .env_remove("CLAUDECODE")
            .env_remove("CODEX_THREAD_ID")
            .output()
            .unwrap()
    };
    assert!(
        citrus(&["task", "Schema change", "--scope", "migrations"])
            .status
            .success()
    );
    // A blocker names both the blocked action and what it needs.
    assert!(!citrus(&["task", "--blocked", "deploy"]).status.success());
    assert!(
        citrus(&["task", "--blocked", "deploy", "--needs", "owner approval"])
            .status
            .success()
    );
    let (tasks, _) = project.json(&["tasks", "--all", "--base", "main"]);
    let second = tasks["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["branch"] == "second")
        .unwrap()
        .clone();
    assert_eq!(second["title"], "Schema change", "{tasks}");
    assert_eq!(second["scope"], "migrations");
    assert_eq!(second["needs"], "owner approval");
    assert_eq!(second["agent"], "agent-b");
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["tasks"][0]["blocked"], "deploy", "{status}");
    assert!(citrus(&["task", "--clear-blocker"]).status.success());
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["tasks"][0]["blocked"], "", "{status}");
    assert_eq!(status["tasks"][0]["title"], "Schema change");
}

#[test]
fn agreements_keep_revisions_and_refuse_stale_updates() {
    let project = Project::new("");
    let agree = |revision: &str, terms: &str| {
        project.json(&[
            "agree",
            "release-owner",
            "--terms",
            terms,
            "--reopen",
            "the owner changes",
            "--evidence",
            "thread 42",
            "--revision",
            revision,
        ])
    };
    let (first, code) = agree("0", "Task A releases the backend");
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["agreement"]["revision"], 1);
    // The same content again changes nothing.
    assert_eq!(
        agree("0", "Task A releases the backend").0["agreement"]["revision"],
        1
    );
    // A change must name the revision it read.
    let (stale, code) = agree("0", "Task B releases the backend");
    assert_ne!(code, 0);
    assert!(
        stale["error"].as_str().unwrap().contains("--revision 1"),
        "{stale}"
    );
    assert_eq!(
        agree("1", "Task B releases the backend").0["agreement"]["revision"],
        2
    );
    let (tasks, _) = project.json(&["tasks"]);
    assert_eq!(
        tasks["agreements"][0]["terms"], "Task B releases the backend",
        "{tasks}"
    );
    assert!(
        project
            .json(&[
                "agree",
                "Bad Key",
                "--terms",
                "t",
                "--reopen",
                "r",
                "--evidence",
                "e"
            ])
            .0["error"]
            .is_string()
    );
}

#[test]
fn overview_lists_commands_and_the_project_catalog() {
    let config = r#"#![command("release", "make deploy", "ship it")]

"#;
    let project = Project::new(config);
    let (overview, code) = project.json(&[]);
    assert_eq!(code, 0, "{overview}");
    assert!(
        overview["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["command"] == "citrus integrate [--push]")
    );
    assert_eq!(overview["catalog"][0]["command"], "make deploy");
    assert_eq!(overview["catalog"][0]["group"], "release");
    let (targets, _) = project.json(&["targets"]);
    assert_eq!(targets["targets"].as_array().unwrap().len(), 4);
}

const RELEASES: &str = r#"/// Production: one release at a time.
environment prod;

/// The test app.
#[environment(prod)]
#[version(initial = "1.0.0-app")]
release app {
    step build(r: Release) {
        run!("sh -c 'echo built {r.version}'")?;
    }

    #[production]
    #[recover(recover_app)]
    step deploy(r: Release) {
        run!("sh deploy.sh {r.version}")?;
    }

    #[production]
    rollback(r: Release) {
        run!("sh -c 'echo rolled back to {r.version} from {r.previous}'")?;
    }
}

fn recover_app(r: Release) -> Result<()> {
    run!("sh -c 'echo recovered {r.version}'")?;
    Ok(())
}

"#;

fn release_project() -> Project {
    let project = Project::new(&format!(
        "#![main(\"main\")]\n#![free_version(cmd!(\"sh free.sh\"))]\n{RELEASES}"
    ));
    // The registry: 1.0.5-app is already published.
    project.write(
        "free.sh",
        "if [ \"$CITRUS_VERSION\" = 1.0.5-app ]; then echo RELEASE=1.0.6-app; else echo RELEASE=$CITRUS_VERSION; fi\n",
    );
    project.write("deploy.sh", "if [ -f .fail ]; then echo 'Error: cluster unreachable'; exit 1; fi\nif [ -f .slow ]; then sleep 30; fi\necho deployed $1\n");
    project.write(".gitignore", ".scratch/\n.fail\n.slow\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "releases",
    ]);
    project
}

fn step<'a>(release: &'a Value, name: &str) -> &'a Value {
    release["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("no step {name} in {release}"))
}

#[test]
fn release_runs_steps_with_versions_and_gates() {
    let project = release_project();
    let refused = project.json(&["release", "start", "app"]).0;
    assert!(
        refused["error"].as_str().unwrap().contains("--approve"),
        "{refused}"
    );

    let (first, code) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["release"]["state"], "passed");
    assert_eq!(first["release"]["version"], "1.0.0-app");
    assert_eq!(step(&first, "deploy")["state"], "passed");
    let (second, _) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(second["release"]["version"], "1.0.1-app");
    assert_eq!(second["release"]["previous"], "1.0.0-app");

    let (back, code) = project.json(&["release", "rollback", "app", "--approve"]);
    assert_eq!(code, 0, "{back}");
    assert_eq!(back["release"]["kind"], "rollback");
    assert_eq!(back["release"]["version"], "1.0.0-app");
    let log = project.citrus(&[
        "release",
        "log",
        back["release"]["id"].as_str().unwrap(),
        "--full",
    ]);
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("rolled back to 1.0.0-app from 1.0.1-app")
    );

    // Checks gate: a change the plan selects must be proven first.
    project.git(&["checkout", "-q", "-b", "feature"]);
    project.write("src/a.txt", "changed\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "change",
    ]);
    let gated = project.json(&["release", "start", "app", "--approve"]).0;
    assert!(
        gated["error"]
            .as_str()
            .unwrap()
            .contains("checks not proven"),
        "{gated}"
    );
    assert_eq!(project.json(&["run"]).1, 0);
    assert_eq!(
        project.json(&["release", "start", "app", "--approve"]).0["release"]["state"],
        "passed"
    );
    project.write("src/a.txt", "dirty\n");
    assert!(
        project.json(&["release", "start", "app", "--approve"]).0["error"]
            .as_str()
            .unwrap()
            .contains("commit")
    );
}

#[test]
fn versions_belong_to_one_source_and_skip_taken_ones() {
    let project = release_project();
    let text = |args: &[&str]| {
        let output = project.citrus(&[args, &["--text"]].concat());
        (
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
            output.status.success(),
        )
    };
    let reserve = |start: &str, scope: &str| text(&["version", "reserve", start, "--scope", scope]);
    assert_eq!(reserve("1.0.0-app", "app-image").0, "1.0.0-app");
    // A retry of the same source gets its version back.
    assert_eq!(reserve("1.0.0-app", "app-image").0, "1.0.0-app");
    // Other images may use the same number; overlapping ones may not.
    assert_eq!(reserve("1.0.0-app", "docs-image").0, "1.0.0-app");
    assert_eq!(reserve("1.0.0-app", "web-image,app-image").0, "1.0.1-app");
    // free_version says what is published outside Citrus.
    assert_eq!(reserve("1.0.5-app", "app-image").0, "1.0.6-app");
    assert!(!reserve("one", "app-image").2);
    let head = || {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project.root())
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    };
    assert_eq!(text(&["version", "source", "1.0.0-app"]).0, head());
    assert!(text(&["version", "check", "1.0.0-app"]).2);

    project.write("src/a.txt", "changed\n");
    assert!(
        reserve("2.0.0", "app-image").1.contains("commit"),
        "a dirty source cannot hold a version"
    );
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "change",
    ]);
    let (_, error, ok) = text(&["version", "check", "1.0.0-app", "--scope", "app-image"]);
    assert!(!ok && error.contains("reserve a new version"), "{error}");
    assert!(
        text(&["version", "check", "9.9.9"]).2,
        "an unreserved version passes"
    );
    assert_eq!(reserve("1.0.0-app", "app-image").0, "1.0.2-app");
    let (list, _) = project.json(&["version", "list"]);
    assert_eq!(list["reservations"].as_array().unwrap().len(), 5, "{list}");

    // A release reserves its own version for its scope (the unit's name).
    assert_eq!(project.json(&["run"]).1, 0);
    let (release, code) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(code, 0, "{release}");
    assert_eq!(release["release"]["version"], "1.0.0-app");
    assert_eq!(text(&["version", "source", "1.0.0-app"]).0, head());
}

#[test]
fn parallel_reservations_of_overlapping_images_get_distinct_versions() {
    let project = release_project();
    let children: Vec<_> = (0..8)
        .map(|i| {
            citrus_command()
                // The machine's pool (~/.config/citrus/pool) is not the tests'.
                .env("CITRUS_POOL", "")
                .args([
                    "version",
                    "reserve",
                    "3.0.0",
                    "--scope",
                    &format!("shared,own-{i}"),
                    "--text",
                ])
                .current_dir(project.root())
                .env("CITRUS_AGENT", "test")
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let mut versions: Vec<String> = children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        })
        .collect();
    versions.sort();
    versions.dedup();
    assert_eq!(versions.len(), 8, "{versions:?}");
}

#[test]
fn a_check_whose_program_is_missing_fails_with_the_reason() {
    let project = Project::new(
        r#"#[paths("src/**")]
check gone {
    run!("no-such-program-citrus")?;
}

"#,
    );
    let (run, code) = project.json(&["run", "gone"]);
    assert_eq!(code, 1, "{run}");
    let target = &run["targets"][0];
    assert_eq!(target["result"], "failed", "{run}");
    assert!(
        target["first_error"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot run no-such-program-citrus"),
        "{run}"
    );
}

#[test]
fn failed_release_resumes_from_the_failed_step() {
    let project = release_project();
    project.write(".fail", "");
    let (failed, code) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(code, 1, "{failed}");
    assert_eq!(step(&failed, "build")["state"], "passed");
    assert!(
        step(&failed, "deploy")["first_error"]
            .as_str()
            .unwrap()
            .contains("cluster unreachable")
    );
    fs::remove_file(project.root().join(".fail")).unwrap();
    let id = failed["release"]["id"].as_str().unwrap().to_owned();
    let (resumed, code) = project.json(&["release", "resume", &id, "--approve"]);
    assert_eq!(code, 0, "{resumed}");
    assert_eq!(resumed["release"]["version"], "1.0.0-app");
    let log = String::from_utf8_lossy(&project.citrus(&["release", "log", &id, "--full"]).stdout)
        .into_owned();
    assert_eq!(
        log.lines()
            .filter(|line| *line == "built 1.0.0-app")
            .count(),
        1,
        "build must not repeat: {log}"
    );
}

#[test]
fn interrupted_release_keeps_the_environment_and_recovers() {
    let project = release_project();
    project.write(".slow", "");
    let (started, _) = project.json(&["release", "start", "app", "--approve", "--detach"]);
    let id = started["release"]["id"].as_str().unwrap().to_owned();
    // Wait until deploy runs, then kill the worker's whole process group.
    let deadline = Instant::now() + Duration::from_secs(15);
    let pid = loop {
        let (shown, _) = project.json(&["release", "show", &id]);
        if step(&shown, "deploy")["state"] == "running" {
            break shown["release"]["pid"].as_i64().unwrap();
        }
        assert!(Instant::now() < deadline, "deploy never started: {shown}");
        std::thread::sleep(Duration::from_millis(200));
    };
    // SAFETY: test-only signal to the release worker's process group.
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(300));
    let (shown, code) = project.json(&["release", "show", &id]);
    assert_eq!(shown["release"]["state"], "unknown", "{shown}");
    assert_eq!(code, 3);
    let blocked = project.json(&["release", "start", "app", "--approve"]).0;
    assert!(
        blocked["error"].as_str().unwrap().contains("is held by"),
        "{blocked}"
    );

    fs::remove_file(project.root().join(".slow")).unwrap();
    let (resumed, code) = project.json(&["release", "resume", &id, "--approve"]);
    assert_eq!(code, 0, "{resumed}");
    assert_eq!(step(&resumed, "deploy")["state"], "recovered");
    let log = String::from_utf8_lossy(&project.citrus(&["release", "log", &id, "--full"]).stdout)
        .into_owned();
    assert!(
        log.contains("recovered 1.0.0-app") && !log.contains("deployed 1.0.0-app"),
        "{log}"
    );

    // Abandon frees the environment of a release given up by hand.
    project.write(".slow", "");
    let (started, _) = project.json(&["release", "start", "app", "--approve", "--detach"]);
    let other = started["release"]["id"].as_str().unwrap().to_owned();
    let pid = project.json(&["release", "show", &other]).0["release"]["pid"]
        .as_i64()
        .unwrap();
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    std::thread::sleep(Duration::from_millis(300));
    let (abandoned, _) =
        project.json(&["release", "abandon", &other, "--reason", "checked by hand"]);
    assert_eq!(abandoned["release"]["state"], "abandoned", "{abandoned}");
    fs::remove_file(project.root().join(".slow")).unwrap();
    let (after, _) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(after["release"]["state"], "passed", "{after}");
}

#[test]
fn integrate_runs_the_after_merge_hook() {
    let config = r#"#![after_merge(cmd!("sh -c 'mkdir -p .scratch && echo $0 > .scratch/hook' {{before}}"))]

"#;
    let project = Project::new(config);
    let origin = with_origin(&project);
    let before = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project.root())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    commit_upstream(&origin, "docs/new.md", "x\n");
    let (merged, code) = project.json(&["integrate", "--no-run"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(
        fs::read_to_string(project.root().join(".scratch/hook"))
            .unwrap()
            .trim(),
        before.trim()
    );

    project.write(
        "citrus.ci",
        &format!("{BASE}\n#![after_merge(cmd!(\"false\"))]\n"),
    );
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "failing hook",
    ]);
    commit_upstream(&origin, "docs/other.md", "y\n");
    let failed = project.json(&["integrate"]).0;
    assert!(
        failed["error"]
            .as_str()
            .unwrap()
            .contains("after_merge failed"),
        "{failed}"
    );
}

#[test]
fn version_names_the_source_commit() {
    let output = citrus_command()
        // The machine's pool (~/.config/citrus/pool) is not the tests'.
        .env("CITRUS_POOL", "")
        .arg("--version")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    let commit = text
        .trim()
        .rsplit_once(" (")
        .map(|(_, rest)| rest.trim_end_matches(')'))
        .unwrap_or_default();
    assert!(!commit.is_empty() && commit != "unknown", "{text}");
}

#[test]
fn logs_and_state_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let project = Project::new(
        r#"#![logs(".private/logs")]

"#,
    );
    project.write(".gitignore", ".scratch/\n.private/\n");
    project.json(&["run", "ok"]);
    for dir in [".private", ".private/logs", ".git/citrus"] {
        let mode = fs::metadata(project.root().join(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "{dir}");
    }
}

const KUBECTL: &str = r#"#!/bin/sh
release=$(cat .citrus-release 2>/dev/null || echo 1.0.0)
ready=$(cat .citrus-ready 2>/dev/null || echo 1)
cat <<JSON
{"metadata": {"generation": 3, "annotations": {"example.com/release": "$release"}},
 "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": "api", "image": "registry.example/api@sha256:aaaa"}]}}},
 "status": {"observedGeneration": 3, "readyReplicas": $ready}}
JSON
"#;

#[test]
fn a_release_continues_from_what_the_environment_runs() {
    let project = Project::new("");
    project.declare(
        r#"#[inputs(["src/**"])]
artifact api_image;

#[kubernetes(kubectl = "./kubectl.sh", namespace = "shop")]
#[record(annotation = "example.com/release")]
#[deploy("api", api_image)]
environment prod;

#[environment(prod)]
#[checks(none)]
#[version(initial = "0.1.0")]
release api {
    step deploy(r: Release) {
        run!("true")?;
    }
}

"#,
    );
    project.write("kubectl.sh", KUBECTL);
    let mut perms = fs::metadata(project.root().join("kubectl.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("kubectl.sh"), perms).unwrap();
    // Released by other means: Citrus has no history of it.
    project.write(".citrus-release", "1.4.2");
    project.write(".gitignore", ".scratch/\n.citrus-release\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "release",
    ]);
    let (plan, code) = project.json(&["release", "start", "api", "--dry-run"]);
    assert_eq!(code, 0, "{plan}");
    assert_eq!(plan["dry_run"]["previous"], "1.4.2", "{plan}");
    assert_eq!(plan["dry_run"]["next_version"], "1.4.3", "{plan}");
}

#[test]
fn diff_compares_what_runs_with_what_head_would_build() {
    let project = Project::new("");
    project.declare(
        r#"#[inputs(["src/**"])]
artifact api;

#[kubernetes(kubectl = "./kubectl.sh", namespace = "shop")]
#[record(annotation = "example.com/release", tag_prefix = "v")]
#[deploy("api", api)]
environment prod;

"#,
    );
    project.write("kubectl.sh", KUBECTL);
    let mut perms = fs::metadata(project.root().join("kubectl.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("kubectl.sh"), perms).unwrap();
    project.write(".gitignore", ".scratch/\n.citrus-release\n.citrus-ready\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "deploy config",
    ]);
    project.git(&["tag", "v1.0.0"]);

    let (same, code) = project.json(&["diff", "prod"]);
    assert_eq!(code, 0, "{same}");
    assert_eq!(same["diff"]["found_by"], "tag v1.0.0");
    assert_eq!(same["diff"]["workloads"][0]["change"], "unchanged");
    assert_eq!(
        same["diff"]["workloads"][0]["running_digest"],
        "sha256:aaaa"
    );
    assert_eq!(same["diff"]["actions"], serde_json::json!([]));

    project.write("docs.md", "outside the artifact\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "docs",
    ]);
    assert_eq!(
        project.json(&["diff", "prod"]).0["diff"]["workloads"][0]["change"],
        "unchanged"
    );

    project.write("src/a.txt", "new behaviour\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "change",
    ]);
    let (changed, _) = project.json(&["diff", "prod"]);
    assert_eq!(changed["diff"]["workloads"][0]["change"], "changed");
    assert_eq!(
        changed["diff"]["workloads"][0]["changed_inputs"],
        serde_json::json!(["src/a.txt"])
    );
    assert_eq!(changed["diff"]["actions"].as_array().unwrap().len(), 2);
    let (artifacts, _) = project.json(&["artifacts", "--at", "v1.0.0"]);
    assert_ne!(
        artifacts["artifacts"][0]["key"],
        changed["diff"]["workloads"][0]["desired_key"]
    );

    project.write(".citrus-release", "9.9.9");
    project.write(".citrus-ready", "0");
    let (unknown, _) = project.json(&["diff", "prod"]);
    assert_eq!(unknown["diff"]["workloads"][0]["change"], "unknown");
    assert!(
        unknown["diff"]["actions"][0]
            .as_str()
            .unwrap()
            .contains("not ready"),
        "{unknown}"
    );
}

#[test]
fn artifact_inputs_can_come_from_a_command() {
    let project = Project::new("");
    project.declare(
        r#"#[inputs(cmd!("sh -c 'echo src/a.txt; echo Makefile'"))]
artifact api;

#[kubernetes(kubectl = "./kubectl.sh")]
#[record(annotation = "example.com/release", tag_prefix = "v")]
#[deploy("api", api)]
environment prod;

"#,
    );
    project.write("kubectl.sh", KUBECTL);
    let mut perms = fs::metadata(project.root().join("kubectl.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("kubectl.sh"), perms).unwrap();
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "config",
    ]);
    project.git(&["tag", "v1.0.0"]);
    project.write("other/x", "not an input\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "other",
    ]);
    assert_eq!(
        project.json(&["diff", "prod"]).0["diff"]["workloads"][0]["change"],
        "unchanged"
    );
    project.write("Makefile", &MAKEFILE.replace("echo fine", "echo changed"));
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "make",
    ]);
    let (changed, _) = project.json(&["diff", "prod"]);
    assert_eq!(
        changed["diff"]["workloads"][0]["changed_inputs"],
        serde_json::json!(["Makefile"]),
        "{changed}"
    );
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            r#"#[inputs(cmd!("sh -c 'echo src/a.txt; echo Makefile'"))]"#,
            "#[inputs([])]",
        ),
    );
    let error = project.json(&["artifacts"]).0;
    assert!(
        error["error"].as_str().unwrap().contains("#[inputs("),
        "{error}"
    );
}

const FAKE_KUBECTL: &str = r#"#!/usr/bin/env python3
import json, os, sys
state_path = ".kube/state.json"
state = json.load(open(state_path))
args = [a for a in sys.argv[1:]]
# drop global flags
while args and args[0].startswith("--"):
    args = args[2:]
open(".kube/calls", "a").write(" ".join(args) + "\n")
def save():
    json.dump(state, open(state_path, "w"))
verb = args[0]
if verb == "get" and args[1] == "deployment":
    d = state["deployments"][args[2]]
    print(json.dumps({"metadata": {"generation": 1, "annotations": d["annotations"]},
        "spec": {"replicas": 1, "template": {"spec": {"containers": [{"name": args[2], "image": d["image"]}]}}},
        "status": {"observedGeneration": 1, "readyReplicas": 1}}))
elif verb == "get" and args[1] == "cronjob" and "jsonpath={.spec.suspend}" in args:
    print("true" if state["cronjobs"][args[2]]["suspend"] else "false")
elif verb == "get" and args[1] == "cronjob":
    c = state["cronjobs"][args[2]]
    print(json.dumps({"metadata": {"annotations": c["annotations"]},
        "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": [{"name": args[2], "image": c["image"]}]}}}}}}))
elif verb == "get" and args[1] == "lease":
    print(state["lease"])
elif verb == "patch":
    kind, name, patch = args[1], args[2], json.loads(args[args.index("-p") + 1])
    target = state["deployments" if kind == "deployment" else "cronjobs"][name]
    if "suspend" in patch.get("spec", {}):
        target["suspend"] = patch["spec"]["suspend"]
    target["annotations"].update(patch.get("metadata", {}).get("annotations", {}))
    spec = patch.get("spec", {})
    pod = spec.get("template", {}).get("spec") or spec.get("jobTemplate", {}).get("spec", {}).get("template", {}).get("spec")
    if pod:
        target["image"] = pod["containers"][0]["image"]
        if kind == "deployment":
            state["lease"] = name + "-new-" + target["image"][-6:]
    save()
elif verb == "rollout":
    sys.exit(1 if os.path.exists(".kube/fail-rollout") else 0)
elif verb == "create" and "--dry-run=client" in args:
    if "-k" in args:
        # kubectl prints several objects one after another, not a List.
        for item in json.load(open(os.path.join(args[args.index("-k") + 1], "all.json")))["items"]:
            print(json.dumps(item, indent=2))
    else:
        print(open(args[args.index("-f") + 1]).read())
elif verb == "apply":
    manifest = sys.stdin.read()
    try:
        parsed = json.loads(manifest)
    except ValueError:
        parsed = None
    if parsed and parsed.get("kind") == "List":
        state.setdefault("applied", []).append(parsed)
        for item in parsed["items"]:
            if item["kind"] == "Deployment":
                target = state["deployments"][item["metadata"]["name"]]
                target["image"] = item["spec"]["template"]["spec"]["containers"][0]["image"]
                # Like kubectl apply: what the applied set leaves out goes away.
                target["annotations"] = dict(item["metadata"].get("annotations", {}))
                state["lease"] = item["metadata"]["name"] + "-new-" + target["image"][-6:]
    else:
        state["jobs"].append(manifest)
    save()
elif verb == "wait":
    sys.exit(0 if state["jobs"] else 1)
else:
    sys.exit("unsupported: " + " ".join(args))
"#;

fn apply_project() -> Project {
    let project = Project::new("");
    project.declare(
        r#"
/// The API image; builds are logged in .kube/builds.
#[inputs("src/**")]
#[build(provider = "command", run = cmd!("sh -c 'echo {{artifact}} >> .kube/builds; echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact api;

/// The backup job's image.
#[inputs("other/**")]
#[build(provider = "command", run = cmd!("sh -c 'echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact backup;

/// Database migrations, run as a Job.
#[inputs("migrations/**")]
#[build(provider = "command", run = cmd!("sh -c 'echo {{artifact}} >> .kube/builds; echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact migrations;

#[kubernetes(kubectl = "./kubectl.py", namespace = "shop")]
#[record(annotation = "example.com/release", tag_prefix = "v")]
#[migrations(artifact = migrations, job = "job.yaml")]
#[deploy("api", api, fence = "api-lease", timeout = 10s, version_env = "APP_VERSION")]
#[deploy("backup", backup, kind = "cronjob", quiesce = true)]
environment prod;
"#,
    );
    project.write(
        "job.yaml",
        "kind: Job\nmetadata: {name: \"{name}\"}\nimage: \"{image}\"\n",
    );
    project.write("migrations/1.sql", "create table t ();\n");
    project.write("kubectl.py", FAKE_KUBECTL);
    let mut perms = fs::metadata(project.root().join("kubectl.py"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("kubectl.py"), perms).unwrap();
    project.write(".gitignore", ".scratch/\n.kube/\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "deploy config",
    ]);
    project.git(&["tag", "v1.0.0"]);
    let state = serde_json::json!({
        "deployments": {"api": {"image": "registry.example/api@sha256:old", "annotations": {"example.com/release": "1.0.0"}}},
        "cronjobs": {"backup": {"image": "registry.example/backup@sha256:old", "annotations": {}, "suspend": false}},
        "lease": "api-old", "jobs": []
    });
    project.write(".kube/state.json", &state.to_string());
    project
}

fn kube_state(project: &Project) -> Value {
    serde_json::from_str(&fs::read_to_string(project.root().join(".kube/state.json")).unwrap())
        .unwrap()
}

#[test]
fn apply_rolls_a_workload_from_its_manifest_with_the_built_image() {
    let project = apply_project();
    project.declare(
        r#"
/// The API image and its manifest.
#[inputs("src/**", "k8s/api.json")]
#[build(provider = "command", run = cmd!("sh -c 'echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact web;

#[kubernetes(kubectl = "./kubectl.py", namespace = "shop")]
#[deploy("api", web, manifest = "k8s/api.json", version_env = "APP_VERSION")]
environment staging;
"#,
    );
    // kubectl reads YAML or JSON; the fake one reads JSON.
    project.write(
        "k8s/api.json",
        r#"{"kind": "List", "items": [
  {"kind": "Service", "metadata": {"name": "api"}},
  {"kind": "Deployment", "metadata": {"name": "api"}, "spec": {"template": {"spec": {"initContainers": [{"name": "prepare", "image": "placeholder"}], "containers": [
    {"name": "api", "image": "placeholder", "env": [{"name": "LIMIT", "value": "5"}]}]}}}}
]}"#,
    );
    project.write("src/a.txt", "v2\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "manifest",
    ]);
    let (applied, code) = project.json(&["apply", "staging", "--approve"]);
    assert_eq!(code, 0, "{applied}");
    let state = kube_state(&project);
    let api = &state["deployments"]["api"];
    assert!(
        api["image"]
            .as_str()
            .unwrap()
            .starts_with("registry.example/web@sha256:"),
        "{state}"
    );
    assert!(
        api["annotations"]["citrus.dev/commit"].is_string(),
        "{state}"
    );
    // The whole file was applied: its Service too, the spec's own variables kept.
    let items = &state["applied"][0]["items"];
    assert_eq!(items[0]["kind"], "Service", "{state}");
    let env = &items[1]["spec"]["template"]["spec"]["containers"][0]["env"];
    assert_eq!(env[0]["name"], "LIMIT", "{state}");
    assert_eq!(env[1]["name"], "APP_VERSION", "{state}");
    // An init container of the same image takes the build too.
    let init = &items[1]["spec"]["template"]["spec"]["initContainers"][0]["image"];
    assert!(
        init.as_str()
            .unwrap()
            .starts_with("registry.example/web@sha256:"),
        "{state}"
    );
}

#[test]
fn apply_applies_the_environment_manifests_keeping_what_does_not_roll() {
    let project = apply_project();
    project.declare(
        r#"
#[inputs("src/**")]
#[build(provider = "command", run = cmd!("sh -c 'echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact web;

#[inputs("other/**")]
#[build(provider = "command", run = cmd!("sh -c 'echo IMAGE=registry.example/{{artifact}}@sha256:{{key}}'"))]
artifact work;

#[kubernetes(kubectl = "./kubectl.py", namespace = "shop")]
#[manifests("k8s")]
#[deploy("api", web)]
#[deploy("worker", work)]
environment gitops;
"#,
    );
    let deployment = |name: &str| {
        format!(
            r#"{{"kind": "Deployment", "metadata": {{"name": "{name}"}}, "spec": {{"template": {{"spec": {{"containers": [{{"name": "{name}", "image": "registry.example/{name}@sha256:stale-pin"}}]}}}}}}}}"#
        )
    };
    project.write(
        "k8s/all.json",
        &format!(
            r#"{{"kind": "List", "items": [{}, {}]}}"#,
            deployment("api"),
            deployment("worker")
        ),
    );
    project.write("other/w.txt", "1\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "gitops",
    ]);
    let mut state = kube_state(&project);
    state["deployments"]["worker"] =
        serde_json::json!({"image": "registry.example/work@sha256:running", "annotations": {}});
    project.write(".kube/state.json", &state.to_string());
    let (first, code) = project.json(&["apply", "gitops", "--approve"]);
    assert_eq!(code, 0, "{first}");

    // Only the API changes: the worker keeps the image it runs, not the pin.
    project.write("src/a.txt", "v9\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "api only",
    ]);
    project.write(".kube/state.json", &{
        let mut state = kube_state(&project);
        state["applied"] = serde_json::json!([]);
        state.to_string()
    });
    let (applied, code) = project.json(&["apply", "gitops", "--approve"]);
    assert_eq!(code, 0, "{applied}");
    let state = kube_state(&project);
    let items = &state["applied"][0]["items"];
    let image = |index: usize| {
        items[index]["spec"]["template"]["spec"]["containers"][0]["image"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(
        image(0).starts_with("registry.example/web@sha256:"),
        "{state}"
    );
    // Both rolled in the first apply; each kept its records through the
    // other's apply of the whole set.
    for name in ["api", "worker"] {
        assert!(
            state["deployments"][name]["annotations"]["citrus.dev/commit"].is_string(),
            "{state}"
        );
    }
    let worker = image(1);
    assert!(
        worker.starts_with("registry.example/work@sha256:") && !worker.ends_with("stale-pin"),
        "{state}"
    );
}

#[test]
fn an_artifact_builds_alone_once_per_inputs() {
    let project = apply_project();
    project.write("src/a.txt", "v1\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "src",
    ]);
    let (built, code) = project.json(&["artifacts", "--build", "api"]);
    assert_eq!(code, 0, "{built}");
    assert!(
        built["reference"]
            .as_str()
            .unwrap()
            .starts_with("registry.example/api@sha256:"),
        "{built}"
    );
    let (again, _) = project.json(&["artifacts", "--build", "api"]);
    assert_eq!(again["reference"], built["reference"]);
    let builds = fs::read_to_string(project.root().join(".kube/builds")).unwrap();
    assert_eq!(builds.lines().count(), 1, "{builds}");
}

#[test]
fn apply_builds_by_key_rolls_by_digest_and_records_the_commit() {
    let project = apply_project();
    project.write("src/a.txt", "v2\n");
    project.write("migrations/2.sql", "alter table t;\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "feature",
    ]);
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(project.root())
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_owned();

    assert!(
        project.json(&["apply", "prod"]).0["error"]
            .as_str()
            .unwrap()
            .contains("--approve")
    );
    assert!(
        project
            .json(&["apply", "prod", "--approve", "--plan", "deadbeef"])
            .0["error"]
            .as_str()
            .unwrap()
            .contains("plan changed")
    );
    let plan = project.json(&["diff", "prod"]).0["diff"]["plan_hash"]
        .as_str()
        .unwrap()[..12]
        .to_owned();
    let (applied, code) = project.json(&["apply", "prod", "--approve", "--plan", &plan]);
    assert_eq!(code, 0, "{applied}");
    let steps: Vec<&str> = applied["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|step| step["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        steps,
        [
            "build:api",
            "build:migrations",
            // The unproven check runs while the images build; nothing
            // changes before it passed.
            "gate",
            "quiesce",
            "migrate",
            "roll:api",
            "resume",
            "verify"
        ],
        "{applied}"
    );

    let state = kube_state(&project);
    let api = &state["deployments"]["api"];
    assert!(
        api["image"]
            .as_str()
            .unwrap()
            .starts_with("registry.example/api@sha256:")
    );
    assert_eq!(api["annotations"]["citrus.dev/commit"], head.as_str());
    assert_eq!(state["cronjobs"]["backup"]["suspend"], false);
    assert!(
        state["jobs"][0]
            .as_str()
            .unwrap()
            .contains("registry.example/migrations@sha256:")
    );
    let calls = fs::read_to_string(project.root().join(".kube/calls")).unwrap();
    let suspend = calls.find(r#"{"spec":{"suspend":true}}"#).unwrap();
    let roll = calls.find("patch deployment api").unwrap();
    // The container learns its release (the name apply records).
    assert!(calls.contains(r#""name":"APP_VERSION""#), "{calls}");
    let resume = calls.find(r#"{"spec":{"suspend":false}}"#).unwrap();
    assert!(suspend < roll && roll < resume, "{calls}");

    let (diff, _) = project.json(&["diff", "prod"]);
    assert_eq!(diff["diff"]["found_by"], "annotation citrus.dev/commit");
    assert_eq!(diff["diff"]["workloads"][0]["change"], "unchanged");
    assert_eq!(
        project.json(&["apply", "prod", "--approve"]).0["state"],
        "unchanged"
    );

    // Back to earlier inputs: the image built from them is reused, not rebuilt.
    let builds = fs::read_to_string(project.root().join(".kube/builds"))
        .unwrap()
        .lines()
        .count();
    project.write("src/a.txt", "v3\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "v3",
    ]);
    assert_eq!(project.json(&["apply", "prod", "--approve"]).1, 0);
    project.write("src/a.txt", "v2\n");
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "back to v2",
    ]);
    assert_eq!(project.json(&["apply", "prod", "--approve"]).1, 0);
    let after = fs::read_to_string(project.root().join(".kube/builds"))
        .unwrap()
        .lines()
        .count();
    assert_eq!(after, builds + 1, "only v3 was new");
}

#[test]
fn a_failed_apply_resumes_quiesced_work_and_frees_the_environment() {
    let project = apply_project();
    project.write("src/a.txt", "v2\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "feature",
    ]);
    project.write(".kube/fail-rollout", "");
    let (failed, code) = project.json(&["apply", "prod", "--approve"]);
    assert_eq!(code, 1, "{failed}");
    assert_eq!(step(&failed, "roll:api")["state"], "failed");
    assert_eq!(
        kube_state(&project)["cronjobs"]["backup"]["suspend"],
        false,
        "cronjob must not stay suspended"
    );
    fs::remove_file(project.root().join(".kube/fail-rollout")).unwrap();
    let id = failed["release"]["id"].as_str().unwrap().to_owned();
    let (resumed, code) = project.json(&["release", "resume", &id, "--approve"]);
    assert_eq!(code, 0, "{resumed}");
    assert_eq!(step(&resumed, "build:api")["state"], "passed");
}

#[test]
fn a_worker_that_dies_without_a_result_does_not_hang_wait() {
    // The worker of this release has no unit to run and exits at once.
    let project = release_project();
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            "#[environment(prod)]",
            "#[environment(prod)]\n#[checks(none)]",
        ),
    );
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qam",
        "gate off",
    ]);
    let (started, _) = project.json(&["release", "start", "app", "--approve", "--detach"]);
    let id = started["release"]["id"].as_str().unwrap().to_owned();
    fs::write(project.root().join("citrus.ci"), BASE).unwrap();
    let begun = Instant::now();
    let (waited, _) = project.json(&["release", "wait", &id]);
    assert!(
        begun.elapsed() < Duration::from_secs(20),
        "wait hung: {waited}"
    );
}

#[test]
fn an_identical_image_is_recorded_without_touching_the_pod_template() {
    let project = apply_project();
    // A change in the artifact's inputs whose build yields the image already running.
    project.write("src/a.txt", "v2\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "feature",
    ]);
    let key = project.json(&["diff", "prod"]).0["diff"]["workloads"][0]["desired_key"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut state = kube_state(&project);
    state["deployments"]["api"]["image"] =
        Value::String(format!("registry.example/api@sha256:{key}"));
    project.write(".kube/state.json", &state.to_string());
    let (applied, code) = project.json(&["apply", "prod", "--approve", "--unchecked"]);
    assert_eq!(code, 0, "{applied}");
    let calls = fs::read_to_string(project.root().join(".kube/calls")).unwrap();
    let patch = calls
        .lines()
        .find(|line| line.starts_with("patch deployment api"))
        .unwrap();
    assert!(!patch.contains("template"), "{patch}");
}

fn ci_project(source: &str) -> Project {
    let project = Project::new("");
    project.write("citrus.ci", source);
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "citrus.ci",
    ]);
    project
}

const CI: &str = r#"#![citrus(2)]
#![main("main")]

/// `src/{name}.txt` says ok.
fn says_ok(name: str) -> Result<()> {
    let text = std::fs::read("src/{name}.txt")?;
    assert text.contains("ok"), "Error: src/{name}.txt is not ok";
}

#[paths("src/a.txt")]
check check_a {
    says_ok("a")?;
}

#[paths("src/b.txt")]
check check_b {
    says_ok("b")?;
}

/// Copy a file once it exists.
task prepare {
    std::wait::file("ready.txt", 5s)?;
    std::fs::copy("ready.txt", "out/copied.txt")?;
}
"#;

#[test]
fn checks_declared_in_citrus_ci_run_their_steps_and_are_reused() {
    let project = ci_project(CI);
    project.write("src/a.txt", "ok\n");
    project.write("src/b.txt", "bad\n");
    let (run, code) = project.json(&["run", "check-a", "check-b"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(target(&run, "check-a")["result"], "passed");
    assert!(
        target(&run, "check-b")["first_error"]
            .as_str()
            .unwrap()
            .contains("src/b.txt is not ok"),
        "{run}"
    );
    let (again, _) = project.json(&["run", "check-a"]);
    assert_eq!(target(&again, "check-a")["result"], "reused");

    // Changing what a check runs, here a function it calls, invalidates its earlier pass.
    project.write(
        "citrus.ci",
        &CI.replace("text.contains(\"ok\")", "text.trim().contains(\"ok\")"),
    );
    let (changed, _) = project.json(&["run", "check-a"]);
    assert_eq!(target(&changed, "check-a")["result"], "passed", "{changed}");

    let (checked, code) = project.json(&["check"]);
    assert_eq!(code, 0, "{checked}");
    assert_eq!(checked["checks"], 2);
    assert_eq!(checked["tasks"], 1);
}

#[test]
fn a_shell_brace_in_a_string_explains_interpolation() {
    let project = ci_project(
        "#![citrus(2)]\n#[paths(\"src/**\")]\ncheck x {\n    run!(\"sh -c 'a || { b; }'\")?;\n}\n",
    );
    let message = project.json(&["status"]).0["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("write `{{`"), "{message}");
}

#[test]
fn an_error_in_citrus_ci_points_at_the_line() {
    let project = ci_project(
        "#![citrus(2)]\n#[paths(\"src/**\")]\ncheck x {\n    run!(\"make x\")?;\n}\n#[pths(\"src/**\")]\ncheck y {\n    run!(\"make y\")?;\n}\n",
    );
    let (error, code) = project.json(&["status"]);
    assert_eq!(code, 2);
    let message = error["error"].as_str().unwrap();
    assert!(
        message.contains("citrus.ci:6:1") && message.contains("did you mean `#[paths]`?"),
        "{message}"
    );
}

#[test]
fn tasks_run_built_in_steps_without_a_shell() {
    let project = ci_project(CI);
    project.write("ready.txt", "hello\n");
    let (done, code) = project.json(&["do", "prepare"]);
    assert_eq!(code, 0, "{done}");
    assert_eq!(
        fs::read_to_string(project.root().join("out/copied.txt")).unwrap(),
        "hello\n"
    );
    fs::remove_file(project.root().join("ready.txt")).unwrap();
    let (failed, code) = project.json(&["do", "prepare"]);
    assert_eq!(code, 1);
    assert_eq!(failed["source"], "citrus.ci:20");
    assert!(
        project.json(&["do", "prepar"]).0["error"]
            .as_str()
            .unwrap()
            .contains("did you mean prepare")
    );
}

#[test]
fn doctor_flags_a_leftover_toml_configuration() {
    let project = ci_project(CI);

    project.write("citrus.toml", "[run]\n");
    let (doctor, _) = project.json(&["doctor"]);
    let warn = doctor["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["check"] == "citrus.toml")
        .unwrap();
    assert_eq!(warn["status"], "warn", "{doctor}");
}

#[test]
fn an_edited_declaration_selects_its_check() {
    let project = Project::new(
        r#"#![main("main")]

"#,
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            r#"check fail {
    run!("make fail")?;
}

"#,
            r#"check fail {
    run!("make plain")?;
}

"#,
        ),
    );
    project.commit("fail runs plain now");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["fail"]),
        "{plan}"
    );
    assert_eq!(plan["plan"]["unmapped"], serde_json::json!([]), "{plan}");

    // A change outside checks maps the file to the configuration, not to every check.
    project.declare("#![command(\"misc\", \"make deploy\", \"ship it\")]\n");
    project.commit("catalog");
    project.write(
        "citrus.ci",
        &fs::read_to_string(project.root().join("citrus.ci"))
            .unwrap()
            .replace(
                r#"check fail {
    run!("make plain")?;
}

"#,
                r#"check fail {
    run!("make fail")?;
}

"#,
            ),
    );
    project.commit("back");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["plan"]["targets"], serde_json::json!([]), "{plan}");
    assert_eq!(plan["plan"]["mapped"][0][1], "config", "{plan}");
}

#[test]
fn project_tools_read_the_declared_checks() {
    let project = Project::new(
        r#"#![runner(cmd!("sh remote.sh"))]

#[paths("web/**")]
#[meta(linux = true, snapshot = ["assets"])]
check e2e {
    run!("make plain")?;
}

"#,
    );
    project.write("remote.sh", "mkdir -p .scratch && cp \"$CITRUS_CHECKS\" .scratch/checks.json\necho 'CITRUS_TARGET target=e2e status=PASS exit=0'\n");
    project.commit("runner");
    project.write("web/x", "x\n");
    let (run, code) = project.json(&["run", "--remote"]);
    assert_eq!(code, 0, "{run}");
    let checks: Value = serde_json::from_str(
        &fs::read_to_string(project.root().join(".scratch/checks.json")).unwrap(),
    )
    .unwrap();
    let e2e = checks["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["target"] == "e2e")
        .unwrap();
    assert_eq!(e2e["meta"]["linux"], true);
    assert_eq!(
        e2e["declaration"]["run"],
        serde_json::json!([["run", "make", "plain"]])
    );
    assert_eq!(checks["files"], serde_json::json!(["citrus.ci"]));
    let (targets, _) = project.json(&["targets"]);
    assert!(
        targets["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["source"] == "citrus.ci:27"),
        "{targets}"
    );
}

#[test]
fn an_artifact_ignores_dockerfile_stages_it_is_not_built_from() {
    let project = Project::new(
        r#"#[inputs(["Dockerfile"])]
#[dockerfile("Dockerfile", target = "api")]
artifact api;

"#,
    );
    let dockerfile = "ARG V=1\nFROM a AS build\nRUN make\nFROM b AS other\nRUN other\nFROM c AS api\nCOPY --from=build /x /x\n";
    project.write("Dockerfile", dockerfile);
    project.commit("dockerfile");
    let key = |project: &Project| project.json(&["artifacts"]).0["artifacts"][0]["key"].clone();
    let before = key(&project);
    project.write(
        "Dockerfile",
        &dockerfile.replace("RUN other", "RUN changed"),
    );
    project.commit("other stage");
    assert_eq!(
        key(&project),
        before,
        "another stage must not change the key"
    );
    project.write(
        "Dockerfile",
        &dockerfile.replace("RUN make", "RUN make all"),
    );
    project.commit("build stage");
    assert_ne!(
        key(&project),
        before,
        "a stage the target copies from must change the key"
    );
}

#[test]
fn release_steps_run_built_in_actions_with_a_version_given_by_hand() {
    let project = Project::new(
        r#"/// Where the site is published.
environment web;

#[environment(web)]
#[checks(none)]
release site {
    step build(r: Release) {
        std::fs::copy("src/a.txt", "out/{r.version}.txt")?;
        std::docs::check_links("*.md")?;
    }
}

"#,
    );
    project.write(".gitignore", ".scratch/\nout/\n");
    project.write("README.md", "[source](src/a.txt)\n");
    project.commit("site");
    let refused = project.json(&["release", "start", "site"]).0;
    assert!(
        refused["error"].as_str().unwrap().contains("--version"),
        "{refused}"
    );
    let (released, code) = project.json(&["release", "start", "site", "--version", "2.0.0"]);
    assert_eq!(code, 0, "{released}");
    assert_eq!(released["release"]["version"], "2.0.0", "{released}");
    assert!(project.root().join("out/2.0.0.txt").exists());

    project.write("README.md", "[gone](src/missing.txt)\n");
    project.commit("broken link");
    let (failed, code) = project.json(&["release", "start", "site", "--version", "2.0.1"]);
    assert_eq!(code, 1, "{failed}");
    assert!(
        step(&failed, "build")["first_error"]
            .as_str()
            .unwrap()
            .contains("src/missing.txt"),
        "{failed}"
    );
}

#[test]
fn a_cargo_closure_keeps_a_rust_check_reused_while_unrelated_crates_change() {
    let project = Project::new(
        r#"#[paths("crates/app/**")]
#[reads(std::paths::cargo("app"))]
check test_app {
    run!("make ok")?;
}

"#,
    );
    project.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    project.write(
        "crates/app/Cargo.toml",
        "[package]\nname = \"app\"\n[dependencies]\nlib = { path = \"../lib\" }\n",
    );
    project.write("crates/app/src/main.rs", "fn main() {}\n");
    project.write("crates/lib/Cargo.toml", "[package]\nname = \"lib\"\n");
    project.write("crates/lib/src/lib.rs", "\n");
    project.write("crates/other/Cargo.toml", "[package]\nname = \"other\"\n");
    project.write("crates/other/src/lib.rs", "\n");
    project.commit("crates");
    assert_eq!(
        target(&project.json(&["run", "test-app"]).0, "test-app")["result"],
        "passed"
    );
    project.write("crates/other/src/lib.rs", "// unrelated\n");
    let (again, _) = project.json(&["run", "test-app"]);
    assert_eq!(target(&again, "test-app")["result"], "reused", "{again}");
    project.write("crates/lib/src/lib.rs", "// a dependency changed\n");
    let (changed, _) = project.json(&["run", "test-app"]);
    assert_eq!(
        target(&changed, "test-app")["result"],
        "passed",
        "{changed}"
    );
}

#[test]
fn checks_that_passed_quickly_run_locally_others_in_the_pool() {
    let project = Project::new(
        r#"#![runner(cmd!("sh remote.sh"))]

"#,
    );
    project.write(
        "remote.sh",
        "echo 'CITRUS_TARGET target=ok status=PASS exit=0 seconds=3'\n",
    );
    project.commit("pool");
    project.write("src/a.txt", "changed\n");
    let (first, _) = project.json(&["run"]);
    assert_eq!(
        first["run"]["mode"], "remote",
        "never passed: assumed heavy {first}"
    );
    project.write("src/a.txt", "changed again\n");
    let (second, _) = project.json(&["run"]);
    assert_eq!(second["run"]["mode"], "local", "{second}");
}

#[test]
fn a_pool_that_skips_a_planned_check_does_not_pass_it() {
    let project = Project::new(ALPHA_BETA);
    // The runner sees which checks to run, but runs only the first one.
    project.write(
        "remote.sh",
        "mkdir -p .scratch && cp \"$CITRUS_TARGETS\" .scratch/wanted\necho 'CITRUS_TARGET target=lib.alpha status=PASS exit=0'\n",
    );
    project.commit("runner");
    project.write("lib/x", "changed\n");
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(target(&run, "lib.alpha")["result"], "passed");
    assert_eq!(target(&run, "lib.beta")["result"], "not_run", "{run}");
    assert!(
        target(&run, "suite")["first_error"]
            .as_str()
            .unwrap()
            .contains("beta"),
        "{run}"
    );
    assert_eq!(
        fs::read_to_string(project.root().join(".scratch/wanted")).unwrap(),
        "lib.alpha\nlib.beta\n"
    );
}

#[test]
fn a_check_the_pool_passed_is_reused_by_its_inputs() {
    let project = Project::new(
        r#"#![runner(cmd!("sh remote.sh"))]

"#,
    );
    project.write(
        "remote.sh",
        "echo 'CITRUS_TARGET target=ok status=START'\necho 'CITRUS_TARGET target=ok status=PASS exit=0 seconds=40'\n",
    );
    project.commit("runner");
    project.write("src/a.txt", "changed\n");
    let (first, code) = project.json(&["run", "--remote"]);
    assert_eq!(code, 0, "{first}");
    // An unrelated file changes the snapshot but not the inputs of `ok`.
    project.write("notes.txt", "changed\n");
    let (again, _) = project.json(&["run", "--remote"]);
    assert_eq!(target(&again, "ok")["result"], "reused", "{again}");
}

#[test]
fn a_submodule_path_is_a_valid_input() {
    let project = Project::new(
        r#"#[paths("vendor/lib")]
check sub {
    run!("make ok")?;
}

"#,
    );
    project.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        "160000,1111111111111111111111111111111111111111,vendor/lib",
    ]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "gitlink",
    ]);
    let (checked, _) = project.json(&["check"]);
    assert_eq!(checked["warnings"], serde_json::json!([]), "{checked}");
    let (doctor, _) = project.json(&["doctor"]);
    assert!(
        !doctor["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["check"] == "check sub" && f["status"] == "fail"),
        "{doctor}"
    );
}

#[test]
fn an_excluded_path_neither_selects_nor_invalidates_a_check() {
    let project = Project::new(
        r#"#[paths("lib/**", "!lib/vendor/**")]
check lib {
    run!("make ok")?;
}

"#,
    );
    project.write("lib/a.txt", "a\n");
    project.write("lib/vendor/b.txt", "b\n");
    project.commit("lib");
    assert_eq!(
        target(&project.json(&["run", "lib"]).0, "lib")["result"],
        "passed"
    );
    project.write("lib/vendor/b.txt", "changed\n");
    assert_eq!(
        target(&project.json(&["run", "lib"]).0, "lib")["result"],
        "reused"
    );
    let (plan, _) = project.json(&["plan", "--base", "HEAD"]);
    assert_eq!(
        plan["plan"]["unmapped"],
        serde_json::json!(["lib/vendor/b.txt"]),
        "{plan}"
    );
}

#[test]
fn profiles_and_covered_checks_shape_the_plan() {
    let project = Project::new(
        r#"#![main("main")]

profile fast;

profile e2e;

#[paths("lib/**")]
#[profile(fast)]
check unit {
    run!("make ok")?;
}

#[paths("lib/**")]
#[profile(e2e)]
check e2e_only {
    run!("make plain")?;
}

#[when(touched(e2e_only) && profile(e2e))]
check slow_only {
    run!("make ok")?;
}

#[paths("lib/**")]
check part {
    run!("make ok")?;
}

/// Runs `part` itself.
#[paths("lib/**")]
#[covers(part)]
check whole {
    run!("make ok")?;
}

"#,
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    project.write("lib/a.txt", "a\n");
    project.commit("lib");
    let targets =
        |args: &[&str]| project.json(&[&["plan"], args].concat()).0["plan"]["targets"].clone();
    assert_eq!(targets(&[]), serde_json::json!(["unit", "whole"]));
    assert_eq!(
        targets(&["--profile", "e2e"]),
        serde_json::json!(["e2e-only", "slow-only", "whole"])
    );
    let unknown = project.json(&["plan", "--profile", "nightly"]).0;
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .contains("no profile `nightly`"),
        "{unknown}"
    );
}

#[test]
fn groups_conditions_and_signals_choose_checks() {
    let project = Project::new(
        r#"#![main("main")]
#![signals(cmd!("sh signals.sh"))]
#![label("scope-mixed", touched(clyer) && touched(pipeline))]

/// Documentation: nothing to run.
#[paths("*.md")]
group docs {}

#[paths("clyer/**")]
group clyer {
    check bot {
        run!("make ok")?;
    }
}

#[paths("scripts/**")]
group pipeline {
    /// Only pipeline files changed.
    #[when(only(pipeline))]
    check contract_alone {
        run!("make ok")?;
    }

    /// Pipeline and other files, but no Clyer.
    #[when(!only(pipeline) && without(clyer))]
    check contract_main {
        run!("make plain")?;
    }

    /// Pipeline and Clyer files.
    #[when(!only(pipeline) && !without(clyer))]
    check contract {
        run!("make fail")?;
    }
}

#[paths("crates/**")]
#[when(signal("product:garvis"))]
check backend {
    run!("make ok")?;
}

#[when(selected(backend))]
check after_backend {
    run!("make ok")?;
}

"#,
    );
    // Paths under crates/ mean the garvis product unless they name clyer.
    project.write("signals.sh", "grep -q '^crates/' \"$CITRUS_PATHS\" && ! grep -q clyer \"$CITRUS_PATHS\" && echo 'SIGNAL product:garvis'; true\n");
    project.commit("plan");
    let plan_for = |files: &[(&str, &str)]| {
        project.git(&["checkout", "-q", "-B", "feature", "main"]);
        for (path, text) in files {
            project.write(path, text);
        }
        project.commit("change");
        let (plan, _) = project.json(&["plan"]);
        plan["plan"].clone()
    };
    let plan = plan_for(&[("scripts/run.sh", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["pipeline.contract-alone"]),
        "{plan}"
    );
    let plan = plan_for(&[("scripts/run.sh", "y\n"), ("README.md", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["pipeline.contract-main"]),
        "{plan}"
    );
    let (full, _) = project.json(&["plan"]);
    assert_eq!(
        full["run"]["pipeline.contract-main"],
        serde_json::json!([["run", "make", "plain"]]),
        "{full}"
    );
    assert_eq!(plan["unmapped"], serde_json::json!([]), "{plan}");
    let plan = plan_for(&[("scripts/run.sh", "z\n"), ("clyer/bot.rs", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["clyer.bot", "pipeline.contract"]),
        "{plan}"
    );
    assert_eq!(plan["labels"], serde_json::json!(["scope-mixed"]), "{plan}");
    let plan = plan_for(&[("crates/api/lib.rs", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["backend", "after-backend"]),
        "{plan}"
    );
    let plan = plan_for(&[("crates/clyer/lib.rs", "x\n")]);
    assert_eq!(plan["targets"], serde_json::json!([]), "{plan}");

    // The selected check is what runs.
    project.git(&["checkout", "-q", "-B", "feature", "main"]);
    project.write("scripts/run.sh", "w\n");
    project.write("README.md", "w\n");
    project.commit("change");
    let (run, _) = project.json(&["run"]);
    assert_eq!(
        target(&run, "pipeline.contract-main")["result"],
        "passed",
        "{run}"
    );

    // An explicit path list plans just those paths, with groups and signals.
    project.write("paths.txt", "clyer/bot.rs\nscripts/x.sh\n");
    let (listed, _) = project.json(&["plan", "--paths-file", "paths.txt"]);
    assert_eq!(
        listed["plan"]["groups"],
        serde_json::json!(["clyer", "pipeline"]),
        "{listed}"
    );
}

#[test]
fn a_signal_command_can_claim_paths_for_groups() {
    let project = Project::new(
        r#"#![signals(cmd!("sh classify.sh"))]

/// Removed code: only its removal appears in a diff.
#[paths("old/**", "!old/keep/**")]
group removed {
    check contracts {
        run!("make ok")?;
    }
}

"#,
    );
    project.write(
        "classify.sh",
        "grep '^gone/' \"$CITRUS_PATHS\" | sed 's/^/CLAIM /; s/$/ removed/'\n",
    );
    project.commit("classify");
    project.write("paths.txt", "gone/old.sh\n");
    let output = citrus_command()
        // The machine's pool (~/.config/citrus/pool) is not the tests'.
        .env("CITRUS_POOL", "")
        .args(["plan", "--paths-file", "paths.txt", "--json"])
        .current_dir(project.root())
        .env("CITRUS_AGENT", "test")
        .output()
        .unwrap();
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["removed.contracts"]),
        "{plan}"
    );
    assert_eq!(
        plan["plan"]["mapped"][0][1], "target:removed.contracts,group:removed",
        "{plan}"
    );
    project.write("paths.txt", "old/x\nold/keep/y\n");
    let output = citrus_command()
        // The machine's pool (~/.config/citrus/pool) is not the tests'.
        .env("CITRUS_POOL", "")
        .args(["plan", "--paths-file", "paths.txt", "--json"])
        .current_dir(project.root())
        .output()
        .unwrap();
    let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        plan["plan"]["unmapped"],
        serde_json::json!(["old/keep/y"]),
        "{plan}"
    );
}

#[test]
fn an_edited_ci_file_still_selects_the_checks_that_own_it() {
    let project = Project::new(
        r#"#![main("main")]

#[paths("citrus.ci")]
check config {
    run!("make ok")?;
}

"#,
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    project.declare("// a comment\n");
    project.commit("comment");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["config"]),
        "{plan}"
    );
}

#[test]
fn a_citrus_directory_holds_one_file_per_product() {
    let project = Project::new("");
    fs::remove_file(project.root().join("citrus.ci")).unwrap();
    project.write(
        ".citrus/project.ci",
        r#"#![citrus(2)]
#![main("main")]

const SOURCES = ["src/*.txt"];

"#,
    );
    project.write(
        ".citrus/app.ci",
        r#"#![citrus(2)]

/// The app: its sources and tests.
#[paths(SOURCES)]
group app {
    check unit {
        run!("make ok")?;
    }
}

"#,
    );
    project.commit("directory");
    let (targets, code) = project.json(&["targets"]);
    assert_eq!(code, 0, "{targets}");
    let unit = &targets["targets"][0];
    assert_eq!(unit["target"], "app.unit", "{targets}");
    assert_eq!(unit["source"], ".citrus/app.ci:6", "{targets}");
    project.write("src/a.txt", "changed\n");
    assert_eq!(
        project.json(&["plan"]).0["plan"]["targets"],
        serde_json::json!(["app.unit"])
    );

    project.write("citrus.ci", BASE);
    let error = project.json(&["status"]).0;
    assert!(
        error["error"].as_str().unwrap().contains("keep one"),
        "{error}"
    );
}

#[test]
fn a_comment_above_a_declaration_describes_it() {
    let project = Project::new(
        r#"/// Reads the logs back.
#[paths("logs/**")]
check logs {
    run!("make ok")?;
}

"#,
    );
    let (targets, _) = project.json(&["targets"]);
    let logs = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"] == "logs")
        .unwrap();
    assert_eq!(logs["description"], "Reads the logs back.", "{targets}");
}

#[test]
fn names_refer_to_declarations() {
    let error = |config: &str| {
        let project = Project::new(config);
        project.json(&["status"]).0["error"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    };
    let missing = error(
        r#"profile e2e;

#[paths("src/**")]
#[profile(e3)]
check db {
    run!("make ok")?;
}

"#,
    );
    assert!(
        missing.contains("no profile named `e3`") && missing.contains("did you mean `e2e`?"),
        "{missing}"
    );
    let service = error(
        r#"#[paths("src/**")]
#[needs(database)]
check db {
    run!("make ok")?;
}
"#,
    );
    assert!(service.contains("no service named `database`"), "{service}");
    let check = error(
        r#"#[paths("src/**")]
#[covers(bd)]
check all {
    run!("make ok")?;
}

#[paths("src/**")]
check db {
    run!("make ok")?;
}
"#,
    );
    assert!(
        check.contains("no check named `bd`") && check.contains("did you mean `db`?"),
        "{check}"
    );

    // A service checks need is a resource the runner provides.
    let project = Project::new(
        r#"#[limit(2)]
service browser;

#[paths("web/**")]
#[needs(browser)]
check e2e {
    run!("make ok")?;
}

"#,
    );
    let (targets, _) = project.json(&["targets"]);
    let e2e = targets["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"] == "e2e")
        .unwrap();
    assert_eq!(
        e2e["resources"],
        serde_json::json!(["browser"]),
        "{targets}"
    );
}

#[test]
fn a_check_that_replaces_parts_runs_instead_of_several() {
    let project = Project::new(
        r#"#![main("main")]

#[paths("api/**")]
group api {
    #[paths("api/users/**")]
    check users {
        run!("make ok")?;
    }

    #[paths("api/orders/**")]
    check orders {
        run!("make ok")?;
    }

    /// Every API test at once: cheaper than several parts.
    #[replaces(users, orders)]
    check all {
        run!("make ok")?;
    }
}

"#,
    );
    let plan_for = |paths: &str| {
        project.write("paths.txt", paths);
        project.json(&["plan", "--paths-file", "paths.txt"]).0["plan"]["targets"].clone()
    };
    assert_eq!(plan_for("api/users/a\n"), serde_json::json!(["api.users"]));
    assert_eq!(
        plan_for("api/users/a\napi/orders/b\n"),
        serde_json::json!(["api.all"])
    );
    assert_eq!(plan_for("api/shared.rs\n"), serde_json::json!(["api.all"]));
}

#[test]
fn a_service_starts_once_before_the_checks_that_need_it() {
    let project = Project::new(
        r#"/// A stand-in database: a file appears when it is up.
service database {
    start {
        run!("sh -c 'echo started >> .scratch/db; touch .scratch/ready'")?;
    }
    ready {
        std::wait::file(".scratch/ready", 5s)?;
    }
}

#[paths("db/**")]
#[needs(database)]
group db {
    check one {
        run!("make ok")?;
    }

    check two {
        run!("make ok")?;
    }
}

"#,
    );
    project.write(".scratch/.keep", "");
    project.write("db/schema.sql", "x\n");
    let (run, code) = project.json(&["run", "db.one", "db.two"]);
    assert_eq!(code, 0, "{run}");
    assert_eq!(
        fs::read_to_string(project.root().join(".scratch/db")).unwrap(),
        "started\n"
    );
}

#[test]
fn conditions_and_paths_take_lists_of_globs() {
    let project = Project::new(
        r#"const TOOLS = ["scripts/tool.py", "scripts/test_tool.py"];

const CLYER = ["clyer/**"];

/// The tool's own tests.
#[paths(TOOLS)]
check tool {
    run!("make ok")?;
}

/// Everything else under scripts/; the tool is not part of it.
#[paths(["scripts/**"] - TOOLS)]
group infra {
    /// Without Clyer changes: the cheaper contract.
    #[when(without(CLYER))]
    check contract_main {
        run!("make ok")?;
    }

    /// With Clyer changes: the whole contract.
    #[when(!without(CLYER))]
    check contract {
        run!("make plain")?;
    }
}

#[when(only(CLYER + ["scripts/**"]) && touched(CLYER))]
check clyer_only {
    run!("make ok")?;
}

"#,
    );
    let plan_for = |paths: &str| {
        project.write("paths.txt", paths);
        project.json(&["plan", "--paths-file", "paths.txt"]).0["plan"]["targets"].clone()
    };
    assert_eq!(plan_for("scripts/tool.py\n"), serde_json::json!(["tool"]));
    assert_eq!(
        plan_for("scripts/run.sh\n"),
        serde_json::json!(["infra.contract-main"])
    );
    assert_eq!(
        plan_for("scripts/run.sh\nclyer/bot.rs\n"),
        serde_json::json!(["infra.contract", "clyer-only"])
    );
}

#[test]
fn a_path_a_check_names_is_that_checks_alone() {
    let project = Project::new(
        r#"#[paths("scripts/**")]
group infra {
    check contract {
        run!("make ok")?;
    }
}

/// The tool's own tests: editing the tool does not run the infra contract.
#[paths("scripts/tool.py")]
#[reads("scripts/lib.py")]
check tool {
    run!("make ok")?;
}

"#,
    );
    let plan_for = |paths: &str| {
        project.write("paths.txt", paths);
        project.json(&["plan", "--paths-file", "paths.txt"]).0["plan"]["targets"].clone()
    };
    assert_eq!(plan_for("scripts/tool.py\n"), serde_json::json!(["tool"]));
    assert_eq!(
        plan_for("scripts/lib.py\n"),
        serde_json::json!(["infra.contract"])
    );
    assert_eq!(
        plan_for("scripts/tool.py\nscripts/run.sh\n"),
        serde_json::json!(["infra.contract", "tool"])
    );
}

#[test]
fn a_check_runs_for_a_group_it_names_and_its_own_paths() {
    let project = Project::new(
        r#"/// Platform crates every product builds on.
#[paths("platform/**")]
group platform {}

#[paths("vpn/**")]
group vpn {
    #[paths(platform, "vpn/backend/**")]
    check backend {
        run!("make ok")?;
    }
}

#[paths("platform/README.md")]
check docs {
    run!("make ok")?;
}

/// Runs for the platform's paths only.
#[paths(platform)]
#[when(!touched(["vpn/**"]))]
check platform_only {
    run!("make ok")?;
}

"#,
    );
    let plan_for = |paths: &str| {
        project.write("paths.txt", paths);
        project.json(&["plan", "--paths-file", "paths.txt"]).0["plan"]["targets"].clone()
    };
    assert_eq!(
        plan_for("platform/lib.rs\n"),
        serde_json::json!(["vpn.backend", "platform-only"])
    );
    assert_eq!(
        plan_for("vpn/backend/main.rs\n"),
        serde_json::json!(["vpn.backend"])
    );
    assert_eq!(plan_for("vpn/web/page.tsx\n"), serde_json::json!([]));
    assert_eq!(
        plan_for("elsewhere.txt\n"),
        serde_json::json!([]),
        "a group it names is not a condition"
    );
    // A path another check names is not the group's.
    assert_eq!(
        plan_for("platform/README.md\n"),
        serde_json::json!(["docs"])
    );
    let unknown = Project::new(
        r#"#[paths(platfrm)]
check x {
    run!("make ok")?;
}

#[paths("src/**")]
group platform {}

"#,
    );
    let error = unknown.json(&["status"]).0["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("no group or constant `platfrm`")
            && error.contains("did you mean `platform`?"),
        "{error}"
    );
}

#[test]
fn only_checks_with_known_inputs_are_reused() {
    let project = Project::new(
        r#"#![cache(false)]

#[paths("lib/**")]
#[cache(true)]
group lib {
    check unit {
        run!("make ok")?;
    }
}

/// Selected by a condition: nothing tells what it reads (its Makefile is
/// not all it reads), so even asked to it is not reused.
#[when(selected(lib::unit))]
#[cache(true)]
check after {
    run!("make ok")?;
}

#[paths(lib, "extra/**")]
#[cache(true)]
check named {
    run!("make ok")?;
}

/// Chosen by a condition, but it says what it reads: a group's paths.
#[when(selected(lib::unit))]
#[reads(lib)]
#[cache(true)]
check reader {
    run!("make ok")?;
}

"#,
    );
    let (targets, _) = project.json(&["targets"]);
    let row = |name: &str| {
        targets["targets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["target"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(row("lib.unit")["cache"], true, "{targets}");
    assert_eq!(row("after")["cache"], false, "{targets}");
    assert_eq!(
        row("ok")["cache"],
        false,
        "the project's default: {targets}"
    );
    assert_eq!(
        row("named")["extra_inputs"],
        serde_json::json!(["lib/**", "Makefile"]),
        "a group it names is an input: {targets}"
    );
    assert_eq!(row("reader")["cache"], true, "{targets}");
    assert_eq!(
        row("reader")["extra_inputs"],
        serde_json::json!(["lib/**", "Makefile"]),
        "a group it reads is an input: {targets}"
    );
}

#[test]
fn a_signal_command_can_give_a_path_to_one_check() {
    let project = Project::new(
        r#"#![signals(cmd!("sh classify.sh"))]

#[paths("web/**")]
group web {
    check build {
        run!("make ok")?;
    }
}

/// Only the audited part of the workspace file changed.
#[when(signal("audit-only"))]
check audit {
    run!("make ok")?;
}

"#,
    );
    project.write(
        "classify.sh",
        "grep -q '^web/workspace.yaml$' \"$CITRUS_PATHS\" && echo 'SIGNAL audit-only' && echo 'OWN web/workspace.yaml audit'; true\n",
    );
    project.commit("classify");
    project.write("paths.txt", "web/workspace.yaml\n");
    let (plan, _) = project.json(&["plan", "--paths-file", "paths.txt"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["audit"]),
        "{plan}"
    );
    assert_eq!(plan["plan"]["mapped"][0][1], "target:audit", "{plan}");
    project.write("paths.txt", "web/workspace.yaml\nweb/page.tsx\n");
    let (plan, _) = project.json(&["plan", "--paths-file", "paths.txt"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["web.build", "audit"]),
        "{plan}"
    );
}

#[test]
fn an_edited_check_still_needs_its_condition() {
    let project = Project::new(
        r#"#![main("main")]
#![signals(cmd!("true"))]

#[when(signal("release"))]
check gated {
    run!("make ok")?;
}

"#,
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            r#"check gated {
    run!("make ok")?;
}

"#,
            r#"check gated {
    run!("make plain")?;
}

"#,
        ),
    );
    project.commit("edit gated");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["plan"]["targets"], serde_json::json!([]), "{plan}");
    let (plan, _) = project.json(&["plan", "--base", "main"]);
    assert!(
        !plan["plan"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "gated"),
        "{plan}"
    );
}

#[test]
fn check_warns_about_missing_inputs_of_reused_checks_only() {
    let project = Project::new(
        r#"/// Removed code: only its removal appears in a diff.
#[paths("gone/**")]
group retired {}

#[paths("src/*.txt")]
#[reads("missing/**")]
check reused {
    run!("make ok")?;
}

"#,
    );
    let (checked, code) = project.json(&["check"]);
    assert_eq!(code, 0, "{checked}");
    let messages: Vec<&str> = checked["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|warning| warning["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        messages,
        ["check reused reuses passes, but its input `missing/**` matches no file"],
        "{checked}"
    );
}

const V2: &str = r#"#![citrus(2)]

/// What the checks read.
const SOURCES: list<glob> = ["src/**"];

/// One file's text, or why it could not be read.
fn text(file: path) -> Result<str> {
    std::fs::read(file).context("reading {file}")
}

/// The sources say "one".
#[paths(SOURCES)]
check content {
    let found = text("src/a.txt")?;
    assert found.trim() == "one", "src/a.txt says {found.trim()}";
}

/// A program's output, judged by Citrus.
#[paths(SOURCES)]
check program {
    let out = std::proc::Command::new("sh").args(["-c", "echo ready; exit 3"]).output()?;
    let code = match out.code {
        0 => "ok",
        3 => "three",
        _ => "other",
    };
    assert code == "three" && out.stdout.contains("ready");
}
"#;

#[test]
fn v2_checks_run_their_bodies_and_fail_at_the_line() {
    let project = Project::v2(V2);
    let (first, code) = project.json(&["run"]);
    assert_eq!(code, 0, "{first}");
    // The same body and inputs: proven, not run again.
    let (second, _) = project.json(&["run"]);
    assert!(
        second["targets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["result"] == "reused"),
        "{second}"
    );
    project.write("src/a.txt", "two\n");
    let (failed, code) = project.json(&["run", "content"]);
    assert_eq!(code, 1, "{failed}");
    let error = failed["targets"][0]["first_error"]
        .as_str()
        .unwrap_or_default();
    assert!(error.contains("src/a.txt says two"), "{failed}");
    let log = String::from_utf8_lossy(
        &project
            .citrus(&["log", failed["run"]["id"].as_str().unwrap(), "--full"])
            .stdout,
    )
    .into_owned();
    assert!(log.contains("--> citrus.ci:15:5"), "{log}");
    // A changed body is a new check: its pass is not reused.
    project.write("src/a.txt", "one\n");
    project.write("citrus.ci", &V2.replace("echo ready", "echo  ready"));
    let (rerun, _) = project.json(&["run", "program"]);
    assert_eq!(rerun["targets"][0]["result"], "passed", "{rerun}");
}

#[test]
fn v2_checker_explains_mistakes_before_anything_runs() {
    let cases = [
        (
            "check c { let xs = [1]; for x in xs.iter() { } }",
            "no iterators or closures",
        ),
        ("check c { let xs = [1]; xs.push(2); }", "`let mut`"),
        (
            "const fn f() -> bool { std::fs::exists(\"x\") }",
            "not allowed in a constant or a `const fn`",
        ),
        ("check c { std::fs::read(\"x\"); }", "add `?`"),
        (
            "fn f() -> Result<int> { let x = std::env::var(\"X\")?; Ok(1) }",
            "ok_or",
        ),
        (
            "const SOURCE: list<glob> = [];\ncheck c { let x = SOURSE; }",
            "did you mean `SOURCE`",
        ),
        ("fn f(x: String) -> int { 1 }", "text is `str`"),
        ("check c { let x = 1; x = 2; }", "not mutable"),
        (
            "check c { let v = std::env::var(\"X\"); let s = match v { Some(x) => x }; }",
            "add `_ => …`",
        ),
        ("#[pths(\"x\")]\ncheck c { }", "did you mean `#[paths]`"),
    ];
    for (body, expected) in cases {
        let project = Project::v2(&format!("#![citrus(2)]\n{body}\n"));
        let output = project.citrus(&["check", "--text"]);
        let text = String::from_utf8_lossy(&output.stderr).into_owned()
            + &String::from_utf8_lossy(&output.stdout);
        assert!(!output.status.success(), "{body}: accepted");
        assert!(text.contains(expected), "{body}: {text}");
    }
}

#[test]
fn v2_releases_pass_the_release_to_steps_and_recover_with_a_function() {
    let project = Project::v2(
        r#"#![citrus(2)]

environment prod;

#[environment(prod)]
#[version(initial = "1.0.0-app")]
release app {
    step build(r: Release) {
        let previous = match r.previous { Some(v) => "{v}", None => "none" };
        std::log::info("building {r.version} after {previous}");
    }
    #[production]
    #[recover(reconcile)]
    step deploy(r: Release) {
        assert std::fs::exists("deploy-ok"), "cluster unreachable";
    }
}

fn reconcile(r: Release) -> Result<()> {
    std::log::info("reconciled {r.version}");
    Ok(())
}
"#,
    );
    project.write(".gitignore", ".scratch/\ndeploy-ok\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "ignore",
    ]);
    let (failed, code) = project.json(&["release", "start", "app", "--approve"]);
    assert_eq!(code, 1, "{failed}");
    assert!(
        step(&failed, "deploy")["first_error"]
            .as_str()
            .unwrap_or_default()
            .contains("cluster unreachable"),
        "{failed}"
    );
    project.write("deploy-ok", "");
    let id = failed["release"]["id"].as_str().unwrap().to_owned();
    let (resumed, code) = project.json(&["release", "resume", &id, "--approve"]);
    assert_eq!(code, 0, "{resumed}");
    let log = String::from_utf8_lossy(&project.citrus(&["release", "log", &id, "--full"]).stdout)
        .into_owned();
    assert!(log.contains("building 1.0.0-app after none"), "{log}");
}

#[test]
fn v2_commands_split_like_a_terminal_without_a_shell() {
    let project = Project::v2(
        r#"#![citrus(2)]

/// Arguments as the program sees them.
#[paths("src/**")]
check words {
    let name = "two words";
    let flags = ["-a", "-b"];
    let out = cmd!("GREETING=hi sh -c 'printf \"%s|\" \"$GREETING\" \"$@\"' sh {name} x{name}y {flags...} 'a b' > *").output()?;
    assert out.stdout == "hi|two words|xtwo wordsy|-a|-b|a b|>|*|", "got {out.stdout}";
}
"#,
    );
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 0, "{run}");
}

#[test]
fn v2_understands_cargo_commands_and_their_inputs() {
    let project = Project::v2(
        r#"#![citrus(2)]

/// The API crate's tests: no paths, Citrus reads them from Cargo.
check api {
    run!("cargo test --locked -p api -- --nocapture")?;
}
"#,
    );
    project.write("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    project.write(
        "crates/api/Cargo.toml",
        "[package]\nname = \"api\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
    );
    project.write("crates/core/Cargo.toml", "[package]\nname = \"core\"\n");
    project.write("crates/web/Cargo.toml", "[package]\nname = \"web\"\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "crates",
    ]);
    let (targets, code) = project.json(&["targets"]);
    assert_eq!(code, 0, "{targets}");
    // Inferred inputs select the check and make its pass stale; they do not
    // become paths it owns.
    let inputs = format!(
        "{}{}",
        targets["targets"][0]["inputs"], targets["targets"][0]["extra_inputs"]
    );
    assert!(
        inputs.contains("crates/api/**") && inputs.contains("crates/core/**"),
        "{inputs}"
    );
    assert!(!inputs.contains("crates/web"), "{inputs}");
    assert_eq!(
        targets["targets"][0]["meta"]["understood"][0],
        "cargo test -p api"
    );

    // A wrapper declared as a Cargo command line is understood like it.
    project.write(
        "scripts/cargo-test.sh",
        "#!/bin/sh\nexec cargo test \"$@\"\n",
    );
    project.write(
        "citrus.ci",
        r#"#![citrus(2)]
#![tool("scripts/cargo-test.sh", cmd!("cargo test"))]

check api {
    run!("SQLX_OFFLINE=true ./scripts/cargo-test.sh -p api --lib")?;
}
"#,
    );
    project.git(&["add", "-A"]);
    let (targets, code) = project.json(&["targets"]);
    assert_eq!(code, 0, "{targets}");
    let inputs = format!(
        "{}{}",
        targets["targets"][0]["inputs"], targets["targets"][0]["extra_inputs"]
    );
    assert!(
        inputs.contains("crates/api/**")
            && inputs.contains("crates/core/**")
            && inputs.contains("scripts/cargo-test.sh"),
        "{inputs}"
    );
    assert_eq!(
        targets["targets"][0]["meta"]["understood"][0],
        "scripts/cargo-test.sh = cargo test -p api"
    );
    // Inside a Make recipe the same commands are understood: the target's
    // checks read the crates, and say what the target runs.
    project.write(
        "Makefile",
        "test-api:\n\t@SQLX_OFFLINE=true ./scripts/cargo-test.sh \\\n\t\t-p api --lib -- $(ARGS)\n",
    );
    project.write(
        "citrus.ci",
        r#"#![citrus(2)]
#![tool("scripts/cargo-test.sh", cmd!("cargo test"))]

#[paths("crates/api/**")]
check api {
    run!("make test-api")?;
}
"#,
    );
    project.git(&["add", "-A"]);
    let (targets, code) = project.json(&["targets"]);
    assert_eq!(code, 0, "{targets}");
    assert_eq!(
        targets["targets"][0]["meta"]["understood"][0],
        "make test-api (scripts/cargo-test.sh = cargo test -p api)"
    );
    let reads = targets["targets"][0]["extra_inputs"].to_string();
    assert!(
        reads.contains("Makefile")
            && reads.contains("crates/core/**")
            && reads.contains("scripts/cargo-test.sh"),
        "{reads}"
    );
    project.write(
        "citrus.ci",
        "#![citrus(2)]\n#![tool(\"scripts/missing.sh\", cmd!(\"cargo test\"))]\n",
    );
    let output = project.citrus(&["check", "--text"]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no file scripts/missing.sh"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    for (command, expected) in [
        ("cargo test -p apy", "did you mean `api`"),
        ("cargo tset", "did you mean `cargo test`"),
        ("make tset", "no Make target `tset`"),
    ] {
        project.write(
            "citrus.ci",
            &format!("#![citrus(2)]\ncheck api {{\n    run!(\"{command}\")?;\n}}\n"),
        );
        let output = project.citrus(&["check", "--text"]);
        let text = String::from_utf8_lossy(&output.stderr).into_owned()
            + &String::from_utf8_lossy(&output.stdout);
        assert!(
            !output.status.success() && text.contains(expected),
            "{command}: {text}"
        );
    }
}

#[test]
fn tests_assert_what_a_change_would_run() {
    let project = Project::new(
        r#"#![signals(cmd!("sh signals.sh"))]
#![label("scope-clyer", signal("scope:clyer"))]

profile fast;
profile e2e;

#[paths("scripts/**")]
group pipeline {
    check contract {
        run!("make ok")?;
    }

    #[profile(e2e)]
    check e2e {
        run!("make ok")?;
    }
}

#[paths("docs/**")]
group docs {}

#[test]
fn a_script_runs_the_contract() -> Result<()> {
    let plan = std::plan::of(["scripts/x.sh"])?;
    assert plan.checks == ["pipeline.contract"];
    assert plan.selects("pipeline.contract"), "not selected";
    assert plan.owners("scripts/x.sh") == ["pipeline.contract", "pipeline.e2e"];
    assert plan.groups_of("scripts/x.sh") == ["pipeline"];
    assert plan.unclaimed.is_empty();
}

#[test]
fn the_e2e_profile_adds_its_check() -> Result<()> {
    let plan = std::plan::change(["scripts/x.sh"]).profile("e2e").plan()?;
    assert plan.checks == ["pipeline.contract", "pipeline.e2e"];
}

#[test]
fn the_signal_command_sees_the_environment() -> Result<()> {
    let plan = std::plan::change(["docs/a.md"]).env("SCOPE", "clyer").plan()?;
    assert plan.labels == ["scope-clyer"];
    let plain = std::plan::of(["docs/a.md"])?;
    assert plain.labels.is_empty();
}

#[test]
fn an_unknown_path_is_unclaimed() -> Result<()> {
    let plan = std::plan::of(["elsewhere/x"])?;
    assert plan.checks.is_empty(), "nothing to run";
    assert plan.unclaimed == ["elsewhere/x"];
}

#[test]
fn a_wrong_expectation_fails() -> Result<()> {
    let plan = std::plan::of(["docs/a.md"])?;
    assert plan.checks == ["pipeline.contract"], "docs ran the contract";
}

#[test]
fn a_misspelled_check_fails() -> Result<()> {
    let plan = std::plan::of(["docs/a.md"])?;
    assert !plan.selects("pipeline.contracts"), "selected";
}
"#,
    );
    project.write(
        "signals.sh",
        "if [ \"$SCOPE\" = clyer ]; then echo 'SIGNAL scope:clyer'; fi\n",
    );
    project.commit("tests");
    let output = project.citrus(&["test", "--text"]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(1), "{text}");
    for passing in [
        "a-script-runs-the-contract",
        "the-e2e-profile-adds-its-check",
        "the-signal-command-sees-the-environment",
        "an-unknown-path-is-unclaimed",
    ] {
        assert!(text.contains(&format!("test {passing} ... ok")), "{text}");
    }
    assert!(
        text.contains("test a-wrong-expectation-fails ... FAILED"),
        "{text}"
    );
    assert!(
        text.contains("docs ran the contract\n  left: []\n right: [pipeline.contract]"),
        "{text}"
    );
    assert!(
        text.contains("test a-misspelled-check-fails ... FAILED"),
        "{text}"
    );
    assert!(
        text.contains("no check `pipeline.contracts`; did you mean `pipeline.contract`?"),
        "{text}"
    );
    assert!(text.contains("4 passed, 2 failed"), "{text}");
    let output = project.citrus(&["test", "unknown_path", "--text"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let output = project.citrus(&["test", "nothing-like-this"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn deps_finds_files_the_compiler_read_outside_the_inputs() {
    let project = Project::v2(
        r#"#![citrus(2)]

check api {
    run!("cargo check -p api")?;
}
"#,
    );
    project.write(
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\nresolver = \"2\"\n",
    );
    project.write(
        "crates/api/Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    // A path Citrus cannot read when the file loads: only the compiler knows it.
    project.write(
        "crates/api/src/lib.rs",
        "pub const MESSAGE: &str = include_str!(concat!(\"../../../shared/\", \"msg.txt\"));\n",
    );
    project.write("shared/msg.txt", "hello\n");
    project.git(&["add", "-A"]);
    let target = project.root().join("target");
    let built = Command::new("cargo")
        .args(["check", "-q", "-p", "api"])
        .current_dir(project.root())
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let output = project.citrus(&["deps", "--text"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("✗ api reads shared/msg.txt (crate crates/api/src/lib.rs)"),
        "{text}"
    );
    project.write(
        "citrus.ci",
        "#![citrus(2)]\n\n#[reads(\"shared/**\")]\ncheck api {\n    run!(\"cargo check -p api\")?;\n}\n",
    );
    let output = project.citrus(&["deps", "--text"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{text}");
    assert!(text.contains("1 checks understood as Cargo"), "{text}");
}

#[test]
fn a_reused_check_must_read_only_its_inputs() {
    let project = Project::v2(
        r#"#![citrus(2)]

#[paths("tools/check.py")]
#[cache]
check tool {
    run!("python3 -B tools/check.py")?;
}
"#,
    );
    project.write(
        "tools/check.py",
        // Built at run time: no word of the script names the file.
        "import pathlib\nassert (pathlib.Path('data') / 'limits.json').read_text().strip() == '{}'\n",
    );
    project.write("data/limits.json", "{}\n");
    project.git(&["add", "-A"]);
    let output = project.citrus(&["run", "tool", "--local", "--text"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("ran; read outside its inputs: data/limits.json"),
        "{text}"
    );
    // Not reused: it runs again.
    let (run, _) = project.json(&["run", "tool", "--local"]);
    assert_eq!(target(&run, "tool")["result"], "passed", "{run}");

    project.write(
        "citrus.ci",
        "#![citrus(2)]\n\n#[paths(\"tools/check.py\")]\n#[reads(\"data/**\")]\n#[cache]\ncheck tool {\n    run!(\"python3 -B tools/check.py\")?;\n}\n",
    );
    project.git(&["add", "-A"]);
    let (run, _) = project.json(&["run", "tool", "--local"]);
    assert_eq!(target(&run, "tool")["result"], "passed", "{run}");
    let (run, _) = project.json(&["run", "tool", "--local"]);
    assert_eq!(target(&run, "tool")["result"], "reused", "{run}");
}

#[test]
fn a_change_to_what_a_recipe_reads_selects_its_check() {
    let project = Project::v2(
        r#"#![citrus(2)]

#[paths("api/**")]
check api {
    run!("make test-api")?;
}

#[paths("web/**")]
check web {
    run!("make test-web")?;
}

/// The build machinery.
#[paths("make/**", "scripts/**")]
check machinery {
    run!("make ok")?;
}
"#,
    );
    project.write("Makefile", "include make/*.mk\nok:\n\t@true\n");
    project.write("make/api.mk", "test-api:\n\t@./scripts/api-test.sh\n");
    project.write("make/web.mk", "test-web:\n\t@true\n");
    project.write("scripts/api-test.sh", "#!/bin/sh\ntrue\n");
    project.write("api/a.txt", "a\n");
    project.write("web/w.txt", "w\n");
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "make",
    ]);
    let plan = |paths: &str| {
        project.write("paths.txt", paths);
        let output = project.citrus(&["plan", "--paths-file", "paths.txt", "--json"]);
        let plan: Value = serde_json::from_slice(&output.stdout).unwrap();
        plan["plan"]["targets"].clone()
    };
    // The recipe's Makefile and the script it runs select the check that
    // runs them, and still their owner.
    assert_eq!(
        plan("make/api.mk\n"),
        serde_json::json!(["api", "machinery"])
    );
    assert_eq!(
        plan("scripts/api-test.sh\n"),
        serde_json::json!(["api", "machinery"])
    );
    assert_eq!(
        plan("make/web.mk\n"),
        serde_json::json!(["web", "machinery"])
    );
    assert_eq!(plan("api/a.txt\n"), serde_json::json!(["api"]));
}

#[test]
fn checks_run_in_parallel_with_their_output_whole() {
    let project = Project::v2(
        r#"#![citrus(2)]

/// One browser at a time.
#[limit(1)]
service browser;

#[paths("src/**")]
group slow {
    check a {
        run!("sh -c 'echo a-begins; sleep 1; echo a-ends'")?;
    }

    check b {
        run!("sh -c 'echo b-begins; sleep 1; echo b-ends'")?;
    }

    check c {
        run!("sh -c 'echo c-begins; sleep 1; echo AssertionError: c broke; exit 3'")?;
    }

    #[needs(browser)]
    check e2e_one {
        run!("sh -c 'date +%s > e2e-one; sleep 1; date +%s >> e2e-one'")?;
    }

    #[needs(browser)]
    check e2e_two {
        run!("sh -c 'date +%s > e2e-two; sleep 1; date +%s >> e2e-two'")?;
    }
}
"#,
    );
    let started = std::time::Instant::now();
    let (run, code) = project.json(&[
        "run", "--local", "--jobs", "5", "slow.a", "slow.b", "slow.c",
    ]);
    let elapsed = started.elapsed();
    assert_eq!(code, 1, "{run}");
    assert!(
        elapsed < std::time::Duration::from_millis(2900),
        "{elapsed:?}: {run}"
    );
    assert_eq!(target(&run, "slow.a")["result"], "passed", "{run}");
    assert_eq!(target(&run, "slow.c")["result"], "failed", "{run}");
    assert!(
        target(&run, "slow.c")["first_error"]
            .as_str()
            .unwrap_or_default()
            .contains("AssertionError: c broke"),
        "{run}"
    );
    // Each check's output stays between its own markers.
    let log = fs::read_to_string(run["run"]["log"].as_str().unwrap()).unwrap();
    for name in ["a", "b"] {
        let start = log
            .find(&format!("CITRUS_TARGET target=slow.{name} status=START"))
            .unwrap();
        let end = log
            .find(&format!("CITRUS_TARGET target=slow.{name} status=PASS"))
            .unwrap();
        let segment = &log[start..end];
        assert!(
            segment.contains(&format!("{name}-begins"))
                && segment.contains(&format!("{name}-ends")),
            "{log}"
        );
        let other = if name == "a" { "b" } else { "a" };
        assert!(!segment.contains(&format!("{other}-begins")), "{log}");
    }
    // A service's limit keeps its checks apart.
    let (run, code) = project.json(&[
        "run",
        "--local",
        "--jobs",
        "4",
        "slow.e2e-one",
        "slow.e2e-two",
    ]);
    assert_eq!(code, 0, "{run}");
    let times = |file: &str| -> Vec<u64> {
        fs::read_to_string(project.root().join(file))
            .unwrap()
            .lines()
            .map(|line| line.trim().parse().unwrap())
            .collect()
    };
    let (one, two) = (times("e2e-one"), times("e2e-two"));
    assert!(
        one[1] <= two[0] || two[1] <= one[0],
        "overlapped: {one:?} {two:?}"
    );
}

#[test]
fn a_run_streams_its_protocol_for_an_outer_citrus() {
    let project = Project::new("");
    let output = citrus_command()
        // The machine's pool (~/.config/citrus/pool) is not the tests'.
        .env("CITRUS_POOL", "")
        .args(["run", "ok", "--local", "--text"])
        .current_dir(project.root())
        .env("CITRUS_PROTOCOL", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(
        text.contains("CITRUS_TARGET target=ok status=START"),
        "{text}"
    );
    assert!(text.contains("fine"), "{text}");
    assert!(
        text.contains("CITRUS_TARGET target=ok status=PASS"),
        "{text}"
    );
}

#[test]
fn a_snapshot_without_git_runs_with_an_outside_repository() {
    let snapshot = tempfile::tempdir().unwrap();
    let repository = tempfile::tempdir().unwrap();
    let root = snapshot.path();
    fs::write(
        root.join("citrus.ci"),
        r#"#![citrus(2)]

/// The checks' programs see no Git checkout here.
#[paths("src/**")]
check nogit {
    run!("sh -c 'if git rev-parse --git-dir >/dev/null 2>&1; then exit 1; fi; echo no-git-here'")?;
}
"#,
    )
    .unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/a.txt"), "a\n").unwrap();
    let git_dir = repository.path().join("snapshot.git");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "core.worktree", root.to_str().unwrap()]);
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "snapshot",
    ]);
    assert!(!root.join(".git").exists());
    let output = citrus_command()
        // The machine's pool (~/.config/citrus/pool) is not the tests'.
        .env("CITRUS_POOL", "")
        .args(["run", "nogit", "--local", "--text"])
        .current_dir(root)
        .env("CITRUS_GIT_DIR", &git_dir)
        .env("CITRUS_PROTOCOL", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("no-git-here"), "{text}");
}

#[test]
fn a_service_stops_when_the_run_is_over() {
    let project = Project::v2(
        r#"#![citrus(2)]

/// A database for the checks.
service database {
    start { run!("sh -c 'echo started >> service.log'")?; }
    ready { run!("test -f service.log")?; }
    stop { run!("sh -c 'echo stopped >> service.log'")?; }
}

#[paths("src/**")]
#[needs(database)]
group db {
    check reads {
        run!("grep -q started service.log")?;
    }

    check breaks {
        run!("false")?;
    }
}
"#,
    );
    for jobs in ["1", "2"] {
        let _ = fs::remove_file(project.root().join("service.log"));
        let (run, code) = project.json(&[
            "run",
            "--local",
            "--force",
            "--jobs",
            jobs,
            "db.reads",
            "db.breaks",
        ]);
        assert_eq!(code, 1, "{run}");
        assert_eq!(target(&run, "db.reads")["result"], "passed", "{run}");
        let log = fs::read_to_string(project.root().join("service.log")).unwrap();
        assert_eq!(log, "started\nstopped\n", "jobs {jobs}: {run}");
    }
}

#[test]
fn a_run_plans_for_given_paths_and_tells_the_runner() {
    let project = Project::new(
        r#"#![runner(cmd!("sh pool.sh"))]
"#,
    );
    project.write(
        "pool.sh",
        "cp \"$CITRUS_PATHS\" given-paths\nfor t in $(cat \"$CITRUS_TARGETS\"); do echo \"CITRUS_TARGET target=$t status=PASS exit=0 seconds=0\"; done\n",
    );
    project.commit("pool");
    project.write("paths.txt", "other/x\n");
    // No diff with the base: the given path alone decides.
    let (run, code) = project.json(&["run", "--paths-file", "paths.txt", "--remote"]);
    assert_eq!(code, 0, "{run}");
    let names: Vec<&str> = run["targets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["target"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["fail"], "{run}");
    assert_eq!(
        fs::read_to_string(project.root().join("given-paths")).unwrap(),
        "other/x\n"
    );
}

/// `#[env_file]` adds a file's `KEY=VALUE` lines to a check's environment and
/// `${{NAME}}` in `#[env]` (`${NAME}` once read) names a variable set before it.
#[test]
fn a_check_takes_variables_from_an_env_file_and_refers_to_them() {
    let project = Project::new(
        r#"
#[paths("config/**")]
#[env_file("config/test.env")]
#[env(DATABASE_URL = "${{TEST_URL}}/db")]
check env {
    run!("sh -c 'test \"$DATABASE_URL\" = postgres://h:5432/db && test \"$PORT\" = 5432'")?;
}
"#,
    );
    project.write(
        "config/test.env",
        "# test stack\nTEST_URL=postgres://h:5432\nexport PORT=\"5432\"\n",
    );
    let (run, code) = project.json(&["run", "env"]);
    assert_eq!(code, 0, "{run}");
    assert_eq!(target(&run, "env")["result"], "passed", "{run}");
}
