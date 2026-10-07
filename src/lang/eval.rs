//! Evaluation of `.ci` files into declarations. No side effects: the only
//! reads are `use`d files and declared inputs (`glob`), and nothing runs.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::rc::Rc;

use super::ast::{Body, Expr, Item, StrPart};
use super::parser::parse_file;
use super::{Error, Sources, Span, suggest};

#[derive(Clone)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    /// Seconds.
    Duration(u64),
    Str(String),
    List(Vec<Value>),
    Map(Vec<(String, Value)>),
    Func(Rc<Closure>),
    Builtin(&'static str),
    Action(Rc<Action>),
}

/// Work for Citrus to do: a builtin name, its arguments and where it was written.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Action {
    pub kind: String,
    pub args: Vec<Value>,
    pub named: Vec<(String, Value)>,
    pub span: Span,
}

pub struct Closure {
    params: Vec<(String, Option<Expr>)>,
    body: Body,
    scope: Scope,
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::None => write!(f, "none"),
            Value::Bool(value) => write!(f, "{value}"),
            Value::Int(value) => write!(f, "{value}"),
            Value::Duration(value) => write!(f, "{value}s"),
            Value::Str(value) => write!(f, "{value:?}"),
            Value::List(items) => f.debug_list().entries(items).finish(),
            Value::Map(entries) => f
                .debug_map()
                .entries(entries.iter().map(|(key, value)| (key, value)))
                .finish(),
            Value::Func(_) => write!(f, "<function>"),
            Value::Builtin(name) => write!(f, "<builtin {name}>"),
            Value::Action(action) => write!(f, "{}({:?})", action.kind, action.args),
        }
    }
}

impl serde::Serialize for Value {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::None => serializer.serialize_none(),
            Value::Bool(value) => serializer.serialize_bool(*value),
            Value::Int(value) => serializer.serialize_i64(*value),
            Value::Duration(value) => serializer.serialize_u64(*value),
            Value::Str(value) => serializer.serialize_str(value),
            Value::List(items) => items.serialize(serializer),
            Value::Map(entries) => entries
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
                .serialize(serializer),
            Value::Action(action) => action.serialize(serializer),
            other => serializer.serialize_str(&format!("{other:?}")),
        }
    }
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "none",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Duration(_) => "duration",
            Value::Str(_) => "string",
            Value::List(_) => "list",
            Value::Map(_) => "map",
            Value::Func(_) | Value::Builtin(_) => "function",
            Value::Action(_) => "action",
        }
    }

    fn truthy(&self) -> bool {
        match self {
            Value::None => false,
            Value::Bool(value) => *value,
            Value::Int(value) => *value != 0,
            Value::Str(value) => !value.is_empty(),
            Value::List(items) => !items.is_empty(),
            Value::Map(entries) => !entries.is_empty(),
            _ => true,
        }
    }

    fn display(&self) -> String {
        match self {
            Value::Str(value) => value.clone(),
            Value::Int(value) => value.to_string(),
            Value::Bool(value) => value.to_string(),
            Value::Duration(value) => format!("{value}s"),
            Value::None => String::new(),
            other => format!("{other:?}"),
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        if let Value::Str(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::None, Value::None) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Duration(a), Value::Duration(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::List(a), Value::List(b)) => a == b,
            (Value::Map(a), Value::Map(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.iter().any(|(other, item)| other == key && item == value)
                    })
            }
            _ => false,
        }
    }
}

/// A declaration produced by evaluation: `kind "name" { fields; children }`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Decl {
    pub kind: String,
    pub name: Option<String>,
    pub fields: Vec<(String, Value, Span)>,
    pub children: Vec<Decl>,
    pub span: Span,
    /// Loop bindings that produced this declaration, e.g. `name = "alerts"`.
    pub instance: Vec<(String, String)>,
}

impl Decl {
    pub fn field(&self, name: &str) -> Option<&Value> {
        self.fields
            .iter()
            .find(|(field, ..)| field == name)
            .map(|(_, value, _)| value)
    }

    pub fn field_span(&self, name: &str) -> Span {
        self.fields
            .iter()
            .find(|(field, ..)| field == name)
            .map_or(self.span, |(_, _, span)| *span)
    }
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Graph {
    pub decls: Vec<Decl>,
}

/// Lexical scope: a chain of frames.
#[derive(Clone, Default)]
pub struct Scope {
    frames: Vec<Rc<std::cell::RefCell<BTreeMap<String, Value>>>>,
}

impl Scope {
    fn child(&self) -> Scope {
        let mut frames = self.frames.clone();
        frames.push(Rc::default());
        Scope { frames }
    }

    fn get(&self, name: &str) -> Option<Value> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.borrow().get(name).cloned())
    }

    fn set(&self, name: &str, value: Value) {
        if let Some(frame) = self.frames.last() {
            frame.borrow_mut().insert(name.to_owned(), value);
        }
    }

    fn names(&self) -> Vec<String> {
        self.frames
            .iter()
            .flat_map(|frame| frame.borrow().keys().cloned().collect::<Vec<_>>())
            .collect()
    }
}

/// Builtins that describe work (actions); `namespace.name` for grouped ones.
pub const ACTIONS: &[&str] = &[
    "run",
    "make",
    "sh",
    "check",
    "copy",
    "archive",
    "http",
    "job",
    "lease",
    "annotation",
    "docker",
    "kubectl",
    "kubernetes",
    "github.release",
    "checksums",
    "cargo.test",
    "cargo.build",
    "cargo.fmt",
    "cargo.clippy",
    "compose.up",
    "compose.down",
    "wait.tcp",
    "wait.http",
    "wait.file",
    "links.check",
    "external",
    "arg",
    "progress",
    "after",
    "remote",
    "secret",
    "env",
    "rust_closure",
    "inputs_of",
];
/// Pure helpers evaluated immediately.
const FUNCTIONS: &[&str] = &[
    "len",
    "str",
    "keys",
    "values",
    "range",
    "glob",
    "cargo.closure",
];
/// Values Citrus fills in while running: `before` (the commit before a merge),
/// `version`, `next`, `previous` (releases), `release`, `short`, `commit`.
pub const PLACEHOLDERS: &[&str] = &[
    "before", "version", "next", "previous", "unit", "release", "short", "commit", "artifact",
    "key", "tag",
];
/// Named constants.
const CONSTANTS: &[&str] = &[
    "local",
    "linux",
    "required",
    "proven",
    "rolling",
    "recreate",
    "deployment",
    "cronjob",
];

fn globals() -> Scope {
    let scope = Scope::default().child();
    let mut namespaces: BTreeMap<&str, Vec<(String, Value)>> = BTreeMap::new();
    for name in ACTIONS.iter().chain(FUNCTIONS) {
        match name.split_once('.') {
            Some((space, member)) => namespaces
                .entry(space)
                .or_default()
                .push((member.to_owned(), Value::Builtin(name))),
            None => scope.set(name, Value::Builtin(name)),
        }
    }
    for (space, members) in namespaces {
        scope.set(space, Value::Map(members));
    }
    for constant in CONSTANTS {
        scope.set(constant, Value::Str((*constant).to_owned()));
    }
    // Values known only while Citrus runs; actions receive them as placeholders.
    for name in PLACEHOLDERS {
        scope.set(name, Value::Str(format!("{{{name}}}")));
    }
    scope
}

/// Repository files as the evaluator sees them, for builtins that read manifests.
struct RepositoryFiles<'a> {
    root: &'a Path,
    revision: Option<&'a str>,
    list: Vec<String>,
}

impl super::cargo::Files for RepositoryFiles<'_> {
    fn read(&self, path: &str) -> Option<String> {
        match self.revision {
            None => std::fs::read_to_string(self.root.join(path)).ok(),
            Some(revision) => {
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(self.root)
                    .args(["show", &format!("{revision}:{path}")])
                    .output()
                    .ok()?;
                output
                    .status
                    .success()
                    .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
            }
        }
    }

    fn list(&self) -> &[String] {
        &self.list
    }
}

pub struct Evaluator<'a> {
    root: &'a Path,
    sources: &'a mut Sources,
    loaded: Vec<String>,
    files: Option<Vec<String>>,
    /// Read files as committed at this revision instead of the working tree.
    revision: Option<&'a str>,
}

pub fn evaluate_project(
    root: &Path,
    entry: &str,
    revision: Option<&str>,
    sources: &mut Sources,
) -> Result<Graph, Error> {
    let mut evaluator = Evaluator {
        root,
        sources,
        loaded: Vec::new(),
        files: None,
        revision,
    };
    let mut graph = Graph::default();
    let scope = globals();
    evaluator.file(entry, &scope, &mut graph.decls, Span::default())?;
    Ok(graph)
}

impl Evaluator<'_> {
    fn file(
        &mut self,
        relative: &str,
        scope: &Scope,
        out: &mut Vec<Decl>,
        at: Span,
    ) -> Result<(), Error> {
        if relative.starts_with('/') || relative.split('/').any(|part| part == "..") {
            return Err(Error::at(
                at,
                format!("`use` path must stay inside the repository: {relative}"),
            ));
        }
        if self.loaded.iter().any(|loaded| loaded == relative) {
            return Ok(());
        }
        self.loaded.push(relative.to_owned());
        let text = match self.revision {
            None => std::fs::read_to_string(self.root.join(relative))
                .map_err(|error| Error::at(at, format!("cannot read {relative}: {error}")))?,
            Some(revision) => self
                .git(&["show", &format!("{revision}:{relative}")])
                .ok_or_else(|| Error::at(at, format!("cannot read {relative} at {revision}")))?,
        };
        let id = self
            .sources
            .add(Path::new(relative).to_path_buf(), text.clone());
        let file = parse_file(id, &text)?;
        self.items(&file.items, scope, out, &[], true)
    }

    fn items(
        &mut self,
        items: &[Item],
        scope: &Scope,
        out: &mut Vec<Decl>,
        instance: &[(String, String)],
        top: bool,
    ) -> Result<(), Error> {
        for item in items {
            match item {
                Item::Use { path, span } => self.file(path, scope, out, *span)?,
                Item::Let { name, value, .. } => {
                    let value = self.expr(value, scope)?;
                    scope.set(name, value);
                }
                Item::Fn {
                    name, params, body, ..
                } => {
                    scope.set(
                        name,
                        Value::Func(Rc::new(Closure {
                            params: params.clone(),
                            body: body.clone(),
                            scope: scope.clone(),
                        })),
                    );
                }
                Item::Field { name, value, span } => {
                    if top {
                        return Err(Error::at(
                            *span,
                            format!("`{name} = …` belongs inside a block"),
                        )
                        .help(format!("put it in `project {{ {name} = … }}`")));
                    }
                    // Fields of the enclosing block are collected by `block`.
                    let _ = (name, value);
                }
                Item::Block {
                    kind,
                    label,
                    items: body,
                    span,
                } => {
                    let decl = self.block(kind, label.as_ref(), body, scope, instance, *span)?;
                    out.push(decl);
                }
                Item::For {
                    var,
                    iter,
                    items: body,
                    span,
                } => {
                    let Value::List(values) = self.expr(iter, scope)? else {
                        return Err(Error::at(iter.span(), "`for` needs a list")
                            .help("e.g. `for name in [\"api\", \"web\"] { … }`"));
                    };
                    for value in values {
                        let inner = scope.child();
                        let mut bound = instance.to_vec();
                        bound.push((var.clone(), value.display()));
                        inner.set(var, value);
                        let _ = span;
                        self.items(body, &inner, out, &bound, top)?;
                    }
                }
                Item::If {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
                    let branch = if self.expr(cond, scope)?.truthy() {
                        then
                    } else {
                        otherwise
                    };
                    self.items(branch, &scope.child(), out, instance, top)?;
                }
            }
        }
        Ok(())
    }

    fn block(
        &mut self,
        kind: &str,
        label: Option<&Expr>,
        body: &[Item],
        scope: &Scope,
        instance: &[(String, String)],
        span: Span,
    ) -> Result<Decl, Error> {
        let name = match label {
            Some(expr) => match self.expr(expr, scope)? {
                Value::Str(name) => Some(name),
                other => {
                    return Err(Error::at(
                        expr.span(),
                        format!("a block label must be a string, not {}", other.type_name()),
                    ));
                }
            },
            None => None,
        };
        let inner = scope.child();
        let mut decl = Decl {
            kind: kind.to_owned(),
            name,
            fields: Vec::new(),
            children: Vec::new(),
            span,
            instance: instance.to_vec(),
        };
        self.block_items(body, &inner, &mut decl, instance)?;
        Ok(decl)
    }

    fn block_items(
        &mut self,
        body: &[Item],
        scope: &Scope,
        decl: &mut Decl,
        instance: &[(String, String)],
    ) -> Result<(), Error> {
        for item in body {
            match item {
                Item::Field { name, value, span } => {
                    if decl.fields.iter().any(|(field, ..)| field == name) {
                        return Err(Error::at(
                            *span,
                            format!("`{name}` is set twice in this block"),
                        ));
                    }
                    let value = self.expr(value, scope)?;
                    decl.fields.push((name.clone(), value, *span));
                }
                Item::For {
                    var, iter, items, ..
                } => {
                    let Value::List(values) = self.expr(iter, scope)? else {
                        return Err(Error::at(iter.span(), "`for` needs a list"));
                    };
                    for value in values {
                        let inner = scope.child();
                        let mut bound = instance.to_vec();
                        bound.push((var.clone(), value.display()));
                        inner.set(var, value);
                        self.block_items(items, &inner, decl, &bound)?;
                    }
                }
                Item::If {
                    cond,
                    then,
                    otherwise,
                    ..
                } => {
                    let branch = if self.expr(cond, scope)?.truthy() {
                        then
                    } else {
                        otherwise
                    };
                    self.block_items(branch, &scope.child(), decl, instance)?;
                }
                Item::Use { span, .. } => {
                    return Err(Error::at(*span, "`use` belongs at the top of a file"));
                }
                other => {
                    let mut children = Vec::new();
                    self.items(
                        std::slice::from_ref(other),
                        scope,
                        &mut children,
                        instance,
                        false,
                    )?;
                    decl.children.extend(children);
                }
            }
        }
        Ok(())
    }

    fn body(&mut self, body: &Body, scope: &Scope) -> Result<Value, Error> {
        let inner = scope.child();
        for (name, value, _) in &body.lets {
            let value = self.expr(value, &inner)?;
            inner.set(name, value);
        }
        self.expr(&body.value, &inner)
    }

    pub fn expr(&mut self, expr: &Expr, scope: &Scope) -> Result<Value, Error> {
        Ok(match expr {
            Expr::None(_) => Value::None,
            Expr::Bool(value, _) => Value::Bool(*value),
            Expr::Int(value, _) => Value::Int(*value),
            Expr::Duration(value, _) => Value::Duration(*value),
            Expr::Str(parts, _) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        StrPart::Lit(literal) => text.push_str(literal),
                        StrPart::Expr(inner) => {
                            let value = self.expr(inner, scope)?;
                            if matches!(
                                value,
                                Value::List(_)
                                    | Value::Map(_)
                                    | Value::Func(_)
                                    | Value::Builtin(_)
                                    | Value::Action(_)
                            ) {
                                return Err(Error::at(
                                    inner.span(),
                                    format!("cannot put a {} inside a string", value.type_name()),
                                ));
                            }
                            text.push_str(&value.display());
                        }
                    }
                }
                Value::Str(text)
            }
            Expr::Name(name, span) => scope.get(name).ok_or_else(|| {
                let names = scope.names();
                let error = Error::at(*span, format!("unknown name `{name}`"));
                match suggest(name, names.iter().map(String::as_str)) {
                    Some(close) => error.help(format!("did you mean `{close}`?")),
                    None => error,
                }
            })?,
            Expr::List(items, _) => Value::List(
                items
                    .iter()
                    .map(|item| self.expr(item, scope))
                    .collect::<Result<_, _>>()?,
            ),
            Expr::Map(entries, _) => Value::Map(
                entries
                    .iter()
                    .map(|(key, value)| Ok((key.clone(), self.expr(value, scope)?)))
                    .collect::<Result<_, Error>>()?,
            ),
            Expr::Comprehension {
                value,
                var,
                iter,
                cond,
                ..
            } => {
                let Value::List(items) = self.expr(iter, scope)? else {
                    return Err(Error::at(iter.span(), "`for` needs a list"));
                };
                let mut out = Vec::new();
                for item in items {
                    let inner = scope.child();
                    inner.set(var, item);
                    if let Some(cond) = cond
                        && !self.expr(cond, &inner)?.truthy()
                    {
                        continue;
                    }
                    out.push(self.expr(value, &inner)?);
                }
                Value::List(out)
            }
            Expr::Field(base, field, span) => {
                let base_value = self.expr(base, scope)?;
                match &base_value {
                    Value::Map(entries) => match entries.iter().find(|(key, _)| key == field) {
                        Some((_, value)) => value.clone(),
                        None => {
                            let error = Error::at(*span, format!("no `{field}` here"));
                            return Err(
                                match suggest(field, entries.iter().map(|(key, _)| key.as_str())) {
                                    Some(close) => error.help(format!("did you mean `{close}`?")),
                                    None => error,
                                },
                            );
                        }
                    },
                    _ => Value::Map(vec![
                        ("__method".into(), Value::Str(field.clone())),
                        ("__self".into(), base_value.clone()),
                    ]),
                }
            }
            Expr::Index(base, index, span) => {
                let base = self.expr(base, scope)?;
                let index = self.expr(index, scope)?;
                match (&base, &index) {
                    (Value::List(items), Value::Int(position)) => {
                        let position = if *position < 0 {
                            items.len() as i64 + position
                        } else {
                            *position
                        };
                        items.get(position as usize).cloned().ok_or_else(|| {
                            Error::at(
                                *span,
                                format!("index {position} is outside a list of {}", items.len()),
                            )
                        })?
                    }
                    (Value::Map(entries), Value::Str(key)) => entries
                        .iter()
                        .find(|(name, _)| name == key)
                        .map(|(_, value)| value.clone())
                        .ok_or_else(|| Error::at(*span, format!("no key {key:?}")))?,
                    _ => {
                        return Err(Error::at(
                            *span,
                            format!(
                                "cannot index a {} with a {}",
                                base.type_name(),
                                index.type_name()
                            ),
                        ));
                    }
                }
            }
            Expr::Call { callee, args, span } => {
                let function = self.expr(callee, scope)?;
                let mut positional = Vec::new();
                let mut named = Vec::new();
                for (name, value) in args {
                    let value = self.expr(value, scope)?;
                    match name {
                        Some(name) => named.push((name.clone(), value)),
                        None => positional.push(value),
                    }
                }
                self.call(function, positional, named, *span)?
            }
            Expr::Unary(op, value, span) => {
                let value = self.expr(value, scope)?;
                match (*op, &value) {
                    ("not", value) => Value::Bool(!value.truthy()),
                    ("-", Value::Int(number)) => Value::Int(-number),
                    _ => {
                        return Err(Error::at(
                            *span,
                            format!("cannot apply `{op}` to a {}", value.type_name()),
                        ));
                    }
                }
            }
            Expr::Binary(op, left, right, span) => {
                let left_value = self.expr(left, scope)?;
                if *op == "and" && !left_value.truthy() {
                    return Ok(Value::Bool(false));
                }
                if *op == "or" && left_value.truthy() {
                    return Ok(left_value);
                }
                if *op == "??" && !matches!(left_value, Value::None) {
                    return Ok(left_value);
                }
                let right_value = self.expr(right, scope)?;
                binary(op, left_value, right_value, *span)?
            }
            Expr::If {
                cond,
                then,
                otherwise,
                ..
            } => {
                if self.expr(cond, scope)?.truthy() {
                    self.body(then, scope)?
                } else {
                    self.body(otherwise, scope)?
                }
            }
        })
    }

    fn call(
        &mut self,
        function: Value,
        args: Vec<Value>,
        named: Vec<(String, Value)>,
        span: Span,
    ) -> Result<Value, Error> {
        match function {
            Value::Func(closure) => {
                let inner = closure.scope.child();
                if args.len() > closure.params.len() {
                    return Err(Error::at(
                        span,
                        format!(
                            "this function takes {} arguments, given {}",
                            closure.params.len(),
                            args.len()
                        ),
                    ));
                }
                for (index, (param, default)) in closure.params.iter().enumerate() {
                    let value = if let Some(value) = args.get(index) {
                        value.clone()
                    } else if let Some((_, value)) = named.iter().find(|(name, _)| name == param) {
                        value.clone()
                    } else if let Some(default) = default {
                        self.expr(default, &closure.scope)?
                    } else {
                        return Err(Error::at(span, format!("missing argument `{param}`")));
                    };
                    inner.set(param, value);
                }
                if let Some((name, _)) = named
                    .iter()
                    .find(|(name, _)| !closure.params.iter().any(|(param, _)| param == name))
                {
                    return Err(Error::at(span, format!("unknown argument `{name}`")));
                }
                let closure = closure.clone();
                self.body(&closure.body, &inner)
            }
            Value::Builtin(name) if FUNCTIONS.contains(&name) => self.function(name, args, span),
            Value::Builtin(name) => Ok(Value::Action(Rc::new(Action {
                kind: name.to_owned(),
                args,
                named,
                span,
            }))),
            Value::Map(entries) if entries.iter().any(|(key, _)| key == "__method") => {
                let method = entries
                    .iter()
                    .find(|(key, _)| key == "__method")
                    .and_then(|(_, value)| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                let target = entries
                    .iter()
                    .find(|(key, _)| key == "__self")
                    .map(|(_, value)| value.clone())
                    .unwrap_or(Value::None);
                method_call(&method, target, args, span)
            }
            other => Err(Error::at(
                span,
                format!("a {} cannot be called", other.type_name()),
            )),
        }
    }

    fn function(&mut self, name: &str, args: Vec<Value>, span: Span) -> Result<Value, Error> {
        let one = || {
            args.first()
                .cloned()
                .ok_or_else(|| Error::at(span, format!("`{name}` needs an argument")))
        };
        Ok(match name {
            "len" => match one()? {
                Value::Str(text) => Value::Int(text.chars().count() as i64),
                Value::List(items) => Value::Int(items.len() as i64),
                Value::Map(entries) => Value::Int(entries.len() as i64),
                other => return Err(Error::at(span, format!("`len` of a {}", other.type_name()))),
            },
            "str" => Value::Str(one()?.display()),
            "keys" => match one()? {
                Value::Map(entries) => Value::List(
                    entries
                        .into_iter()
                        .map(|(key, _)| Value::Str(key))
                        .collect(),
                ),
                other => {
                    return Err(Error::at(
                        span,
                        format!("`keys` of a {}", other.type_name()),
                    ));
                }
            },
            "values" => match one()? {
                Value::Map(entries) => {
                    Value::List(entries.into_iter().map(|(_, value)| value).collect())
                }
                other => {
                    return Err(Error::at(
                        span,
                        format!("`values` of a {}", other.type_name()),
                    ));
                }
            },
            "range" => match one()? {
                Value::Int(end) => Value::List((0..end).map(Value::Int).collect()),
                other => {
                    return Err(Error::at(
                        span,
                        format!("`range` of a {}", other.type_name()),
                    ));
                }
            },
            "glob" => {
                let Value::Str(pattern) = one()? else {
                    return Err(Error::at(span, "`glob` needs a pattern string"));
                };
                let files = self.repository_files();
                let mut matched: Vec<Value> = files
                    .iter()
                    .filter(|path| {
                        crate::manifest::pattern_matches_any(&pattern, std::slice::from_ref(*path))
                            .unwrap_or(false)
                    })
                    .map(|path| Value::Str(path.clone()))
                    .collect();
                matched.sort_by_key(|value| value.display());
                Value::List(matched)
            }
            "cargo.closure" => {
                let Value::Str(package) = one()? else {
                    return Err(Error::at(span, "`cargo.closure` needs a package name"));
                };
                let files = RepositoryFiles {
                    root: self.root,
                    revision: self.revision,
                    list: self.repository_files().clone(),
                };
                let globs = super::cargo::closure(&files, &package)
                    .map_err(|error| Error::at(span, error))?;
                Value::List(globs.into_iter().map(Value::Str).collect())
            }
            _ => return Err(Error::at(span, format!("unknown function `{name}`"))),
        })
    }

    fn git(&self, args: &[&str]) -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(self.root)
            .args(args)
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn repository_files(&mut self) -> &Vec<String> {
        if self.files.is_none() {
            let listing = match self.revision {
                None => self.git(&["ls-files", "--cached", "--others", "--exclude-standard"]),
                Some(revision) => self.git(&["ls-tree", "-r", "--name-only", revision]),
            };
            let output = listing
                .map(|text| text.lines().map(str::to_owned).collect())
                .unwrap_or_default();
            self.files = Some(output);
        }
        self.files.get_or_insert_with(Vec::new)
    }
}

fn binary(op: &str, left: Value, right: Value, span: Span) -> Result<Value, Error> {
    use Value::*;
    let mismatch = |left: &Value, right: &Value| {
        Error::at(
            span,
            format!(
                "cannot use `{op}` with a {} and a {}",
                left.type_name(),
                right.type_name()
            ),
        )
    };
    Ok(match (op, &left, &right) {
        ("==", _, _) => Bool(left == right),
        ("!=", _, _) => Bool(left != right),
        ("??", _, _) => right,
        ("and", _, _) => Bool(right.truthy()),
        ("or", _, _) => right,
        ("in", _, List(items)) => Bool(items.contains(&left)),
        ("in", Str(needle), Str(hay)) => Bool(hay.contains(needle.as_str())),
        ("in", Str(key), Map(entries)) => Bool(entries.iter().any(|(name, _)| name == key)),
        ("+", Int(a), Int(b)) => Int(a + b),
        ("-", Int(a), Int(b)) => Int(a - b),
        ("*", Int(a), Int(b)) => Int(a * b),
        ("/", Int(_), Int(0)) | ("%", Int(_), Int(0)) => {
            return Err(Error::at(span, "division by zero"));
        }
        ("/", Int(a), Int(b)) => Int(a / b),
        ("%", Int(a), Int(b)) => Int(a % b),
        ("+", Duration(a), Duration(b)) => Duration(a + b),
        ("*", Duration(a), Int(b)) => Duration(a * (*b).max(0) as u64),
        ("+", Str(a), Str(b)) => Str(format!("{a}{b}")),
        ("+", List(a), List(b)) => List(a.iter().chain(b).cloned().collect()),
        ("+", Map(a), Map(b)) => {
            let mut merged = a.clone();
            for (key, value) in b {
                merged.retain(|(name, _)| name != key);
                merged.push((key.clone(), value.clone()));
            }
            Map(merged)
        }
        ("<", Int(a), Int(b)) => Bool(a < b),
        ("<=", Int(a), Int(b)) => Bool(a <= b),
        (">", Int(a), Int(b)) => Bool(a > b),
        (">=", Int(a), Int(b)) => Bool(a >= b),
        ("<", Str(a), Str(b)) => Bool(a < b),
        (">", Str(a), Str(b)) => Bool(a > b),
        _ => return Err(mismatch(&left, &right)),
    })
}

fn method_call(method: &str, target: Value, args: Vec<Value>, span: Span) -> Result<Value, Error> {
    let text_arg = |index: usize| {
        args.get(index)
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| Error::at(span, format!("`.{method}` needs a string argument")))
    };
    Ok(match (method, &target) {
        ("join", Value::List(items)) => Value::Str(
            items
                .iter()
                .map(Value::display)
                .collect::<Vec<_>>()
                .join(&text_arg(0)?),
        ),
        ("contains", Value::List(items)) => {
            Value::Bool(items.contains(args.first().unwrap_or(&Value::None)))
        }
        ("contains", Value::Str(text)) => Value::Bool(text.contains(text_arg(0)?.as_str())),
        ("starts_with", Value::Str(text)) => Value::Bool(text.starts_with(text_arg(0)?.as_str())),
        ("ends_with", Value::Str(text)) => Value::Bool(text.ends_with(text_arg(0)?.as_str())),
        ("replace", Value::Str(text)) => {
            Value::Str(text.replace(text_arg(0)?.as_str(), &text_arg(1)?))
        }
        ("upper", Value::Str(text)) => Value::Str(text.to_uppercase()),
        ("lower", Value::Str(text)) => Value::Str(text.to_lowercase()),
        ("split", Value::Str(text)) => Value::List(
            text.split(text_arg(0)?.as_str())
                .map(|part| Value::Str(part.to_owned()))
                .collect(),
        ),
        ("trim", Value::Str(text)) => Value::Str(text.trim().to_owned()),
        _ => {
            return Err(Error::at(
                span,
                format!("a {} has no method `.{method}`", target.type_name()),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(source: &str) -> Result<Graph, String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("citrus.ci"), source).unwrap();
        let mut sources = Sources::default();
        evaluate_project(dir.path(), "citrus.ci", None, &mut sources)
            .map_err(|error| sources.render(&error))
    }

    #[test]
    fn loops_functions_and_interpolation_build_declarations() {
        let graph = evaluate(
            "citrus 1\nlet products = [\"api\", \"web\"]\nfn owned(name) { [\"src/{name}/**\"] + [\"Cargo.lock\"] }\n\
             for name in products {\n  check \"test-{name}\" {\n    owns = owned(name)\n    run = make(\"test-{name}\", JOBS: 2)\n    cache = name != \"web\"\n  }\n}\n",
        )
        .unwrap();
        assert_eq!(graph.decls.len(), 2);
        let web = &graph.decls[1];
        assert_eq!(web.name.as_deref(), Some("test-web"));
        assert_eq!(web.instance, vec![("name".to_owned(), "web".to_owned())]);
        assert_eq!(web.field("cache"), Some(&Value::Bool(false)));
        assert_eq!(
            web.field("owns"),
            Some(&Value::List(vec![
                Value::Str("src/web/**".into()),
                Value::Str("Cargo.lock".into())
            ]))
        );
        let Some(Value::Action(action)) = web.field("run") else {
            panic!()
        };
        assert_eq!(action.kind, "make");
        assert_eq!(action.named[0].0, "JOBS");
    }

    #[test]
    fn expressions_behave_like_their_obvious_reading() {
        let graph = evaluate(
            "citrus 1\nlet xs = [x * 2 for x in range(5) if x % 2 == 0]\nlet m = { a: 1 } + { b: 2 }\n\
             project { doubled = xs, keys = keys(m), joined = [\"a\", \"b\"].join(\"-\"), pick = none ?? \"default\", t = 2m + 30s,\n\
               label = if len(xs) > 2 { \"many\" } else { \"few\" } }\n",
        )
        .unwrap();
        let project = &graph.decls[0];
        assert_eq!(
            project.field("doubled"),
            Some(&Value::List(vec![
                Value::Int(0),
                Value::Int(4),
                Value::Int(8)
            ]))
        );
        assert_eq!(project.field("joined"), Some(&Value::Str("a-b".into())));
        assert_eq!(project.field("pick"), Some(&Value::Str("default".into())));
        assert_eq!(project.field("t"), Some(&Value::Duration(150)));
        assert_eq!(project.field("label"), Some(&Value::Str("many".into())));
    }

    #[test]
    fn errors_point_at_the_source_with_a_hint() {
        let rendered = evaluate("citrus 1\ncheck \"a\" {\n  run = mak(\"t\")\n}\n").unwrap_err();
        assert!(rendered.contains("unknown name `mak`"), "{rendered}");
        assert!(rendered.contains("citrus.ci:3:9"), "{rendered}");
        assert!(rendered.contains("did you mean `make`?"), "{rendered}");
        assert!(rendered.contains("^^^"), "{rendered}");
        let rendered =
            evaluate("citrus 1\ncheck \"a\" { cache = true, cache = false }\n").unwrap_err();
        assert!(rendered.contains("set twice"), "{rendered}");
    }
}
