//! The pool against a real Postgres: set CITRUS_TEST_POOL to a database URL
//! (CI starts one; locally `docker run -e POSTGRES_PASSWORD=t -p 55432:5432 postgres:17`
//! and CITRUS_TEST_POOL=postgres://postgres:t@localhost:55432/postgres?sslmode=disable).

use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

use serde_json::Value;

const CONFIG: &str = r#"#![citrus(2)]

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
    assert_eq!(output.status.code(), Some(1), "{run}");
    assert_eq!(result(&run, "seen")["result"], "passed", "{run}");
    assert_eq!(result(&run, "special")["result"], "passed", "{run}");
    assert_eq!(result(&run, "broken")["result"], "failed", "{run}");
    assert!(
        result(&run, "broken")["first_error"]
            .as_str()
            .unwrap_or_default()
            .contains("broken thing"),
        "{run}"
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
