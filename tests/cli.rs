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
