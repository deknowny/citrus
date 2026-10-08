//! What Citrus plans and runs, compiled from the configuration
//! (src/lang): project settings, checks with their steps, groups, services,
//! tasks and releases; and how one step executes.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::lang::{Error, Sources, Span};

/// One unit of work Citrus executes, with where it was declared.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub span: Span,
    /// Human label, e.g. `make test-api` or `wait.tcp localhost:5432`.
    pub label: String,
    pub work: Work,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Work {
    Process {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        portable: bool,
    },
    WaitTcp {
        address: String,
        timeout: u64,
    },
    WaitHttp {
        url: String,
        timeout: u64,
    },
    WaitFile {
        path: String,
        timeout: u64,
    },
    Copy {
        from: String,
        to: String,
    },
    /// Relative links in Markdown files matching `pattern` point at existing files.
    LinksCheck {
        pattern: String,
    },
    /// A body written in language v2 (`check:name`, `step:release:name`, …);
    /// `digest` changes with its source, `args` are the release's values.
    Script {
        item: String,
        digest: String,
        args: Vec<(String, String)>,
        /// Environment of every program the body runs (check and profile `env`).
        #[serde(default)]
        env: Vec<(String, String)>,
    },
}

impl Work {
    /// Short human form, e.g. `make test-api` or `wait.tcp localhost:5432`.
    pub fn label(&self) -> String {
        match self {
            Work::Process { argv, .. } => argv
                .iter()
                .filter(|part| *part != "--no-print-directory")
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
            Work::WaitTcp { address, .. } => format!("wait.tcp {address}"),
            Work::WaitHttp { url, .. } => format!("wait.http {url}"),
            Work::WaitFile { path, .. } => format!("wait.file {path}"),
            Work::Copy { from, to } => format!("copy {from} → {to}"),
            Work::LinksCheck { pattern } => format!("links.check {pattern}"),
            Work::Script { item, .. } => item.clone(),
        }
    }

    /// The same work with every text passed through `apply` (runtime values).
    pub fn map_text(&self, apply: impl Fn(&str) -> String) -> Work {
        match self {
            Work::Process {
                argv,
                env,
                portable,
            } => Work::Process {
                argv: argv.iter().map(|part| apply(part)).collect(),
                env: env
                    .iter()
                    .map(|(key, value)| (key.clone(), apply(value)))
                    .collect(),
                portable: *portable,
            },
            Work::WaitTcp { address, timeout } => Work::WaitTcp {
                address: apply(address),
                timeout: *timeout,
            },
            Work::WaitHttp { url, timeout } => Work::WaitHttp {
                url: apply(url),
                timeout: *timeout,
            },
            Work::WaitFile { path, timeout } => Work::WaitFile {
                path: apply(path),
                timeout: *timeout,
            },
            Work::Copy { from, to } => Work::Copy {
                from: apply(from),
                to: apply(to),
            },
            Work::LinksCheck { pattern } => Work::LinksCheck {
                pattern: apply(pattern),
            },
            Work::Script {
                item,
                digest,
                args,
                env,
            } => Work::Script {
                item: item.clone(),
                digest: digest.clone(),
                args: args
                    .iter()
                    .map(|(key, value)| (key.clone(), apply(value)))
                    .collect(),
                env: env
                    .iter()
                    .map(|(key, value)| (key.clone(), apply(value)))
                    .collect(),
            },
        }
    }

    /// `[kind, arguments…]`, the documented form hashed into fingerprints.
    pub fn canonical(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Work::Process { argv, .. } => {
                out.push("run".to_owned());
                out.extend(argv.iter().cloned());
            }
            Work::WaitTcp { address, timeout } => {
                out.extend(["wait.tcp".into(), address.clone(), timeout.to_string()])
            }
            Work::WaitHttp { url, timeout } => {
                out.extend(["wait.http".into(), url.clone(), timeout.to_string()])
            }
            Work::WaitFile { path, timeout } => {
                out.extend(["wait.file".into(), path.clone(), timeout.to_string()])
            }
            Work::Copy { from, to } => out.extend(["copy".into(), from.clone(), to.clone()]),
            Work::LinksCheck { pattern } => out.extend(["links.check".into(), pattern.clone()]),
            Work::Script { item, digest, .. } => {
                out.extend(["script".into(), item.clone(), digest.clone()])
            }
        }
        out
    }
}

/// What `touched`, `only` and `without` look at: a group or check by name,
/// or a list of path globs.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Paths {
    Name(String),
    Globs(Vec<String>),
}

/// A plan-time condition (`when`), evaluated once the changed paths are known.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cond {
    Always(bool),
    /// A changed path is in this group, check or glob list.
    Touched(Paths),
    /// This check is in the plan.
    Selected(String),
    /// The project's signal command printed this signal.
    Signal(String),
    /// The plan is for this profile.
    Profile(String),
    /// Every changed path is in it.
    Only(Paths),
    /// No changed path is in it.
    Without(Paths),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
}

impl Cond {
    pub fn eval(&self, facts: &dyn Fn(&Cond) -> bool) -> bool {
        match self {
            Cond::Always(value) => *value,
            Cond::And(left, right) => left.eval(facts) && right.eval(facts),
            Cond::Or(left, right) => left.eval(facts) || right.eval(facts),
            Cond::Not(inner) => !inner.eval(facts),
            leaf => facts(leaf),
        }
    }
}

/// A set of paths and the checks that protect it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Group {
    pub name: String,
    pub owns: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    /// Its qualified name: `clyer.bot`.
    pub name: String,
    pub description: Option<String>,
    pub owns: Vec<String>,
    pub reads: Vec<String>,
    pub cache: bool,
    /// `cache` as the check or its group says; otherwise the project's default.
    #[serde(skip)]
    pub cache_set: Option<bool>,
    /// Its paths or `#[reads]` say what it reads; what Citrus inferred from
    /// a Make recipe alone does not let a pass be reused.
    #[serde(skip)]
    pub known_inputs: bool,
    pub resources: Vec<String>,
    /// Project-specific data for the project's own tools (`meta = { … }`).
    pub meta: BTreeMap<String, serde_json::Value>,
    pub env: Vec<(String, String)>,
    pub steps: Vec<Step>,
    /// The group it is declared in; `narrows`: it has its own `paths`.
    pub group: Option<String>,
    pub narrows: bool,
    /// Groups whose paths select it too (`paths = [platform, …]`).
    pub via: Vec<String>,
    /// Groups whose paths are read but do not select it (`#[reads(group)]`).
    #[serde(skip)]
    pub reads_via: Vec<String>,
    /// What its `make` recipes read: a change there selects it too, without
    /// taking the path from the checks and groups that own it.
    #[serde(skip)]
    pub follows: Vec<String>,
    /// Profiles the check belongs to; empty: every profile.
    pub profiles: Vec<String>,
    /// Checks that already run this one: with one of them in a plan, this one is dropped.
    pub covered_by: Vec<String>,
    /// Selected only when this holds (and, with `owns`, a path it owns changed).
    pub when: Option<Cond>,
    /// Checks it runs instead of when the change goes beyond one of them.
    pub replaces: Vec<String>,
    pub span: Span,
}

/// A service checks need: started by Citrus when it has an action,
/// otherwise a resource the runner provides.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Service {
    pub name: String,
    pub description: Option<String>,
    pub start: Vec<Step>,
    pub ready: Vec<Step>,
    /// Run when the run is over, whatever its result.
    pub stop: Vec<Step>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub name: String,
    pub about: String,
    pub steps: Vec<Step>,
    pub span: Span,
}

/// A runner: runs the planned checks elsewhere and reports them in the
/// runner protocol (docs/protocol.md).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Pool {
    pub argv: Vec<String>,
    /// Prints `CITRUS_RESOURCE key=value…` lines describing the runner's machines.
    pub status: Vec<String>,
}

/// `#![image(dockerfile = "…", target = "…", context = "…")]`: built by each
/// pool agent with Docker; the checks run inside it.
#[derive(Debug, Default, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Image {
    pub dockerfile: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default = "Image::default_context")]
    pub context: String,
}

impl Image {
    fn default_context() -> String {
        ".".into()
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Project {
    pub base: Option<String>,
    /// Profiles checks may belong to; the first one is the default.
    pub profiles: Vec<String>,
    pub logs: Option<String>,
    pub toolchain: Vec<String>,
    pub checks: Vec<Check>,
    pub tasks: Vec<Task>,
    pub receipts: Option<String>,
    /// Environment of each profile's checks.
    pub profile_env: Vec<(String, Vec<(String, String)>)>,
    pub services: Vec<Service>,
    pub pool: Option<Pool>,
    /// The image checks run in on pool agents (`#![image(...)]`).
    pub image: Option<Image>,
    /// Paths a pool snapshot never carries (`#![private(...)]`).
    pub private: Vec<String>,
    pub after_merge: Vec<String>,
    pub commands: Vec<(String, String, String)>,
    pub groups: Vec<Group>,
    /// Named conditions reported with the plan (`label "scope:main" { when = … }`).
    pub labels: Vec<(String, Cond)>,
    /// Prints `SIGNAL <name>` lines for the changed paths (in CITRUS_PATHS).
    pub signals: Vec<String>,
    /// Prints the first free version at or after CITRUS_VERSION for the names in CITRUS_SCOPE.
    #[serde(skip)]
    pub free_version: Vec<String>,
    #[serde(skip)]
    pub releases: BTreeMap<String, crate::release::Unit>,
    #[serde(skip)]
    pub artifacts: BTreeMap<String, crate::deploy::Artifact>,
    #[serde(skip)]
    pub environments: BTreeMap<String, crate::deploy::Environment>,
    /// Repository-relative `.ci` files this project was read from.
    pub files: Vec<String>,
    /// Declarations Citrus reads but does not execute from `.ci` yet.
    pub pending: Vec<(String, String, Span)>,
    pub warnings: Vec<Error>,
}

pub fn dead_globs(project: &Project, root: &Path) -> Vec<Error> {
    let files = crate::repo::Repo::discover_at(root)
        .and_then(|repo| repo.paths())
        .unwrap_or_default();
    let mut dead = Vec::new();
    if files.is_empty() {
        return dead;
    }
    for check in project.checks.iter().filter(|check| check.cache) {
        let mut reported: Vec<&String> = Vec::new();
        for pattern in check.owns.iter().chain(&check.reads) {
            if pattern.starts_with('!')
                || reported.contains(&pattern)
                || crate::manifest::pattern_matches_any(pattern, &files).unwrap_or(true)
            {
                continue;
            }
            reported.push(pattern);
            dead.push(Error::at(
                check.span,
                format!(
                    "check {} reuses passes, but its input `{pattern}` matches no file",
                    check.name
                ),
            ));
        }
    }
    dead
}

/// Execute one step in `root`; the process inherits stdout/stderr.
/// `stdout_to_stderr` keeps a JSON answer on stdout clean while steps print.
pub fn execute(step: &Step, root: &Path, stdout_to_stderr: bool) -> anyhow::Result<i32> {
    execute_env(step, root, stdout_to_stderr, &[])
}

/// `execute` with more environment for the programs the step starts.
pub fn execute_env(
    step: &Step,
    root: &Path,
    stdout_to_stderr: bool,
    extra: &[(String, String)],
) -> anyhow::Result<i32> {
    let deadline = |seconds: u64| Instant::now() + Duration::from_secs(seconds);
    match &step.work {
        Work::Process { argv, env, .. } => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("empty command"))?;
            let mut command = std::process::Command::new(program);
            command
                .args(args)
                .envs(env.iter().cloned())
                .envs(extra.iter().cloned())
                .current_dir(root)
                .stdin(std::process::Stdio::null());
            if stdout_to_stderr {
                use std::os::fd::AsFd;
                command.stdout(std::process::Stdio::from(
                    std::io::stderr().as_fd().try_clone_to_owned()?,
                ));
            }
            // A program that cannot start fails its check like a shell would
            // (127), with the reason in the log, rather than ending the run.
            match command.status() {
                Ok(status) => Ok(status.code().unwrap_or(-1)),
                Err(error) => {
                    eprintln!("error: cannot run {program}: {error}");
                    Ok(if error.kind() == std::io::ErrorKind::NotFound {
                        127
                    } else {
                        126
                    })
                }
            }
        }
        Work::WaitTcp { address, timeout } => {
            let until = deadline(*timeout);
            loop {
                if std::net::TcpStream::connect(address.as_str()).is_ok() {
                    return Ok(0);
                }
                if Instant::now() > until {
                    eprintln!("{address} did not accept connections within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Work::WaitHttp { url, timeout } => {
            let until = deadline(*timeout);
            loop {
                let ok = std::process::Command::new("curl")
                    .args([
                        "-fsS",
                        "--max-time",
                        "5",
                        "-o",
                        if cfg!(windows) { "NUL" } else { "/dev/null" },
                        url,
                    ])
                    .status()
                    .is_ok_and(|status| status.success());
                if ok {
                    return Ok(0);
                }
                if Instant::now() > until {
                    eprintln!("{url} did not answer within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Work::WaitFile { path, timeout } => {
            let until = deadline(*timeout);
            while !root.join(path).exists() {
                if Instant::now() > until {
                    eprintln!("{path} did not appear within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(0)
        }
        Work::Script {
            item, args, env, ..
        } => {
            let env: Vec<(String, String)> = env.iter().chain(extra).cloned().collect();
            Ok(crate::lang::run_item(root, item, args, &env))
        }
        Work::LinksCheck { pattern } => {
            let files = crate::repo::Repo::discover_at(root)?.files()?;
            let mut broken = 0;
            let mut checked = 0;
            for file in &files {
                if !crate::manifest::pattern_matches_any(pattern, std::slice::from_ref(file))? {
                    continue;
                }
                checked += 1;
                let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
                let dir = Path::new(file).parent().unwrap_or(Path::new(""));
                for link in markdown_links(&text) {
                    let target = link.split('#').next().unwrap_or_default();
                    if target.is_empty() || target.contains("://") || target.starts_with("mailto:")
                    {
                        continue;
                    }
                    if !root.join(dir).join(target).exists() {
                        eprintln!("{file}: broken link {link}");
                        broken += 1;
                    }
                }
            }
            if broken == 0 {
                eprintln!("links ok: {checked} files");
            }
            Ok(i32::from(broken > 0))
        }
        Work::Copy { from, to } => {
            let target = root.join(to);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(root.join(from), target)?;
            Ok(0)
        }
    }
}

/// Targets of `[text](target)` links.
fn markdown_links(text: &str) -> Vec<&str> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find(')') {
            let link = &rest[..end];
            if !link.contains(char::is_whitespace) {
                links.push(link);
            }
            rest = &rest[end..];
        }
    }
    links
}

/// The project in `citrus.ci` or `.citrus/*.ci` (not both), or None.
pub fn load(root: &Path) -> Result<Option<(Project, Sources)>, String> {
    let file = root.join("citrus.ci").exists();
    let directory = root.join(".citrus").is_dir()
        && std::fs::read_dir(root.join(".citrus"))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().extension().is_some_and(|ext| ext == "ci"))
            })
            .unwrap_or(false);
    match (file, directory) {
        (true, true) => Err(
            "error: both citrus.ci and .citrus/ hold a configuration; keep one\nhelp: a small project uses citrus.ci, a larger one .citrus/*.ci\n".into(),
        ),
        (false, false) => Ok(None),
        _ => load_at(root, None),
    }
}

/// The project as committed at `revision` (or the working tree), compiled.
pub fn load_at(root: &Path, revision: Option<&str>) -> Result<Option<(Project, Sources)>, String> {
    let entry = match revision {
        None if root.join("citrus.ci").exists() => "citrus.ci",
        None => ".citrus",
        Some(revision) => {
            let listed = crate::repo::git()
                .arg("-C")
                .arg(root)
                .args(["cat-file", "-e", &format!("{revision}:citrus.ci")])
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if listed { "citrus.ci" } else { ".citrus" }
        }
    };
    crate::lang::load(root, entry, revision).map(Some)
}
