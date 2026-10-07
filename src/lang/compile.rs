//! From evaluated declarations to what Citrus runs: project settings, checks
//! with their steps, tasks. Validation that needs the meaning of a block
//! (known kinds and fields, portable actions, globs that match) lives here.

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

#[derive(Debug, Clone, serde::Serialize)]
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
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    pub name: String,
    pub description: Option<String>,
    pub owns: Vec<String>,
    pub reads: Vec<String>,
    pub cache: bool,
    pub resources: Vec<String>,
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

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Project {
    pub base: Option<String>,
    pub logs: Option<String>,
    pub toolchain: Vec<String>,
    pub checks: Vec<Check>,
    pub tasks: Vec<Task>,
    /// Declarations Citrus reads but does not execute from `.ci` yet.
    pub pending: Vec<(String, String, Span)>,
    pub warnings: Vec<Error>,
}

const KINDS: &[&str] = &[
    "project",
    "check",
    "task",
    "artifact",
    "environment",
    "pool",
];

pub fn compile(graph: &Graph, root: &Path) -> Result<Project, Error> {
    let mut project = Project::default();
    for decl in &graph.decls {
        match decl.kind.as_str() {
            "project" => {
                known_fields(decl, &["base", "logs", "toolchain"])?;
                project.base = optional_string(decl, "base")?;
                project.logs = optional_string(decl, "logs")?;
                project.toolchain = strings(decl, "toolchain")?;
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
                        "env",
                        "on",
                    ],
                )?;
                let name = label(decl)?;
                if project.checks.iter().any(|check| check.name == name) {
                    return Err(Error::at(
                        decl.span,
                        format!("check \"{name}\" is declared twice"),
                    ));
                }
                let owns = strings(decl, "owns")?;
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
                    reads: strings(decl, "reads")?,
                    cache: matches!(decl.field("cache"), Some(Value::Bool(true))),
                    resources: strings(decl, "resources")?,
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
        .and_then(|repo| repo.files())
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
        other => {
            return Err(
                Error::at(action.span, format!("`{other}` cannot be a step here yet"))
                    .help("steps are run, make, sh, cargo.*, compose.*, wait.*, copy"),
            );
        }
    };
    let label = match &work {
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
    };
    Ok(Step {
        span: action.span,
        label,
        work,
    })
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

/// `citrus.ci` of a repository, compiled; None when there is no such file.
pub fn load(root: &Path) -> Result<Option<(Project, Sources)>, String> {
    if !root.join("citrus.ci").exists() {
        return Ok(None);
    }
    match super::load(root, "citrus.ci") {
        Ok((graph, sources)) => match compile(&graph, root) {
            Ok(project) => Ok(Some((project, sources))),
            Err(error) => Err(sources.render(&error)),
        },
        Err((error, sources)) => Err(sources.render(&error)),
    }
}
