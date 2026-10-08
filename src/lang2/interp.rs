//! Evaluation of language v2: constants when the file loads, bodies when
//! a check, task or step runs. Values are copied; nothing is shared.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::rc::Rc;

use super::ast::*;
use crate::lang::compile::{Step as WorkStep, Work};
use crate::lang::{Sources, Span};

/// An error a body ends with: its message, where, and what it happened in.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub message: String,
    pub span: Span,
    pub context: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Str(Rc<str>),
    Path(Rc<str>),
    Glob(Rc<str>),
    Duration(u64),
    Version(Rc<str>),
    List(Rc<Vec<Value>>),
    Some(Box<Value>),
    None,
    Ok(Box<Value>),
    Err(Rc<Failure>),
    /// An `Error` bound by `Err(e)`.
    Error(Rc<Failure>),
    Struct(Rc<str>, Rc<BTreeMap<String, Value>>),
    Command(Rc<CommandSpec>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub dir: Option<String>,
}

impl CommandSpec {
    fn label(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Value {
    fn text(&self) -> String {
        match self {
            Value::Unit => "()".into(),
            Value::Bool(flag) => flag.to_string(),
            Value::Int(number) => number.to_string(),
            Value::Str(text) | Value::Path(text) | Value::Glob(text) | Value::Version(text) => {
                text.to_string()
            }
            Value::Duration(seconds) => format!("{seconds}s"),
            Value::List(items) => format!(
                "[{}]",
                items.iter().map(Value::text).collect::<Vec<_>>().join(", ")
            ),
            Value::Some(inner) => format!("Some({})", inner.text()),
            Value::None => "None".into(),
            Value::Ok(inner) => format!("Ok({})", inner.text()),
            Value::Err(failure) => format!("Err({})", failure.message),
            Value::Error(failure) => failure.message.clone(),
            Value::Struct(name, fields) => format!(
                "{name} {{ {} }}",
                fields
                    .iter()
                    .map(|(k, v)| format!("{k}: {}", v.text()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Command(spec) => spec.label(),
        }
    }

    /// Text of a value; a list joins its items.
    pub fn as_text(&self) -> String {
        let mut out = Vec::new();
        self.globs(&mut out);
        out.join("")
    }

    fn as_str(&self) -> &str {
        match self {
            Value::Str(text) | Value::Path(text) | Value::Glob(text) | Value::Version(text) => text,
            _ => "",
        }
    }

    fn as_int(&self) -> i64 {
        match self {
            Value::Int(number) => *number,
            Value::Duration(seconds) => *seconds as i64,
            _ => 0,
        }
    }

    fn truthy(&self) -> bool {
        matches!(self, Value::Bool(true))
    }

    /// Globs of a `#[paths]` argument: a glob, or a list of them (nested lists flatten).
    pub fn globs(&self, out: &mut Vec<String>) {
        match self {
            Value::List(items) => items.iter().for_each(|item| item.globs(out)),
            other => out.push(other.as_str().to_owned()),
        }
    }

    pub fn str(text: impl Into<Rc<str>>) -> Value {
        Value::Str(text.into())
    }
}

/// Why evaluation stopped early.
pub enum Flow {
    /// `return`, `?` or a failed `assert`: the function's value.
    Return(Value),
    /// A bug in the program (an index out of range, a division by zero).
    Panic(Failure),
}

type Eval<T> = Result<T, Flow>;

pub struct Interp<'a> {
    pub fns: BTreeMap<String, &'a FnDecl>,
    pub consts: BTreeMap<String, Value>,
    pub root: &'a Path,
    /// Environment of every program a body runs (the check's and profile's `env`).
    pub env: Vec<(String, String)>,
    /// The commit the configuration is read at (None: the working tree).
    pub revision: Option<String>,
    scopes: Vec<BTreeMap<String, Value>>,
}

fn fail(span: Span, message: impl Into<String>) -> Flow {
    Flow::Return(Value::Err(Rc::new(Failure {
        message: message.into(),
        span,
        context: Vec::new(),
    })))
}

fn panic(span: Span, message: impl Into<String>) -> Flow {
    Flow::Panic(Failure {
        message: message.into(),
        span,
        context: Vec::new(),
    })
}

impl<'a> Interp<'a> {
    pub fn new(program: &'a Program, root: &'a Path) -> Interp<'a> {
        let fns = program
            .items
            .iter()
            .filter_map(|item| match &item.kind {
                ItemKind::Fn(function) => Some((item.name.clone(), function)),
                _ => None,
            })
            .collect();
        Interp {
            fns,
            consts: BTreeMap::new(),
            root,
            env: Vec::new(),
            revision: None,
            scopes: vec![BTreeMap::new()],
        }
    }

    /// Constants, in the order they are declared.
    pub fn load_consts(&mut self, program: &Program) -> Result<(), Failure> {
        for item in &program.items {
            if let ItemKind::Const { value, ty } = &item.kind {
                let mut found = self.value(value)?;
                if let Some(ty) = ty {
                    found = coerce(
                        found,
                        &ty.name,
                        ty.args.first().map(|arg| arg.name.as_str()),
                    );
                }
                self.consts.insert(item.name.clone(), found);
            }
        }
        Ok(())
    }

    /// An expression that must not return early (constants, attributes).
    pub fn value(&mut self, expr: &Expr) -> Result<Value, Failure> {
        match self.expr(expr) {
            Ok(value) => Ok(value),
            Err(Flow::Panic(failure)) => Err(failure),
            Err(Flow::Return(Value::Err(failure))) => Err((*failure).clone()),
            Err(Flow::Return(_)) => Err(Failure {
                message: "`?` returned early here".into(),
                span: expr.span(),
                context: Vec::new(),
            }),
        }
    }

    /// Run a body that returns `Result<()>`: Ok, or the failure it ended with.
    pub fn run_body(
        &mut self,
        body: &Block,
        bindings: Vec<(String, Value)>,
    ) -> Result<(), Failure> {
        self.scopes = vec![bindings.into_iter().collect()];
        let outcome = match self.block(body) {
            Ok(value) => value,
            Err(Flow::Return(value)) => value,
            Err(Flow::Panic(failure)) => return Err(failure),
        };
        match outcome {
            Value::Err(failure) => Err((*failure).clone()),
            _ => Ok(()),
        }
    }

    pub fn call_fn(&mut self, name: &str, args: Vec<Value>, span: Span) -> Eval<Value> {
        let Some(function) = self.fns.get(name).copied() else {
            return Err(panic(span, format!("no function `{name}`")));
        };
        let saved = std::mem::replace(
            &mut self.scopes,
            vec![
                function
                    .params
                    .iter()
                    .map(|p| p.name.clone())
                    .zip(args)
                    .collect(),
            ],
        );
        let outcome = self.block(&function.body);
        self.scopes = saved;
        match outcome {
            Ok(value) => Ok(value),
            Err(Flow::Return(value)) => Ok(value),
            Err(panic) => Err(panic),
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
            .or_else(|| self.consts.get(name).cloned())
    }

    fn set(&mut self, name: &str, value: Value) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return;
            }
        }
    }

    fn block(&mut self, block: &Block) -> Eval<Value> {
        self.scopes.push(BTreeMap::new());
        let outcome = (|| {
            for stmt in &block.stmts {
                self.stmt(stmt)?;
            }
            match &block.tail {
                Some(tail) => self.expr(tail),
                None => Ok(Value::Unit),
            }
        })();
        self.scopes.pop();
        outcome
    }

    fn stmt(&mut self, stmt: &Stmt) -> Eval<()> {
        match stmt {
            Stmt::Let {
                name, value, ty, ..
            } => {
                let mut found = self.expr(value)?;
                if let Some(ty) = ty {
                    found = coerce(
                        found,
                        &ty.name,
                        ty.args.first().map(|arg| arg.name.as_str()),
                    );
                }
                self.scopes
                    .last_mut()
                    .expect("scope")
                    .insert(name.clone(), found);
            }
            Stmt::Assign { name, value, .. } => {
                let found = self.expr(value)?;
                self.set(name, found);
            }
            Stmt::Assert {
                cond,
                message,
                span,
            } => {
                if !self.expr(cond)?.truthy() {
                    let text = match message {
                        Some(message) => self.expr(message)?.text(),
                        None => format!("assertion failed: {}", self.source_hint(cond)),
                    };
                    return Err(fail(*span, text));
                }
            }
            Stmt::Return { value, .. } => {
                let found = match value {
                    Some(value) => self.expr(value)?,
                    None => Value::Ok(Box::new(Value::Unit)),
                };
                return Err(Flow::Return(found));
            }
            Stmt::For {
                var, iter, body, ..
            } => {
                let Value::List(items) = self.expr(iter)? else {
                    return Ok(());
                };
                for item in items.iter() {
                    self.scopes
                        .push(BTreeMap::from([(var.clone(), item.clone())]));
                    let outcome = self.block(body);
                    self.scopes.pop();
                    outcome?;
                }
            }
            Stmt::Expr(expr) => {
                self.expr(expr)?;
            }
        }
        Ok(())
    }

    fn source_hint(&self, expr: &Expr) -> String {
        let _ = expr;
        "the condition is false".into()
    }

    fn expr(&mut self, expr: &Expr) -> Eval<Value> {
        Ok(match expr {
            Expr::Unit(_) => Value::Unit,
            Expr::Bool(flag, _) => Value::Bool(*flag),
            Expr::Int(number, _) => Value::Int(*number),
            Expr::Duration(seconds, _) => Value::Duration(*seconds),
            Expr::Str(parts, _) => {
                let mut text = String::new();
                for part in parts {
                    match part {
                        StrPart::Lit(literal) => text.push_str(literal),
                        StrPart::Expr(inner) => {
                            let value = self.expr(inner)?;
                            text.push_str(&match value {
                                Value::Some(inner) => inner.text(),
                                Value::None => "none".into(),
                                other => other.text(),
                            });
                        }
                    }
                }
                Value::str(text)
            }
            Expr::Path(segments, span) => {
                let name = segments.join("::");
                if name == "None" {
                    return Ok(Value::None);
                }
                self.lookup(&name)
                    .ok_or_else(|| panic(*span, format!("unknown name `{name}`")))?
            }
            Expr::List(items, _) => {
                let mut out = Vec::new();
                for item in items {
                    out.push(self.expr(item)?);
                }
                Value::List(Rc::new(out))
            }
            Expr::StructLit { name, fields, .. } => {
                let mut out = BTreeMap::new();
                for (field, value) in fields {
                    out.insert(field.clone(), self.expr(value)?);
                }
                Value::Struct(name.as_str().into(), Rc::new(out))
            }
            Expr::Field(receiver, name, span) => match self.expr(receiver)? {
                Value::Struct(_, fields) => fields
                    .get(name)
                    .cloned()
                    .ok_or_else(|| panic(*span, format!("no field `{name}`")))?,
                Value::Error(failure) | Value::Err(failure) if name == "message" => {
                    Value::str(failure.message.as_str())
                }
                other => {
                    return Err(panic(
                        *span,
                        format!("{} has no field `{name}`", other.text()),
                    ));
                }
            },
            Expr::Index(list, index, span) => {
                let Value::List(items) = self.expr(list)? else {
                    return Err(panic(*span, "not a list"));
                };
                let position = self.expr(index)?.as_int();
                items
                    .get(usize::try_from(position).unwrap_or(usize::MAX))
                    .cloned()
                    .ok_or_else(|| {
                        panic(
                            *span,
                            format!("index {position} is outside a list of {}", items.len()),
                        )
                    })?
            }
            Expr::Call { callee, args, span } => {
                let Expr::Path(segments, _) = &**callee else {
                    return Err(panic(*span, "not a function"));
                };
                // Plan conditions are compiled from their source, not evaluated;
                // so is a `const fn` that returns one.
                if segments.len() == 1
                    && (super::check::CONDITION_FNS.contains(&segments[0].as_str())
                        || self.fns.get(&segments[0]).is_some_and(|function| {
                            function.ret.as_ref().is_some_and(|ret| ret.name == "Cond")
                        }))
                {
                    return Ok(Value::Unit);
                }
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.expr(arg)?);
                }
                let name = segments.join("::");
                match name.as_str() {
                    "Some" => Value::Some(Box::new(values.remove(0))),
                    "Ok" => Value::Ok(Box::new(values.remove(0))),
                    "Err" => Value::Err(Rc::new(Failure {
                        message: values[0].text(),
                        span: *span,
                        context: Vec::new(),
                    })),
                    // Plan conditions are compiled from their source, not evaluated.
                    _ if segments.len() == 1
                        && super::check::CONDITION_FNS.contains(&name.as_str()) =>
                    {
                        Value::Unit
                    }
                    _ if segments[0] == "std" => self.std_call(&name, values, *span)?,
                    _ => self.call_fn(&name, values, *span)?,
                }
            }
            Expr::Method {
                receiver,
                name,
                args,
                span,
            } => {
                let target = self.expr(receiver)?;
                let mut values = Vec::new();
                for arg in args {
                    values.push(self.expr(arg)?);
                }
                if name == "push" {
                    let Expr::Path(segments, _) = &**receiver else {
                        return Err(panic(*span, "push on a temporary"));
                    };
                    let Value::List(mut items) = target else {
                        return Err(panic(*span, "push on a non-list"));
                    };
                    Rc::make_mut(&mut items).push(values.remove(0));
                    self.set(&segments[0], Value::List(items));
                    return Ok(Value::Unit);
                }
                self.method(target, name, values, *span)?
            }
            Expr::Unary(op, inner, span) => match (*op, self.expr(inner)?) {
                ("!", Value::Bool(flag)) => Value::Bool(!flag),
                // A plan condition stays a placeholder (compiled from source).
                ("!", Value::Unit) => Value::Unit,
                ("-", Value::Int(number)) => Value::Int(-number),
                _ => return Err(panic(*span, format!("`{op}` on a wrong value"))),
            },
            Expr::Binary(op, left, right, span) => {
                if *op == "&&" || *op == "||" {
                    let first = self.expr(left)?;
                    if first == Value::Unit {
                        return Ok(Value::Unit);
                    }
                    let first = first.truthy();
                    if (*op == "&&" && !first) || (*op == "||" && first) {
                        return Ok(Value::Bool(first));
                    }
                    return Ok(Value::Bool(self.expr(right)?.truthy()));
                }
                let a = self.expr(left)?;
                let b = self.expr(right)?;
                binary(op, a, b, *span)?
            }
            Expr::Try(inner, _) => match self.expr(inner)? {
                Value::Ok(value) | Value::Some(value) => *value,
                Value::None => return Err(Flow::Return(Value::None)),
                err @ Value::Err(_) => return Err(Flow::Return(err)),
                other => other,
            },
            Expr::If {
                cond,
                then,
                otherwise,
                ..
            } => {
                if self.expr(cond)?.truthy() {
                    self.block(then)?
                } else if let Some(otherwise) = otherwise {
                    self.expr(otherwise)?
                } else {
                    Value::Unit
                }
            }
            Expr::Match { value, arms, span } => {
                let found = self.expr(value)?;
                for (pattern, body) in arms {
                    let mut bound = BTreeMap::new();
                    if self.matches(pattern, &found, &mut bound)? {
                        self.scopes.push(bound);
                        let outcome = self.expr(body);
                        self.scopes.pop();
                        return outcome;
                    }
                }
                return Err(panic(*span, format!("no `match` arm for {}", found.text())));
            }
            Expr::Block(block) => self.block(block)?,
            Expr::Command {
                run,
                env,
                words,
                span,
            } => {
                let mut argv = Vec::new();
                for word in words {
                    self.command_word(word, &mut argv)?;
                }
                let mut spec = CommandSpec {
                    program: argv.remove(0),
                    args: argv,
                    ..CommandSpec::default()
                };
                for (key, word) in env {
                    let mut value = Vec::new();
                    self.command_word(word, &mut value)?;
                    spec.env.push((key.clone(), value.join("")));
                }
                if *run {
                    self.run(&spec, *span)
                } else {
                    Value::Command(Rc::new(spec))
                }
            }
        })
    }

    fn command_word(&mut self, word: &CmdWord, out: &mut Vec<String>) -> Eval<()> {
        match word {
            CmdWord::Word(pieces) => {
                let mut text = String::new();
                for piece in pieces {
                    match piece {
                        CmdPiece::Lit(literal) => text.push_str(literal),
                        CmdPiece::Expr(expr) => match self.expr(expr)? {
                            Value::Some(inner) => text.push_str(&inner.text()),
                            Value::None => {
                                return Err(fail(expr.span(), "this argument has no value (None)"));
                            }
                            value => text.push_str(&value.text()),
                        },
                    }
                }
                out.push(text);
            }
            CmdWord::Splat(expr) => {
                if let Value::List(items) = self.expr(expr)? {
                    out.extend(items.iter().map(Value::text));
                }
            }
        }
        Ok(())
    }

    fn matches(
        &mut self,
        pattern: &Pattern,
        value: &Value,
        bound: &mut BTreeMap<String, Value>,
    ) -> Eval<bool> {
        Ok(match pattern {
            Pattern::Wild(_) => true,
            Pattern::Bind(name, _) => {
                bound.insert(name.clone(), value.clone());
                true
            }
            Pattern::Lit(expr) => {
                let literal = self.expr(expr)?;
                literal.as_str() == value.as_str() && literal.text() == value.text()
            }
            Pattern::Variant(name, inner, _) => {
                let held = match (name.as_str(), value) {
                    ("Some", Value::Some(inner)) | ("Ok", Value::Ok(inner)) => {
                        Some((**inner).clone())
                    }
                    ("Err", Value::Err(failure)) => Some(Value::Error(failure.clone())),
                    ("None", Value::None) => None,
                    _ => return Ok(false),
                };
                match (inner, held) {
                    (Some(inner), Some(held)) => self.matches(inner, &held, bound)?,
                    _ => true,
                }
            }
        })
    }

    fn method(&mut self, target: Value, name: &str, args: Vec<Value>, span: Span) -> Eval<Value> {
        let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Unit);
        let text = target.as_str().to_owned();
        Ok(match (&target, name) {
            (Value::Str(_) | Value::Path(_), "len") => Value::Int(text.chars().count() as i64),
            (Value::Str(_) | Value::Path(_), "count") => {
                Value::Int(text.matches(arg(0).as_str()).count() as i64)
            }
            (Value::Str(_) | Value::Path(_), "contains") => {
                Value::Bool(text.contains(arg(0).as_str()))
            }
            (Value::Str(_) | Value::Path(_), "starts_with") => {
                Value::Bool(text.starts_with(arg(0).as_str()))
            }
            (Value::Str(_) | Value::Path(_), "ends_with") => {
                Value::Bool(text.ends_with(arg(0).as_str()))
            }
            (Value::Str(_) | Value::Path(_), "find") => match text.find(arg(0).as_str()) {
                Some(at) => Value::Some(Box::new(Value::Int(text[..at].chars().count() as i64))),
                None => Value::None,
            },
            (Value::Str(_) | Value::Path(_), "trim") => Value::str(text.trim()),
            (Value::Str(_) | Value::Path(_), "is_empty") => Value::Bool(text.is_empty()),
            (Value::Str(_) | Value::Path(_), "lines") => {
                Value::List(Rc::new(text.lines().map(Value::str).collect()))
            }
            (Value::Str(_) | Value::Path(_), "split") => Value::List(Rc::new(
                text.split(arg(0).as_str()).map(Value::str).collect(),
            )),
            (Value::Path(_), "matches") => Value::Bool(glob_matches(arg(0).as_str(), &text)),
            (Value::List(items), "len") => Value::Int(items.len() as i64),
            (Value::List(items), "is_empty") => Value::Bool(items.is_empty()),
            (Value::List(items), "contains") => {
                Value::Bool(items.iter().any(|item| item.text() == arg(0).text()))
            }
            (Value::List(items), "first") => items
                .first()
                .cloned()
                .map_or(Value::None, |v| Value::Some(Box::new(v))),
            (Value::List(items), "last") => items
                .last()
                .cloned()
                .map_or(Value::None, |v| Value::Some(Box::new(v))),
            (Value::List(items), "join") => Value::str(
                items
                    .iter()
                    .map(Value::text)
                    .collect::<Vec<_>>()
                    .join(arg(0).as_str()),
            ),
            (Value::Some(_), "is_some") | (Value::None, "is_none") => Value::Bool(true),
            (Value::Some(_), "is_none") | (Value::None, "is_some") => Value::Bool(false),
            (Value::Some(inner), "unwrap_or") => (**inner).clone(),
            (Value::None, "unwrap_or") => arg(0),
            (Value::Some(inner), "ok_or") => Value::Ok(inner.clone()),
            (Value::None, "ok_or") => Value::Err(Rc::new(Failure {
                message: arg(0).text(),
                span,
                context: Vec::new(),
            })),
            (Value::Ok(_), "is_ok") | (Value::Err(_), "is_err") => Value::Bool(true),
            (Value::Ok(_), "is_err") | (Value::Err(_), "is_ok") => Value::Bool(false),
            (Value::Ok(_), "context") => target.clone(),
            (Value::Err(failure), "context") => {
                let mut failure = (**failure).clone();
                failure.context.push(arg(0).text());
                Value::Err(Rc::new(failure))
            }
            (Value::Ok(inner), "ok") => Value::Some(inner.clone()),
            (Value::Err(_), "ok") => Value::None,
            (Value::Version(version), "bump") => match crate::release::bump(version) {
                Some(next) => Value::Version(next.into()),
                None => return Err(panic(span, format!("cannot bump version {version}"))),
            },
            (Value::Command(spec), _) => {
                let mut spec = (**spec).clone();
                match name {
                    "arg" => spec.args.push(arg(0).text()),
                    "args" => {
                        if let Value::List(items) = arg(0) {
                            spec.args.extend(items.iter().map(Value::text));
                        }
                    }
                    "env" => spec.env.push((arg(0).text(), arg(1).text())),
                    "current_dir" => spec.dir = Some(arg(0).text()),
                    "run" => return Ok(self.run(&spec, span)),
                    "output" => return Ok(self.output(&spec, span)),
                    _ => return Err(panic(span, format!("Command has no method `{name}`"))),
                }
                Value::Command(Rc::new(spec))
            }
            _ => {
                return Err(panic(
                    span,
                    format!("{} has no method `{name}`", target.text()),
                ));
            }
        })
    }

    fn command(&self, spec: &CommandSpec) -> Command {
        let mut command = Command::new(&spec.program);
        command
            .args(&spec.args)
            .envs(self.env.iter().cloned())
            .envs(spec.env.iter().cloned())
            .current_dir(
                spec.dir
                    .as_ref()
                    .map_or(self.root.to_path_buf(), |dir| self.root.join(dir)),
            )
            .stdin(Stdio::null());
        command
    }

    fn run(&self, spec: &CommandSpec, span: Span) -> Value {
        println!("── {}", spec.label());
        let failure = |message: String| {
            Value::Err(Rc::new(Failure {
                message,
                span,
                context: Vec::new(),
            }))
        };
        match self.command(spec).status() {
            Ok(status) if status.success() => Value::Ok(Box::new(Value::Unit)),
            Ok(status) => failure(format!(
                "`{}` exited with {}",
                spec.label(),
                status
                    .code()
                    .map_or("a signal".into(), |code| code.to_string())
            )),
            Err(error) => failure(format!("cannot run {}: {error}", spec.program)),
        }
    }

    fn output(&self, spec: &CommandSpec, span: Span) -> Value {
        match self
            .command(spec)
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .output()
        {
            Ok(output) => Value::Ok(Box::new(Value::Struct(
                "Output".into(),
                Rc::new(BTreeMap::from([
                    (
                        "code".to_owned(),
                        Value::Int(i64::from(output.status.code().unwrap_or(-1))),
                    ),
                    (
                        "stdout".to_owned(),
                        Value::str(String::from_utf8_lossy(&output.stdout).as_ref()),
                    ),
                    (
                        "stderr".to_owned(),
                        Value::str(String::from_utf8_lossy(&output.stderr).as_ref()),
                    ),
                ])),
            ))),
            Err(error) => Value::Err(Rc::new(Failure {
                message: format!("cannot run {}: {error}", spec.program),
                span,
                context: Vec::new(),
            })),
        }
    }

    fn builtin(&self, work: Work, span: Span) -> Value {
        let label = work.label();
        let step = WorkStep {
            span,
            label: label.clone(),
            work,
        };
        match crate::lang::compile::execute(&step, self.root, false) {
            Ok(0) => Value::Ok(Box::new(Value::Unit)),
            Ok(code) => Value::Err(Rc::new(Failure {
                message: format!("{label} failed ({code})"),
                span,
                context: Vec::new(),
            })),
            Err(error) => Value::Err(Rc::new(Failure {
                message: format!("{label}: {error:#}"),
                span,
                context: Vec::new(),
            })),
        }
    }

    fn std_call(&mut self, name: &str, args: Vec<Value>, span: Span) -> Eval<Value> {
        let arg = |index: usize| args.get(index).cloned().unwrap_or(Value::Unit);
        Ok(match name {
            "std::proc::Command::new" => Value::Command(Rc::new(CommandSpec {
                program: arg(0).text(),
                ..CommandSpec::default()
            })),
            "std::fs::read" => match std::fs::read_to_string(self.root.join(arg(0).as_str())) {
                Ok(text) => Value::Ok(Box::new(Value::str(text))),
                Err(error) => Value::Err(Rc::new(Failure {
                    message: format!("cannot read {}: {error}", arg(0).as_str()),
                    span,
                    context: Vec::new(),
                })),
            },
            "std::fs::exists" => Value::Bool(self.root.join(arg(0).as_str()).exists()),
            "std::fs::glob" => {
                let pattern = arg(0).text();
                let listed = Command::new("git")
                    .args(["ls-files", "-co", "--exclude-standard"])
                    .current_dir(self.root)
                    .output()
                    .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
                    .unwrap_or_default();
                Value::List(Rc::new(
                    listed
                        .lines()
                        .filter(|path| glob_matches(&pattern, path))
                        .map(|path| Value::Path(path.into()))
                        .collect(),
                ))
            }
            "std::paths::cargo" | "std::paths::next" | "std::paths::package" => {
                let files = super::tools::RepoFiles::new(self.root, self.revision.as_deref());
                let names: Vec<String> = args.iter().map(Value::text).collect();
                let found = match name {
                    "std::paths::cargo" => crate::lang::cargo::crates(&files, &names),
                    "std::paths::next" => {
                        crate::lang::web::closure(&files, &names, crate::lang::web::Kind::Next)
                    }
                    _ => crate::lang::web::closure(&files, &names, crate::lang::web::Kind::Package),
                };
                match found {
                    Ok(globs) => Value::List(Rc::new(
                        globs
                            .into_iter()
                            .map(|glob| Value::Glob(glob.into()))
                            .collect(),
                    )),
                    Err(message) => return Err(panic(span, message)),
                }
            }
            "std::env::var" => match std::env::var(arg(0).as_str()) {
                Ok(value) => Value::Some(Box::new(Value::str(value))),
                Err(_) => Value::None,
            },
            "std::wait::http" => self.builtin(
                Work::WaitHttp {
                    url: arg(0).text(),
                    timeout: arg(1).as_int() as u64,
                },
                span,
            ),
            "std::wait::tcp" => self.builtin(
                Work::WaitTcp {
                    address: arg(0).text(),
                    timeout: arg(1).as_int() as u64,
                },
                span,
            ),
            "std::wait::file" => self.builtin(
                Work::WaitFile {
                    path: arg(0).text(),
                    timeout: arg(1).as_int() as u64,
                },
                span,
            ),
            "std::fs::copy" => self.builtin(
                Work::Copy {
                    from: arg(0).text(),
                    to: arg(1).text(),
                },
                span,
            ),
            "std::docs::check_links" => self.builtin(
                Work::LinksCheck {
                    pattern: arg(0).text(),
                },
                span,
            ),
            "std::log::info" => {
                println!("{}", arg(0).text());
                Value::Unit
            }
            other => return Err(panic(span, format!("unknown function `{other}`"))),
        })
    }
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    crate::manifest::GlobList::new(&[pattern.to_owned()]).is_ok_and(|globs| globs.matches(path))
}

/// Text written where a `path`, `glob` or `Version` is declared becomes one.
fn coerce(value: Value, ty: &str, item: Option<&str>) -> Value {
    match (value, ty) {
        (Value::Str(text), "path") => Value::Path(text),
        (Value::Str(text), "glob") => Value::Glob(text),
        (Value::Str(text), "Version") => Value::Version(text),
        (Value::List(items), "list") if item.is_some() => Value::List(Rc::new(
            items
                .iter()
                .cloned()
                .map(|v| coerce(v, item.unwrap_or_default(), None))
                .collect(),
        )),
        (value, _) => value,
    }
}

fn binary(op: &str, a: Value, b: Value, span: Span) -> Eval<Value> {
    Ok(match (op, &a, &b) {
        ("==", ..) => Value::Bool(a.text() == b.text()),
        ("!=", ..) => Value::Bool(a.text() != b.text()),
        ("<", Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
        ("<=", Value::Int(x), Value::Int(y)) => Value::Bool(x <= y),
        (">", Value::Int(x), Value::Int(y)) => Value::Bool(x > y),
        (">=", Value::Int(x), Value::Int(y)) => Value::Bool(x >= y),
        ("<" | "<=" | ">" | ">=", ..) => {
            let (x, y) = (a.as_int(), b.as_int());
            let (x, y) = if matches!(a, Value::Str(_)) {
                return Ok(Value::Bool(match op {
                    "<" => a.as_str() < b.as_str(),
                    "<=" => a.as_str() <= b.as_str(),
                    ">" => a.as_str() > b.as_str(),
                    _ => a.as_str() >= b.as_str(),
                }));
            } else {
                (x, y)
            };
            Value::Bool(match op {
                "<" => x < y,
                "<=" => x <= y,
                ">" => x > y,
                _ => x >= y,
            })
        }
        ("+", Value::Int(x), Value::Int(y)) => Value::Int(x + y),
        ("+", Value::Duration(x), Value::Duration(y)) => Value::Duration(x + y),
        ("+", Value::List(x), Value::List(y)) => {
            let mut out = (**x).clone();
            out.extend(y.iter().cloned());
            Value::List(Rc::new(out))
        }
        ("+", ..) => Value::str(format!("{}{}", a.as_str(), b.as_str())),
        ("-", Value::Int(x), Value::Int(y)) => Value::Int(x - y),
        ("-", Value::List(x), Value::List(y)) => {
            let mut out = (**x).clone();
            out.extend(
                y.iter()
                    .map(|glob| Value::Glob(format!("!{}", glob.as_str()).into())),
            );
            Value::List(Rc::new(out))
        }
        ("*", Value::Int(x), Value::Int(y)) => Value::Int(x * y),
        ("/" | "%", Value::Int(_), Value::Int(0)) => return Err(panic(span, "division by zero")),
        ("/", Value::Int(x), Value::Int(y)) => Value::Int(x / y),
        ("%", Value::Int(x), Value::Int(y)) => Value::Int(x % y),
        _ => {
            return Err(panic(
                span,
                format!("`{op}` on {} and {}", a.text(), b.text()),
            ));
        }
    })
}

/// `error: message` with its place, then what it happened in.
pub fn render(failure: &Failure, sources: &Sources) -> String {
    let mut out = sources.render(&crate::lang::Error::at(
        failure.span,
        failure.message.clone(),
    ));
    for context in failure.context.iter().rev() {
        out.push_str(&format!("  while: {context}\n"));
    }
    out
}
