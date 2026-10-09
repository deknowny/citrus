//! Names, types and phases of language v2, checked before anything runs.

use std::collections::BTreeMap;
use std::fmt;

use super::ast::*;
use crate::lang::{Error, Span, suggest};

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Unit,
    Bool,
    Int,
    Str,
    Path,
    Glob,
    Duration,
    Version,
    List(Box<Ty>),
    Option(Box<Ty>),
    Result(Box<Ty>),
    Struct(String),
    Command,
    Output,
    Release,
    Error,
    /// A change a `#[test]` plans: `std::plan::change(paths)`.
    Change,
    /// What Citrus would run for a change.
    Plan,
    /// A plan condition: `touched(…)`, `signal("…")`, combined with `&&`, `||`, `!`.
    Cond,
    /// Not known yet: an empty list, a `return` that never falls through.
    Unknown,
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Unit => write!(f, "()"),
            Ty::Bool => write!(f, "bool"),
            Ty::Int => write!(f, "int"),
            Ty::Str => write!(f, "str"),
            Ty::Path => write!(f, "path"),
            Ty::Glob => write!(f, "glob"),
            Ty::Duration => write!(f, "duration"),
            Ty::Version => write!(f, "Version"),
            Ty::List(item) => write!(f, "list<{item}>"),
            Ty::Option(item) => write!(f, "Option<{item}>"),
            Ty::Result(item) => write!(f, "Result<{item}>"),
            Ty::Struct(name) => write!(f, "{name}"),
            Ty::Command => write!(f, "Command"),
            Ty::Output => write!(f, "Output"),
            Ty::Release => write!(f, "Release"),
            Ty::Error => write!(f, "Error"),
            Ty::Change => write!(f, "Change"),
            Ty::Plan => write!(f, "Plan"),
            Ty::Cond => write!(f, "Cond"),
            Ty::Unknown => write!(f, "_"),
        }
    }
}

/// A value of type `from` may stand where `to` is expected.
pub fn assignable(from: &Ty, to: &Ty) -> bool {
    match (from, to) {
        (Ty::Unknown, _) | (_, Ty::Unknown) => true,
        // Text becomes a path or a glob where one is expected; a path is text.
        (Ty::Str, Ty::Path | Ty::Glob) | (Ty::Path, Ty::Str) => true,
        (Ty::List(a), Ty::List(b))
        | (Ty::Option(a), Ty::Option(b))
        | (Ty::Result(a), Ty::Result(b)) => assignable(a, b),
        (a, b) => a == b,
    }
}

/// The common type of two branches, if any.
fn unify(a: &Ty, b: &Ty) -> Option<Ty> {
    match (a, b) {
        (Ty::Unknown, other) | (other, Ty::Unknown) => Some(other.clone()),
        (Ty::List(x), Ty::List(y)) => unify(x, y).map(|t| Ty::List(Box::new(t))),
        (Ty::Option(x), Ty::Option(y)) => unify(x, y).map(|t| Ty::Option(Box::new(t))),
        (Ty::Result(x), Ty::Result(y)) => unify(x, y).map(|t| Ty::Result(Box::new(t))),
        _ if a == b => Some(a.clone()),
        _ if assignable(a, b) => Some(b.clone()),
        _ if assignable(b, a) => Some(a.clone()),
        _ => None,
    }
}

fn result_unit() -> Ty {
    Ty::Result(Box::new(Ty::Unit))
}

/// A `std` function: parameters, result, and whether it touches the world.
pub struct StdFn {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub io: bool,
}

pub fn std_fn(path: &[String]) -> Option<StdFn> {
    let joined = path.join("::");
    let f = |params: Vec<Ty>, ret: Ty, io: bool| Some(StdFn { params, ret, io });
    match joined.as_str() {
        "std::proc::Command::new" => f(vec![Ty::Str], Ty::Command, false),
        "std::fs::read" => f(vec![Ty::Path], Ty::Result(Box::new(Ty::Str)), true),
        "std::fs::exists" => f(vec![Ty::Path], Ty::Bool, true),
        "std::fs::glob" => f(vec![Ty::Glob], Ty::List(Box::new(Ty::Path)), true),
        "std::env::var" => f(vec![Ty::Str], Ty::Option(Box::new(Ty::Str)), true),
        "std::env::platform" => f(vec![], Ty::Str, true),
        "std::wait::http" => f(vec![Ty::Str, Ty::Duration], result_unit(), true),
        "std::wait::tcp" => f(vec![Ty::Str, Ty::Duration], result_unit(), true),
        "std::wait::file" => f(vec![Ty::Path, Ty::Duration], result_unit(), true),
        "std::fs::copy" => f(vec![Ty::Path, Ty::Path], result_unit(), true),
        "std::docs::check_links" => f(vec![Ty::Glob], result_unit(), true),
        "std::log::info" => f(vec![Ty::Str], Ty::Unit, true),
        "std::plan::change" => f(vec![Ty::List(Box::new(Ty::Path))], Ty::Change, false),
        "std::plan::of" => f(
            vec![Ty::List(Box::new(Ty::Path))],
            Ty::Result(Box::new(Ty::Plan)),
            true,
        ),
        _ => None,
    }
}

pub const STD_FUNCTIONS: &[&str] = &[
    "std::paths::cargo",
    "std::paths::next",
    "std::paths::package",
    "std::proc::Command::new",
    "std::fs::read",
    "std::fs::exists",
    "std::fs::glob",
    "std::env::var",
    "std::env::platform",
    "std::wait::http",
    "std::wait::tcp",
    "std::wait::file",
    "std::fs::copy",
    "std::docs::check_links",
    "std::log::info",
    "std::plan::change",
    "std::plan::of",
];

/// A method of a built-in type: parameters, result, I/O, needs a `let mut` receiver.
pub fn method(receiver: &Ty, name: &str) -> Option<(Vec<Ty>, Ty, bool, bool)> {
    let pure = |params: Vec<Ty>, ret: Ty| Some((params, ret, false, false));
    match (receiver, name) {
        (Ty::Str | Ty::Path, "len") => pure(vec![], Ty::Int),
        (Ty::Str | Ty::Path, "count") => pure(vec![Ty::Str], Ty::Int),
        (Ty::Str | Ty::Path, "contains" | "starts_with" | "ends_with") => {
            pure(vec![Ty::Str], Ty::Bool)
        }
        (Ty::Str | Ty::Path, "find") => pure(vec![Ty::Str], Ty::Option(Box::new(Ty::Int))),
        (Ty::Str | Ty::Path, "trim") => pure(vec![], Ty::Str),
        (Ty::Str | Ty::Path, "is_empty") => pure(vec![], Ty::Bool),
        (Ty::Str | Ty::Path, "lines") => pure(vec![], Ty::List(Box::new(Ty::Str))),
        (Ty::Str | Ty::Path, "split") => pure(vec![Ty::Str], Ty::List(Box::new(Ty::Str))),
        (Ty::Path, "matches") => pure(vec![Ty::Glob], Ty::Bool),
        (Ty::List(_), "len") => pure(vec![], Ty::Int),
        (Ty::List(_), "is_empty") => pure(vec![], Ty::Bool),
        (Ty::List(item), "contains") => pure(vec![(**item).clone()], Ty::Bool),
        (Ty::List(item), "first" | "last") => pure(vec![], Ty::Option(item.clone())),
        (Ty::List(item), "push") => Some((vec![(**item).clone()], Ty::Unit, false, true)),
        (Ty::List(item), "join")
            if matches!(**item, Ty::Str | Ty::Path | Ty::Glob | Ty::Unknown) =>
        {
            pure(vec![Ty::Str], Ty::Str)
        }
        (Ty::Option(_), "is_some" | "is_none") => pure(vec![], Ty::Bool),
        (Ty::Option(item), "unwrap_or") => pure(vec![(**item).clone()], (**item).clone()),
        (Ty::Option(item), "ok_or") => pure(vec![Ty::Str], Ty::Result(item.clone())),
        (Ty::Result(_), "is_ok" | "is_err") => pure(vec![], Ty::Bool),
        (Ty::Result(item), "context") => pure(vec![Ty::Str], Ty::Result(item.clone())),
        (Ty::Result(item), "ok") => pure(vec![], Ty::Option(item.clone())),
        (Ty::Version, "bump") => pure(vec![], Ty::Version),
        (Ty::Command, "arg") => pure(vec![Ty::Str], Ty::Command),
        (Ty::Command, "args") => pure(vec![Ty::List(Box::new(Ty::Str))], Ty::Command),
        (Ty::Command, "env") => pure(vec![Ty::Str, Ty::Str], Ty::Command),
        (Ty::Command, "current_dir") => pure(vec![Ty::Path], Ty::Command),
        (Ty::Command, "run") => Some((vec![], result_unit(), true, false)),
        (Ty::Command, "output") => Some((vec![], Ty::Result(Box::new(Ty::Output)), true, false)),
        (Ty::Change, "profile") => pure(vec![Ty::Str], Ty::Change),
        (Ty::Change, "env") => pure(vec![Ty::Str, Ty::Str], Ty::Change),
        (Ty::Change, "plan") => Some((vec![], Ty::Result(Box::new(Ty::Plan)), true, false)),
        (Ty::Plan, "selects") => pure(vec![Ty::Str], Ty::Bool),
        (Ty::Plan, "owners" | "groups_of") => pure(vec![Ty::Path], Ty::List(Box::new(Ty::Str))),
        _ => None,
    }
}

fn methods_of(receiver: &Ty) -> Vec<&'static str> {
    [
        "len",
        "count",
        "contains",
        "starts_with",
        "ends_with",
        "find",
        "trim",
        "is_empty",
        "lines",
        "split",
        "matches",
        "first",
        "last",
        "push",
        "join",
        "is_some",
        "is_none",
        "unwrap_or",
        "ok_or",
        "is_ok",
        "is_err",
        "context",
        "ok",
        "bump",
        "arg",
        "args",
        "env",
        "current_dir",
        "run",
        "output",
        "profile",
        "plan",
        "selects",
        "owners",
        "groups_of",
    ]
    .into_iter()
    .filter(|name| method(receiver, name).is_some())
    .collect()
}

/// Fields of built-in structs.
pub fn builtin_field(receiver: &Ty, name: &str) -> Option<Ty> {
    match (receiver, name) {
        (Ty::Output, "code") => Some(Ty::Int),
        (Ty::Output, "stdout" | "stderr") => Some(Ty::Str),
        (Ty::Release, "version") => Some(Ty::Version),
        (Ty::Release, "previous") => Some(Ty::Option(Box::new(Ty::Version))),
        (Ty::Release, "commit" | "unit") => Some(Ty::Str),
        (Ty::Error, "message") => Some(Ty::Str),
        (Ty::Plan, "checks" | "groups" | "labels" | "signals" | "notes") => {
            Some(Ty::List(Box::new(Ty::Str)))
        }
        (Ty::Plan, "unclaimed") => Some(Ty::List(Box::new(Ty::Path))),
        _ => None,
    }
}

pub struct FnSig {
    pub params: Vec<Ty>,
    pub ret: Ty,
    pub is_const: bool,
}

/// Item names, constants, functions and structs of a program.
pub struct Globals {
    pub consts: BTreeMap<String, Ty>,
    pub fns: BTreeMap<String, FnSig>,
    pub structs: BTreeMap<String, Vec<(String, Ty)>>,
    /// group, check, task, environment, release names.
    pub items: BTreeMap<String, &'static str>,
}

fn kind_name(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Const { .. } => "const",
        ItemKind::Fn(_) => "fn",
        ItemKind::Struct { .. } => "struct",
        ItemKind::Group { .. } => "group",
        ItemKind::Check { .. } => "check",
        ItemKind::Task { .. } => "task",
        ItemKind::Environment => "environment",
        ItemKind::Profile => "profile",
        ItemKind::Service { .. } => "service",
        ItemKind::Artifact => "artifact",
        ItemKind::Release { .. } => "release",
    }
}

pub fn resolve_type(
    ty: &TypeExpr,
    structs: &BTreeMap<String, Vec<(String, Ty)>>,
    known: &[String],
) -> Result<Ty, Error> {
    let arity = |n: usize| -> Result<(), Error> {
        if ty.args.len() == n {
            Ok(())
        } else {
            Err(Error::at(
                ty.span,
                format!("`{}` takes {n} type argument(s)", ty.name),
            ))
        }
    };
    let inner = |index: usize| resolve_type(&ty.args[index], structs, known).map(Box::new);
    Ok(match ty.name.as_str() {
        "()" => Ty::Unit,
        "bool" => Ty::Bool,
        "int" => Ty::Int,
        "str" => Ty::Str,
        "path" => Ty::Path,
        "glob" => Ty::Glob,
        "duration" => Ty::Duration,
        "Version" => Ty::Version,
        "Command" => Ty::Command,
        "Output" => Ty::Output,
        "Release" => Ty::Release,
        "Error" => Ty::Error,
        "Change" => Ty::Change,
        "Plan" => Ty::Plan,
        "Cond" => Ty::Cond,
        "list" => {
            arity(1)?;
            Ty::List(inner(0)?)
        }
        "Option" => {
            arity(1)?;
            Ty::Option(inner(0)?)
        }
        "Result" => {
            if ty.args.len() == 2 {
                return Err(
                    Error::at(ty.span, "`Result` has one error type: write `Result<T>`").help(
                        "an error is a message with its place; add one with `.context(\"…\")`",
                    ),
                );
            }
            arity(1)?;
            Ty::Result(inner(0)?)
        }
        name if structs.contains_key(name) || known.iter().any(|known| known == name) => {
            Ty::Struct(name.to_owned())
        }
        "String" | "string" => {
            return Err(
                Error::at(ty.span, format!("unknown type `{}`", ty.name)).help("text is `str`")
            );
        }
        "Vec" => return Err(Error::at(ty.span, "unknown type `Vec`").help("a list is `list<T>`")),
        "i64" | "u64" | "i32" | "usize" => {
            return Err(
                Error::at(ty.span, format!("unknown type `{}`", ty.name)).help("numbers are `int`")
            );
        }
        other => {
            return Err(Error::at(ty.span, format!("unknown type `{other}`")).help(
                "types: bool, int, str, path, glob, duration, Version, list<T>, Option<T>, Result<T>, structs",
            ));
        }
    })
}

/// Collect the program's names and check every body.
pub fn check(program: &Program) -> Result<Globals, Error> {
    let mut globals = Globals {
        consts: BTreeMap::new(),
        fns: BTreeMap::new(),
        structs: BTreeMap::new(),
        items: BTreeMap::new(),
    };
    let mut seen: BTreeMap<String, Span> = BTreeMap::new();
    let mut declare = |name: &str, span: Span| -> Result<(), Error> {
        if seen.insert(name.to_owned(), span).is_some() {
            return Err(Error::at(span, format!("`{name}` is declared twice")));
        }
        Ok(())
    };
    let struct_names: Vec<String> = program
        .items
        .iter()
        .filter(|item| matches!(item.kind, ItemKind::Struct { .. }))
        .map(|item| item.name.clone())
        .collect();
    for item in &program.items {
        declare(&item.name, item.name_span)?;
        if let ItemKind::Group { items } = &item.kind {
            for inner in items {
                if !matches!(inner.kind, ItemKind::Check { .. }) {
                    return Err(Error::at(inner.span, "a group holds checks")
                        .help("declare functions and constants outside the group"));
                }
                declare(&format!("{}::{}", item.name, inner.name), inner.name_span)?;
                globals
                    .items
                    .insert(format!("{}::{}", item.name, inner.name), "check");
            }
        }
        globals
            .items
            .insert(item.name.clone(), kind_name(&item.kind));
    }
    for item in &program.items {
        if let ItemKind::Struct { fields } = &item.kind {
            let mut resolved = Vec::new();
            for (name, ty) in fields {
                resolved.push((
                    name.clone(),
                    resolve_type(ty, &BTreeMap::new(), &struct_names)?,
                ));
            }
            globals.structs.insert(item.name.clone(), resolved);
        }
    }
    for item in &program.items {
        if let ItemKind::Fn(function) = &item.kind {
            let mut params = Vec::new();
            for param in &function.params {
                params.push(resolve_type(&param.ty, &globals.structs, &[])?);
            }
            let ret = match &function.ret {
                Some(ty) => resolve_type(ty, &globals.structs, &[])?,
                None => Ty::Unit,
            };
            globals.fns.insert(
                item.name.clone(),
                FnSig {
                    params,
                    ret,
                    is_const: function.is_const,
                },
            );
        }
    }
    // Constants in order: each may use the ones before it.
    for item in &program.items {
        if let ItemKind::Const { ty, value } = &item.kind {
            if !item
                .name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return Err(Error::at(
                    item.span,
                    format!("constants are UPPER_CASE: `{}`", item.name.to_uppercase()),
                ));
            }
            let mut checker = Checker::new(&globals, Phase::Load, Ty::Unknown);
            let found = checker.expr(value)?;
            let declared = match ty {
                Some(ty) => {
                    let declared = resolve_type(ty, &globals.structs, &[])?;
                    expect(&found, &declared, value.span())?;
                    declared
                }
                None => found,
            };
            globals.consts.insert(item.name.clone(), declared);
        }
    }
    for attr in &program.inner {
        check_attr(attr, &globals)?;
    }
    for item in &program.items {
        check_item(item, &globals)?;
    }
    Ok(globals)
}

fn check_item(item: &Item, globals: &Globals) -> Result<(), Error> {
    check_item_in(item, globals, None)
}

fn check_item_in(item: &Item, globals: &Globals, group: Option<&str>) -> Result<(), Error> {
    for attr in &item.attrs {
        check_attr_in(attr, globals, group)?;
    }
    match &item.kind {
        ItemKind::Fn(function) => {
            let sig = &globals.fns[&item.name];
            if item.attrs.iter().any(|attr| attr.name == "test")
                && (function.is_const
                    || !function.params.is_empty()
                    || !matches!(&sig.ret, Ty::Unit) && sig.ret != result_unit())
            {
                return Err(Error::at(
                    item.span,
                    "a `#[test]` is `fn name() -> Result<()>`: no parameters, not `const`",
                ));
            }
            let phase = if function.is_const {
                Phase::Plan
            } else {
                Phase::Run
            };
            let mut checker = Checker::new(globals, phase, sig.ret.clone());
            for (param, ty) in function.params.iter().zip(&sig.params) {
                checker.bind(&param.name, ty.clone(), false);
            }
            let found = checker.block(&function.body)?;
            checker.finish(&function.body, &found, &sig.ret)?;
        }
        ItemKind::Check { body } | ItemKind::Task { body } => {
            let mut checker = Checker::new(globals, Phase::Run, result_unit());
            let found = checker.block(body)?;
            checker.finish(body, &found, &result_unit())?;
        }
        ItemKind::Group { items } => {
            for inner in items {
                check_item_in(inner, globals, Some(&item.name))?;
            }
        }
        ItemKind::Release { steps, rollback } => {
            for step in steps.iter().chain(rollback) {
                for attr in &step.attrs {
                    check_attr(attr, globals)?;
                }
                let mut checker = Checker::new(globals, Phase::Run, result_unit());
                if let Some(param) = &step.param {
                    let ty = resolve_type(&param.ty, &globals.structs, &[])?;
                    if ty != Ty::Release {
                        return Err(Error::at(param.ty.span, "a step takes `Release`").help(
                            format!("step {}({}: Release) {{ … }}", step.name, param.name),
                        ));
                    }
                    checker.bind(&param.name, Ty::Release, false);
                }
                let found = checker.block(&step.body)?;
                checker.finish(&step.body, &found, &result_unit())?;
            }
        }
        ItemKind::Service { start, ready, stop } => {
            for body in start.iter().chain(ready).chain(stop) {
                let mut checker = Checker::new(globals, Phase::Run, result_unit());
                let found = checker.block(body)?;
                checker.finish(body, &found, &result_unit())?;
            }
        }
        ItemKind::Const { .. }
        | ItemKind::Struct { .. }
        | ItemKind::Environment
        | ItemKind::Profile
        | ItemKind::Artifact => {}
    }
    Ok(())
}

/// Attribute arguments are evaluated when the file loads.
/// Conditions of the plan, usable in `#[when]`, `#![label]`, `const` and `const fn`.
pub const CONDITION_FNS: &[&str] = &[
    "touched", "only", "without", "signal", "selected", "profile", "env",
];

/// Attributes that name items, and the kind each names.
const NAMING: &[(&str, &[&str])] = &[
    ("needs", &["service"]),
    ("after", &["check"]),
    ("covers", &["check"]),
    ("replaces", &["check"]),
    ("profile", &["profile"]),
    ("environment", &["environment"]),
    ("recover", &["fn"]),
];

/// Attributes whose arguments are configuration values (text, numbers,
/// lists, commands, item names), checked as constants.
const CONFIG: &[&str] = &[
    "env",
    "env_file",
    "meta",
    "limit",
    "inputs",
    "dockerfile",
    "build",
    "publish",
    "kubernetes",
    "record",
    "deploy",
    "approval",
    "checks",
    "release_name",
    "manifests",
    "gate",
    "prepare",
    "migrations",
    "verify",
    "main",
    "toolchain",
    "logs",
    "receipts",
    "signals",
    "free_version",
    "after_merge",
    "runner",
    "image",
    "private",
    "prepare",
    "command",
    "citrus",
    "tool",
    "pin",
];

/// `#[when(…)]` and `#![label(…)]` hold plan conditions, compiled separately.
const CONDITIONS: &[&str] = &["when", "label"];

fn not_found(globals: &Globals, name: &str, kinds: &[&str], span: Span) -> Error {
    let known = globals
        .items
        .iter()
        .filter(|(_, kind)| kinds.contains(kind))
        .map(|(name, _)| name.as_str());
    let mut error = Error::at(span, format!("no {} named `{name}`", kinds.join(" or ")));
    if let Some(close) = suggest(name, known) {
        error = error.help(format!("did you mean `{close}`?"));
    }
    error
}

pub fn check_attr(attr: &Attr, globals: &Globals) -> Result<(), Error> {
    check_attr_in(attr, globals, None)
}

/// `group`: the group the attribute's item is in (its checks' names are relative).
pub fn check_attr_in(attr: &Attr, globals: &Globals, group: Option<&str>) -> Result<(), Error> {
    let name = attr.name.as_str();
    if let Some((_, kinds)) = NAMING.iter().find(|(known, _)| *known == name) {
        for (_, arg) in &attr.args {
            let Expr::Path(segments, span) = arg else {
                return Err(Error::at(
                    arg.span(),
                    format!("`#[{name}]` takes names of {}", kinds.join(" or ")),
                ));
            };
            let item = segments.join("::");
            let known = |name: &str| {
                globals
                    .items
                    .get(name)
                    .is_some_and(|kind| kinds.contains(kind))
            };
            let relative = group.is_some_and(|group| known(&format!("{group}::{item}")));
            if !known(&item) && !relative {
                return Err(not_found(globals, &item, kinds, *span));
            }
        }
        return Ok(());
    }
    match name {
        "paths" | "reads" | "outputs" => {
            for (_, arg) in &attr.args {
                // A group's name: its paths select the check too.
                if let Expr::Path(segments, span) = arg {
                    let name = segments.join("::");
                    if globals.items.get(&name) == Some(&"group") {
                        continue;
                    }
                    if !globals.consts.contains_key(&name) {
                        let known = globals
                            .items
                            .iter()
                            .filter(|(_, kind)| **kind == "group")
                            .map(|(name, _)| name.as_str())
                            .chain(globals.consts.keys().map(String::as_str));
                        let mut error = Error::at(*span, format!("no group or constant `{name}`"));
                        if let Some(close) = suggest(&name, known) {
                            error = error.help(format!("did you mean `{close}`?"));
                        }
                        return Err(error);
                    }
                }
                let mut checker = Checker::new(globals, Phase::Load, Ty::Unknown);
                let ty = checker.expr(arg)?;
                if !matches!(ty, Ty::Glob | Ty::Str | Ty::Unknown)
                    && !assignable(&ty, &Ty::List(Box::new(Ty::Glob)))
                {
                    return Err(Error::at(
                        arg.span(),
                        format!("`#[{name}]` takes globs or group names, not {ty}"),
                    ));
                }
            }
        }
        "test" if attr.args.is_empty() => {}
        "test" => return Err(Error::at(attr.span, "`#[test]` takes no arguments")),
        "cache" | "production" => {
            for (_, arg) in &attr.args {
                let mut checker = Checker::new(globals, Phase::Load, Ty::Unknown);
                let ty = checker.expr(arg)?;
                expect(&ty, &Ty::Bool, arg.span())?;
            }
        }
        "version" => {
            for (key, arg) in &attr.args {
                let mut checker = Checker::new(globals, Phase::Load, Ty::Unknown);
                let ty = checker.expr(arg)?;
                match key.as_deref() {
                    Some("initial") => expect(&ty, &Ty::Str, arg.span())?,
                    Some("scope") => expect(&ty, &Ty::List(Box::new(Ty::Str)), arg.span())?,
                    _ => {
                        return Err(Error::at(
                            arg.span(),
                            "`#[version]` takes `initial = \"…\"` and `scope = [\"…\"]`",
                        ));
                    }
                }
            }
        }
        _ if CONDITIONS.contains(&name) => {
            let last = attr.args.last().map(|(_, arg)| arg);
            if let Some(arg) = last {
                let mut checker = Checker::new(globals, Phase::Load, Ty::Unknown);
                let ty = checker.expr(arg)?;
                if !matches!(ty, Ty::Cond | Ty::Bool | Ty::Unknown) {
                    return Err(Error::at(
                        arg.span(),
                        format!("`#[{name}]` takes a condition, not {ty}"),
                    ));
                }
            }
        }
        _ if CONFIG.contains(&name) => {
            for (_, arg) in &attr.args {
                config_value(arg, globals)?;
            }
        }
        other => {
            let known = NAMING
                .iter()
                .map(|(name, _)| *name)
                .chain(CONFIG.iter().copied())
                .chain(CONDITIONS.iter().copied())
                .chain([
                    "paths",
                    "reads",
                    "outputs",
                    "cache",
                    "production",
                    "version",
                    "test",
                ]);
            let mut error = Error::at(attr.span, format!("unknown attribute `#[{other}]`"));
            if let Some(close) = suggest(other, known) {
                error = error.help(format!("did you mean `#[{close}]`?"));
            }
            return Err(error);
        }
    }
    Ok(())
}

/// A configuration value: an item's name, or a constant expression.
fn config_value(arg: &Expr, globals: &Globals) -> Result<(), Error> {
    match arg {
        Expr::Path(segments, _) if globals.items.contains_key(&segments.join("::")) => Ok(()),
        // Bare words of a configuration (`#[approval(none)]`, `#[command(release, …)]`).
        Expr::Path(segments, _)
            if segments.len() == 1 && !globals.consts.contains_key(&segments[0]) =>
        {
            Ok(())
        }
        Expr::List(items, _) => items
            .iter()
            .try_for_each(|item| config_value(item, globals)),
        _ => {
            let mut checker = Checker::new(globals, Phase::Load, Ty::Unknown);
            checker.expr(arg).map(|_| ())
        }
    }
}

fn expect(found: &Ty, wanted: &Ty, span: Span) -> Result<(), Error> {
    if assignable(found, wanted) {
        Ok(())
    } else {
        Err(Error::at(span, format!("expected {wanted}, found {found}")))
    }
}

#[derive(Clone, Copy, PartialEq)]
pub enum Phase {
    /// Constants and attributes: no I/O, only `const fn`.
    Load,
    /// `const fn` bodies: the same rules.
    Plan,
    /// Bodies of checks, tasks, steps and plain functions.
    Run,
}

struct Checker<'a> {
    globals: &'a Globals,
    phase: Phase,
    ret: Ty,
    scopes: Vec<BTreeMap<String, (Ty, bool)>>,
}

impl<'a> Checker<'a> {
    fn new(globals: &'a Globals, phase: Phase, ret: Ty) -> Checker<'a> {
        Checker {
            globals,
            phase,
            ret,
            scopes: vec![BTreeMap::new()],
        }
    }

    fn bind(&mut self, name: &str, ty: Ty, mutable: bool) {
        self.scopes
            .last_mut()
            .expect("scope")
            .insert(name.to_owned(), (ty, mutable));
    }

    fn local(&self, name: &str) -> Option<&(Ty, bool)> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    fn pure_only(&self) -> bool {
        self.phase != Phase::Run
    }

    /// A body that falls off its end returns its tail, or `Ok(())` for `Result<()>`.
    fn finish(&self, body: &Block, found: &Ty, ret: &Ty) -> Result<(), Error> {
        if body.tail.is_some() {
            return expect(
                found,
                ret,
                body.tail.as_ref().map_or(body.span, |tail| tail.span()),
            );
        }
        let diverges = matches!(body.stmts.last(), Some(Stmt::Return { .. }));
        if diverges || *ret == Ty::Unit || *ret == result_unit() {
            return Ok(());
        }
        Err(Error::at(
            body.span,
            format!("this body must end with a value of type {ret}"),
        ))
    }

    fn block(&mut self, block: &Block) -> Result<Ty, Error> {
        self.scopes.push(BTreeMap::new());
        let result = self.block_inner(block);
        self.scopes.pop();
        result
    }

    fn block_inner(&mut self, block: &Block) -> Result<Ty, Error> {
        for stmt in &block.stmts {
            self.stmt(stmt)?;
        }
        match &block.tail {
            Some(tail) => self.expr(tail),
            None if matches!(block.stmts.last(), Some(Stmt::Return { .. })) => Ok(Ty::Unknown),
            None => Ok(Ty::Unit),
        }
    }

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), Error> {
        match stmt {
            Stmt::Let {
                name,
                mutable,
                ty,
                value,
                ..
            } => {
                let found = self.expr(value)?;
                let ty = match ty {
                    Some(ty) => {
                        let declared = resolve_type(ty, &self.globals.structs, &[])?;
                        expect(&found, &declared, value.span())?;
                        declared
                    }
                    None => found,
                };
                self.bind(name, ty, *mutable);
            }
            Stmt::Assign { name, value, span } => {
                let Some((ty, mutable)) = self.local(name).cloned() else {
                    return Err(self.unknown(name, *span));
                };
                if !mutable {
                    return Err(Error::at(*span, format!("`{name}` is not mutable"))
                        .help(format!("declare it with `let mut {name}`")));
                }
                let found = self.expr(value)?;
                expect(&found, &ty, value.span())?;
                if ty == Ty::Unknown || matches!(&ty, Ty::List(item) if **item == Ty::Unknown) {
                    for scope in self.scopes.iter_mut().rev() {
                        if let Some(slot) = scope.get_mut(name) {
                            slot.0 = found;
                            break;
                        }
                    }
                }
            }
            Stmt::Assert {
                cond,
                message,
                span,
            } => {
                if !matches!(self.ret, Ty::Result(_) | Ty::Unknown) {
                    return Err(Error::at(
                        *span,
                        "`assert` fails the body: it needs a function that returns `Result`",
                    ));
                }
                let found = self.expr(cond)?;
                expect(&found, &Ty::Bool, cond.span())?;
                if let Some(message) = message {
                    let found = self.expr(message)?;
                    expect(&found, &Ty::Str, message.span())?;
                }
            }
            Stmt::Return { value, span } => {
                let found = match value {
                    Some(value) => self.expr(value)?,
                    None => Ty::Unit,
                };
                let ret = self.ret.clone();
                if !assignable(&found, &ret) {
                    // `return Ok(())` and a bare `return` in a `Result<()>` body.
                    if !(value.is_none() && ret == result_unit()) {
                        return Err(Error::at(*span, format!("returns {found}, expected {ret}")));
                    }
                }
            }
            Stmt::For {
                var, iter, body, ..
            } => {
                let found = self.expr(iter)?;
                let item = match found {
                    Ty::List(item) => *item,
                    Ty::Unknown => Ty::Unknown,
                    other => {
                        return Err(Error::at(
                            iter.span(),
                            format!("`for` loops over a list, not {other}"),
                        ));
                    }
                };
                self.scopes.push(BTreeMap::new());
                self.bind(var, item, false);
                let found = self.block(body);
                self.scopes.pop();
                let found = found?;
                expect(&found, &Ty::Unit, body.span)?;
            }
            Stmt::Expr(expr) => {
                let found = self.expr(expr)?;
                if matches!(found, Ty::Result(_))
                    && !matches!(expr, Expr::If { .. } | Expr::Match { .. } | Expr::Block(_))
                {
                    return Err(Error::at(expr.span(), "this `Result` is ignored")
                        .help("add `?` to fail with its error"));
                }
            }
        }
        Ok(())
    }

    fn unknown(&self, name: &str, span: Span) -> Error {
        let locals: Vec<&str> = self
            .scopes
            .iter()
            .flat_map(|scope| scope.keys().map(String::as_str))
            .collect();
        let known = locals
            .into_iter()
            .chain(self.globals.consts.keys().map(String::as_str))
            .chain(self.globals.fns.keys().map(String::as_str));
        let mut error = Error::at(span, format!("unknown name `{name}`"));
        if let Some(close) = suggest(name, known) {
            error = error.help(format!("did you mean `{close}`?"));
        }
        error
    }

    pub fn expr(&mut self, expr: &Expr) -> Result<Ty, Error> {
        Ok(match expr {
            Expr::Unit(_) => Ty::Unit,
            Expr::Bool(..) => Ty::Bool,
            Expr::Int(..) => Ty::Int,
            Expr::Duration(..) => Ty::Duration,
            Expr::Str(parts, _) => {
                for part in parts {
                    if let StrPart::Expr(inner) = part {
                        let ty = self.expr(inner)?;
                        if matches!(ty, Ty::List(_) | Ty::Command | Ty::Unit) {
                            return Err(Error::at(
                                inner.span(),
                                format!("a {ty} cannot be put into a string"),
                            )
                            .help(if matches!(ty, Ty::List(_)) {
                                "`{xs.join(\", \")}`"
                            } else {
                                "interpolate a field or a text"
                            }));
                        }
                    }
                }
                Ty::Str
            }
            Expr::Path(segments, span) => {
                let name = segments.join("::");
                if segments.len() == 1 {
                    if name == "None" {
                        return Ok(Ty::Option(Box::new(Ty::Unknown)));
                    }
                    if let Some((ty, _)) = self.local(&name) {
                        return Ok(ty.clone());
                    }
                    if let Some(ty) = self.globals.consts.get(&name) {
                        return Ok(ty.clone());
                    }
                    if self.globals.fns.contains_key(&name) {
                        return Err(Error::at(
                            *span,
                            format!("`{name}` is a function: call it, `{name}(…)`"),
                        ));
                    }
                    if let Some(kind) = self.globals.items.get(&name) {
                        return Err(
                            Error::at(*span, format!("`{name}` is a {kind}, not a value"))
                                .help("items are named in attributes, e.g. `#[needs(…)]`"),
                        );
                    }
                }
                return Err(self.unknown(&name, *span));
            }
            Expr::List(items, span) => {
                let mut item_ty = Ty::Unknown;
                for item in items {
                    let found = self.expr(item)?;
                    // A glob list may hold other glob lists (`[RUST, "x/**"]`): they are flattened.
                    let found = match (&item_ty, found) {
                        (_, Ty::List(inner))
                            if matches!(*inner, Ty::Glob | Ty::Str)
                                && matches!(item_ty, Ty::Glob | Ty::Str | Ty::Unknown) =>
                        {
                            *inner
                        }
                        (_, found) => found,
                    };
                    item_ty = unify(&item_ty, &found).ok_or_else(|| {
                        Error::at(
                            item.span(),
                            format!("a list holds one type: {item_ty} and {found}"),
                        )
                    })?;
                }
                let _ = span;
                Ty::List(Box::new(item_ty))
            }
            Expr::StructLit { name, fields, span } => {
                let Some(declared) = self.globals.structs.get(name).cloned() else {
                    let mut error = Error::at(*span, format!("unknown struct `{name}`"));
                    if let Some(close) =
                        suggest(name, self.globals.structs.keys().map(String::as_str))
                    {
                        error = error.help(format!("did you mean `{close}`?"));
                    }
                    return Err(error);
                };
                for (field, value) in fields {
                    let Some((_, ty)) = declared.iter().find(|(name, _)| name == field) else {
                        return Err(Error::at(
                            value.span(),
                            format!("`{name}` has no field `{field}`"),
                        ));
                    };
                    let found = self.expr(value)?;
                    expect(&found, ty, value.span())?;
                }
                for (field, _) in &declared {
                    if !fields.iter().any(|(name, _)| name == field) {
                        return Err(Error::at(
                            *span,
                            format!("missing field `{field}` of `{name}`"),
                        ));
                    }
                }
                Ty::Struct(name.clone())
            }
            Expr::Field(receiver, name, span) => {
                let ty = self.expr(receiver)?;
                if let Some(found) = builtin_field(&ty, name) {
                    return Ok(found);
                }
                if let Ty::Struct(struct_name) = &ty
                    && let Some((_, found)) = self
                        .globals
                        .structs
                        .get(struct_name)
                        .and_then(|fields| fields.iter().find(|(field, _)| field == name))
                {
                    return Ok(found.clone());
                }
                return Err(Error::at(*span, format!("{ty} has no field `{name}`")));
            }
            Expr::Index(list, index, span) => {
                let ty = self.expr(list)?;
                let index_ty = self.expr(index)?;
                expect(&index_ty, &Ty::Int, index.span())?;
                match ty {
                    Ty::List(item) => *item,
                    other => return Err(Error::at(*span, format!("{other} cannot be indexed"))),
                }
            }
            Expr::Call { callee, args, span } => self.call(callee, args, *span)?,
            Expr::Method {
                receiver,
                name,
                args,
                span,
            } => {
                let ty = self.expr(receiver)?;
                let Some((params, ret, io, needs_mut)) = method(&ty, name) else {
                    let mut error = Error::at(*span, format!("{ty} has no method `{name}`"));
                    if matches!(
                        name.as_str(),
                        "iter" | "map" | "filter" | "collect" | "into_iter" | "for_each"
                    ) {
                        error = error.help("Citrus has no iterators or closures: loop with `for`");
                    } else if matches!(name.as_str(), "unwrap" | "expect") {
                        error = error.help("use `?` to fail with the error, or `.unwrap_or(…)`");
                    } else if let Some(close) = suggest(name, methods_of(&ty).into_iter()) {
                        error = error.help(format!("did you mean `{close}`?"));
                    }
                    return Err(error);
                };
                if io && self.pure_only() {
                    return Err(Error::at(
                        *span,
                        format!(
                            "`.{name}()` runs a program: not allowed in a constant or a `const fn`"
                        ),
                    ));
                }
                if needs_mut {
                    let mutable = matches!(&**receiver, Expr::Path(segments, _) if segments.len() == 1 && self.local(&segments[0]).is_some_and(|(_, mutable)| *mutable));
                    if !mutable {
                        return Err(Error::at(
                            *span,
                            format!(
                                "`.{name}()` changes the list: call it on a `let mut` variable"
                            ),
                        ));
                    }
                }
                self.args(&params, args, *span, &format!(".{name}()"))?;
                // `xs.push(x)` on `let mut xs = []` fixes the list's type.
                if needs_mut
                    && let Expr::Path(segments, _) = &**receiver
                    && let Some(first) = args.first()
                {
                    let item = self.expr(first)?;
                    for scope in self.scopes.iter_mut().rev() {
                        if let Some(slot) = scope.get_mut(&segments[0]) {
                            if matches!(&slot.0, Ty::List(inner) if **inner == Ty::Unknown) {
                                slot.0 = Ty::List(Box::new(item));
                            }
                            break;
                        }
                    }
                }
                ret
            }
            Expr::Unary(op, inner, span) => {
                let ty = self.expr(inner)?;
                match (*op, &ty) {
                    ("!", Ty::Bool) => Ty::Bool,
                    ("!", Ty::Cond) => Ty::Cond,
                    ("-", Ty::Int) => Ty::Int,
                    _ => return Err(Error::at(*span, format!("`{op}` does not apply to {ty}"))),
                }
            }
            Expr::Binary(op, left, right, span) => {
                let a = self.expr(left)?;
                let b = self.expr(right)?;
                match *op {
                    "&&" | "||" if a == Ty::Cond || b == Ty::Cond => {
                        for (ty, side) in [(&a, left), (&b, right)] {
                            if !matches!(ty, Ty::Cond | Ty::Bool | Ty::Unknown) {
                                return Err(Error::at(
                                    side.span(),
                                    format!("a condition combines conditions, not {ty}"),
                                ));
                            }
                        }
                        Ty::Cond
                    }
                    "&&" | "||" => {
                        expect(&a, &Ty::Bool, left.span())?;
                        expect(&b, &Ty::Bool, right.span())?;
                        Ty::Bool
                    }
                    "==" | "!=" => {
                        if unify(&a, &b).is_none() {
                            return Err(Error::at(*span, format!("cannot compare {a} with {b}")));
                        }
                        Ty::Bool
                    }
                    "<" | "<=" | ">" | ">=" => match (&a, &b) {
                        (Ty::Int, Ty::Int) | (Ty::Duration, Ty::Duration) | (Ty::Str, Ty::Str) => {
                            Ty::Bool
                        }
                        _ => return Err(Error::at(*span, format!("cannot order {a} and {b}"))),
                    },
                    "+" => match (&a, &b) {
                        (Ty::Int, Ty::Int) => Ty::Int,
                        (Ty::Duration, Ty::Duration) => Ty::Duration,
                        (Ty::Str | Ty::Path, Ty::Str | Ty::Path) => Ty::Str,
                        (Ty::List(_), Ty::List(_)) => match unify(&a, &b) {
                            Some(ty) => ty,
                            None => {
                                return Err(Error::at(*span, format!("cannot join {a} and {b}")));
                            }
                        },
                        _ => return Err(Error::at(*span, format!("cannot add {a} and {b}"))),
                    },
                    "-" if matches!((&a, &b), (Ty::List(_), Ty::List(_))) => {
                        // `paths - excluded`: the globs of the right side become exclusions.
                        let text = |ty: &Ty| matches!(ty, Ty::List(item) if matches!(**item, Ty::Glob | Ty::Str | Ty::Unknown));
                        if !text(&a) || !text(&b) {
                            return Err(Error::at(
                                *span,
                                format!("`-` removes globs from globs, not {b} from {a}"),
                            ));
                        }
                        Ty::List(Box::new(Ty::Glob))
                    }
                    _ => match (&a, &b) {
                        (Ty::Int, Ty::Int) => Ty::Int,
                        _ => {
                            return Err(Error::at(
                                *span,
                                format!("`{op}` needs two ints, found {a} and {b}"),
                            ));
                        }
                    },
                }
            }
            Expr::Try(inner, span) => {
                let ty = self.expr(inner)?;
                match (&ty, &self.ret) {
                    (Ty::Result(item), Ty::Result(_) | Ty::Unknown) => (**item).clone(),
                    (Ty::Option(item), Ty::Option(_) | Ty::Unknown) => (**item).clone(),
                    (Ty::Option(_), Ty::Result(_)) => {
                        return Err(Error::at(
                            *span,
                            "`?` on an Option needs a function that returns Option",
                        )
                        .help("turn it into a Result first: `.ok_or(\"why\")?`"));
                    }
                    (Ty::Result(_) | Ty::Option(_), ret) => {
                        return Err(Error::at(
                            *span,
                            format!("`?` returns early, but this function returns {ret}"),
                        ));
                    }
                    (other, _) => {
                        return Err(Error::at(
                            *span,
                            format!("`?` applies to Result or Option, not {other}"),
                        ));
                    }
                }
            }
            Expr::If {
                cond,
                then,
                otherwise,
                span,
            } => {
                let found = self.expr(cond)?;
                expect(&found, &Ty::Bool, cond.span())?;
                let then_ty = self.block(then)?;
                match otherwise {
                    None => {
                        if !assignable(&then_ty, &Ty::Unit) {
                            return Err(Error::at(
                                *span,
                                format!(
                                    "an `if` without `else` has no value; this branch gives {then_ty}"
                                ),
                            ));
                        }
                        Ty::Unit
                    }
                    Some(otherwise) => {
                        let else_ty = self.expr(otherwise)?;
                        unify(&then_ty, &else_ty).ok_or_else(|| {
                            Error::at(
                                *span,
                                format!("`if` branches differ: {then_ty} and {else_ty}"),
                            )
                        })?
                    }
                }
            }
            Expr::Match { value, arms, span } => {
                let ty = self.expr(value)?;
                let mut result = Ty::Unknown;
                let mut covered: Vec<String> = Vec::new();
                for (pattern, body) in arms {
                    self.scopes.push(BTreeMap::new());
                    let outcome = self
                        .pattern(pattern, &ty, &mut covered)
                        .and_then(|_| self.expr(body));
                    self.scopes.pop();
                    let found = outcome?;
                    result = unify(&result, &found).ok_or_else(|| {
                        Error::at(
                            body.span(),
                            format!("`match` arms differ: {result} and {found}"),
                        )
                    })?;
                }
                let total = covered.iter().any(|c| c == "_")
                    || (matches!(ty, Ty::Option(_))
                        && covered.contains(&"Some".into())
                        && covered.contains(&"None".into()))
                    || (matches!(ty, Ty::Result(_))
                        && covered.contains(&"Ok".into())
                        && covered.contains(&"Err".into()))
                    || (ty == Ty::Bool
                        && covered.contains(&"true".into())
                        && covered.contains(&"false".into()));
                if !total {
                    return Err(
                        Error::at(*span, "`match` does not cover every value").help("add `_ => …`")
                    );
                }
                result
            }
            Expr::Block(block) => self.block(block)?,
            Expr::Command {
                run,
                env,
                words,
                span,
            } => {
                if *run && self.pure_only() {
                    return Err(Error::at(
                        *span,
                        "`run!` runs a program: not allowed in a constant or a `const fn`",
                    ));
                }
                for word in env.iter().map(|(_, word)| word).chain(words) {
                    self.command_word(word)?;
                }
                if *run { result_unit() } else { Ty::Command }
            }
        })
    }

    fn command_word(&mut self, word: &CmdWord) -> Result<(), Error> {
        let plain = |ty: &Ty| {
            matches!(
                ty,
                Ty::Str | Ty::Path | Ty::Glob | Ty::Int | Ty::Version | Ty::Unknown
            )
        };
        // An Option is its value; a missing one fails the command (a release's `previous`).
        let text_like = |ty: &Ty| plain(ty) || matches!(ty, Ty::Option(inner) if plain(inner));
        match word {
            CmdWord::Word(pieces) => {
                for piece in pieces {
                    if let CmdPiece::Expr(expr) = piece {
                        let ty = self.expr(expr)?;
                        if !text_like(&ty) {
                            return Err(Error::at(
                                expr.span(),
                                format!("a {ty} cannot be an argument"),
                            )
                            .help(if matches!(ty, Ty::List(_)) {
                                "spread a list into arguments: `{list...}`"
                            } else {
                                "pass text, a path or a number"
                            }));
                        }
                    }
                }
            }
            CmdWord::Splat(expr) => {
                let ty = self.expr(expr)?;
                match &ty {
                    Ty::List(item) if text_like(item) => {}
                    other => {
                        return Err(Error::at(
                            expr.span(),
                            format!("`{{…...}}` spreads a list of text, not {other}"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn pattern(
        &mut self,
        pattern: &Pattern,
        ty: &Ty,
        covered: &mut Vec<String>,
    ) -> Result<(), Error> {
        match pattern {
            Pattern::Wild(_) => covered.push("_".into()),
            Pattern::Bind(name, _) => {
                covered.push("_".into());
                self.bind(name, ty.clone(), false);
            }
            Pattern::Lit(expr) => {
                let found = self.expr(expr)?;
                expect(&found, ty, expr.span())?;
                if let Expr::Bool(flag, _) = expr {
                    covered.push(flag.to_string());
                }
            }
            Pattern::Variant(name, inner, span) => {
                let bound = match (name.as_str(), ty) {
                    ("Some", Ty::Option(item)) => Some((**item).clone()),
                    ("None", Ty::Option(_)) => None,
                    ("Ok", Ty::Result(item)) => Some((**item).clone()),
                    ("Err", Ty::Result(_)) => Some(Ty::Error),
                    (_, Ty::Unknown) => Some(Ty::Unknown),
                    _ => return Err(Error::at(*span, format!("`{name}` does not match a {ty}"))),
                };
                let full = match inner.as_deref() {
                    None | Some(Pattern::Wild(_) | Pattern::Bind(..)) => true,
                    Some(_) => false,
                };
                if full {
                    covered.push(name.clone());
                }
                match (inner, bound) {
                    (Some(inner), Some(bound)) => self.pattern(inner, &bound, &mut Vec::new())?,
                    (None, None) => {}
                    (Some(_), None) => return Err(Error::at(*span, "`None` holds no value")),
                    (None, Some(_)) => {
                        return Err(Error::at(*span, format!("`{name}(…)` holds a value")));
                    }
                }
            }
        }
        Ok(())
    }

    fn args(&mut self, params: &[Ty], args: &[Expr], span: Span, what: &str) -> Result<(), Error> {
        if params.len() != args.len() {
            return Err(Error::at(
                span,
                format!(
                    "{what} takes {} argument(s), given {}",
                    params.len(),
                    args.len()
                ),
            ));
        }
        for (param, arg) in params.iter().zip(args) {
            let found = self.expr(arg)?;
            expect(&found, param, arg.span())?;
        }
        Ok(())
    }

    fn call(&mut self, callee: &Expr, args: &[Expr], span: Span) -> Result<Ty, Error> {
        let Expr::Path(segments, path_span) = callee else {
            return Err(Error::at(
                callee.span(),
                "only named functions can be called",
            ));
        };
        let name = segments.join("::");
        // Plan conditions: their argument names a group or a check, or holds globs.
        if segments.len() == 1 && CONDITION_FNS.contains(&name.as_str()) {
            let [arg] = args else {
                return Err(Error::at(span, format!("`{name}` takes one argument")));
            };
            let is_item = matches!(arg, Expr::Path(segments, _) if self.globals.items.contains_key(&segments.join("::")));
            if !is_item {
                let ty = self.expr(arg)?;
                let fits = match name.as_str() {
                    "signal" => matches!(ty, Ty::Str),
                    "touched" | "only" | "without" => {
                        matches!(ty, Ty::Str | Ty::Glob | Ty::Unknown)
                            || assignable(&ty, &Ty::List(Box::new(Ty::Glob)))
                    }
                    _ => matches!(ty, Ty::Str),
                };
                if !fits {
                    return Err(Error::at(
                        arg.span(),
                        format!("`{name}` takes a group or check name or globs, not {ty}"),
                    ));
                }
            }
            return Ok(Ty::Cond);
        }
        match name.as_str() {
            "Some" | "Ok" => {
                if args.len() != 1 {
                    return Err(Error::at(span, format!("`{name}` takes one value")));
                }
                let inner = Box::new(self.expr(&args[0])?);
                return Ok(if name == "Some" {
                    Ty::Option(inner)
                } else {
                    Ty::Result(inner)
                });
            }
            "Err" => {
                self.args(&[Ty::Str], args, span, "Err")?;
                return Ok(Ty::Result(Box::new(Ty::Unknown)));
            }
            _ => {}
        }
        // The files a package is built from, read when the file loads.
        if matches!(
            name.as_str(),
            "std::paths::cargo" | "std::paths::next" | "std::paths::package"
        ) {
            if args.is_empty() {
                return Err(Error::at(span, format!("`{name}` takes package names")));
            }
            for arg in args {
                let ty = self.expr(arg)?;
                expect(&ty, &Ty::Str, arg.span())?;
            }
            return Ok(Ty::List(Box::new(Ty::Glob)));
        }
        if segments[0] == "std" {
            let Some(found) = std_fn(segments) else {
                let mut error = Error::at(*path_span, format!("unknown function `{name}`"));
                if let Some(close) = suggest(&name, STD_FUNCTIONS.iter().copied()) {
                    error = error.help(format!("did you mean `{close}`?"));
                }
                return Err(error);
            };
            if found.io && self.pure_only() {
                return Err(Error::at(
                    span,
                    format!(
                        "`{name}` touches files or programs: not allowed in a constant or a `const fn`"
                    ),
                ));
            }
            self.args(&found.params, args, span, &name)?;
            return Ok(found.ret);
        }
        if segments.len() == 1
            && let Some(sig) = self.globals.fns.get(&name)
        {
            if self.pure_only() && !sig.is_const {
                return Err(Error::at(
                    span,
                    format!(
                        "`{name}` is not a `const fn`: it cannot run while the file loads or the plan is made"
                    ),
                ));
            }
            let (params, ret) = (sig.params.clone(), sig.ret.clone());
            self.args(&params, args, span, &name)?;
            return Ok(ret);
        }
        let mut error = Error::at(*path_span, format!("unknown function `{name}`"));
        if let Some(close) = suggest(&name, self.globals.fns.keys().map(String::as_str)) {
            error = error.help(format!("did you mean `{close}`?"));
        } else if name.contains("::") {
            error = error.help("built-in functions live in `std::` (citrus std)");
        }
        Err(error)
    }
}
