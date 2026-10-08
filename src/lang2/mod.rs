//! Language v2 (prototype, docs/design/language-v2.md): Rust-like items with
//! typed bodies. A file whose first line is `#![citrus(2)]` is read by this
//! module; it compiles into the same project model as the v1 language, and a
//! check's body runs through `Work::Script` in the check's worker.

pub mod ast;
pub mod check;
pub mod compile;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod tools;

use std::collections::BTreeMap;
use std::path::Path;

use crate::lang::compile::Project;
use crate::lang::{Error, Sources, Span};
use ast::{ItemKind, Program};
use interp::{Interp, Value};

/// `#![citrus(2)]` is the first line that is not blank or a comment.
pub fn is_v2(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("//"))
        .is_some_and(|line| line.replace(' ', "") == "#![citrus(2)]")
}

fn read(root: &Path, relative: &str, revision: Option<&str>) -> Option<String> {
    match revision {
        None => std::fs::read_to_string(root.join(relative)).ok(),
        Some(revision) => std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["show", &format!("{revision}:{relative}")])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned()),
    }
}

/// The configuration files: `citrus.ci`, or `.citrus/*.ci` (project.ci first).
fn files(root: &Path, entry: &str, revision: Option<&str>) -> Vec<String> {
    if entry != ".citrus" {
        return vec![entry.to_owned()];
    }
    let listed = match revision {
        None => std::fs::read_dir(root.join(".citrus"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| format!(".citrus/{}", entry.file_name().to_string_lossy()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        Some(revision) => std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["ls-tree", "--name-only", &format!("{revision}:.citrus")])
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|name| format!(".citrus/{name}"))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let mut files: Vec<String> = listed
        .into_iter()
        .filter(|path| path.ends_with(".ci"))
        .collect();
    files.sort_by_key(|path| (path != ".citrus/project.ci", path.clone()));
    files
}

/// Whether the project at `entry` is written in language v2.
pub fn detect(root: &Path, entry: &str, revision: Option<&str>) -> bool {
    files(root, entry, revision)
        .first()
        .and_then(|first| read(root, first, revision))
        .is_some_and(|text| is_v2(&text))
}

/// Parse and check every file.
pub fn parse(
    root: &Path,
    entry: &str,
    revision: Option<&str>,
) -> Result<(Program, Sources), (Error, Sources)> {
    let mut sources = Sources::default();
    let mut program = Program::default();
    for relative in files(root, entry, revision) {
        let Some(text) = read(root, &relative, revision) else {
            continue;
        };
        let file = sources.add(relative.clone().into(), text.clone());
        if let Err(error) = parser::parse_file(file, &text, &mut program) {
            return Err((error, sources));
        }
    }
    if let Err(error) = check::check(&program) {
        return Err((error, sources));
    }
    Ok((program, sources))
}

/// The project model of a v2 configuration.
pub fn load(
    root: &Path,
    entry: &str,
    revision: Option<&str>,
) -> Result<(Project, Sources), String> {
    let (program, sources) =
        parse(root, entry, revision).map_err(|(error, sources)| sources.render(&error))?;
    compile::compile(&program, &sources, root, revision)
        .map(|project| (project, sources.clone()))
        .map_err(|error| sources.render(&error))
}

/// Run one body in the working tree at `root`: `check:name`, `task:name`,
/// `step:release:step`, `rollback:release` or `fn:name`. Exit code 0 or 1;
/// a failure is printed as `error: …` with its place.
pub fn run_item(
    root: &Path,
    item: &str,
    args: &[(String, String)],
    env: &[(String, String)],
) -> i32 {
    let entry = if root.join("citrus.ci").exists() {
        "citrus.ci"
    } else {
        ".citrus"
    };
    let (program, sources) = match parse(root, entry, None) {
        Ok(parsed) => parsed,
        Err((error, sources)) => {
            eprint!("{}", sources.render(&error));
            return 1;
        }
    };
    let mut interp = Interp::new(&program, root);
    interp.env = env.to_vec();
    if let Err(failure) = interp.load_consts(&program) {
        eprint!("{}", interp::render(&failure, &sources));
        return 1;
    }
    let release = || {
        let get = |key: &str| {
            args.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let previous = get("previous");
        Value::Struct(
            "Release".into(),
            std::rc::Rc::new(BTreeMap::from([
                ("version".to_owned(), Value::Version(get("version").into())),
                (
                    "previous".to_owned(),
                    if previous.is_empty() {
                        Value::None
                    } else {
                        Value::Some(Box::new(Value::Version(previous.into())))
                    },
                ),
                ("commit".to_owned(), Value::str(get("commit"))),
                ("unit".to_owned(), Value::str(get("unit"))),
            ])),
        )
    };
    let (kind, name) = item.split_once(':').unwrap_or(("", item));
    let outcome = match kind {
        "fn" => match interp.call_fn(name, vec![release()], Span::default()) {
            Ok(Value::Err(failure)) => Err((*failure).clone()),
            Ok(_) => Ok(()),
            Err(interp::Flow::Panic(failure)) => Err(failure),
            Err(interp::Flow::Return(_)) => Ok(()),
        },
        _ => {
            let found = find_body(&program, kind, name);
            match found {
                None => {
                    eprintln!("error: no {kind} {name} in the configuration");
                    return 1;
                }
                Some((body, param)) => {
                    let bindings = param
                        .map(|param| vec![(param, release())])
                        .unwrap_or_default();
                    interp.run_body(body, bindings)
                }
            }
        }
    };
    match outcome {
        Ok(()) => 0,
        Err(failure) => {
            eprint!("{}", interp::render(&failure, &sources));
            1
        }
    }
}

fn find_body<'a>(
    program: &'a Program,
    kind: &str,
    name: &str,
) -> Option<(&'a ast::Block, Option<String>)> {
    for item in &program.items {
        match (&item.kind, kind) {
            (ItemKind::Check { body }, "check") | (ItemKind::Task { body }, "task")
                if item.name == name =>
            {
                return Some((body, None));
            }
            (ItemKind::Group { items }, "check") => {
                for inner in items {
                    if let ItemKind::Check { body } = &inner.kind
                        && format!("{}.{}", item.name, inner.name) == name
                    {
                        return Some((body, None));
                    }
                }
            }
            (ItemKind::Service { start, ready }, "service-start" | "service-ready")
                if item.name == name =>
            {
                let body = if kind == "service-start" {
                    start
                } else {
                    ready
                };
                if let Some(body) = body {
                    return Some((body, None));
                }
            }
            (ItemKind::Release { steps, rollback }, "step" | "rollback") => {
                let (release, step) = name.split_once(':').unwrap_or((name, "rollback"));
                if item.name != release {
                    continue;
                }
                let decl = if kind == "rollback" {
                    rollback.as_ref()
                } else {
                    steps.iter().find(|decl| decl.name == step)
                };
                if let Some(decl) = decl {
                    return Some((
                        &decl.body,
                        decl.param.as_ref().map(|param| param.name.clone()),
                    ));
                }
            }
            _ => {}
        }
    }
    None
}
