//! From evaluated declarations to what Citrus runs: project settings, checks
//! with their steps, tasks. Validation that needs the meaning of a block
//! (known kinds and fields, portable actions, globs that match) lives here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use super::eval::{Action, Decl, Graph, Value};
use super::{Error, Sources, Span};

/// One unit of work Citrus executes, with where it was declared.
#[derive(Debug, Clone, serde::Serialize)]
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
        }
        out
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub name: String,
    pub description: Option<String>,
    pub owns: Vec<String>,
    pub reads: Vec<String>,
    pub cache: bool,
    pub resources: Vec<String>,
    /// Project-specific data for the project's own tools (`meta = { … }`).
    pub meta: BTreeMap<String, serde_json::Value>,
    pub env: Vec<(String, String)>,
    pub steps: Vec<Step>,
    pub span: Span,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub name: String,
    pub about: String,
    pub steps: Vec<Step>,
    pub span: Span,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Planner {
    pub argv: Vec<String>,
    pub base_var: Option<String>,
    pub paths_var: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Pool {
    pub argv: Vec<String>,
    pub progress: Vec<String>,
    pub waiting: Option<String>,
    pub acquired: Vec<String>,
    pub stage: Option<String>,
    pub log_after: Vec<String>,
    pub status: Vec<String>,
    pub status_prefix: Option<String>,
    pub refresh: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Project {
    pub base: Option<String>,
    pub logs: Option<String>,
    pub toolchain: Vec<String>,
    pub checks: Vec<Check>,
    pub tasks: Vec<Task>,
    pub receipts: Option<String>,
    /// Extra environment per check name.
    pub check_env: Vec<(String, Vec<(String, String)>)>,
    pub planner: Option<Planner>,
    pub pool: Option<Pool>,
    pub after_merge: Vec<String>,
    pub commands: Vec<(String, String, String)>,
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

const KINDS: &[&str] = &[
    "project",
    "planner",
    "command",
    "check",
    "task",
    "artifact",
    "environment",
    "pool",
    "release",
];

pub fn compile(graph: &Graph, root: &Path) -> Result<Project, Error> {
    let mut project = Project::default();
    for decl in &graph.decls {
        match decl.kind.as_str() {
            "project" => {
                known_fields(
                    decl,
                    &[
                        "base",
                        "logs",
                        "toolchain",
                        "receipts",
                        "check_env",
                        "after_merge",
                    ],
                )?;
                project.base = optional_string(decl, "base")?;
                project.logs = optional_string(decl, "logs")?;
                project.toolchain = strings(decl, "toolchain")?;
                project.receipts = optional_string(decl, "receipts")?;
                if let Some(value) = decl.field("check_env") {
                    let Value::Map(entries) = value else {
                        return Err(Error::at(
                            decl.field_span("check_env"),
                            "`check_env` is a map from check name to a map of variables",
                        ));
                    };
                    for (name, vars) in entries {
                        let Value::Map(vars) = vars else {
                            return Err(Error::at(
                                decl.field_span("check_env"),
                                format!("`check_env.{name}` must be a map of variables"),
                            ));
                        };
                        project.check_env.push((
                            name.clone(),
                            vars.iter()
                                .map(|(key, value)| (key.clone(), value_text(value)))
                                .collect(),
                        ));
                    }
                }
                if let Some(Value::Action(action)) = decl.field("after_merge") {
                    project.after_merge = argv(action)?;
                }
            }
            "planner" => {
                known_fields(decl, &["run", "base_var", "paths_var"])?;
                let Some(Value::Action(action)) = decl.field("run") else {
                    return Err(Error::at(
                        decl.span,
                        "`planner` needs `run = make(\"…\")` printing TARGET lines",
                    ));
                };
                project.planner = Some(Planner {
                    argv: argv(action)?,
                    base_var: optional_string(decl, "base_var")?,
                    paths_var: optional_string(decl, "paths_var")?,
                });
            }
            "pool" => {
                known_fields(
                    decl,
                    &[
                        "run",
                        "progress",
                        "waiting",
                        "acquired",
                        "stage",
                        "log_after",
                        "status",
                        "status_prefix",
                        "refresh",
                    ],
                )?;
                let Some(Value::Action(action)) = decl.field("run") else {
                    return Err(Error::at(
                        decl.span,
                        "`pool` needs `run = …`: the command that runs the planned checks there",
                    ));
                };
                let status = match decl.field("status") {
                    Some(Value::Action(action)) => argv(action)?,
                    _ => Vec::new(),
                };
                project.pool = Some(Pool {
                    argv: argv(action)?,
                    progress: strings(decl, "progress")?,
                    waiting: optional_string(decl, "waiting")?,
                    acquired: strings(decl, "acquired")?,
                    stage: optional_string(decl, "stage")?,
                    log_after: strings(decl, "log_after")?,
                    status,
                    status_prefix: optional_string(decl, "status_prefix")?,
                    refresh: match decl.field("refresh") {
                        Some(Value::Duration(seconds)) => *seconds,
                        _ => 60,
                    },
                });
            }
            "command" => {
                known_fields(decl, &["about", "group"])?;
                project.commands.push((
                    label(decl)?,
                    optional_string(decl, "about")?.unwrap_or_default(),
                    optional_string(decl, "group")?.unwrap_or_default(),
                ));
            }
            "check" => {
                known_fields(
                    decl,
                    &[
                        "owns",
                        "reads",
                        "run",
                        "cache",
                        "about",
                        "resources",
                        "meta",
                        "env",
                        "on",
                    ],
                )?;
                let name = label(decl)?;
                if !crate::manifest::valid_name(&name) {
                    return Err(Error::at(
                        decl.span,
                        format!(
                            "check name \"{name}\" must be lowercase letters, digits, `.`, `_` or `-`"
                        ),
                    ));
                }
                if project.checks.iter().any(|check| check.name == name) {
                    return Err(Error::at(
                        decl.span,
                        format!("check \"{name}\" is declared twice"),
                    ));
                }
                let owns = unique(strings(decl, "owns")?);
                if owns.is_empty() {
                    return Err(
                        Error::at(decl.span, format!("check \"{name}\" owns no files")).help(
                            "add `owns = [\"path/**\"]`: changing those files selects the check",
                        ),
                    );
                }
                let env = string_map(decl, "env")?;
                let steps = steps(decl, "run", &env, &mut project.warnings)?;
                if steps.is_empty() {
                    return Err(Error::at(
                        decl.span,
                        format!("check \"{name}\" has nothing to run"),
                    )
                    .help("add `run = make(\"…\")` or another action"));
                }
                project.checks.push(Check {
                    name,
                    description: optional_string(decl, "about")?,
                    owns,
                    reads: unique(strings(decl, "reads")?),
                    cache: matches!(decl.field("cache"), Some(Value::Bool(true))),
                    resources: strings(decl, "resources")?,
                    meta: match decl.field("meta") {
                        None => BTreeMap::new(),
                        Some(value @ Value::Map(_)) => {
                            match to_json(value, decl.field_span("meta"))? {
                                serde_json::Value::Object(map) => map.into_iter().collect(),
                                _ => BTreeMap::new(),
                            }
                        }
                        Some(other) => {
                            return Err(Error::at(
                                decl.field_span("meta"),
                                format!("`meta` must be a map, not a {}", other.type_name()),
                            ));
                        }
                    },
                    env,
                    steps,
                    span: decl.span,
                });
            }
            "task" => {
                known_fields(decl, &["about", "steps", "env"])?;
                let name = label(decl)?;
                let env = string_map(decl, "env")?;
                project.tasks.push(Task {
                    about: optional_string(decl, "about")?.unwrap_or_default(),
                    steps: steps(decl, "steps", &env, &mut project.warnings)?,
                    name,
                    span: decl.span,
                });
            }
            "release" => {
                let name = label(decl)?;
                let unit: crate::release::Unit = typed(decl, release_json(decl)?)?;
                unit.validate(&name)
                    .map_err(|error| Error::at(decl.span, error.to_string()))?;
                project.releases.insert(name, unit);
            }
            "artifact" => {
                let name = label(decl)?;
                project
                    .artifacts
                    .insert(name, typed(decl, artifact_json(decl)?)?);
            }
            "environment" => {
                let name = label(decl)?;
                project
                    .environments
                    .insert(name, typed(decl, environment_json(decl)?)?);
            }
            kind if KINDS.contains(&kind) => project.pending.push((
                kind.to_owned(),
                decl.name.clone().unwrap_or_default(),
                decl.span,
            )),
            other => {
                let error = Error::at(decl.span, format!("unknown block `{other}`"));
                return Err(match super::suggest(other, KINDS.iter().copied()) {
                    Some(close) => error.help(format!("did you mean `{close}`?")),
                    None => error.help(format!("blocks are: {}", KINDS.join(", "))),
                });
            }
        }
    }
    // Globs that match nothing are almost always typos.
    let files = crate::repo::Repo::discover_at(root)
        .and_then(|repo| repo.paths())
        .unwrap_or_default();
    if !files.is_empty() {
        for check in &project.checks {
            for pattern in check.owns.iter().chain(&check.reads) {
                if !crate::manifest::pattern_matches_any(pattern, &files).unwrap_or(true) {
                    project.warnings.push(Error::at(
                        check.span,
                        format!("check \"{}\": `{pattern}` matches no file", check.name),
                    ));
                }
            }
        }
    }
    Ok(project)
}

/// First occurrence of each entry, in order (input lists are often joined).
fn unique(items: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

fn known_fields(decl: &Decl, known: &[&str]) -> Result<(), Error> {
    for (name, _, span) in &decl.fields {
        if !known.contains(&name.as_str()) {
            let error = Error::at(
                *span,
                format!("`{}` blocks have no field `{name}`", decl.kind),
            );
            return Err(match super::suggest(name, known.iter().copied()) {
                Some(close) => error.help(format!("did you mean `{close}`?")),
                None => error.help(format!("fields are: {}", known.join(", "))),
            });
        }
    }
    if let Some(child) = decl.children.first() {
        return Err(Error::at(
            child.span,
            format!(
                "`{}` blocks cannot contain `{}` blocks",
                decl.kind, child.kind
            ),
        ));
    }
    Ok(())
}

fn label(decl: &Decl) -> Result<String, Error> {
    decl.name.clone().ok_or_else(|| {
        Error::at(decl.span, format!("a `{}` block needs a name", decl.kind))
            .help(format!("{} \"name\" {{ … }}", decl.kind))
    })
}

fn optional_string(decl: &Decl, field: &str) -> Result<Option<String>, Error> {
    match decl.field(field) {
        None | Some(Value::None) => Ok(None),
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(other) => Err(Error::at(
            decl.field_span(field),
            format!("`{field}` must be a string, not a {}", other.type_name()),
        )),
    }
}

fn strings(decl: &Decl, field: &str) -> Result<Vec<String>, Error> {
    match decl.field(field) {
        None | Some(Value::None) => Ok(Vec::new()),
        Some(Value::Str(value)) => Ok(vec![value.clone()]),
        Some(Value::List(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::at(
                        decl.field_span(field),
                        format!(
                            "`{field}` must be a list of strings; found a {}",
                            item.type_name()
                        ),
                    )
                })
            })
            .collect(),
        Some(other) => Err(Error::at(
            decl.field_span(field),
            format!(
                "`{field}` must be a list of strings, not a {}",
                other.type_name()
            ),
        )),
    }
}

fn string_map(decl: &Decl, field: &str) -> Result<Vec<(String, String)>, Error> {
    match decl.field(field) {
        None => Ok(Vec::new()),
        Some(Value::Map(entries)) => entries
            .iter()
            .map(|(key, value)| match value {
                Value::Str(text) => Ok((key.clone(), text.clone())),
                Value::Int(number) => Ok((key.clone(), number.to_string())),
                Value::Bool(flag) => Ok((key.clone(), flag.to_string())),
                other => Err(Error::at(
                    decl.field_span(field),
                    format!(
                        "`{field}.{key}` must be a string, not a {}",
                        other.type_name()
                    ),
                )),
            })
            .collect(),
        Some(other) => Err(Error::at(
            decl.field_span(field),
            format!("`{field}` must be a map, not a {}", other.type_name()),
        )),
    }
}

fn steps(
    decl: &Decl,
    field: &str,
    env: &[(String, String)],
    warnings: &mut Vec<Error>,
) -> Result<Vec<Step>, Error> {
    let actions: Vec<Value> = match decl.field(field) {
        None => return Ok(Vec::new()),
        Some(Value::List(items)) => items.clone(),
        Some(other) => vec![other.clone()],
    };
    actions
        .iter()
        .map(|value| match value {
            Value::Action(action) => {
                let step = step(action, env)?;
                if let Work::Process {
                    portable: false, ..
                } = step.work
                {
                    warnings.push(
                        Error::at(
                            action.span,
                            format!(
                                "`{}` runs a shell command: it will not work on Windows",
                                action.kind
                            ),
                        )
                        .help("prefer run(...) or a built-in action"),
                    );
                }
                Ok(step)
            }
            other => Err(Error::at(
                decl.field_span(field),
                format!(
                    "`{field}` expects actions such as make(\"…\"), not a {}",
                    other.type_name()
                ),
            )),
        })
        .collect()
}

fn text(action: &Action, index: usize) -> Result<String, Error> {
    match action.args.get(index) {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(Value::Int(value)) => Ok(value.to_string()),
        Some(other) => Err(Error::at(
            action.span,
            format!(
                "`{}` argument {} must be a string, not a {}",
                action.kind,
                index + 1,
                other.type_name()
            ),
        )),
        None => Err(Error::at(
            action.span,
            format!("`{}` needs argument {}", action.kind, index + 1),
        )),
    }
}

fn named<'a>(action: &'a Action, name: &str) -> Option<&'a Value> {
    action
        .named
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn timeout(action: &Action) -> u64 {
    match named(action, "timeout") {
        Some(Value::Duration(seconds)) => *seconds,
        Some(Value::Int(seconds)) => *seconds as u64,
        _ => 60,
    }
}

fn step(action: &Action, env: &[(String, String)]) -> Result<Step, Error> {
    let process = |argv: Vec<String>, portable: bool| Work::Process {
        argv,
        env: env.to_vec(),
        portable,
    };
    let all_text = |action: &Action| -> Result<Vec<String>, Error> {
        (0..action.args.len())
            .map(|index| text(action, index))
            .collect()
    };
    let work = match action.kind.as_str() {
        "run" => {
            let argv = all_text(action)?;
            if argv.is_empty() {
                return Err(Error::at(action.span, "`run` needs a program"));
            }
            process(argv, true)
        }
        "make" => {
            let mut argv = vec![
                "make".to_owned(),
                "--no-print-directory".to_owned(),
                text(action, 0)?,
            ];
            for (key, value) in &action.named {
                argv.push(format!("{key}={}", value_text(value)));
            }
            process(argv, true)
        }
        "sh" => process(vec!["sh".into(), "-c".into(), text(action, 0)?], false),
        "cargo.test" => {
            let mut argv = vec!["cargo".into(), "test".into(), "--locked".into()];
            if !action.args.is_empty() {
                argv.extend(["-p".into(), text(action, 0)?]);
            }
            process(argv, true)
        }
        "cargo.build" => {
            let mut argv = vec!["cargo".into(), "build".into(), "--locked".into()];
            if matches!(named(action, "release"), Some(Value::Bool(true))) {
                argv.push("--release".into());
            }
            if let Some(Value::Str(target)) = named(action, "target") {
                argv.extend(["--target".into(), target.clone()]);
            }
            process(argv, true)
        }
        "cargo.fmt" => {
            let mut argv = vec!["cargo".into(), "fmt".into()];
            if matches!(named(action, "check"), Some(Value::Bool(true))) {
                argv.push("--check".into());
            }
            process(argv, true)
        }
        "cargo.clippy" => {
            let mut argv = vec![
                "cargo".into(),
                "clippy".into(),
                "--all-targets".into(),
                "--locked".into(),
            ];
            if let Some(Value::Str(level)) = named(action, "deny") {
                argv.extend(["--".into(), "-D".into(), level.clone()]);
            }
            process(argv, true)
        }
        "compose.up" => process(
            ["docker", "compose", "up", "-d"]
                .into_iter()
                .map(str::to_owned)
                .chain(all_text(action)?)
                .collect(),
            true,
        ),
        "compose.down" => process(
            ["docker", "compose", "down"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            true,
        ),
        "wait.tcp" => Work::WaitTcp {
            address: text(action, 0)?,
            timeout: timeout(action),
        },
        "wait.http" => Work::WaitHttp {
            url: text(action, 0)?,
            timeout: timeout(action),
        },
        "wait.file" => Work::WaitFile {
            path: text(action, 0)?,
            timeout: timeout(action),
        },
        "copy" => Work::Copy {
            from: text(action, 0)?,
            to: text(action, 1)?,
        },
        "links.check" => Work::LinksCheck {
            pattern: if action.args.is_empty() {
                "**/*.md".into()
            } else {
                text(action, 0)?
            },
        },
        other => {
            return Err(
                Error::at(action.span, format!("`{other}` cannot be a step here yet"))
                    .help("steps are run, make, sh, cargo.*, compose.*, wait.*, copy, links.check"),
            );
        }
    };
    let label = work.label();
    Ok(Step {
        span: action.span,
        label,
        work,
    })
}

/// The program and arguments of a process action.
fn argv(action: &Action) -> Result<Vec<String>, Error> {
    match step(action, &[])?.work {
        Work::Process { argv, .. } => Ok(argv),
        _ => Err(Error::at(
            action.span,
            format!("`{}` is not a command here", action.kind),
        )),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        Value::Int(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute one step in `root`; the process inherits stdout/stderr.
/// `stdout_to_stderr` keeps a JSON answer on stdout clean while steps print.
pub fn execute(step: &Step, root: &Path, stdout_to_stderr: bool) -> anyhow::Result<i32> {
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
                .current_dir(root)
                .stdin(std::process::Stdio::null());
            if stdout_to_stderr {
                use std::os::fd::AsFd;
                command.stdout(std::process::Stdio::from(
                    std::io::stderr().as_fd().try_clone_to_owned()?,
                ));
            }
            let status = command.status()?;
            Ok(status.code().unwrap_or(-1))
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

/// `citrus.ci` of a repository, compiled; None when there is no such file.
pub fn load(root: &Path) -> Result<Option<(Project, Sources)>, String> {
    if !root.join("citrus.ci").exists() {
        return Ok(None);
    }
    load_at(root, None)
}

/// `citrus.ci` as committed at `revision` (or the working tree), compiled.
pub fn load_at(root: &Path, revision: Option<&str>) -> Result<Option<(Project, Sources)>, String> {
    match super::load_at(root, "citrus.ci", revision) {
        Ok((graph, sources)) => match compile(&graph, root) {
            Ok(mut project) => {
                project.files = sources
                    .files
                    .iter()
                    .map(|(path, _)| path.display().to_string())
                    .collect();
                Ok(Some((project, sources)))
            }
            Err(error) => Err(sources.render(&error)),
        },
        Err((error, sources)) => Err(sources.render(&error)),
    }
}

/// A `.ci` value as JSON: durations become seconds, commands their argv.
fn to_json(value: &Value, span: Span) -> Result<serde_json::Value, Error> {
    use serde_json::Value as Json;
    Ok(match value {
        Value::None => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Duration(seconds) => Json::from(*seconds),
        Value::Str(text) => Json::String(text.clone()),
        Value::List(items) => Json::Array(
            items
                .iter()
                .map(|item| to_json(item, span))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), to_json(value, span)?)))
                .collect::<Result<_, Error>>()?,
        ),
        Value::Action(action) => Json::from(argv(action)?),
        other => {
            return Err(Error::at(
                span,
                format!("a {} cannot be used as data", other.type_name()),
            ));
        }
    })
}

/// Fields of a block as a JSON object; `about` is stored as `description`.
fn fields_json(
    decl: &Decl,
    skip: &[&str],
) -> Result<serde_json::Map<String, serde_json::Value>, Error> {
    let mut object = serde_json::Map::new();
    for (name, value, span) in &decl.fields {
        if skip.contains(&name.as_str()) {
            continue;
        }
        let key = if name == "about" {
            "description"
        } else {
            name.as_str()
        };
        object.insert(key.to_owned(), to_json(value, *span)?);
    }
    Ok(object)
}

fn no_children(decl: &Decl, allowed: &str) -> Result<(), Error> {
    match decl.children.iter().find(|child| child.kind != allowed) {
        Some(child) => Err(Error::at(
            child.span,
            format!(
                "`{}` blocks cannot contain `{}` blocks",
                decl.kind, child.kind
            ),
        )),
        None => Ok(()),
    }
}

/// `checks = none` / `approval = none` read as the word, not as absence.
fn none_as_word(object: &mut serde_json::Map<String, serde_json::Value>, fields: &[&str]) {
    for field in fields {
        if object.get(*field).is_some_and(serde_json::Value::is_null) {
            object.insert((*field).to_owned(), "none".into());
        }
    }
}

/// Actions of a release step as JSON `Work` values: commands and built-in steps.
fn works_json(value: &Value, span: Span) -> Result<serde_json::Value, Error> {
    let items = match value {
        Value::List(items) => items.clone(),
        other => vec![other.clone()],
    };
    let works = items
        .iter()
        .map(|item| match item {
            Value::Action(action) => Ok(step(action, &[])?.work),
            other => Err(Error::at(
                span,
                format!(
                    "expected actions such as make(\"…\"), not a {}",
                    other.type_name()
                ),
            )),
        })
        .collect::<Result<Vec<_>, Error>>()?;
    serde_json::to_value(works).map_err(|error| Error::at(span, error.to_string()))
}

/// A release step or rollback: `run`/`recover` hold actions, other fields data.
fn release_step_json(
    fields: &[(String, Value, Span)],
) -> Result<serde_json::Map<String, serde_json::Value>, Error> {
    let mut object = serde_json::Map::new();
    for (name, value, span) in fields {
        let json = if name == "run" || name == "recover" {
            works_json(value, *span)?
        } else {
            to_json(value, *span)?
        };
        object.insert(name.clone(), json);
    }
    Ok(object)
}

fn release_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    no_children(decl, "step")?;
    let mut object = fields_json(decl, &["rollback"])?;
    none_as_word(&mut object, &["checks"]);
    if let Some(Value::Map(entries)) = decl.field("rollback") {
        let span = decl.field_span("rollback");
        let fields: Vec<(String, Value, Span)> = entries
            .iter()
            .map(|(key, value)| (key.clone(), value.clone(), span))
            .collect();
        object.insert("rollback".into(), release_step_json(&fields)?.into());
    }
    let steps = decl
        .children
        .iter()
        .map(|step| {
            no_children(step, "")?;
            let mut fields = release_step_json(&step.fields)?;
            fields.insert("name".into(), label(step)?.into());
            Ok(serde_json::Value::Object(fields))
        })
        .collect::<Result<Vec<_>, Error>>()?;
    object.insert("steps".into(), steps.into());
    Ok(object.into())
}

fn artifact_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    no_children(decl, "")?;
    let mut object = fields_json(decl, &["inputs"])?;
    match decl.field("inputs") {
        Some(Value::Action(action)) if action.kind == "inputs_of" => {
            let Some(Value::Action(command)) = action.args.first() else {
                return Err(Error::at(
                    action.span,
                    "`inputs_of` takes a command, e.g. inputs_of(run(\"…\"))",
                ));
            };
            object.insert("inputs_command".into(), argv(command)?.into());
        }
        Some(value) => {
            object.insert("inputs".into(), to_json(value, decl.field_span("inputs"))?);
        }
        None => {}
    }
    Ok(object.into())
}

fn environment_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    no_children(decl, "deploy")?;
    let mut object = fields_json(decl, &["on"])?;
    none_as_word(&mut object, &["checks", "approval"]);
    match decl.field("on") {
        Some(Value::Action(action)) => {
            object.insert("provider".into(), action.kind.clone().into());
            let mut connection = serde_json::Map::new();
            for (key, value) in &action.named {
                connection.insert(key.clone(), value_text(value).into());
            }
            object.insert("connection".into(), connection.into());
        }
        _ => {
            return Err(Error::at(
                decl.span,
                format!(
                    "environment \"{}\" needs `on`",
                    decl.name.clone().unwrap_or_default()
                ),
            )
            .help("on = kubernetes(context: \"…\", namespace: \"…\")"));
        }
    }
    let mut workloads = serde_json::Map::new();
    for deploy in &decl.children {
        no_children(deploy, "")?;
        workloads.insert(label(deploy)?, fields_json(deploy, &[])?.into());
    }
    object.insert("workloads".into(), workloads.into());
    Ok(object.into())
}

/// Read a block's JSON form into Citrus's own type; errors point at the block.
fn typed<T: serde::de::DeserializeOwned>(decl: &Decl, json: serde_json::Value) -> Result<T, Error> {
    serde_json::from_value(json).map_err(|error| {
        Error::at(
            decl.span,
            format!(
                "{} \"{}\": {}",
                decl.kind,
                decl.name.clone().unwrap_or_default(),
                error.to_string().replace("description", "about")
            ),
        )
    })
}
