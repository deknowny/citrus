//! What Citrus knows about the programs a `run!`/`cmd!` line starts: the
//! files a command reads (so a check needs no `#[paths]`), mistakes it can
//! name before anything runs. A program Citrus does not know is a plain
//! process: its check declares `#[paths]`.

use std::path::Path;

use super::ast::{CmdWord, Expr};
use crate::lang::cargo::Files;
use crate::lang::{Error, Span, suggest};

/// What a command was understood as.
#[derive(Debug, Clone, PartialEq)]
pub struct Understood {
    /// `cargo test -p backend`
    pub summary: String,
    /// Repository globs the command reads.
    pub inputs: Vec<String>,
}

/// Repository files for the tool readers: the working tree or a commit.
pub struct RepoFiles<'a> {
    root: &'a Path,
    revision: Option<&'a str>,
    listed: Vec<String>,
}

impl<'a> RepoFiles<'a> {
    pub fn new(root: &'a Path, revision: Option<&'a str>) -> RepoFiles<'a> {
        let args: Vec<&str> = match revision {
            None => vec!["ls-files", "-co", "--exclude-standard"],
            Some(revision) => vec!["ls-tree", "-r", "--name-only", revision],
        };
        let listed = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .map(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        RepoFiles {
            root,
            revision,
            listed,
        }
    }
}

impl Files for RepoFiles<'_> {
    fn read(&self, path: &str) -> Option<String> {
        super::read(self.root, path, self.revision)
    }

    fn list(&self) -> &[String] {
        &self.listed
    }
}

const CARGO_SUBCOMMANDS: &[&str] = &[
    "test", "build", "check", "clippy", "run", "bench", "doc", "fmt", "nextest",
];

/// Workspace packages (the names `-p` accepts).
fn packages(files: &dyn Files) -> Vec<String> {
    files
        .list()
        .iter()
        .filter(|path| path.ends_with("Cargo.toml"))
        .filter_map(|path| files.read(path))
        .filter_map(|text| text.parse::<toml::Table>().ok())
        .filter_map(|manifest| {
            manifest
                .get("package")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

/// Understand a command line, or None when its program is not one Citrus
/// knows or its words cannot be read when the file loads.
pub fn understand(
    words: &[CmdWord],
    span: Span,
    files: &dyn Files,
) -> Result<Option<Understood>, Error> {
    let literal: Vec<Option<String>> = words.iter().map(CmdWord::literal).collect();
    match literal.first() {
        Some(Some(program)) if program == "cargo" => cargo(&literal[1..], span, files),
        _ => Ok(None),
    }
}

fn cargo(
    args: &[Option<String>],
    span: Span,
    files: &dyn Files,
) -> Result<Option<Understood>, Error> {
    // Everything after `--` goes to the test binary or the tool, not to Cargo.
    let cargo_args: Vec<&Option<String>> = args
        .iter()
        .take_while(|arg| arg.as_deref() != Some("--"))
        .collect();
    let mut rest = cargo_args.iter().map(|arg| arg.as_deref());
    let mut subcommand = None;
    for arg in rest.by_ref() {
        match arg {
            None => return Ok(None),
            Some(arg) if arg.starts_with('+') => continue,
            Some(arg) => {
                subcommand = Some(arg.to_owned());
                break;
            }
        }
    }
    let Some(subcommand) = subcommand else {
        return Ok(None);
    };
    if subcommand.starts_with('-') {
        return Ok(None);
    }
    if !CARGO_SUBCOMMANDS.contains(&subcommand.as_str()) {
        let mut error = Error::at(
            span,
            format!("`cargo {subcommand}` is not a Cargo command Citrus knows"),
        );
        if let Some(close) = suggest(&subcommand, CARGO_SUBCOMMANDS.iter().copied()) {
            error = error.help(format!("did you mean `cargo {close}`?"));
        } else {
            error = error.help("declare its inputs with `#[paths(…)]` and run it through `cmd!` with a variable program");
        }
        return Err(error);
    }
    let known = packages(files);
    let mut chosen: Vec<String> = Vec::new();
    let mut all = false;
    let mut words: Vec<Option<&str>> = rest.collect();
    words.reverse();
    while let Some(word) = words.pop() {
        let Some(word) = word else {
            // A value Citrus cannot see at load time may name packages.
            return Ok(None);
        };
        let package = if word == "-p" || word == "--package" {
            match words.pop() {
                Some(Some(name)) => Some(name.to_owned()),
                _ => return Ok(None),
            }
        } else if let Some(name) = word.strip_prefix("--package=") {
            Some(name.to_owned())
        } else if let Some(name) = word.strip_prefix("-p").filter(|name| !name.is_empty()) {
            Some(name.to_owned())
        } else {
            if word == "--manifest-path" || word.starts_with("--manifest-path=") {
                return Ok(None);
            }
            if word == "--workspace" || word == "--all" {
                all = true;
            }
            None
        };
        if let Some(package) = package {
            if !known.contains(&package) && !package.contains('*') {
                let mut error = Error::at(
                    span,
                    format!("no Cargo package `{package}` in this repository"),
                );
                if let Some(close) = suggest(&package, known.iter().map(String::as_str)) {
                    error = error.help(format!("did you mean `{close}`?"));
                }
                return Err(error);
            }
            chosen.push(package);
        }
    }
    // Without `-p` Cargo works on the workspace's members, or on the root
    // package when the root is not a workspace; `cargo fmt` likewise.
    if chosen.is_empty() || all {
        let root: Option<toml::Table> = files.read("Cargo.toml").and_then(|text| text.parse().ok());
        let is_workspace = root
            .as_ref()
            .is_some_and(|root| root.contains_key("workspace"));
        let root_package = root.as_ref().and_then(|root| {
            root.get("package")?
                .get("name")?
                .as_str()
                .map(str::to_owned)
        });
        chosen = match (is_workspace, root_package) {
            (false, Some(name)) => vec![name],
            _ => known,
        };
        all = is_workspace;
    }
    if chosen.is_empty() {
        return Ok(None);
    }
    let inputs =
        crate::lang::cargo::crates(files, &chosen).map_err(|message| Error::at(span, message))?;
    let summary = if all {
        format!("cargo {subcommand} (workspace)")
    } else {
        format!("cargo {subcommand} -p {}", chosen.join(" -p "))
    };
    Ok(Some(Understood { summary, inputs }))
}

/// Every `run!`/`cmd!` and every call in an expression tree, in order.
pub fn commands<'e>(expr: &'e Expr, out: &mut Vec<&'e Expr>) {
    use super::ast::StrPart;
    let visit = |expr: &'e Expr, out: &mut Vec<&'e Expr>| commands(expr, out);
    match expr {
        Expr::Command { .. } => out.push(expr),
        Expr::Str(parts, _) => {
            for part in parts {
                if let StrPart::Expr(inner) = part {
                    visit(inner, out);
                }
            }
        }
        Expr::List(items, _) => items.iter().for_each(|item| visit(item, out)),
        Expr::StructLit { fields, .. } => fields.iter().for_each(|(_, value)| visit(value, out)),
        Expr::Field(inner, _, _) | Expr::Unary(_, inner, _) | Expr::Try(inner, _) => {
            visit(inner, out)
        }
        Expr::Index(a, b, _) | Expr::Binary(_, a, b, _) => {
            visit(a, out);
            visit(b, out);
        }
        Expr::Call { callee, args, .. } => {
            out.push(expr);
            visit(callee, out);
            args.iter().for_each(|arg| visit(arg, out));
        }
        Expr::Method { receiver, args, .. } => {
            visit(receiver, out);
            args.iter().for_each(|arg| visit(arg, out));
        }
        Expr::If {
            cond,
            then,
            otherwise,
            ..
        } => {
            visit(cond, out);
            block_commands(then, out);
            if let Some(otherwise) = otherwise {
                visit(otherwise, out);
            }
        }
        Expr::Match { value, arms, .. } => {
            visit(value, out);
            arms.iter().for_each(|(_, body)| visit(body, out));
        }
        Expr::Block(block) => block_commands(block, out),
        _ => {}
    }
}

pub fn block_commands<'e>(block: &'e super::ast::Block, out: &mut Vec<&'e Expr>) {
    use super::ast::Stmt;
    for stmt in &block.stmts {
        match stmt {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } => commands(value, out),
            Stmt::Assert { cond, message, .. } => {
                commands(cond, out);
                if let Some(message) = message {
                    commands(message, out);
                }
            }
            Stmt::Return {
                value: Some(value), ..
            } => commands(value, out),
            Stmt::Return { value: None, .. } => {}
            Stmt::For { iter, body, .. } => {
                commands(iter, out);
                block_commands(body, out);
            }
            Stmt::Expr(expr) => commands(expr, out),
        }
    }
    if let Some(tail) = &block.tail {
        commands(tail, out);
    }
}
