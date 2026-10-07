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

check "ok" {
  owns = ["src/*.txt"]
  run = make("ok")
  cache = true
  resources = ["contracts"]
}

check "fail" {
  owns = ["other/*"]
  run = make("fail")
  resources = ["contracts"]
}
"#;

impl Project {
    /// `config`: more of citrus.ci after the two base checks.
    fn new(config: &str) -> Project {
        let project = Project {
            dir: tempfile::tempdir().unwrap(),
        };
        project.write("Makefile", MAKEFILE);
        project.write("citrus.ci", &format!("{BASE}\n{config}"));
        project.write("src/a.txt", "one\n");
        project.write("other/x", "x\n");
        project.write(".gitignore", ".citrus/\n");
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
fn undeclared_target_is_reused_only_for_identical_sources() {
    let project = Project::new("");
    assert_eq!(
        target(&project.json(&["run", "plain"]).0, "plain")["result"],
        "passed"
    );
    assert_eq!(
        target(&project.json(&["run", "plain"]).0, "plain")["reason"],
        "same_snapshot"
    );
    project.write("new-untracked.txt", "x");
    let (rerun, _) = project.json(&["run", "plain"]);
    assert_eq!(target(&rerun, "plain")["result"], "passed");
    let forced = project.json(&["run", "plain", "--force"]).0;
    assert_eq!(target(&forced, "plain")["result"], "passed");
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

#[test]
fn remote_runner_progress_is_recorded_per_target() {
    let config = r#"
planner { run = run("sh", "plan.sh") }
pool "builders" { run = run("sh", "remote.sh"), progress = ["LANE"], waiting = "QUEUED resource=" }
"#;
    let project = Project::new(config);
    project.write("plan.sh", "printf 'PLAN\\tstatus=complete\\tfiles=1\\nTARGET\\tmake:alpha\\nTARGET\\tmake:beta\\nTARGET\\tno-heavy:docs\\n'\n");
    project.write(
        "remote.sh",
        "echo 'QUEUED resource=builder'\necho 'LANE target=alpha status=START'\necho 'LANE target=alpha status=PASS exit=0 seconds=3'\n\
         echo 'LANE target=beta status=START'\necho 'error: beta exploded'\necho 'LANE target=beta status=FAIL exit=2 seconds=4'\nexit 2\n",
    );
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(run["run"]["mode"], "remote");
    assert_eq!(target(&run, "alpha")["result"], "passed");
    assert_eq!(target(&run, "beta")["result"], "failed");
    assert!(
        target(&run, "beta")["first_error"]
            .as_str()
            .unwrap()
            .contains("beta exploded")
    );
    assert_eq!(target(&run, "beta")["seconds"], 4);
    assert!(
        project.json(&["run", "alpha", "--remote"]).0["error"]
            .as_str()
            .unwrap()
            .contains("drop the target names")
    );
}

#[test]
fn remote_details_are_read_from_the_linked_log() {
    let config = r#"
planner { run = run("sh", "plan.sh") }
pool "builders" { run = run("sh", "remote.sh"), progress = ["LANE"], log_after = ["full log: "] }
"#;
    let project = Project::new(config);
    project.write(
        "plan.sh",
        "printf 'TARGET\\tmake:alpha\\nTARGET\\tmake:beta\\n'\n",
    );
    project.write(
        ".citrus/detail.log",
        "LANE target=alpha status=START\nLANE target=alpha status=PASS exit=0\nLANE target=beta status=START\nerror: beta is broken in detail\nLANE target=beta status=FAIL exit=1\n",
    );
    project.write(
        "remote.sh",
        "echo 'ERROR remote gate failed; full log: .citrus/detail.log'\nexit 2\n",
    );
    let (run, code) = project.json(&["run"]);
    assert_eq!(code, 1, "{run}");
    assert_eq!(target(&run, "alpha")["result"], "passed");
    assert!(
        target(&run, "beta")["first_error"]
            .as_str()
            .unwrap()
            .contains("beta is broken in detail")
    );
    assert!(
        run["run"]["linked_log"]
            .as_str()
            .unwrap()
            .ends_with(".citrus/detail.log")
    );

    project.write(
        "remote.sh",
        "echo 'FAIL transport: descriptor mismatch for x'\nexit 2\n",
    );
    project.write("src/a.txt", "changed so the plan runs again\n");
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
fn add_declares_a_check_after_validating_it() {
    let project = Project::new("");
    let missing = project
        .json(&["add", "plain", "--inputs", "nothing/*.txt"])
        .0;
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("matches no file"),
        "{missing}"
    );
    let undefined = project
        .json(&[
            "add",
            "nope",
            "--inputs",
            "src/*.txt",
            "--resources",
            "contracts",
        ])
        .0;
    assert!(
        undefined["error"]
            .as_str()
            .unwrap()
            .contains("no `nope:` rule"),
        "{undefined}"
    );
    let duplicate = project.json(&["add", "ok", "--inputs", "src/*.txt"]).0;
    assert!(
        duplicate["error"]
            .as_str()
            .unwrap()
            .contains("already declared")
    );
    let no_resources = project.json(&["add", "plain", "--inputs", "Makefile"]).0;
    assert!(
        no_resources["error"]
            .as_str()
            .unwrap()
            .contains("--resources"),
        "{no_resources}"
    );

    let (added, code) = project.json(&[
        "add",
        "plain",
        "--inputs",
        "Makefile",
        "--cache",
        "--resources",
        "contracts",
        "--description",
        "plain check",
    ]);
    assert_eq!(code, 0, "{added}");
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    assert!(
        text.contains("check \"plain\" {\n  about = \"plain check\"\n  owns = [\"Makefile\"]\n  run = make(\"plain\")\n  cache = true\n  resources = [\"contracts\"]\n}"),
        "{text}"
    );
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["plan"]["unmapped"], serde_json::json!([]), "{plan}");
    assert!(
        plan["plan"]["targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "plain"),
        "{plan}"
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
    let config = "pool \"builders\" { run = run(\"true\"), status = run(\"sh\", \"res.sh\"), status_prefix = \"BUILDER \", refresh = 1m }\n";
    let project = Project::new(config);
    project.write("res.sh", "sleep 5\necho 'BUILDER host=root@b1 state=busy operation=remote-test owner=agent-a elapsed_seconds=90'\necho 'other line'\n");
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

    project.declare("check \"ghost\" { owns = [\"missing/*\"], run = make(\"ghost\") }\n");
    project.write(".gitignore", "");
    let (broken, code) = project.json(&["doctor"]);
    assert_eq!(code, 1);
    let failed: Vec<&str> = broken["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|finding| finding["status"] == "fail")
        .map(|finding| finding["check"].as_str().unwrap())
        .collect();
    assert!(failed.contains(&"target ghost"), "{broken}");
    assert!(failed.contains(&"logs"), "{broken}");
}

#[test]
fn bundled_examples_are_valid_configurations() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    for entry in fs::read_dir(&examples).unwrap() {
        let dir = entry.unwrap().path();
        let project = Project::new("");
        project.write(
            "citrus.ci",
            &fs::read_to_string(dir.join("citrus.ci")).unwrap(),
        );
        let (checked, code) = project.json(&["check"]);
        assert_eq!(code, 0, "{}: {checked}", dir.display());
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

const PLANNER: &str = "project { base = \"origin/main\" }\nplanner { run = run(\"sh\", \"plan.sh\"), paths_var = \"PATHS\" }\n";
// Changes under web/ select `plain`; without a path list the plan is this task's own changes.
const PLAN_SH: &str = "if [ -n \"$1\" ]; then grep -q '^web/' \"${1#PATHS=}\" && printf 'TARGET\\tmake:plain\\n'; printf 'PLAN\\tstatus=complete\\n'; else printf 'TARGET\\tmake:plain\\n'; fi\n";

#[test]
fn integrate_keeps_checks_the_incoming_changes_do_not_touch() {
    let project = Project::new(PLANNER);
    project.write("plan.sh", PLAN_SH);
    project.git(&["add", "-A"]);
    project.git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-qm",
        "planner",
    ]);
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
    assert_eq!(
        target(&project.json(&["run"]).0, "plain")["result"],
        "passed"
    );

    commit_upstream(&origin, "docs/readme.md", "unrelated\n");
    let (merged, code) = project.json(&["integrate", "--no-run"]);
    assert_eq!(code, 0, "{merged}");
    assert_eq!(merged["integration"]["outcome"], "merged");
    assert_eq!(
        merged["integration"]["carried"],
        serde_json::json!(["plain"]),
        "{merged}"
    );
    let (status, _) = project.json(&["status"]);
    assert_eq!(status["targets"][0]["reason"], "carried_over", "{status}");

    commit_upstream(&origin, "web/other.txt", "touches web\n");
    let (merged, _) = project.json(&["integrate", "--no-run"]);
    assert_eq!(
        merged["integration"]["reselected"],
        serde_json::json!(["plain"]),
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
fn tasks_and_notes_show_other_worktrees() {
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
    let output = Command::new(env!("CARGO_BIN_EXE_citrus"))
        .args(["note", "waiting for the schema change", "--json"])
        .current_dir(&other)
        .env("CITRUS_AGENT", "agent-b")
        .env_remove("CLAUDECODE")
        .env_remove("CODEX_THREAD_ID")
        .output()
        .unwrap();
    assert!(output.status.success());
    let (tasks, _) = project.json(&["tasks", "--all", "--base", "main"]);
    let list = tasks["tasks"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{tasks}");
    let second = list.iter().find(|task| task["branch"] == "second").unwrap();
    assert_eq!(second["note"], "waiting for the schema change");
    assert_eq!(second["note_agent"], "agent-b");
    let (status, _) = project.json(&["status"]);
    assert_eq!(
        status["notes"][0]["note"], "waiting for the schema change",
        "{status}"
    );
    project.json(&["note", "--clear"]);
}

#[test]
fn overview_lists_commands_and_the_project_catalog() {
    let config = "command \"make deploy\" { about = \"ship it\", group = \"release\" }\n";
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
    let (targets, _) = project.json(&["targets"]);
    assert_eq!(targets["targets"].as_array().unwrap().len(), 2);
}

const RELEASES: &str = r#"
release "app" {
  about = "test app"
  environment = "prod"
  # Reserves the next free version; prints RELEASE=<version>.
  version = { reserve: run("sh", "-c", "echo reserving {next}; echo RELEASE={next}"), initial: "1.0.0-app" }
  step "build" { run = run("sh", "-c", "echo built {version}") }
  step "deploy" {
    production = true
    run = run("sh", "deploy.sh", version)
    recover = run("sh", "-c", "echo recovered {version}")
  }
  rollback = { production: true, run: run("sh", "-c", "echo rolled back to {version} from {previous}") }
}
"#;

fn release_project() -> Project {
    let project = Project::new(&format!("project {{ base = \"main\" }}\n{RELEASES}"));
    project.write("deploy.sh", "if [ -f .fail ]; then echo 'Error: cluster unreachable'; exit 1; fi\nif [ -f .slow ]; then sleep 30; fi\necho deployed $1\n");
    project.write(".gitignore", ".citrus/\n.fail\n.slow\n");
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
    let config = "project { after_merge = run(\"sh\", \"-c\", \"mkdir -p .citrus && echo $0 > .citrus/hook\", before) }\n";
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
        fs::read_to_string(project.root().join(".citrus/hook"))
            .unwrap()
            .trim(),
        before.trim()
    );

    project.write(
        "citrus.ci",
        &format!("{BASE}\nproject {{ after_merge = run(\"false\") }}\n"),
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
    let project = Project::new("project { logs = \".private/logs\" }\n");
    project.write(".gitignore", ".citrus/\n.private/\n");
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

#[test]
fn an_external_planner_that_saw_nothing_cannot_pass_unmerged_commits() {
    // The planner reports zero changed files although the branch is ahead of the base.
    let config = "project { base = \"main\" }\nplanner { run = run(\"sh\", \"-c\", \"printf 'PLAN\\\\tstatus=complete\\\\tfiles=0\\\\n'\") }\n";
    let project = Project::new(config);
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
    let refused = project.json(&["run"]).0;
    assert!(
        refused["error"].as_str().unwrap().contains("--base main"),
        "{refused}"
    );
    project.git(&["checkout", "-q", "main"]);
    assert_eq!(project.json(&["run"]).0["run"]["state"], "passed");
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
fn diff_compares_what_runs_with_what_head_would_build() {
    let project = Project::new("");
    project.declare(
        r#"
artifact "api" { inputs = ["src/**"] }

environment "prod" {
  on = kubernetes(kubectl: "./kubectl.sh", namespace: "shop")
  record = { annotation: "example.com/release", tag_prefix: "v" }
  deploy "api" { artifact = "api" }
}
"#,
    );
    project.write("kubectl.sh", KUBECTL);
    let mut perms = fs::metadata(project.root().join("kubectl.sh"))
        .unwrap()
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(project.root().join("kubectl.sh"), perms).unwrap();
    project.write(".gitignore", ".citrus/\n.citrus-release\n.citrus-ready\n");
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
artifact "api" { inputs = inputs_of(run("sh", "-c", "echo src/a.txt; echo Makefile")) }

environment "prod" {
  on = kubernetes(kubectl: "./kubectl.sh")
  record = { annotation: "example.com/release", tag_prefix: "v" }
  deploy "api" { artifact = "api" }
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

artifact "api" { inputs = ["src/**"], build = { provider: "command", run: logged() } }
artifact "backup" { inputs = ["other/**"], build = { provider: "command", run: run("sh", "-c", image()) } }
artifact "migrations" { inputs = ["migrations/**"], build = { provider: "command", run: logged() } }

environment "prod" {
  on = kubernetes(kubectl: "./kubectl.py", namespace: "shop")
  record = { annotation: "example.com/release", tag_prefix: "v" }
  migrations = { artifact: "migrations", job: "job.yaml" }
  deploy "api" { artifact = "api", fence = "api-lease", timeout = 10s }
  deploy "backup" { artifact = "backup", kind = cronjob, quiesce = true }
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
    project.write(".gitignore", ".citrus/\n.kube/\n");
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
        &text.replace(
            "environment = \"prod\"",
            "environment = \"prod\"\n  checks = none",
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

const CI: &str = r#"citrus 1

project { base = "main" }

for name in ["a", "b"] {
  check "check-{name}" {
    owns  = ["src/{name}.txt"]
    run   = run("sh", "-c", "grep -q ok src/{name}.txt || {{ echo 'Error: src/{name}.txt is not ok'; exit 1; }}")
    cache = true
  }
}

task "prepare" {
  about = "copy a file once it exists"
  steps = [wait.file("ready.txt", timeout: 5s), copy("ready.txt", "out/copied.txt")]
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
    let project = ci_project(
        "citrus 1\ncheck \"x\" {\n  owns = [\"src/**\"]\n  run = sh(\"a || { b; }\")\n}\n",
    );
    let message = project.json(&["status"]).0["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("write `{{`"), "{message}");
}

#[test]
fn an_error_in_citrus_ci_points_at_the_line() {
    let project =
        ci_project("citrus 1\ncheck \"x\" {\n  owns = [\"src/**\"]\n  run  = mak(\"x\")\n}\n");
    let (error, code) = project.json(&["status"]);
    assert_eq!(code, 2);
    let message = error["error"].as_str().unwrap();
    assert!(
        message.contains("citrus.ci:4:10") && message.contains("did you mean `make`?"),
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
    assert_eq!(failed["source"], "citrus.ci:15");
    assert!(
        project.json(&["do", "prepar"]).0["error"]
            .as_str()
            .unwrap()
            .contains("did you mean prepare")
    );
}

#[test]
fn add_and_doctor_follow_citrus_ci() {
    let project = ci_project(CI);
    let (added, code) = project.json(&["add", "plain", "--inputs", "Makefile", "--cache"]);
    assert_eq!(code, 0, "{added}");
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    assert!(
        text.contains(
            "check \"plain\" {\n  owns = [\"Makefile\"]\n  run = make(\"plain\")\n  cache = true\n}"
        ),
        "{text}"
    );
    assert_eq!(project.json(&["check"]).0["checks"], 3);

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
    let project = Project::new("project { base = \"main\" }\n");
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace("run = make(\"fail\")", "run = make(\"plain\")"),
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
    project.declare("command \"make deploy\" { about = \"ship it\" }\n");
    project.commit("catalog");
    project.write(
        "citrus.ci",
        &fs::read_to_string(project.root().join("citrus.ci"))
            .unwrap()
            .replace("run = make(\"plain\")", "run = make(\"fail\")"),
    );
    project.commit("back");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(plan["plan"]["targets"], serde_json::json!([]), "{plan}");
    assert_eq!(plan["plan"]["mapped"][0][1], "config", "{plan}");
}

#[test]
fn project_tools_read_the_declared_checks() {
    let project = Project::new(
        "planner { run = run(\"sh\", \"plan.sh\") }\ncheck \"e2e\" { owns = [\"web/**\"], run = make(\"plain\"), meta = { linux: true, snapshot: [\"assets\"] } }\n",
    );
    project.write("web/x", "x\n");
    project.write("plan.sh", "mkdir -p .citrus && cp \"$CITRUS_CHECKS\" .citrus/checks.json\nprintf 'TARGET\\tmake:ok\\n'\n");
    project.commit("planner");
    assert_eq!(
        project.json(&["plan"]).0["plan"]["targets"],
        serde_json::json!(["ok"])
    );
    let checks: Value = serde_json::from_str(
        &fs::read_to_string(project.root().join(".citrus/checks.json")).unwrap(),
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
            .any(|row| row["source"] == "citrus.ci:17"),
        "{targets}"
    );
}

#[test]
fn an_incomplete_plan_is_refused_before_anything_runs() {
    let project = Project::new("planner { run = run(\"sh\", \"plan.sh\") }\n");
    project.write(
        "plan.sh",
        "printf 'PLAN\\tstatus=incomplete\\tfiles=2\\nUNMAPPED\\tweird/path\\nTARGET\\tmake:ok\\n'\n",
    );
    project.commit("planner");
    let (refused, code) = project.json(&["run"]);
    assert_eq!(code, 2, "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("weird/path"),
        "{refused}"
    );
    assert_eq!(project.json(&["run", "ok"]).1, 0);
}

#[test]
fn an_artifact_ignores_dockerfile_stages_it_is_not_built_from() {
    let project = Project::new(
        "artifact \"api\" { inputs = [\"Dockerfile\"], dockerfile = { file: \"Dockerfile\", target: \"api\" } }\n",
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
release "site" {
  environment = "web"
  checks = none
  step "build" { run = [copy("src/a.txt", "out/{version}.txt"), links.check("*.md")] }
}
"#,
    );
    project.write(".gitignore", ".citrus/\nout/\n");
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
        "check \"test-app\" { owns = [\"crates/app/**\"], reads = cargo.closure(\"app\"), run = make(\"ok\"), cache = true }\n",
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
fn an_edited_declaration_joins_the_plan_of_an_external_planner() {
    let project =
        Project::new("project { base = \"main\" }\nplanner { run = run(\"sh\", \"plan.sh\") }\n");
    project.write(
        "plan.sh",
        "printf 'PLAN\\tstatus=complete\\tfiles=1\\nTARGET\\tmake:ok\\n'\n",
    );
    project.commit("planner");
    project.git(&["checkout", "-q", "-b", "feature"]);
    let text = fs::read_to_string(project.root().join("citrus.ci")).unwrap();
    project.write(
        "citrus.ci",
        &text.replace("run = make(\"fail\")", "run = make(\"plain\")"),
    );
    project.commit("fail runs plain");
    let (plan, _) = project.json(&["plan"]);
    assert_eq!(
        plan["plan"]["targets"],
        serde_json::json!(["ok", "fail"]),
        "{plan}"
    );
}
