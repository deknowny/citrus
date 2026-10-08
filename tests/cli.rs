//! Behaviour of the real binary on throwaway Git repositories.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use serde_json::Value;

struct Project {
    dir: tempfile::TempDir,
}

const MAKEFILE: &str = "ok:\n\t@echo fine\nplain:\n\t@echo plain\nfail:\n\t@echo building; echo 'AssertionError: broken thing'; exit 1\nslow:\n\t@sleep 30\n";
const BASE: &str = r#"citrus 1

check ok = make("ok") {
  paths = ["src/*.txt"]
}

check fail = make("fail") {
  paths = ["other/*"]
}

check plain = make("plain") {
  paths = ["Makefile"]
  cache = false
}

check slow = make("slow") {
  paths = ["Makefile"]
  cache = false
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
        Command::new(env!("CARGO_BIN_EXE_citrus"))
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
const ALPHA_BETA: &str = r#"
runner builders = run("sh", "remote.sh")

group lib {
  paths = ["lib/**"]
  check alpha = make("ok")
  check beta = make("ok")
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
    let config = "project { toolchain = [\"rust-toolchain.toml\", \".tool-versions\"] }\n";
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
        "ok-b626444adc1a561321355d57268a67907ae6ee9b450da80d37bf49319f1d1cae.pass"
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
    let config = "runner builders = run(\"true\") {\n  status = run(\"sh\", \"res.sh\")\n}\n";
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

    project.declare("project {\n  logs = \"logs\"\n}\n\ncheck ghost = make(\"ghost\") {\n  paths = [\"missing/*\"]\n}\n");
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
const WEB: &str = "check web = make(\"plain\") {\n  paths = [\"web/**\"]\n  cache = false\n}\n";

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
        Command::new(env!("CARGO_BIN_EXE_citrus"))
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
    let config = "commands release {\n  \"make deploy\" = \"ship it\"\n}\n";
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

const RELEASES: &str = r#"
# The test app.
release app {
  environment = prod
  version {
    initial = "1.0.0-app"
  }
  step build = run("sh", "-c", "echo built {version}")
  step deploy = run("sh", "deploy.sh", version) {
    production = true
    recover = run("sh", "-c", "echo recovered {version}")
  }
  rollback = run("sh", "-c", "echo rolled back to {version} from {previous}") {
    production = true
  }
}
"#;

fn release_project() -> Project {
    let project = Project::new(&format!(
        "project {{\n  main = \"main\"\n  free_version = run(\"sh\", \"free.sh\")\n}}\n{RELEASES}"
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
            Command::new(env!("CARGO_BIN_EXE_citrus"))
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
    let project =
        Project::new("check gone = run(\"no-such-program-citrus\") {\n  paths = [\"src/**\"]\n}\n");
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
        log.matches("built 1.0.0-app").count(),
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
    let config = "project {\n  after_merge = run(\"sh\", \"-c\", \"mkdir -p .scratch && echo $0 > .scratch/hook\", before)\n}\n";
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
        &format!("{BASE}\nproject {{\n  after_merge = run(\"false\")\n}}\n"),
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
    let output = Command::new(env!("CARGO_BIN_EXE_citrus"))
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
    let project = Project::new("project {\n  logs = \".private/logs\"\n}\n");
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
        r#"
artifact api {
  inputs = ["src/**"]
}

environment prod = kubernetes(kubectl: "./kubectl.sh", namespace: "shop") {
  record = { annotation: "example.com/release" }
  deploy api = api
}

release api {
  environment = prod
  checks = none
  version {
    initial = "0.1.0"
  }
  step deploy = run("true")
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
        r#"
artifact api {
  inputs = ["src/**"]
}

environment prod = kubernetes(kubectl: "./kubectl.sh", namespace: "shop") {
  record = { annotation: "example.com/release", tag_prefix: "v" }
  deploy api = api
}
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
        r#"
artifact api {
  inputs = inputs_of(run("sh", "-c", "echo src/a.txt; echo Makefile"))
}

environment prod = kubernetes(kubectl: "./kubectl.sh") {
  record = { annotation: "example.com/release", tag_prefix: "v" }
  deploy api = api
}
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
    project.write("Makefile", "ok:\n\t@echo changed\n");
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
            "inputs = inputs_of(run(\"sh\", \"-c\", \"echo src/a.txt; echo Makefile\"))",
            "inputs = []",
        ),
    );
    let error = project.json(&["artifacts"]).0;
    assert!(
        error["error"].as_str().unwrap().contains("inputs_of"),
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
elif verb == "apply":
    manifest = sys.stdin.read()
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
fn image() { "echo IMAGE=registry.example/{artifact}@sha256:{key}" }
fn logged() { run("sh", "-c", "echo {artifact} >> .kube/builds; " + image()) }

artifact api {
  inputs = ["src/**"]
  build = { provider: "command", run: logged() }
}

artifact backup {
  inputs = ["other/**"]
  build = { provider: "command", run: run("sh", "-c", image()) }
}

artifact migrations {
  inputs = ["migrations/**"]
  build = { provider: "command", run: logged() }
}

environment prod = kubernetes(kubectl: "./kubectl.py", namespace: "shop") {
  record = { annotation: "example.com/release", tag_prefix: "v" }
  migrations = { artifact: "migrations", job: "job.yaml" }
  deploy api = api {
    fence = "api-lease"
    timeout = 10s
  }
  deploy backup = backup {
    kind = cronjob
    quiesce = true
  }
}
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
        &text.replace("environment = prod", "environment = prod\n  checks = none"),
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

const CI: &str = r#"citrus 1

project {
  main = "main"
}

for name in ["a", "b"] {
  check "check-{name}" = run("sh", "-c", "grep -q ok src/{name}.txt || {{ echo 'Error: src/{name}.txt is not ok'; exit 1; }}") {
    paths = ["src/{name}.txt"]
  }
}

# Copy a file once it exists.
task prepare = [wait.file("ready.txt", timeout: 5s), copy("ready.txt", "out/copied.txt")]
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

    // Changing what a check runs invalidates its earlier pass.
    project.write("citrus.ci", &CI.replace("grep -q ok", "grep -q 'ok'"));
    let (changed, _) = project.json(&["run", "check-a"]);
    assert_eq!(target(&changed, "check-a")["result"], "passed", "{changed}");

    let (checked, code) = project.json(&["check"]);
    assert_eq!(code, 0, "{checked}");
    assert_eq!(checked["checks"], 2);
    assert_eq!(checked["tasks"], 1);
}

#[test]
fn a_shell_brace_in_a_string_explains_interpolation() {
    let project =
        ci_project("citrus 1\ncheck x = sh(\"a || { b; }\") {\n  paths = [\"src/**\"]\n}\n");
    let message = project.json(&["status"]).0["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("write `{{`"), "{message}");
}

#[test]
fn an_error_in_citrus_ci_points_at_the_line() {
    let project = ci_project(
        "citrus 1\ncheck x {\n  paths = [\"src/**\"]\n}\ncheck y = mak(\"x\") {\n  paths = [\"src/**\"]\n}\n",
    );
    let (error, code) = project.json(&["status"]);
    assert_eq!(code, 2);
    let message = error["error"].as_str().unwrap();
    assert!(
        message.contains("citrus.ci:5:11") && message.contains("did you mean `make`?"),
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
    assert_eq!(failed["source"], "citrus.ci:14");
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
    let project = Project::new("project {\n  main = \"main\"\n}\n");
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            "check fail = make(\"fail\")",
            "check fail = make(\"plain\")",
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
    project.declare("commands {\n  \"make deploy\" = \"ship it\"\n}\n");
    project.commit("catalog");
    project.write(
        "citrus.ci",
        &fs::read_to_string(project.root().join("citrus.ci"))
            .unwrap()
            .replace(
                "check fail = make(\"plain\")",
                "check fail = make(\"fail\")",
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
        "runner builders = run(\"sh\", \"remote.sh\")\n\ncheck e2e = make(\"plain\") {\n  paths = [\"web/**\"]\n  meta = { linux: true, snapshot: [\"assets\"] }\n}\n",
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
        serde_json::json!([["run", "make", "--no-print-directory", "plain"]])
    );
    assert_eq!(checks["files"], serde_json::json!(["citrus.ci"]));
    let (targets, _) = project.json(&["targets"]);
    assert!(
        targets["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["source"] == "citrus.ci:23"),
        "{targets}"
    );
}

#[test]
fn an_artifact_ignores_dockerfile_stages_it_is_not_built_from() {
    let project = Project::new(
        "artifact api {\n  inputs = [\"Dockerfile\"]\n  dockerfile = { file: \"Dockerfile\", target: \"api\" }\n}\n",
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
        r#"
release site {
  environment = web
  checks = none
  step build = [copy("src/a.txt", "out/{version}.txt"), links.check("*.md")]
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
        "check test-app = make(\"ok\") {\n  paths = [\"crates/app/**\"]\n  reads = crate(\"app\")\n}\n",
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
    let project = Project::new("runner builders = run(\"sh\", \"remote.sh\")\n");
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
fn fmt_removes_aligned_columns_and_keeps_meaning() {
    let project = Project::new(
        "check t = make(\"ok\") {\n    paths   = [\"src/a.txt\",\"src/b.txt\"]   # note\n  cache = false\n}\n\ncheck m = match changed {\n  only(t)=>make(\"ok\")\n  _   =>  make(\"plain\")\n} {\n  paths = [\"src/*\"]\n}\n",
    );
    assert_eq!(project.json(&["fmt", "--check"]).1, 1);
    let (written, code) = project.json(&["fmt"]);
    assert_eq!(code, 0, "{written}");
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    assert!(text.contains("check t = make(\"ok\") {\n  paths = [\"src/a.txt\", \"src/b.txt\"]  # note\n  cache = false\n}\n"), "{text}");
    assert!(
        text.contains("  only(t) => make(\"ok\")\n  _ => make(\"plain\")\n"),
        "{text}"
    );
    assert_eq!(project.json(&["fmt", "--check"]).1, 0);
    assert_eq!(project.json(&["check"]).0["checks"], 6);
}

#[test]
fn a_check_the_pool_passed_is_reused_by_its_inputs() {
    let project = Project::new("runner builders = run(\"sh\", \"remote.sh\")\n");
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
    let project = Project::new("check sub = make(\"ok\") {\n  paths = [\"vendor/lib\"]\n}\n");
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
    let project =
        Project::new("check lib = make(\"ok\") {\n  paths = [\"lib/**\", \"!lib/vendor/**\"]\n}\n");
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
        r#"
project {
  main = "main"
}

profile fast
profile e2e

check unit = make("ok") {
  paths = ["lib/**"]
  profile = fast
}

check e2e-only = make("plain") {
  paths = ["lib/**"]
  profile = e2e
}

check slow-only = make("ok") {
  when = touched(e2e-only) and profile(e2e)
}

check part = make("ok") {
  paths = ["lib/**"]
}

# Runs `part` itself.
check whole = make("ok") {
  paths = ["lib/**"]
  covers = [part]
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
        r#"
project {
  main = "main"
  signals = run("sh", "signals.sh")
}

# Documentation: nothing to run.
group docs {
  paths = ["*.md"]
}

group clyer {
  paths = ["clyer/**"]
  check bot = make("ok")
}

group pipeline {
  paths = ["scripts/**"]
  check contract = match changed {
    only(pipeline) => make("ok")
    without(clyer) => make("plain")
    _ => make("fail")
  }
}

check backend = make("ok") {
  paths = ["crates/**"]
  when = signal("product:garvis")
}

check after-backend = make("ok") {
  when = selected(backend)
}

label scope-mixed {
  when = touched(clyer) and touched(pipeline)
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
        serde_json::json!(["pipeline.contract"]),
        "{plan}"
    );
    assert_eq!(plan["arms"]["pipeline.contract"], 0, "{plan}");
    let plan = plan_for(&[("scripts/run.sh", "y\n"), ("README.md", "x\n")]);
    assert_eq!(plan["arms"]["pipeline.contract"], 1, "{plan}");
    let (full, _) = project.json(&["plan"]);
    assert_eq!(
        full["run"]["pipeline.contract"],
        serde_json::json!([["run", "make", "--no-print-directory", "plain"]]),
        "{full}"
    );
    assert_eq!(plan["unmapped"], serde_json::json!([]), "{plan}");
    let plan = plan_for(&[("scripts/run.sh", "z\n"), ("clyer/bot.rs", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["clyer.bot", "pipeline.contract"]),
        "{plan}"
    );
    assert_eq!(plan["arms"], serde_json::json!({}), "the `_` arm: {plan}");
    assert_eq!(plan["labels"], serde_json::json!(["scope-mixed"]), "{plan}");
    let plan = plan_for(&[("crates/api/lib.rs", "x\n")]);
    assert_eq!(
        plan["targets"],
        serde_json::json!(["backend", "after-backend"]),
        "{plan}"
    );
    let plan = plan_for(&[("crates/clyer/lib.rs", "x\n")]);
    assert_eq!(plan["targets"], serde_json::json!([]), "{plan}");

    // The chosen arm is what runs.
    project.git(&["checkout", "-q", "-B", "feature", "main"]);
    project.write("scripts/run.sh", "w\n");
    project.write("README.md", "w\n");
    project.commit("change");
    let (run, _) = project.json(&["run"]);
    assert_eq!(
        target(&run, "pipeline.contract")["result"],
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
        r#"
project {
  signals = run("sh", "classify.sh")
}

# Removed code: only its removal appears in a diff.
group removed {
  paths = ["old/**", "!old/keep/**"]
  check contracts = make("ok")
}
"#,
    );
    project.write(
        "classify.sh",
        "grep '^gone/' \"$CITRUS_PATHS\" | sed 's/^/CLAIM /; s/$/ removed/'\n",
    );
    project.commit("classify");
    project.write("paths.txt", "gone/old.sh\n");
    let output = Command::new(env!("CARGO_BIN_EXE_citrus"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_citrus"))
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
        "project {\n  main = \"main\"\n}\n\ncheck config = make(\"ok\") {\n  paths = [\"citrus.ci\"]\n}\n",
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    project.declare("# a comment\n");
    project.commit("comment");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["config"]),
        "{plan}"
    );
}

#[test]
fn a_cargo_command_written_by_hand_points_at_the_built_in() {
    let project = Project::new("task fmt = run(\"cargo\", \"fmt\")\n");
    let (checked, _) = project.json(&["check"]);
    let warning = checked["warnings"][0]["message"].as_str().unwrap();
    assert!(warning.contains("cargo.fmt(…)"), "{checked}");
}

#[test]
fn a_citrus_directory_holds_one_file_per_product() {
    let project = Project::new("");
    fs::remove_file(project.root().join("citrus.ci")).unwrap();
    project.write(
        ".citrus/project.ci",
        "citrus 1\n\nproject {\n  main = \"main\"\n}\n\nlet sources = [\"src/*.txt\"]\n",
    );
    project.write(
        ".citrus/app.ci",
        "citrus 1\n\n# The app: its sources and tests.\ngroup app {\n  paths = sources\n  check unit = make(\"ok\")\n}\n",
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
        "# Reads the logs back.\ncheck logs = make(\"ok\") {\n  paths = [\"logs/**\"]\n}\n",
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
        "profile e2e\n\ncheck db = make(\"ok\") {\n  paths = [\"src/**\"]\n  profile = e3\n}\n",
    );
    assert!(
        missing.contains("no profile `e3`") && missing.contains("did you mean `e2e`?"),
        "{missing}"
    );
    let service =
        error("check db = make(\"ok\") {\n  paths = [\"src/**\"]\n  needs = [database]\n}\n");
    assert!(
        service.contains("no service `database`") && service.contains("service database"),
        "{service}"
    );
    let call = error("check db = mak(\"ok\") {\n  paths = [\"src/**\"]\n}\n");
    assert!(call.contains("did you mean `make`?"), "{call}");
    let arm = error(
        "check db = match changed {\n  only(x) => make(\"ok\")\n} {\n  paths = [\"src/**\"]\n}\n",
    );
    assert!(arm.contains("needs a last `_ =>"), "{arm}");

    // A service checks need is a resource the runner provides.
    let project = Project::new(
        "service browser {\n  limit = 2\n}\n\ncheck e2e = make(\"ok\") {\n  paths = [\"web/**\"]\n  needs = [browser]\n}\n",
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
        r#"
project {
  main = "main"
}

group api {
  paths = ["api/**"]
  check users = make("ok") {
    paths = ["api/users/**"]
  }
  check orders = make("ok") {
    paths = ["api/orders/**"]
  }
  # Every API test at once: cheaper than several parts.
  check all = make("ok") {
    replaces = [users, orders]
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
        r#"
# A stand-in database: a file appears when it is up.
service database = run("sh", "-c", "echo started >> .scratch/db; touch .scratch/ready") {
  ready = wait.file(".scratch/ready", timeout: 5s)
}

group db {
  paths = ["db/**"]
  needs = [database]
  check one = make("ok")
  check two = make("ok")
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
        r#"
let tools = ["scripts/tool.py", "scripts/test_tool.py"]
let clyer = ["clyer/**"]

# The tool's own tests.
check tool = make("ok") {
  paths = tools
}

# Everything else under scripts/; the tool is not part of it.
group infra {
  paths = ["scripts/**"] - tools
  check contract = match changed {
    without(clyer) => make("ok")
    _ => make("plain")
  }
}

check clyer-only = make("ok") {
  when = only(clyer + ["scripts/**"]) and touched(clyer)
}
"#,
    );
    let plan_for = |paths: &str| {
        project.write("paths.txt", paths);
        let plan = project.json(&["plan", "--paths-file", "paths.txt"]).0["plan"].clone();
        (plan["targets"].clone(), plan["arms"].clone())
    };
    assert_eq!(plan_for("scripts/tool.py\n").0, serde_json::json!(["tool"]));
    let (targets, arms) = plan_for("scripts/run.sh\n");
    assert_eq!(targets, serde_json::json!(["infra.contract"]));
    assert_eq!(arms["infra.contract"], 0);
    let (targets, arms) = plan_for("scripts/run.sh\nclyer/bot.rs\n");
    assert_eq!(targets, serde_json::json!(["infra.contract", "clyer-only"]));
    assert_eq!(arms, serde_json::json!({}));
}

#[test]
fn a_path_a_check_names_is_that_checks_alone() {
    let project = Project::new(
        r#"
group infra {
  paths = ["scripts/**"]
  check contract = make("ok")
}

# The tool's own tests: editing the tool does not run the infra contract.
check tool = make("ok") {
  paths = ["scripts/tool.py"]
  reads = ["scripts/lib.py"]
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
        r#"
# Platform crates every product builds on.
group platform {
  paths = ["platform/**"]
}

group vpn {
  paths = ["vpn/**"]
  check backend = make("ok") {
    paths = [platform, "vpn/backend/**"]
  }
}

check docs = make("ok") {
  paths = ["platform/README.md"]
}

# Runs for the platform's paths only.
check platform-only = make("ok") {
  paths = [platform]
  when = not touched(["vpn/**"])
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
        "check x = make(\"ok\") {\n  paths = [platfrm]\n}\n\ngroup platform {\n  paths = [\"src/**\"]\n}\n",
    );
    let error = unknown.json(&["status"]).0["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        error.contains("no group `platfrm`") && error.contains("did you mean `platform`?"),
        "{error}"
    );
}

#[test]
fn only_checks_with_known_inputs_are_reused() {
    let project = Project::new(
        r#"
project {
  cache = false
}

group lib {
  paths = ["lib/**"]
  cache = true
  check unit = make("ok")
}

# Selected by a condition: nothing tells what it reads.
check after = make("ok") {
  when = selected(lib.unit)
}

check named = make("ok") {
  paths = [lib, "extra/**"]
  cache = true
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
        serde_json::json!(["lib/**"]),
        "a group it names is an input: {targets}"
    );
}

#[test]
fn a_signal_command_can_give_a_path_to_one_check() {
    let project = Project::new(
        r#"
project {
  signals = run("sh", "classify.sh")
}

group web {
  paths = ["web/**"]
  check build = make("ok")
}

# Only the audited part of the workspace file changed.
check audit = make("ok") {
  when = signal("audit-only")
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
        "project {\n  main = \"main\"\n  signals = run(\"true\")\n}\n\ncheck gated = make(\"ok\") {\n  when = signal(\"release\")\n}\n",
    );
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace(
            "check gated = make(\"ok\")",
            "check gated = make(\"plain\")",
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
        "# Removed code: only its removal appears in a diff.\ngroup retired {\n  paths = [\"gone/**\"]\n}\n\ncheck reused = make(\"ok\") {\n  paths = [\"src/*.txt\"]\n  reads = [\"missing/**\"]\n}\n",
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
    let inputs = targets["targets"][0]["inputs"].to_string();
    assert!(
        inputs.contains("crates/api/**") && inputs.contains("crates/core/**"),
        "{inputs}"
    );
    assert!(!inputs.contains("crates/web"), "{inputs}");
    assert_eq!(
        targets["targets"][0]["meta"]["understood"][0],
        "cargo test -p api"
    );

    for (command, expected) in [
        ("cargo test -p apy", "did you mean `api`"),
        ("cargo tset", "did you mean `cargo test`"),
        ("make test", "has no paths"),
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
