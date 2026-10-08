//! The pool against a real Postgres: set CITRUS_TEST_POOL to a database URL
//! (CI starts one; locally `docker run -e POSTGRES_PASSWORD=t -p 55432:5432 postgres:17`
//! and CITRUS_TEST_POOL=postgres://postgres:t@localhost:55432/postgres?sslmode=disable).

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

/// The tests share one database and its queue: one at a time.
static POOL: std::sync::Mutex<()> = std::sync::Mutex::new(());

const CONFIG: &str = r#"#![citrus(2)]
#![private("secret.txt")]
#![prepare(cmd!("sh prep.sh"))]

/// The machine's own secret.txt arrived, the requester's did not, and
/// #![prepare] ran in the tree first.
#[paths("src/**")]
check prepared {
    run!("grep -q machine prepared.txt")?;
    run!("test -n $CITRUS_POOL_RUN")?;
}

/// Sees the requester's uncommitted change.
#[paths("src/**")]
check seen {
    run!("grep -q changed src/a.txt")?;
    run!("test -f src/new.txt")?;
}

#[paths("other/**")]
check broken {
    run!("sh -c 'echo building; echo AssertionError: broken thing; exit 1'")?;
}

/// Only an agent labelled `special` takes it.
#[paths("src/**")]
#[meta(requires = ["special"])]
check special {
    run!("true")?;
}
"#;

fn git(dir: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success(),
        "git {args:?}"
    );
}

fn citrus(dir: &Path, pool: &str, cache: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_citrus"));
    command
        .args(args)
        .current_dir(dir)
        .env("CITRUS_POOL", pool)
        .env("CITRUS_POOL_EXECUTOR", "self")
        .env("CITRUS_AGENT_CACHE", cache)
        .env(
            "CITRUS_AGENT_FILES",
            cache
                .ancestors()
                .find(|dir| dir.join("files").is_dir())
                .map_or_else(|| cache.join("files"), |dir| dir.join("files")),
        )
        .env("CITRUS_AGENT", "test")
        .env_remove("CODEX_THREAD_ID")
        .env_remove("CLAUDECODE");
    command
}

fn agent(dir: &Path, pool: &str, cache: &Path, name: &str, labels: &str) -> Child {
    citrus(
        dir,
        pool,
        cache,
        &[
            "agent",
            "--name",
            name,
            "--slots",
            "2",
            "--labels",
            labels,
            "--idle-exit",
            "4",
        ],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap()
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn result<'a>(run: &'a Value, name: &str) -> &'a Value {
    run["targets"]
        .as_array()
        .unwrap_or_else(|| panic!("no targets in {run}"))
        .iter()
        .find(|target| target["target"] == name)
        .unwrap_or_else(|| panic!("no {name} in {run}"))
}

#[test]
fn checks_run_on_the_agents_that_fit_them() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A clean pool: this test owns the database.
    let reset = Command::new(env!("CARGO_BIN_EXE_citrus"))
        .args(["pool", "--json"])
        .env("CITRUS_POOL", &pool)
        .output()
        .unwrap();
    assert!(
        reset.status.success(),
        "{}",
        String::from_utf8_lossy(&reset.stderr)
    );

    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agents");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::create_dir_all(project.join("other")).unwrap();
    fs::write(project.join("citrus.ci"), CONFIG).unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    fs::write(project.join("other/x"), "x\n").unwrap();
    fs::write(project.join("prep.sh"), "cp secret.txt prepared.txt\n").unwrap();
    fs::write(project.join(".gitignore"), "secret.txt\nprepared.txt\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);

    // The requester's secret stays here; agents bring their own.
    fs::write(project.join("secret.txt"), "requester\n").unwrap();
    git(
        &project,
        &["rm", "-q", "--cached", "--ignore-unmatch", "secret.txt"],
    );
    let files = work.path().join("files/origin");
    fs::create_dir_all(&files).unwrap();
    fs::write(files.join("secret.txt"), "machine\n").unwrap();
    // Uncommitted and untracked changes travel with the run.
    fs::write(project.join("src/a.txt"), "changed\n").unwrap();
    fs::write(project.join("src/new.txt"), "new\n").unwrap();
    fs::write(project.join("other/x"), "y\n").unwrap();

    let mut agents = [
        agent(&project, &pool, &cache.join("one"), "one", "plain"),
        agent(&project, &pool, &cache.join("two"), "two", "special"),
    ];
    let output = citrus(&project, &pool, &cache, &["run", "--remote", "--json"])
        .output()
        .unwrap();
    let run = json(&output);
    // `run` returns at once; wait for the result.
    let id = run["run"]["id"].as_str().unwrap().to_owned();
    let output = citrus(&project, &pool, &cache, &["wait", &id, "--json"])
        .output()
        .unwrap();
    let run = json(&output);
    let log = citrus(
        &project,
        &pool,
        &cache,
        &[
            "log",
            run["run"]["id"].as_str().unwrap_or("last"),
            "--full",
            "--text",
        ],
    )
    .output()
    .unwrap();
    let shown = || format!("{run}\n{}", String::from_utf8_lossy(&log.stdout));
    assert_eq!(output.status.code(), Some(1), "{}", shown());
    assert_eq!(result(&run, "seen")["result"], "passed", "{}", shown());
    assert_eq!(result(&run, "special")["result"], "passed", "{}", shown());
    assert_eq!(result(&run, "prepared")["result"], "passed", "{}", shown());
    assert_eq!(result(&run, "broken")["result"], "failed", "{}", shown());
    assert!(
        result(&run, "broken")["first_error"]
            .as_str()
            .unwrap_or_default()
            .contains("broken thing"),
        "{}",
        shown()
    );

    // The snapshot ref is gone from the remote once the run is over.
    let refs = Command::new("git")
        .args(["ls-remote", origin.to_str().unwrap(), "refs/citrus/*"])
        .output()
        .unwrap();
    assert!(
        refs.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&refs.stdout)
    );

    let overview = json(
        &Command::new(env!("CARGO_BIN_EXE_citrus"))
            .args(["pool", "--json"])
            .env("CITRUS_POOL", &pool)
            .output()
            .unwrap(),
    );
    let names: Vec<&str> = overview["pool"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|agent| agent["name"].as_str())
        .collect();
    assert!(
        names.contains(&"one") && names.contains(&"two"),
        "{overview}"
    );
    for agent in &mut agents {
        assert!(agent.wait().unwrap().success());
    }
}

/// The run's checks run inside its `#![image]` on an agent with Docker of the
/// same platform (CI: Linux).
#[test]
fn checks_run_inside_the_declared_image() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let platform = Command::new("docker")
        .args(["info", "--format", "{{.OSType}}/{{.Architecture}}"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    let host = match std::env::consts::ARCH {
        "x86_64" => "linux/x86_64",
        _ => "linux/aarch64",
    };
    if std::env::consts::OS != "linux"
        || platform
            .as_deref()
            .map(|p| p.replace("amd64", "x86_64").replace("arm64", "aarch64"))
            != Some(host.to_owned())
    {
        eprintln!("needs Docker of this machine's platform: image test skipped");
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agent");
    fs::create_dir_all(project.join("ci")).unwrap();
    fs::write(
        project.join("citrus.ci"),
        r#"#![citrus(2)]
#![image(dockerfile = "ci/Dockerfile")]

#[paths("src/**")]
check inside {
    run!("test -f /made-by-the-image")?;
}
"#,
    )
    .unwrap();
    fs::write(
        project.join("ci/Dockerfile"),
        "FROM ubuntu:24.04\nRUN apt-get update -qq && apt-get install -y -qq --no-install-recommends git >/dev/null && touch /made-by-the-image\n",
    )
    .unwrap();
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    fs::write(project.join("src/a.txt"), "two\n").unwrap();
    let mut worker = agent(&project, &pool, &cache, "boxed", "plain");
    let output = citrus(&project, &pool, &cache, &["run", "--remote", "--json"])
        .output()
        .unwrap();
    let run = json(&output);
    assert_eq!(output.status.code(), Some(0), "{run}");
    assert_eq!(result(&run, "inside")["result"], "passed", "{run}");
    assert!(worker.wait().unwrap().success());
}

/// A stopped agent hands its checks back; another agent finishes them.
#[test]
fn a_stopped_agent_hands_its_checks_back() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agents");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("citrus.ci"),
        "#![citrus(2)]\n\n#[paths(\"src/**\")]\ncheck slow {\n    run!(\"sleep 4\")?;\n}\n",
    )
    .unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    fs::write(project.join("src/a.txt"), "two\n").unwrap();

    let mut first = agent(&project, &pool, &cache.join("first"), "first", "plain");
    let requester = citrus(&project, &pool, &cache, &["run", "--remote", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the first agent runs the check, then stop it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let overview = json(
            &Command::new(env!("CARGO_BIN_EXE_citrus"))
                .args(["pool", "--json"])
                .env("CITRUS_POOL", &pool)
                .output()
                .unwrap(),
        );
        let running = overview["pool"]["running"].as_array().unwrap();
        if running.iter().any(|row| row[2] == "first") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the first agent never took the check"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: plain signal delivery to a child this test started.
    unsafe { libc::kill(first.id() as libc::pid_t, libc::SIGTERM) };
    assert!(first.wait().unwrap().success());
    let mut second = agent(&project, &pool, &cache.join("second"), "second", "plain");
    let output = requester.wait_with_output().unwrap();
    let run = json(&output);
    assert_eq!(output.status.code(), Some(0), "{run}");
    assert_eq!(result(&run, "slow")["result"], "passed", "{run}");
    let id = run["run"]["id"].as_str().unwrap();
    let log = citrus(&project, &pool, &cache, &["log", id, "--full", "--text"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&log.stdout).contains("first stopped: slow back in the queue"),
        "{}",
        String::from_utf8_lossy(&log.stdout)
    );
    assert!(second.wait().unwrap().success());
}

/// The local worker of a pool run dies; `citrus wait` follows the pool run again.
#[test]
fn a_lost_worker_follows_the_pool_run_again() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agents");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("citrus.ci"),
        "#![citrus(2)]\n\n#[paths(\"src/**\")]\ncheck slow {\n    run!(\"sleep 3\")?;\n}\n",
    )
    .unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    fs::write(project.join("src/a.txt"), "two\n").unwrap();

    let mut worker = agent(&project, &pool, &cache.join("one"), "steady", "plain");
    let started = json(
        &citrus(
            &project,
            &pool,
            &cache,
            &["run", "--remote", "--detach", "--json"],
        )
        .output()
        .unwrap(),
    );
    let id = started["run"]["id"].as_str().unwrap().to_owned();
    let pid = started["run"]["pid"].as_i64().unwrap();
    // Let it queue the check, then kill the local worker and its group.
    std::thread::sleep(std::time::Duration::from_secs(2));
    // SAFETY: plain signal delivery to the worker this test started.
    unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_millis(300));
    let output = citrus(&project, &pool, &cache, &["wait", &id, "--json"])
        .output()
        .unwrap();
    let run = json(&output);
    assert_eq!(output.status.code(), Some(0), "{run}");
    assert_eq!(result(&run, "slow")["result"], "passed", "{run}");
    assert!(worker.wait().unwrap().success());
}

/// An agent killed outright comes back under its name and runs its checks again.
#[test]
fn a_killed_agent_takes_its_checks_back_when_it_restarts() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agents");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("citrus.ci"),
        "#![citrus(2)]\n\n#[paths(\"src/**\")]\ncheck slow {\n    run!(\"sleep 4\")?;\n}\n",
    )
    .unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    fs::write(project.join("src/a.txt"), "two\n").unwrap();

    let mut first = agent(&project, &pool, &cache.join("first"), "first", "plain");
    let requester = citrus(&project, &pool, &cache, &["run", "--remote", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Wait until the first agent runs the check, then stop it.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let overview = json(
            &Command::new(env!("CARGO_BIN_EXE_citrus"))
                .args(["pool", "--json"])
                .env("CITRUS_POOL", &pool)
                .output()
                .unwrap(),
        );
        let running = overview["pool"]["running"].as_array().unwrap();
        if running.iter().any(|row| row[2] == "first") {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the first agent never took the check"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    // SAFETY: plain signal delivery to a child this test started.
    unsafe { libc::kill(first.id() as libc::pid_t, libc::SIGKILL) };
    let _ = first.wait();
    let mut second = agent(&project, &pool, &cache.join("first"), "first", "plain");
    let output = requester.wait_with_output().unwrap();
    let run = json(&output);
    assert_eq!(output.status.code(), Some(0), "{run}");
    assert_eq!(result(&run, "slow")["result"], "passed", "{run}");
    assert!(second.wait().unwrap().success());
}

/// A long check holds only its own slot: the other check of its batch is
/// reported, and its slot freed, while the long one still runs.
#[test]
fn a_long_check_does_not_hold_the_agent() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let work = tempfile::tempdir().unwrap();
    let project = work.path().join("project");
    let origin = work.path().join("origin.git");
    let cache = work.path().join("agents");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(
        project.join("citrus.ci"),
        "#![citrus(2)]\n\n#[paths(\"src/**\")]\ncheck slow {\n    run!(\"sleep 15\")?;\n}\n\n\
         #[paths(\"src/**\")]\ncheck fast {\n    run!(\"true\")?;\n}\n",
    )
    .unwrap();
    fs::write(project.join("src/a.txt"), "one\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["add", "-A"]);
    git(
        &project,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            "init",
        ],
    );
    git(
        work.path(),
        &["init", "-q", "--bare", "-b", "main", "origin.git"],
    );
    git(
        &project,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&project, &["push", "-q", "-u", "origin", "main"]);
    fs::write(project.join("src/a.txt"), "two\n").unwrap();

    let mut agent = agent(&project, &pool, &cache.join("one"), "one", "plain");
    let requester = citrus(&project, &pool, &cache, &["run", "--remote", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let running = || -> Vec<String> {
        let overview = json(
            &Command::new(env!("CARGO_BIN_EXE_citrus"))
                .args(["pool", "--json"])
                .env("CITRUS_POOL", &pool)
                .output()
                .unwrap(),
        );
        overview["pool"]["running"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row[1].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    // Both checks are claimed together; the fast one ends long before the
    // slow one and leaves the running list on its own.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut seen_slow = false;
    loop {
        let now = running();
        seen_slow |= now.iter().any(|check| check == "slow");
        if seen_slow && now == ["slow"] {
            break;
        }
        if std::time::Instant::now() >= deadline {
            let _ = agent.kill();
            let output = requester.wait_with_output().unwrap();
            panic!(
                "the fast check was not reported while the slow one ran: {now:?}\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let output = requester.wait_with_output().unwrap();
    let run = json(&output);
    assert_eq!(output.status.code(), Some(0), "{run}");
    assert_eq!(result(&run, "slow")["result"], "passed", "{run}");
    assert_eq!(result(&run, "fast")["result"], "passed", "{run}");
    assert!(agent.wait().unwrap().success());
}

/// A published build is listed under the commit it reports and can be
/// published again (a rebuild replaces it).
#[test]
fn a_published_build_is_held_by_commit_and_platform() {
    let Ok(pool) = std::env::var("CITRUS_TEST_POOL") else {
        eprintln!("CITRUS_TEST_POOL is not set: pool tests skipped");
        return;
    };
    let _serial = POOL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let work = tempfile::tempdir().unwrap();
    let commit = "0123456789abcdef0123456789abcdef01234567";
    let fake = work.path().join("citrus");
    fs::write(
        &fake,
        format!("#!/bin/sh\necho 'citrus 0.3.0 ({commit})'\n"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..2 {
        let output = Command::new(env!("CARGO_BIN_EXE_citrus"))
            .args(["pool", "publish", "--platform", "linux-x86_64"])
            .arg(&fake)
            .env("CITRUS_POOL", &pool)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let listed = json(
        &Command::new(env!("CARGO_BIN_EXE_citrus"))
            .args(["pool", "binaries", "--json"])
            .env("CITRUS_POOL", &pool)
            .output()
            .unwrap(),
    );
    let rows: Vec<&Value> = listed["binaries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["commit"] == commit)
        .collect();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["platform"], "linux-x86_64");
}
