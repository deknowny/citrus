//! The `.ci` language: lexer, parser and evaluator into declarations
//! (docs/design/language.md). Evaluation has no side effects; Citrus executes
//! the resulting graph, and every node keeps the span it was declared at.

pub mod ast;
pub mod cargo;
pub mod compile;
pub mod eval;
pub mod layout;
pub mod lexer;
pub mod migrate;
pub mod parser;
pub mod web;

use std::fmt;
use std::path::{Path, PathBuf};

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

    /// Help unless there is one already (a closer suggestion).
    pub fn or_help(mut self, help: impl Into<String>) -> Error {
        self.help.get_or_insert_with(|| help.into());
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

/// Parse and evaluate `entry` (and the files it `use`s) under `root`, read
/// from the working tree or as committed at `revision`.
pub fn load_at(
    root: &Path,
    entry: &str,
    revision: Option<&str>,
) -> Result<(eval::Graph, Sources), (Error, Sources)> {
    let mut sources = Sources::default();
    match eval::evaluate_project(root, entry, revision, &mut sources) {
        Ok(graph) => Ok((graph, sources)),
        Err(error) => Err((error, sources)),
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
