//! The Citrus language (docs/design/language.md): Rust-like items with typed
//! bodies. Files are parsed and checked (names, types, phases) before
//! anything runs; they compile into the project model (src/model.rs), and a
//! check's body runs through `Work::Script` in the check's worker.

pub mod ast;
pub mod cargo;
pub mod check;
pub mod compile;
pub mod interp;
pub mod lexer;
pub mod parser;
pub mod tools;
pub mod web;

use std::fmt;
use std::path::PathBuf;

/// A byte range in one of the loaded files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub struct Span {
    pub file: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Error {
    pub message: String,
    pub span: Span,
    pub help: Option<String>,
}

impl Error {
    pub fn at(span: Span, message: impl Into<String>) -> Error {
        Error {
            message: message.into(),
            span,
            help: None,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Error {
        self.help = Some(help.into());
        self
    }
}

/// Loaded source files, for spans → `file:line:column` and excerpts.
#[derive(Debug, Default, Clone)]
pub struct Sources {
    pub files: Vec<(PathBuf, String)>,
}

impl Sources {
    pub fn add(&mut self, path: PathBuf, text: String) -> usize {
        self.files.push((path, text));
        self.files.len() - 1
    }

    /// (path, 1-based line, 1-based column)
    pub fn locate(&self, span: Span) -> (String, usize, usize) {
        let Some((path, text)) = self.files.get(span.file) else {
            return ("?".into(), 0, 0);
        };
        let before = &text[..span.start.min(text.len())];
        let line = before.matches('\n').count() + 1;
        let column = before
            .rsplit('\n')
            .next()
            .map_or(0, |tail| tail.chars().count())
            + 1;
        (path.display().to_string(), line, column)
    }

    /// `error: …` with the source line and a caret under the span.
    pub fn render(&self, error: &Error) -> String {
        let (path, line, column) = self.locate(error.span);
        let mut out = format!("error: {}\n  --> {path}:{line}:{column}\n", error.message);
        if let Some((_, text)) = self.files.get(error.span.file)
            && let Some(source_line) = text.lines().nth(line.saturating_sub(1))
        {
            let width = line.to_string().len();
            let length = text[error.span.start.min(text.len())..error.span.end.min(text.len())]
                .chars()
                .count()
                .max(1);
            let length = length.min(
                source_line
                    .chars()
                    .count()
                    .saturating_sub(column - 1)
                    .max(1),
            );
            out.push_str(&format!(
                "{:width$} |\n{line} | {source_line}\n{:width$} | {}{}\n",
                "",
                "",
                " ".repeat(column - 1),
                "^".repeat(length)
            ));
        }
        if let Some(help) = &error.help {
            out.push_str(&format!("help: {help}\n"));
        }
        out
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

/// Closest known name, for "did you mean" hints.
pub fn suggest<'a>(name: &str, known: impl Iterator<Item = &'a str>) -> Option<String> {
    let distance = |a: &str, b: &str| {
        let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let mut row: Vec<usize> = (0..=b.len()).collect();
        for i in 1..=a.len() {
            let mut previous = row[0];
            row[0] = i;
            for j in 1..=b.len() {
                let current = row[j];
                row[j] = (row[j] + 1)
                    .min(row[j - 1] + 1)
                    .min(previous + usize::from(a[i - 1] != b[j - 1]));
                previous = current;
            }
        }
        row[b.len()]
    };
    known
        .map(|candidate| (distance(name, candidate), candidate))
        .filter(|(score, _)| *score <= 2.max(name.len() / 3))
        .min()
        .map(|(_, candidate)| candidate.to_owned())
}

use std::collections::BTreeMap;
use std::path::Path;

use crate::model::Project;
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
        if !is_v2(&text) {
            let error = Error::at(Span { file, start: 0, end: text.find('\n').unwrap_or(text.len()) }, "a .ci file starts with `#![citrus(2)]`")
                .help(if text.trim_start().starts_with("citrus 1") {
                    "this is the old configuration language; rewrite it in the current one (docs/design/language.md)"
                } else {
                    "add `#![citrus(2)]` as its first line"
                });
            return Err((error, sources));
        }
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
