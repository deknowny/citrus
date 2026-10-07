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
const MANIFEST: &str = "[targets.ok]\ninputs = [\"src/*.txt\"]\ncache = true\nresources = [\"contracts\"]\n\n[targets.fail]\ninputs = [\"other/*\"]\nresources = [\"contracts\"]\n";

impl Project {
    fn new(config: &str) -> Project {
        let project = Project {
            dir: tempfile::tempdir().unwrap(),
        };
        project.write("Makefile", MAKEFILE);
        project.write("ci/targets.toml", MANIFEST);
        project.write("src/a.txt", "one\n");
        project.write("other/x", "x\n");
        project.write(".gitignore", ".citrus/\n");
        if !config.is_empty() {
            project.write("citrus.toml", config);
        }
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
    let config = "[plan]\ncommand = [\"sh\", \"plan.sh\"]\n\n[run]\nremote = [\"sh\", \"remote.sh\"]\nprogress_prefixes = [\"LANE\"]\nwaiting_prefix = \"QUEUED resource=\"\n";
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
    let config = "[plan]\ncommand = [\"sh\", \"plan.sh\"]\n\n[run]\nremote = [\"sh\", \"remote.sh\"]\nprogress_prefixes = [\"LANE\"]\nlinked_log_markers = [\"full log: \"]\n";
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
    let config = "toolchain_files = [\"rust-toolchain.toml\", \".tool-versions\"]\n";
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
        "ok-bfc713c0adc9e9b4165a7f72141cc6b5f1ea2c7e4ea1de991c0dce8fa908e80f.pass"
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
    let manifest = fs::read_to_string(project.root().join("ci/targets.toml")).unwrap();
    assert!(
        manifest.contains("[targets.plain]\ndescription = \"plain check\"\ncache = true"),
        "{manifest}"
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
    let config = "[status]\nresources_command = [\"sh\", \"res.sh\"]\nresource_prefix = \"BUILDER \"\nrefresh_seconds = 60\n";
    let project = Project::new(config);
    project.write("res.sh", "sleep 1\necho 'BUILDER host=root@b1 state=busy operation=remote-test owner=agent-a elapsed_seconds=90'\necho 'other line'\n");
    let started = Instant::now();
    let (first, _) = project.json(&["status"]);
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "status must not wait for the resource command"
    );
    assert_eq!(first["resources"]["refreshing"], true);
    let deadline = Instant::now() + Duration::from_secs(10);
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

    project.write(
        "ci/targets.toml",
        &format!("{MANIFEST}\n[targets.ghost]\ninputs = [\"missing/*\"]\n"),
    );
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
        let project = Project::new(&fs::read_to_string(dir.join("citrus.toml")).unwrap());
        project.write(
            "ci/targets.toml",
            &fs::read_to_string(dir.join("targets.toml")).unwrap(),
        );
        let (status, _) = project.json(&["why", "nothing"]);
        assert!(
            status
                .get("error")
                .is_none_or(|error| !error.as_str().unwrap().contains("invalid")),
            "{}: {status}",
            dir.display()
        );
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

const PLANNER: &str =
    "[plan]\ncommand = [\"sh\", \"plan.sh\"]\npaths_arg = \"{file}\"\nbase = \"origin/main\"\n";
// Changes under web/ select `plain`; without a path list the plan is this task's own changes.
const PLAN_SH: &str = "if [ -n \"$1\" ]; then grep -q '^web/' \"$1\" && printf 'TARGET\\tmake:plain\\n'; printf 'PLAN\\tstatus=complete\\n'; else printf 'TARGET\\tmake:plain\\n'; fi\n";

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
    let config =
        "[[catalog]]\ncommand = \"make deploy\"\ndescription = \"ship it\"\ngroup = \"release\"\n";
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
[releases.app]
description = "test app"
environment = "prod"

[releases.app.version]
reserve = ["sh", "-c", "echo reserving {next}; echo RELEASE={next}"]
initial = "1.0.0-app"

[[releases.app.steps]]
name = "build"
run = ["sh", "-c", "echo built {version}"]

[[releases.app.steps]]
name = "deploy"
production = true
run = ["sh", "deploy.sh", "{version}"]
recover = ["sh", "-c", "echo recovered {version}"]

[releases.app.rollback]
production = true
run = ["sh", "-c", "echo rolled back to {version} from {previous}"]
"#;

fn release_project() -> Project {
    let project = Project::new("[plan]\nbase = \"main\"\n");
    project.write("ci/releases.toml", RELEASES);
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
    let config = "[integrate]\nafter_merge = [\"sh\", \"-c\", \"mkdir -p .citrus && echo $0 > .citrus/hook\", \"{before}\"]\n";
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

    project.write("citrus.toml", "[integrate]\nafter_merge = [\"false\"]\n");
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
