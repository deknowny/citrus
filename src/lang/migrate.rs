//! One-off: a v1 `.ci` file rewritten in language v2 (`citrus migrate-v2`).
//! Removed together with v1 once the projects that used it have moved.

use std::collections::{BTreeMap, BTreeSet};

use super::Error;
use super::ast::{Expr, Item, StrPart};
use super::parser::parse_file;

/// Names of the whole project (all its files), so references resolve.
#[derive(Default)]
pub struct Names {
    lets: BTreeSet<String>,
    fns: BTreeSet<String>,
    /// item name → kind (group, check, service, profile, …)
    items: BTreeMap<String, String>,
    /// `runner name = …` declarations, joined into `#![runner]`.
    runners: BTreeMap<String, String>,
}

pub fn collect(sources: &[&str]) -> Result<Names, Error> {
    let mut names = Names::default();
    for (index, source) in sources.iter().enumerate() {
        let file = parse_file(index, source)?;
        for item in &file.items {
            match item {
                Item::Let { name, .. } => {
                    names.lets.insert(name.clone());
                }
                Item::Fn { name, .. } => {
                    names.fns.insert(name.clone());
                }
                Item::Block {
                    kind, label, items, ..
                } => {
                    if let Some(label) = label.as_ref().and_then(label_text) {
                        names.items.insert(label.clone(), kind.clone());
                        if kind == "group" {
                            for inner in items {
                                if let Item::Block {
                                    kind,
                                    label: Some(inner_label),
                                    ..
                                } = inner
                                    && let Some(inner_label) = label_text(inner_label)
                                {
                                    names
                                        .items
                                        .insert(format!("{label}.{inner_label}"), kind.clone());
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(names)
}

fn label_text(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name, _) => Some(name.clone()),
        Expr::Str(parts, _) => parts
            .iter()
            .map(|part| match part {
                StrPart::Lit(text) => Some(text.clone()),
                StrPart::Expr(_) => None,
            })
            .collect(),
        _ => None,
    }
}

fn snake(name: &str) -> String {
    name.replace(['-', '.'], "_")
}

fn upper(name: &str) -> String {
    snake(name).to_uppercase()
}

fn duration(seconds: u64) -> String {
    if seconds > 0 && seconds % 3600 == 0 {
        format!("{}h", seconds / 3600)
    } else if seconds > 0 && seconds % 60 == 0 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// Where an expression is written: runtime names mean different things.
#[derive(Clone, Copy, PartialEq)]
enum Place {
    Config,
    Release,
}

struct Writer<'a> {
    names: &'a Names,
    out: String,
    inner: Vec<String>,
    source: &'a str,
}

/// `#` comment lines right above `offset` (blank lines end them).
fn comments_above(source: &str, offset: usize) -> Vec<String> {
    let before = &source[..offset.min(source.len())];
    let before = &before[..before.rfind('\n').map_or(0, |at| at + 1)];
    let mut lines: Vec<String> = Vec::new();
    for line in before.lines().rev() {
        let trimmed = line.trim();
        match trimmed.strip_prefix('#') {
            Some(text) => lines.push(text.strip_prefix(' ').unwrap_or(text).to_owned()),
            None => break,
        }
    }
    lines.reverse();
    lines
}

fn quote(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('{', "{{")
        .replace('}', "}}")
        .replace('\n', "\\n");
    format!("\"{escaped}\"")
}

impl Writer<'_> {
    fn name(&self, name: &str, place: Place) -> String {
        if place == Place::Release && ["version", "previous", "commit", "unit"].contains(&name) {
            return format!("r.{name}");
        }
        if self.names.lets.contains(name) {
            return upper(name);
        }
        if let Some((group, check)) = name.split_once('.') {
            return format!("{}::{}", snake(group), snake(check));
        }
        snake(name)
    }

    fn expr(&self, expr: &Expr, place: Place) -> Result<String, Error> {
        Ok(match expr {
            Expr::None(_) => "none".into(),
            Expr::Bool(flag, _) => flag.to_string(),
            Expr::Int(number, _) => number.to_string(),
            Expr::Duration(seconds, _) => duration(*seconds),
            Expr::Str(parts, _) => {
                let mut out = String::from("\"");
                for part in parts {
                    match part {
                        StrPart::Lit(text) => out.push_str(&quote(text)[1..quote(text).len() - 1]),
                        StrPart::Expr(inner) => {
                            out.push('{');
                            out.push_str(&self.expr(inner, place)?);
                            out.push('}');
                        }
                    }
                }
                out.push('"');
                out
            }
            Expr::Name(name, _) => self.name(name, place),
            Expr::List(items, _) => {
                let items: Vec<String> = items
                    .iter()
                    .map(|item| self.expr(item, place))
                    .collect::<Result<_, _>>()?;
                let flat = format!("[{}]", items.join(", "));
                // Long path lists read better one per line, as people write them.
                if flat.len() > 90 && items.len() > 2 {
                    format!(
                        "[\n{}\n]",
                        items
                            .iter()
                            .map(|item| format!("    {item},"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )
                } else {
                    flat
                }
            }
            Expr::Field(base, field, _) => match &**base {
                Expr::Name(name, _)
                    if self.names.items.contains_key(&format!("{name}.{field}")) =>
                {
                    format!("{}::{}", snake(name), snake(field))
                }
                other => format!("{}.{field}", self.expr(other, place)?),
            },
            Expr::Unary(op, inner, _) => {
                let op = if *op == "not" { "!" } else { op };
                let inner_text = self.expr(inner, place)?;
                if matches!(**inner, Expr::Binary(..)) {
                    format!("{op}({inner_text})")
                } else {
                    format!("{op}{inner_text}")
                }
            }
            Expr::Binary(op, left, right, _) => {
                let op = match *op {
                    "and" => "&&",
                    "or" => "||",
                    other => other,
                };
                let side = |side: &Expr| -> Result<String, Error> {
                    let text = self.expr(side, place)?;
                    Ok(match side {
                        Expr::Binary(inner, ..)
                            if (*inner == "and" || *inner == "or")
                                && *inner != (if op == "&&" { "and" } else { "or" }) =>
                        {
                            format!("({text})")
                        }
                        _ => text,
                    })
                };
                format!("{} {op} {}", side(left)?, side(right)?)
            }
            Expr::Call { callee, args, span } => {
                let callee_name = match &**callee {
                    Expr::Name(name, _) => name.clone(),
                    Expr::Field(base, field, _) => match &**base {
                        Expr::Name(name, _) => format!("{name}.{field}"),
                        _ => return Err(Error::at(*span, "unsupported call")),
                    },
                    _ => return Err(Error::at(*span, "unsupported call")),
                };
                let args_text = |w: &Self| -> Result<Vec<String>, Error> {
                    args.iter().map(|(_, arg)| w.expr(arg, place)).collect()
                };
                match callee_name.as_str() {
                    "crate" => format!("std::paths::cargo({})", args_text(self)?.join(", ")),
                    "next" => format!("std::paths::next({})", args_text(self)?.join(", ")),
                    "package" => format!("std::paths::package({})", args_text(self)?.join(", ")),
                    "touched" | "only" | "without" | "signal" | "selected" | "profile" => {
                        format!("{callee_name}({})", args_text(self)?.join(", "))
                    }
                    _ if self.names.fns.contains(&callee_name) => {
                        format!("{}({})", snake(&callee_name), args_text(self)?.join(", "))
                    }
                    _ => format!("cmd!({})", self.command_line(expr, place)?),
                }
            }
            Expr::Map(entries, _) => {
                let entries: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| Ok(format!("{key}: {}", self.expr(value, place)?)))
                    .collect::<Result<_, Error>>()?;
                format!("{{ {} }}", entries.join(", "))
            }
            other => {
                return Err(Error::at(
                    other.span(),
                    "this v1 expression has no v2 form here; rewrite it by hand",
                ));
            }
        })
    }

    /// A word of a command line: literal text, or `{expr}`.
    fn word(&self, expr: &Expr, place: Place) -> Result<String, Error> {
        Ok(match expr {
            Expr::Str(parts, _) => {
                let mut out = String::new();
                for part in parts {
                    match part {
                        StrPart::Lit(text) => {
                            out.push_str(&text.replace('{', "{{").replace('}', "}}"))
                        }
                        StrPart::Expr(inner) => {
                            out.push_str(&format!("{{{}}}", self.expr(inner, place)?))
                        }
                    }
                }
                if out.contains(char::is_whitespace) || out.is_empty() {
                    if out.contains('\'') {
                        return Err(Error::at(
                            expr.span(),
                            "an argument with spaces and `'`: write it as a const and `{NAME}`",
                        ));
                    }
                    format!("'{out}'")
                } else {
                    out
                }
            }
            Expr::Int(number, _) => number.to_string(),
            Expr::Bool(flag, _) => flag.to_string(),
            other => format!("{{{}}}", self.expr(other, place)?),
        })
    }

    /// `"make test-api K=v"` for a v1 action, as the inside of `run!(…)`/`cmd!(…)`.
    fn command_line(&self, action: &Expr, place: Place) -> Result<String, Error> {
        let Expr::Call { callee, args, span } = action else {
            return Err(Error::at(action.span(), "expected an action"));
        };
        let kind = match &**callee {
            Expr::Name(name, _) => name.clone(),
            Expr::Field(base, field, _) => match &**base {
                Expr::Name(name, _) => format!("{name}.{field}"),
                _ => String::new(),
            },
            _ => String::new(),
        };
        let positional: Vec<&Expr> = args
            .iter()
            .filter(|(key, _)| key.is_none())
            .map(|(_, arg)| arg)
            .collect();
        let named = |key: &str| {
            args.iter()
                .find(|(name, _)| name.as_deref() == Some(key))
                .map(|(_, arg)| arg)
        };
        let words = |list: &[&Expr]| -> Result<Vec<String>, Error> {
            list.iter().map(|arg| self.word(arg, place)).collect()
        };
        let mut line: Vec<String> = match kind.as_str() {
            "run" => words(&positional)?,
            "make" => {
                let mut line = vec!["make".to_owned()];
                line.extend(words(&positional)?);
                for (key, value) in args
                    .iter()
                    .filter_map(|(key, value)| key.as_ref().map(|key| (key, value)))
                {
                    line.push(format!(
                        "{key}={}",
                        self.word(value, place)?.trim_matches('\'')
                    ));
                }
                line
            }
            "sh" => vec!["sh".into(), "-c".into(), self.word(positional[0], place)?],
            "cargo.test" => {
                let mut line = vec!["cargo".to_owned(), "test".into(), "--locked".into()];
                if let Some(package) = positional.first() {
                    line.extend(["-p".into(), self.word(package, place)?]);
                }
                line
            }
            "cargo.build" => {
                let mut line = vec!["cargo".to_owned(), "build".into(), "--locked".into()];
                if matches!(named("release"), Some(Expr::Bool(true, _))) {
                    line.push("--release".into());
                }
                if let Some(target) = named("target") {
                    line.extend(["--target".into(), self.word(target, place)?]);
                }
                line
            }
            "cargo.fmt" => {
                let mut line = vec!["cargo".to_owned(), "fmt".into()];
                if matches!(named("check"), Some(Expr::Bool(true, _))) {
                    line.push("--check".into());
                }
                line
            }
            "cargo.clippy" => {
                let mut line = vec![
                    "cargo".to_owned(),
                    "clippy".into(),
                    "--all-targets".into(),
                    "--locked".into(),
                ];
                if let Some(level) = named("deny") {
                    line.extend(["--".into(), "-D".into(), self.word(level, place)?]);
                }
                line
            }
            "cargo.run" => {
                let mut line = vec![
                    "cargo".to_owned(),
                    "run".into(),
                    "--quiet".into(),
                    "--locked".into(),
                    "--".into(),
                ];
                line.extend(words(&positional)?);
                line
            }
            "pnpm.test" | "pnpm.build" => {
                let dir = match named("dir") {
                    Some(dir) => self.word(dir, place)?,
                    None => ".".into(),
                };
                vec![
                    "pnpm".into(),
                    "--dir".into(),
                    dir,
                    "--filter".into(),
                    self.word(positional[0], place)?,
                    "run".into(),
                    kind.trim_start_matches("pnpm.").into(),
                ]
            }
            "compose.up" => {
                let mut line = vec![
                    "docker".to_owned(),
                    "compose".into(),
                    "up".into(),
                    "-d".into(),
                ];
                line.extend(words(&positional)?);
                line
            }
            "compose.down" => vec!["docker".into(), "compose".into(), "down".into()],
            other => {
                return Err(Error::at(
                    *span,
                    format!("`{other}` is not a command; rewrite it by hand"),
                ));
            }
        };
        if line.is_empty() {
            return Err(Error::at(*span, "an empty command"));
        }
        line.retain(|word| !word.is_empty());
        Ok(quote(&line.join(" ")).replace("{{", "{").replace("}}", "}"))
    }

    /// Statements running a v1 step value (an action or a list of them).
    fn steps(&self, value: &Expr, place: Place, indent: &str) -> Result<Vec<String>, Error> {
        let actions: Vec<&Expr> = match value {
            Expr::List(items, _) => items.iter().collect(),
            other => vec![other],
        };
        let mut out = Vec::new();
        for action in actions {
            let Expr::Call { callee, args, span } = action else {
                return Err(Error::at(action.span(), "a step is an action"));
            };
            let kind = match &**callee {
                Expr::Name(name, _) => name.clone(),
                Expr::Field(base, field, _) => match &**base {
                    Expr::Name(name, _) => format!("{name}.{field}"),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            let arg = |index: usize| {
                args.get(index)
                    .map(|(_, arg)| self.expr(arg, place))
                    .transpose()
            };
            let timeout = args
                .iter()
                .find(|(key, _)| key.as_deref() == Some("timeout"))
                .map(|(_, value)| self.expr(value, place))
                .transpose()?
                .unwrap_or_else(|| "60s".into());
            let line = match kind.as_str() {
                "wait.tcp" => format!(
                    "std::wait::tcp({}, {timeout})?;",
                    arg(0)?.unwrap_or_default()
                ),
                "wait.http" => format!(
                    "std::wait::http({}, {timeout})?;",
                    arg(0)?.unwrap_or_default()
                ),
                "wait.file" => format!(
                    "std::wait::file({}, {timeout})?;",
                    arg(0)?.unwrap_or_default()
                ),
                "copy" => format!(
                    "std::fs::copy({}, {})?;",
                    arg(0)?.unwrap_or_default(),
                    arg(1)?.unwrap_or_default()
                ),
                "links.check" => format!(
                    "std::docs::check_links({})?;",
                    arg(0)?.unwrap_or_else(|| "\"**/*.md\"".into())
                ),
                _ => {
                    if !matches!(callee.as_ref(), Expr::Name(..) | Expr::Field(..)) {
                        return Err(Error::at(*span, "unsupported step"));
                    }
                    format!("run!({})?;", self.command_line(action, place)?)
                }
            };
            out.push(format!("{indent}{line}"));
        }
        Ok(out)
    }

    fn doc(&mut self, doc: &Option<String>, indent: &str) {
        if let Some(doc) = doc {
            for line in doc.lines() {
                self.out.push_str(&format!(
                    "{indent}///{}{line}\n",
                    if line.is_empty() { "" } else { " " }
                ));
            }
        }
    }

    /// `paths = [group, "x/**"]` → `#[paths(group, "x/**")]`.
    fn paths_attr(&self, name: &str, value: &Expr) -> Result<String, Error> {
        let args = match value {
            Expr::List(items, _) => items
                .iter()
                .map(|item| self.expr(item, Place::Config))
                .collect::<Result<Vec<_>, _>>()?
                .join(", "),
            other => self.expr(other, Place::Config)?,
        };
        Ok(format!("#[{name}({args})]"))
    }

    fn names_attr(&self, name: &str, value: &Expr) -> Result<String, Error> {
        let names = match value {
            Expr::List(items, _) => items
                .iter()
                .map(|item| self.expr(item, Place::Config))
                .collect::<Result<Vec<_>, _>>()?,
            other => vec![self.expr(other, Place::Config)?],
        };
        Ok(format!("#[{name}({})]", names.join(", ")))
    }

    fn env_attr(&self, items: &[Item]) -> Result<Option<String>, Error> {
        let mut pairs = Vec::new();
        for item in items {
            if let Item::Block { kind, items, .. } = item
                && kind == "env"
            {
                for field in items {
                    if let Item::Field { name, value, .. } = field {
                        pairs.push(format!("{name} = {}", self.expr(value, Place::Config)?));
                    }
                }
            }
        }
        Ok((!pairs.is_empty()).then(|| format!("#[env({})]", pairs.join(", "))))
    }

    /// Attributes of a group's or check's fields.
    fn field_attrs(&self, items: &[Item]) -> Result<Vec<String>, Error> {
        let mut out = Vec::new();
        for item in items {
            let Item::Field { name, value, span } = item else {
                continue;
            };
            for line in comments_above(self.source, span.start) {
                out.push(format!(
                    "//{}{line}",
                    if line.is_empty() { "" } else { " " }
                ));
            }
            out.push(match name.as_str() {
                "paths" | "reads" => self.paths_attr(name, value)?,
                "needs" | "covers" | "replaces" | "profile" => self.names_attr(name, value)?,
                "cache" => format!("#[cache({})]", self.expr(value, Place::Config)?),
                "when" => format!("#[when({})]", self.expr(value, Place::Config)?),
                "meta" => match value {
                    Expr::Map(entries, _) => {
                        let entries: Vec<String> = entries
                            .iter()
                            .map(|(key, value)| {
                                Ok(format!("{key} = {}", self.expr(value, Place::Config)?))
                            })
                            .collect::<Result<_, Error>>()?;
                        format!("#[meta({})]", entries.join(", "))
                    }
                    _ => return Err(Error::at(*span, "meta is a map")),
                },
                other => {
                    return Err(Error::at(
                        *span,
                        format!("field `{other}` has no v2 attribute here"),
                    ));
                }
            });
        }
        if let Some(env) = self.env_attr(items)? {
            out.push(env);
        }
        Ok(out)
    }

    fn check(
        &mut self,
        label: &str,
        value: Option<&Expr>,
        items: &[Item],
        doc: &Option<String>,
        indent: &str,
    ) -> Result<(), Error> {
        self.doc(doc, indent);
        for attr in self.field_attrs(items)? {
            self.out.push_str(&format!("{indent}{attr}\n"));
        }
        let value = value.ok_or_else(|| {
            Error::at(
                super::Span::default(),
                format!("check {label} runs nothing"),
            )
        })?;
        // `match changed`: one check per arm, each selected only when the
        // arms before it do not hold.
        if let Expr::Match { arms, span } = value {
            let attrs = self.field_attrs(items)?;
            let own_when = items.iter().find_map(|item| match item {
                Item::Field { name, value, .. } if name == "when" => Some(value.clone()),
                _ => None,
            });
            let mut before: Vec<String> = Vec::new();
            for (index, (condition, action)) in arms.iter().enumerate() {
                let mut parts: Vec<String> = Vec::new();
                if let Some(own) = &own_when {
                    parts.push(format!("({})", self.expr(own, Place::Config)?));
                }
                parts.extend(before.iter().map(|text| format!("!({text})")));
                let name = match condition {
                    Some(condition) => {
                        let text = self.expr(condition, Place::Config)?;
                        parts.push(format!("({text})"));
                        before.push(text);
                        match condition {
                            Expr::Name(name, _) => format!("{}_{}", snake(label), snake(name)),
                            _ => format!("{}_{index}", snake(label)),
                        }
                    }
                    None => snake(label),
                };
                if index > 0 {
                    self.out.push('\n');
                }
                self.doc(doc, indent);
                for attr in attrs.iter().filter(|attr| !attr.starts_with("#[when(")) {
                    self.out.push_str(&format!("{indent}{attr}\n"));
                }
                if !parts.is_empty() {
                    let joined = parts.join(" && ");
                    self.out.push_str(&format!("{indent}#[when({joined})]\n"));
                }
                self.out.push_str(&format!("{indent}check {name} {{\n"));
                for line in self.steps(action, Place::Config, &format!("{indent}    "))? {
                    self.out.push_str(&format!("{line}\n"));
                }
                self.out.push_str(&format!("{indent}}}\n"));
            }
            let _ = span;
            return Ok(());
        }
        self.out
            .push_str(&format!("{indent}check {} {{\n", snake(label)));
        for line in self.steps(value, Place::Config, &format!("{indent}    "))? {
            self.out.push_str(&format!("{line}\n"));
        }
        self.out.push_str(&format!("{indent}}}\n"));
        Ok(())
    }

    fn item(&mut self, item: &Item) -> Result<(), Error> {
        match item {
            Item::Use { .. } => {}
            Item::Let { name, value, span } => {
                for line in comments_above(self.source, span.start) {
                    self.out.push_str(&format!(
                        "///{}{line}\n",
                        if line.is_empty() { "" } else { " " }
                    ));
                }
                self.out.push_str(&format!(
                    "const {} = {};\n\n",
                    upper(name),
                    self.expr(value, Place::Config)?
                ));
            }
            Item::Fn {
                name, params, body, ..
            } => {
                let params: Vec<String> = params
                    .iter()
                    .map(|(param, _)| format!("{param}: list<glob>"))
                    .collect();
                let value = self.expr(&body.value, Place::Config)?;
                self.out.push_str(&format!(
                    "const fn {}({}) -> Cond {{\n    {value}\n}}\n\n",
                    snake(name),
                    params.join(", ")
                ));
            }
            Item::Field { span, .. } | Item::For { span, .. } | Item::If { span, .. } => {
                return Err(Error::at(
                    *span,
                    "a top-level field, `for` or `if` has no v2 form; rewrite it by hand",
                ));
            }
            Item::Block {
                kind,
                label,
                value,
                items,
                doc,
                span,
            } => {
                let label = label.as_ref().and_then(label_text).unwrap_or_default();
                match kind.as_str() {
                    "project" => {
                        for field in items {
                            let Item::Field { name, value, span } = field else {
                                continue;
                            };
                            self.inner.push(match name.as_str() {
                                "main" | "logs" | "receipts" | "toolchain" | "cache" => {
                                    format!("#![{name}({})]", self.expr(value, Place::Config)?)
                                }
                                "signals" | "free_version" | "after_merge" => {
                                    let line = self
                                        .command_line(value, Place::Config)?
                                        .replace("{before}", "{{before}}");
                                    format!("#![{name}(cmd!({line}))]")
                                }
                                "runner" => continue,
                                other => {
                                    return Err(Error::at(
                                        *span,
                                        format!("project field `{other}`"),
                                    ));
                                }
                            });
                        }
                    }
                    "runner" => {
                        let main = self.command_line(
                            value
                                .as_ref()
                                .ok_or_else(|| Error::at(*span, "runner without a command"))?,
                            Place::Config,
                        )?;
                        let status = items.iter().find_map(|field| match field {
                            Item::Field { name, value, .. } if name == "status" => {
                                Some(self.command_line(value, Place::Config))
                            }
                            _ => None,
                        });
                        let mut attr = format!("#![runner(cmd!({main})");
                        if let Some(status) = status {
                            attr.push_str(&format!(", status = cmd!({})", status?));
                        }
                        attr.push_str(")]");
                        self.inner.push(attr);
                    }
                    "profile" => {
                        self.doc(doc, "");
                        if let Some(env) = self.env_attr(items)? {
                            self.out.push_str(&format!("{env}\n"));
                        }
                        self.out
                            .push_str(&format!("profile {};\n\n", snake(&label)));
                    }
                    "service" => {
                        self.doc(doc, "");
                        let mut ready = None;
                        for field in items {
                            if let Item::Field { name, value, .. } = field {
                                match name.as_str() {
                                    "limit" => self.out.push_str(&format!(
                                        "#[limit({})]\n",
                                        self.expr(value, Place::Config)?
                                    )),
                                    "ready" => ready = Some(value.clone()),
                                    _ => {}
                                }
                            }
                        }
                        if value.is_none() && ready.is_none() {
                            self.out
                                .push_str(&format!("service {};\n\n", snake(&label)));
                        } else {
                            self.out
                                .push_str(&format!("service {} {{\n", snake(&label)));
                            if let Some(start) = value {
                                self.out.push_str("    start {\n");
                                for line in self.steps(start, Place::Config, "        ")? {
                                    self.out.push_str(&format!("{line}\n"));
                                }
                                self.out.push_str("    }\n");
                            }
                            if let Some(ready) = &ready {
                                self.out.push_str("    ready {\n");
                                for line in self.steps(ready, Place::Config, "        ")? {
                                    self.out.push_str(&format!("{line}\n"));
                                }
                                self.out.push_str("    }\n");
                            }
                            self.out.push_str("}\n\n");
                        }
                    }
                    "group" => {
                        self.doc(doc, "");
                        let fields: Vec<Item> = items
                            .iter()
                            .filter(
                                |item| !matches!(item, Item::Block { kind, .. } if kind == "check"),
                            )
                            .cloned()
                            .collect();
                        for attr in self.field_attrs(&fields)? {
                            self.out.push_str(&format!("{attr}\n"));
                        }
                        let has_checks = items.iter().any(
                            |item| matches!(item, Item::Block { kind, .. } if kind == "check"),
                        );
                        if !has_checks {
                            self.out
                                .push_str(&format!("group {} {{}}\n\n", snake(&label)));
                            return Ok(());
                        }
                        self.out.push_str(&format!("group {} {{\n", snake(&label)));
                        let mut first = true;
                        for inner in items {
                            if let Item::Block {
                                kind,
                                label: inner_label,
                                value,
                                items,
                                doc,
                                ..
                            } = inner
                                && kind == "check"
                            {
                                if !first {
                                    self.out.push('\n');
                                }
                                first = false;
                                let inner_label = inner_label
                                    .as_ref()
                                    .and_then(label_text)
                                    .unwrap_or_default();
                                self.check(&inner_label, value.as_ref(), items, doc, "    ")?;
                            }
                        }
                        self.out.push_str("}\n\n");
                    }
                    "check" => {
                        self.check(&label, value.as_ref(), items, doc, "")?;
                        self.out.push('\n');
                    }
                    "task" => {
                        self.doc(doc, "");
                        self.out.push_str(&format!("task {} {{\n", snake(&label)));
                        if let Some(value) = value {
                            for line in self.steps(value, Place::Config, "    ")? {
                                self.out.push_str(&format!("{line}\n"));
                            }
                        }
                        self.out.push_str("}\n\n");
                    }
                    "commands" => {
                        for field in items {
                            if let Item::Field { name, value, .. } = field {
                                self.inner.push(format!(
                                    "#![command({}, {}, {})]",
                                    quote(&label),
                                    quote(name),
                                    self.expr(value, Place::Config)?
                                ));
                            }
                        }
                    }
                    "label" => {
                        let when = items.iter().find_map(|field| match field {
                            Item::Field { name, value, .. } if name == "when" => Some(value),
                            _ => None,
                        });
                        let when = when.ok_or_else(|| Error::at(*span, "label without when"))?;
                        self.inner.push(format!(
                            "#![label({}, {})]",
                            quote(&label),
                            self.expr(when, Place::Config)?
                        ));
                    }
                    "release" => self.release(&label, items, doc)?,
                    "artifact" => {
                        self.doc(doc, "");
                        for field in items {
                            let Item::Field { name, value, span } = field else {
                                continue;
                            };
                            match (name.as_str(), value) {
                                ("inputs", Expr::Call { callee, args, .. }) if matches!(&**callee, Expr::Name(n, _) if n == "inputs_of") =>
                                {
                                    self.out.push_str(&format!(
                                        "#[inputs(cmd!({}))]\n",
                                        self.command_line(&args[0].1, Place::Config)?
                                    ));
                                }
                                ("inputs", other) => self.out.push_str(&format!(
                                    "#[inputs({})]\n",
                                    self.expr(other, Place::Config)?
                                )),
                                ("dockerfile", Expr::Map(entries, _)) => {
                                    let get = |key: &str| {
                                        entries
                                            .iter()
                                            .find(|(name, _)| name == key)
                                            .map(|(_, value)| self.expr(value, Place::Config))
                                    };
                                    self.out.push_str(&format!(
                                        "#[dockerfile({}, target = {})]\n",
                                        get("file").transpose()?.unwrap_or_default(),
                                        get("target").transpose()?.unwrap_or_default()
                                    ));
                                }
                                (other, _) => {
                                    return Err(Error::at(
                                        *span,
                                        format!("artifact field `{other}`"),
                                    ));
                                }
                            }
                        }
                        self.out
                            .push_str(&format!("artifact {};\n\n", snake(&label)));
                    }
                    "environment" => self.environment(&label, value.as_ref(), items, doc)?,
                    other => {
                        return Err(Error::at(
                            *span,
                            format!("`{other}` has no v2 form in the converter"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn release(&mut self, label: &str, items: &[Item], doc: &Option<String>) -> Result<(), Error> {
        self.doc(doc, "");
        let mut steps = Vec::new();
        for item in items {
            match item {
                Item::Field { name, value, span } => match name.as_str() {
                    "environment" => self.out.push_str(&format!(
                        "#[environment({})]\n",
                        self.expr(value, Place::Config)?
                    )),
                    "checks" => self.out.push_str("#[checks(none)]\n"),
                    "rollback" => {
                        steps.push(("rollback".to_owned(), value.clone(), Vec::new(), None))
                    }
                    other => return Err(Error::at(*span, format!("release field `{other}`"))),
                },
                Item::Block {
                    kind,
                    label,
                    value,
                    items,
                    doc,
                    ..
                } => match kind.as_str() {
                    "version" => {
                        let mut args = Vec::new();
                        for field in items {
                            if let Item::Field { name, value, .. } = field {
                                args.push(format!("{name} = {}", self.expr(value, Place::Config)?));
                            }
                        }
                        self.out
                            .push_str(&format!("#[version({})]\n", args.join(", ")));
                    }
                    "step" | "rollback" => {
                        let name = if kind == "rollback" {
                            "rollback".to_owned()
                        } else {
                            label.as_ref().and_then(label_text).unwrap_or_default()
                        };
                        steps.push((
                            name,
                            value.clone().unwrap_or(Expr::None(super::Span::default())),
                            items.clone(),
                            doc.clone(),
                        ));
                    }
                    other => {
                        return Err(Error::at(
                            item_span(item),
                            format!("release block `{other}`"),
                        ));
                    }
                },
                _ => {}
            }
        }
        self.out.push_str(&format!("release {} {{\n", snake(label)));
        for (index, (name, value, items, doc)) in steps.iter().enumerate() {
            if index > 0 {
                self.out.push('\n');
            }
            self.doc(doc, "    ");
            for field in items {
                if let Item::Field { name, value, .. } = field {
                    match name.as_str() {
                        "production" => self.out.push_str("    #[production]\n"),
                        "recover" => {
                            let function = format!("recover_{}", snake(label));
                            self.out.push_str(&format!("    #[recover({function})]\n"));
                            let body = self.steps(value, Place::Release, "    ")?;
                            self.inner
                                .push(format!("__fn__{function}\n{}", body.join("\n")));
                        }
                        _ => {}
                    }
                }
            }
            let head = if name == "rollback" {
                "rollback(r: Release)".to_owned()
            } else {
                format!("step {}(r: Release)", snake(name))
            };
            self.out.push_str(&format!("    {head} {{\n"));
            for line in self.steps(value, Place::Release, "        ")? {
                self.out.push_str(&format!("{line}\n"));
            }
            self.out.push_str("    }\n");
        }
        self.out.push_str("}\n\n");
        Ok(())
    }

    fn environment(
        &mut self,
        label: &str,
        value: Option<&Expr>,
        items: &[Item],
        doc: &Option<String>,
    ) -> Result<(), Error> {
        self.doc(doc, "");
        if let Some(Expr::Call { args, .. }) = value {
            let args: Vec<String> = args
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{} = {}",
                        key.clone().unwrap_or_default(),
                        self.expr(value, Place::Config)?
                    ))
                })
                .collect::<Result<_, Error>>()?;
            self.out
                .push_str(&format!("#[kubernetes({})]\n", args.join(", ")));
        }
        for item in items {
            match item {
                Item::Field { name, value, span } => match (name.as_str(), value) {
                    ("record", Expr::Map(entries, _)) => {
                        let entries: Vec<String> = entries
                            .iter()
                            .map(|(key, value)| {
                                Ok(if key == "resolve" {
                                    format!(
                                        "resolve = cmd!({})",
                                        self.command_line(value, Place::Config)?
                                            .replace("{release}", "{{release}}")
                                    )
                                } else {
                                    format!("{key} = {}", self.expr(value, Place::Config)?)
                                })
                            })
                            .collect::<Result<_, Error>>()?;
                        self.out
                            .push_str(&format!("#[record({})]\n", entries.join(", ")));
                    }
                    ("approval" | "checks", _) => self.out.push_str(&format!("#[{name}(none)]\n")),
                    (other, _) => {
                        return Err(Error::at(
                            *span,
                            format!("environment field `{other}`; rewrite it by hand"),
                        ));
                    }
                },
                Item::Block {
                    kind,
                    label,
                    value,
                    items,
                    ..
                } if kind == "deploy" => {
                    let workload = label.as_ref().and_then(label_text).unwrap_or_default();
                    let artifact = value
                        .as_ref()
                        .map(|value| self.expr(value, Place::Config))
                        .transpose()?
                        .unwrap_or_default();
                    let mut extra = String::new();
                    for field in items {
                        if let Item::Field { name, value, .. } = field {
                            extra.push_str(&format!(
                                ", {name} = {}",
                                self.expr(value, Place::Config)?
                            ));
                        }
                    }
                    self.out.push_str(&format!(
                        "#[deploy({}, {artifact}{extra})]\n",
                        quote(&workload)
                    ));
                }
                _ => {}
            }
        }
        self.out
            .push_str(&format!("environment {};\n\n", snake(label)));
        Ok(())
    }
}

fn item_span(item: &Item) -> super::Span {
    match item {
        Item::Use { span, .. }
        | Item::Let { span, .. }
        | Item::Fn { span, .. }
        | Item::Field { span, .. }
        | Item::Block { span, .. }
        | Item::For { span, .. }
        | Item::If { span, .. } => *span,
    }
}

/// One file in v2. `first`: it carries the project's attributes.
pub fn migrate(source: &str, names: &Names) -> Result<String, Error> {
    let file = parse_file(0, source)?;
    let mut writer = Writer {
        names,
        out: String::new(),
        inner: Vec::new(),
        source,
    };
    for item in &file.items {
        writer.item(item)?;
    }
    let mut head = String::from("#![citrus(2)]\n");
    let mut functions = String::new();
    for attr in &writer.inner {
        if let Some(rest) = attr.strip_prefix("__fn__") {
            let (name, body) = rest.split_once('\n').unwrap_or((rest, ""));
            functions.push_str(&format!(
                "fn {name}(r: Release) -> Result<()> {{\n{body}\n    Ok(())\n}}\n\n"
            ));
        } else {
            head.push_str(attr);
            head.push('\n');
        }
    }
    let _ = &names.runners;
    Ok(format!("{head}\n{}{functions}", writer.out)
        .trim_end()
        .to_owned()
        + "\n")
}
