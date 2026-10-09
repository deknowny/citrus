//! A checked v2 program → the project model Citrus plans and runs: checks,
//! groups, tasks, profiles, services, the runner, releases, artifacts and
//! environments. Attribute arguments are evaluated here, once, as constants.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

use super::ast::{Attr, Block, Expr, Item, ItemKind, Program, StepDecl};
use super::interp::{Interp, Value};
use super::tools;
use crate::lang::{Error, Sources, Span};
use crate::model::{Check, Cond, Group, Paths, Pool, Project, Service, Step, Task, Work};

type Compiled<T> = Result<T, Error>;

fn failure(failure: super::interp::Failure) -> Error {
    Error::at(failure.span, failure.message)
}

/// An item's external name: `group::check` → `group.check`.
/// An item's name outside the language (command line, plans, JSON): the
/// identifier with `-` for `_`, as Cargo does for crate names.
pub fn dash(name: &str) -> String {
    name.replace('_', "-")
}

fn external(segments: &[String]) -> String {
    segments
        .iter()
        .map(|segment| dash(segment))
        .collect::<Vec<_>>()
        .join(".")
}

/// The source text of a span.
fn slice(sources: &Sources, span: Span) -> &str {
    sources
        .files
        .get(span.file)
        .and_then(|(_, text)| text.get(span.start..span.end))
        .unwrap_or_default()
}

struct Compiler<'a> {
    program: &'a Program,
    sources: &'a Sources,
    interp: Interp<'a>,
    files: tools::RepoFiles<'a>,
    /// What every body depends on besides itself: functions, constants, structs.
    shared: String,
    /// Programs declared with `#![tool]`: a wrapper and the command line it is.
    tools: Vec<(String, Vec<String>)>,
    /// The project's Makefiles, read once when a body runs `make`.
    make: std::cell::OnceCell<Option<super::make::Makefiles>>,
}

impl<'a> Compiler<'a> {
    fn value(&mut self, expr: &Expr) -> Compiled<Value> {
        self.interp.value(expr).map_err(failure)
    }

    /// A configuration value as JSON: a command becomes its argv, an item's
    /// name its name, a duration its seconds.
    fn json(&mut self, expr: &Expr) -> Compiled<Json> {
        if let Expr::Path(segments, _) = expr
            && (self.is_item(segments) || !self.interp.consts.contains_key(&segments.join("::")))
        {
            return Ok(Json::String(external(segments)));
        }
        if let Expr::List(items, _) = expr {
            return items
                .iter()
                .map(|item| self.json(item))
                .collect::<Compiled<Vec<_>>>()
                .map(Json::Array);
        }
        let value = self.value(expr)?;
        Ok(value_json(&value))
    }

    /// A group or a check: what `touched`, `only` and `without` can name.
    fn is_selectable(&self, segments: &[String]) -> bool {
        let name = segments.join("::");
        self.program.items.iter().any(|item| match &item.kind {
            ItemKind::Group { items } => {
                item.name == name
                    || items
                        .iter()
                        .any(|inner| format!("{}::{}", item.name, inner.name) == name)
            }
            ItemKind::Check { .. } => item.name == name,
            _ => false,
        })
    }

    /// A declared item that is not a value (not a const or a fn).
    fn is_item(&self, segments: &[String]) -> bool {
        let name = segments.join("::");
        self.program.items.iter().any(|item| {
            (item.name == name && !matches!(item.kind, ItemKind::Const { .. } | ItemKind::Fn(_)))
                || matches!(&item.kind, ItemKind::Group { items } if items.iter().any(|inner| format!("{}::{}", item.name, inner.name) == name))
        })
    }

    /// `#[name(a, key = b)]` → {"0": a, "key": b} with positional args by index.
    fn attr_object(&mut self, attr: &Attr) -> Compiled<serde_json::Map<String, Json>> {
        let mut object = serde_json::Map::new();
        let mut position = 0;
        for (key, arg) in &attr.args {
            let value = self.json(arg)?;
            match key {
                Some(key) => {
                    object.insert(key.clone(), value);
                }
                None => {
                    object.insert(position.to_string(), value);
                    position += 1;
                }
            }
        }
        Ok(object)
    }

    fn globs(&mut self, attrs: &[Attr], name: &str) -> Compiled<(Vec<String>, Vec<String>)> {
        let mut globs = Vec::new();
        let mut groups = Vec::new();
        for attr in attrs.iter().filter(|attr| attr.name == name) {
            for (_, arg) in &attr.args {
                if let Expr::Path(segments, _) = arg
                    && self.program.items.iter().any(|item| {
                        item.name == segments.join("::")
                            && matches!(item.kind, ItemKind::Group { .. })
                    })
                {
                    groups.push(external(segments));
                    continue;
                }
                self.value(arg)?.globs(&mut globs);
            }
        }
        Ok((unique(globs), groups))
    }

    fn names(attrs: &[Attr], name: &str) -> Vec<String> {
        attrs
            .iter()
            .filter(|attr| attr.name == name)
            .flat_map(|attr| attr.args.iter())
            .filter_map(|(_, arg)| match arg {
                Expr::Path(segments, _) => Some(external(segments)),
                _ => None,
            })
            .collect()
    }

    fn flag(&mut self, attrs: &[Attr], name: &str) -> Compiled<Option<bool>> {
        let Some(attr) = attrs.iter().find(|attr| attr.name == name) else {
            return Ok(None);
        };
        match attr.args.first() {
            None => Ok(Some(true)),
            Some((_, arg)) => match self.value(arg)? {
                Value::Bool(flag) => Ok(Some(flag)),
                _ => Err(Error::at(
                    attr.span,
                    format!("`#[{name}]` or `#[{name}(false)]`"),
                )),
            },
        }
    }

    /// `#[env(KEY = "value", …)]`, in order.
    fn env(&mut self, attrs: &[Attr]) -> Compiled<Vec<(String, String)>> {
        let mut out = Vec::new();
        for attr in attrs.iter().filter(|attr| attr.name == "env_file") {
            for (_, arg) in &attr.args {
                out.push(("@env_file".to_owned(), self.value(arg)?.as_text()));
            }
        }
        for attr in attrs.iter().filter(|attr| attr.name == "env") {
            for (key, arg) in &attr.args {
                let Some(key) = key else {
                    return Err(Error::at(arg.span(), "`#[env(KEY = \"value\")]`"));
                };
                out.push((key.clone(), self.value(arg)?.as_text()));
            }
        }
        Ok(out)
    }

    /// A `cmd!(…)` argument as argv, its environment as `KEY=value` words first.
    fn argv(&mut self, expr: &Expr) -> Compiled<Vec<String>> {
        match self.value(expr)? {
            Value::Command(spec) => {
                let mut argv: Vec<String> = Vec::new();
                if !spec.env.is_empty() {
                    argv.push("env".into());
                    argv.extend(spec.env.iter().map(|(key, value)| format!("{key}={value}")));
                }
                argv.push(spec.program.clone());
                argv.extend(spec.args.iter().cloned());
                Ok(argv)
            }
            _ => Err(Error::at(expr.span(), "expected a command: cmd!(\"…\")")),
        }
    }

    fn script(
        &self,
        item: String,
        body: Span,
        args: Vec<(String, String)>,
        env: Vec<(String, String)>,
    ) -> Step {
        let digest = hex::encode(Sha256::digest(format!(
            "{}\n{}",
            slice(self.sources, body),
            self.shared
        )));
        Step {
            span: body,
            label: item.clone(),
            work: Work::Script {
                item,
                digest: digest[..16].to_owned(),
                args,
                env,
            },
        }
    }
}

fn value_json(value: &Value) -> Json {
    match value {
        Value::Unit | Value::None => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => json!(number),
        Value::Duration(seconds) => json!(seconds),
        Value::List(items) => Json::Array(items.iter().map(value_json).collect()),
        Value::Some(inner) => value_json(inner),
        Value::Command(spec) => {
            let mut argv = vec![spec.program.clone()];
            argv.extend(spec.args.iter().cloned());
            json!(argv)
        }
        Value::Struct(_, fields) => Json::Object(
            fields
                .iter()
                .map(|(k, v)| (k.clone(), value_json(v)))
                .collect(),
        ),
        other => Json::String(other.as_text()),
    }
}

/// First occurrence of each glob; lists with exclusions keep their order.
fn unique(items: Vec<String>) -> Vec<String> {
    if items.iter().any(|item| item.starts_with('!')) {
        return items;
    }
    let mut seen = std::collections::BTreeSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect()
}

/// A plan condition: `touched(x)`, `only(x)`, `without(x)` (a group, a check
/// or a list of globs), `signal("…")`, `selected(check)`, `profile(p)`, with
/// `&&`, `||`, `!`, `true`, `false`.
fn condition(expr: &Expr, compiler: &mut Compiler) -> Compiled<Cond> {
    condition_in(expr, compiler, &BTreeMap::new())
}

/// `bound`: the arguments of a `const fn` whose body is being expanded.
fn condition_in(
    expr: &Expr,
    compiler: &mut Compiler,
    bound: &BTreeMap<String, Expr>,
) -> Compiled<Cond> {
    let program = compiler.program;
    // A name: a bound argument, or a constant holding a condition.
    if let Expr::Path(segments, _) = expr
        && segments.len() == 1
    {
        if let Some(value) = bound.get(&segments[0]) {
            return condition_in(&value.clone(), compiler, &BTreeMap::new());
        }
        if let Some(ItemKind::Const { value, .. }) = program
            .items
            .iter()
            .find(|item| item.name == segments[0])
            .map(|item| &item.kind)
        {
            return condition_in(value, compiler, &BTreeMap::new());
        }
    }
    // A call of a `const fn` returning a condition: its body with the arguments.
    if let Expr::Call { callee, args, .. } = expr
        && let Expr::Path(segments, _) = &**callee
        && segments.len() == 1
        && let Some(ItemKind::Fn(function)) = program
            .items
            .iter()
            .find(|item| item.name == segments[0])
            .map(|item| &item.kind)
        && let Some(tail) = &function.body.tail
    {
        let mut inner = BTreeMap::new();
        for (param, arg) in function.params.iter().zip(args) {
            let arg = match arg {
                Expr::Path(names, _) if names.len() == 1 && bound.contains_key(&names[0]) => {
                    bound[&names[0]].clone()
                }
                other => other.clone(),
            };
            inner.insert(param.name.clone(), arg);
        }
        return condition_in(tail, compiler, &inner);
    }
    let resolve = |arg: &Expr| -> Expr {
        match arg {
            Expr::Path(names, _) if names.len() == 1 && bound.contains_key(&names[0]) => {
                bound[&names[0]].clone()
            }
            other => other.clone(),
        }
    };
    let help = "conditions: touched(…), only(…), without(…), signal(\"…\"), selected(…), profile(…), with &&, || and !";
    Ok(match expr {
        Expr::Bool(flag, _) => Cond::Always(*flag),
        Expr::Unary("!", inner, _) => Cond::Not(Box::new(condition_in(inner, compiler, bound)?)),
        Expr::Binary(op @ ("&&" | "||"), left, right, _) => {
            let (left, right) = (
                Box::new(condition_in(left, compiler, bound)?),
                Box::new(condition_in(right, compiler, bound)?),
            );
            if *op == "&&" {
                Cond::And(left, right)
            } else {
                Cond::Or(left, right)
            }
        }
        Expr::Call { callee, args, span } => {
            let Expr::Path(segments, _) = &**callee else {
                return Err(Error::at(*span, "not a condition").help(help));
            };
            let [arg] = args.as_slice() else {
                return Err(Error::at(
                    *span,
                    format!("`{}` takes one argument", segments.join("::")),
                ));
            };
            let arg = &resolve(arg);
            let name = |arg: &Expr, compiler: &mut Compiler| -> Compiled<String> {
                match arg {
                    Expr::Path(segments, _) => Ok(external(segments)),
                    other => Ok(compiler.value(other)?.as_text()),
                }
            };
            let paths = |arg: &Expr, compiler: &mut Compiler| -> Compiled<Paths> {
                match arg {
                    Expr::Path(segments, _) if compiler.is_selectable(segments) => {
                        Ok(Paths::Name(external(segments)))
                    }
                    other => {
                        let mut globs = Vec::new();
                        compiler.value(other)?.globs(&mut globs);
                        Ok(Paths::Globs(globs))
                    }
                }
            };
            match segments.join("::").as_str() {
                "touched" => Cond::Touched(paths(arg, compiler)?),
                "only" => Cond::Only(paths(arg, compiler)?),
                "without" => Cond::Without(paths(arg, compiler)?),
                "signal" => Cond::Signal(name(arg, compiler)?),
                "selected" => Cond::Selected(name(arg, compiler)?),
                "profile" => Cond::Profile(name(arg, compiler)?),
                other => {
                    return Err(
                        Error::at(*span, format!("`{other}` is not a condition")).help(help)
                    );
                }
            }
        }
        // A `const` holding a condition is not a thing yet: say what is.
        other => return Err(Error::at(other.span(), "not a condition").help(help)),
    })
}

/// What a body's commands (and the functions it calls) read, as far as
/// Citrus understands them; whether one of them is a plain process.
/// What a body's commands read.
struct Reads {
    /// Inputs that select the check.
    inputs: Vec<String>,
    /// Inputs that only make its pass stale.
    reads: Vec<String>,
    /// What each understood command was understood as.
    summaries: Vec<String>,
    /// Some command was not understood (or does not select).
    opaque: bool,
    /// Inputs that select the check without being its alone.
    follows: Vec<String>,
}

fn understood(compiler: &Compiler, body: &Block) -> Compiled<Reads> {
    let mut found: Vec<&Expr> = Vec::new();
    tools::block_commands(body, &mut found);
    let mut seen_fns: Vec<String> = Vec::new();
    let mut index = 0;
    while index < found.len() {
        if let Expr::Call { callee, .. } = found[index]
            && let Expr::Path(segments, _) = &**callee
            && segments.len() == 1
            && !seen_fns.contains(&segments[0])
            && let Some(ItemKind::Fn(function)) = compiler
                .program
                .items
                .iter()
                .find(|item| item.name == segments[0])
                .map(|item| &item.kind)
        {
            seen_fns.push(segments[0].clone());
            tools::block_commands(&function.body, &mut found);
        }
        index += 1;
    }
    let mut inputs = Vec::new();
    let mut reads: Vec<String> = Vec::new();
    let mut follows: Vec<String> = Vec::new();
    let mut summaries = Vec::new();
    let mut opaque = false;
    for expr in found {
        let Expr::Command { words, span, .. } = expr else {
            if let Expr::Call { callee, .. } = expr
                && let Expr::Path(segments, _) = &**callee
                && segments.first().is_some_and(|first| first == "std")
            {
                opaque = true;
            }
            continue;
        };
        match tools::understand(
            words,
            *span,
            &compiler.files,
            &compiler.tools,
            &compiler.make,
        )? {
            Some(found) => {
                // A Make recipe is shared by many checks: its files make a
                // pass stale but do not choose the check.
                if !found.selects {
                    opaque = true;
                }
                for glob in &found.follows {
                    if !follows.contains(glob) {
                        follows.push(glob.clone());
                    }
                }
                let into = if found.selects {
                    &mut inputs
                } else {
                    &mut reads
                };
                for glob in found.inputs {
                    if !into.contains(&glob) {
                        into.push(glob);
                    }
                }
                summaries.push(found.summary);
            }
            None => opaque = true,
        }
    }
    Ok(Reads {
        inputs,
        reads,
        summaries,
        opaque,
        follows,
    })
}

/// Settings a group passes to its checks.
#[derive(Default, Clone)]
struct Inherited {
    group: Option<String>,
    paths: Vec<String>,
    reads: Vec<String>,
    needs: Vec<String>,
    env: Vec<(String, String)>,
    profiles: Vec<String>,
    cache: Option<bool>,
    when: Option<Cond>,
}

fn check(compiler: &mut Compiler, item: &Item, name: &str, from: &Inherited) -> Compiled<Check> {
    let ItemKind::Check { body } = &item.kind else {
        return Err(Error::at(item.span, "not a check"));
    };
    if !crate::manifest::valid_name(name) {
        return Err(Error::at(
            item.name_span,
            format!("check name {name} must be lowercase letters, digits, `_` or `-`"),
        ));
    }
    let (declared, via) = compiler.globs(&item.attrs, "paths")?;
    let Reads {
        inputs: inferred,
        reads: understood_reads,
        summaries,
        opaque,
        mut follows,
    } = understood(compiler, body)?;
    // Understood inputs select the check on their own only when every command
    // was understood; a plain process next to them needs `#[paths]`. Like a
    // Make recipe's files they choose the check without taking the path from
    // the groups and checks that own it (a crate change still runs its
    // group's other checks), and they make its pass stale.
    let mut owns = declared.clone();
    let mut inferred_reads: Vec<String> = Vec::new();
    if !opaque || !declared.is_empty() || !via.is_empty() {
        for glob in inferred {
            if !follows.contains(&glob) {
                follows.push(glob.clone());
            }
            inferred_reads.push(glob);
        }
    }
    // Inferred inputs add to what selects the check; only declared paths
    // narrow it from its group's.
    let inferred = !inferred_reads.is_empty();
    let narrows = !owns.is_empty() || !via.is_empty();
    if !narrows {
        owns = from.paths.clone();
    }
    let (mut reads, reads_via) = compiler.globs(&item.attrs, "reads")?;
    let (outputs, _) = compiler.globs(&item.attrs, "outputs")?;
    reads.extend(from.reads.iter().cloned());
    for glob in inferred_reads {
        if !reads.contains(&glob) {
            reads.push(glob);
        }
    }
    let known_inputs =
        !owns.is_empty() || !reads.is_empty() || !via.is_empty() || !reads_via.is_empty();
    for glob in &understood_reads {
        if !reads.contains(glob) {
            reads.push(glob.clone());
        }
    }
    let mut env = from.env.clone();
    env.extend(compiler.env(&item.attrs)?);
    let mut resources = from.needs.clone();
    for need in Compiler::names(&item.attrs, "needs") {
        if !resources.contains(&need) {
            resources.push(need);
        }
    }
    let mut profiles = Compiler::names(&item.attrs, "profile");
    if profiles.is_empty() {
        profiles = from.profiles.clone();
    }
    let mut when = None;
    for attr in item.attrs.iter().filter(|attr| attr.name == "when") {
        for (_, arg) in &attr.args {
            when = Some(condition(arg, compiler)?);
        }
    }
    let when = match (from.when.clone(), when) {
        (Some(a), Some(b)) => Some(Cond::And(Box::new(a), Box::new(b))),
        (a, b) => b.or(a),
    };
    let mut meta = BTreeMap::new();
    for attr in item.attrs.iter().filter(|attr| attr.name == "meta") {
        for (key, value) in compiler.attr_object(attr)? {
            meta.insert(key, value);
        }
    }
    if !summaries.is_empty() {
        meta.insert("understood".to_owned(), json!(summaries));
    }
    if owns.is_empty() && via.is_empty() && follows.is_empty() && when.is_none() {
        return Err(Error::at(item.name_span, format!("check {name} has no paths"))
            .help("run a command Citrus understands (cargo …), put it in a group, or give it `#[paths(\"…\")]`"));
    }
    let step = lowered(body, &env, item.span).unwrap_or_else(|| {
        compiler.script(format!("check:{name}"), item.span, Vec::new(), env.clone())
    });
    Ok(Check {
        name: name.to_owned(),
        description: item.doc.clone(),
        owns,
        reads,
        outputs,
        cache: true,
        cache_set: compiler.flag(&item.attrs, "cache")?.or(from.cache),
        known_inputs,
        reads_via,
        follows,
        inferred,
        resources,
        meta,
        env,
        steps: vec![step],
        group: from.group.clone(),
        narrows,
        via,
        profiles,
        covered_by: Vec::new(),
        when,
        replaces: qualify(Compiler::names(&item.attrs, "replaces"), &from.group),
        span: item.span,
    })
}

/// A body that only runs one fixed command line is that process: other
/// tools see its argv (`CITRUS_CHECKS`), and no interpreter starts for it.
fn lowered(body: &Block, env: &[(String, String)], span: Span) -> Option<Step> {
    let expr = match (body.stmts.as_slice(), &body.tail) {
        ([super::ast::Stmt::Expr(expr)], None) => expr,
        ([], Some(tail)) => &**tail,
        _ => return None,
    };
    let Expr::Try(inner, _) = expr else {
        return None;
    };
    let Expr::Command {
        run: true,
        env: own,
        words,
        ..
    } = &**inner
    else {
        return None;
    };
    let argv: Vec<String> = words
        .iter()
        .map(super::ast::CmdWord::literal)
        .collect::<Option<_>>()?;
    let mut all = env.to_vec();
    for (key, word) in own {
        all.push((key.clone(), word.literal()?));
    }
    let work = Work::Process {
        argv,
        env: all,
        portable: true,
    };
    Some(Step {
        span,
        label: work.label(),
        work,
    })
}

/// Names inside a group may leave the group out: `#[replaces(users)]`.
fn qualify(names: Vec<String>, group: &Option<String>) -> Vec<String> {
    names
        .into_iter()
        .map(|name| match group {
            Some(group) if !name.contains('.') => format!("{group}.{name}"),
            _ => name,
        })
        .collect()
}

fn release_args() -> Vec<(String, String)> {
    ["version", "previous", "commit", "unit"]
        .into_iter()
        .map(|name| (name.to_owned(), format!("{{{name}}}")))
        .collect()
}

fn release(
    compiler: &mut Compiler,
    item: &Item,
    steps: &[StepDecl],
    rollback: Option<&StepDecl>,
) -> Compiled<crate::release::Unit> {
    let environment = Compiler::names(&item.attrs, "environment")
        .into_iter()
        .next()
        .ok_or_else(|| {
            Error::at(
                item.name_span,
                format!("release {} needs `#[environment(…)]`", item.name),
            )
        })?;
    let version = match item.attr("version") {
        None => None,
        Some(attr) => {
            let mut initial = String::new();
            let mut scope = Vec::new();
            for (key, arg) in &attr.args {
                let value = compiler.value(arg)?;
                match key.as_deref() {
                    Some("initial") => initial = value.as_text(),
                    _ => value.globs(&mut scope),
                }
            }
            Some(crate::release::Version { initial, scope })
        }
    };
    let checks = match item.attr("checks").and_then(|attr| attr.args.first()) {
        Some((_, Expr::Path(segments, _))) if segments == &["none".to_owned()] => "none".to_owned(),
        Some((_, arg)) => {
            return Err(Error::at(
                arg.span(),
                "`#[checks(none)]` turns the check gate off",
            ));
        }
        None => "proven".to_owned(),
    };
    let step = |decl: &StepDecl, item_name: String| -> crate::release::Step {
        // A function's name stays as written: it is called, not shown.
        let recover = decl
            .attrs
            .iter()
            .filter(|attr| attr.name == "recover")
            .flat_map(|attr| attr.args.iter())
            .filter_map(|(_, arg)| match arg {
                Expr::Path(segments, _) => Some(segments.join("::")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .first()
            .map(|name| {
                vec![
                    compiler
                        .script(format!("fn:{name}"), decl.span, release_args(), Vec::new())
                        .work,
                ]
            })
            .unwrap_or_default();
        crate::release::Step {
            name: dash(&decl.name),
            run: vec![
                compiler
                    .script(item_name, decl.span, release_args(), Vec::new())
                    .work,
            ],
            production: decl.attrs.iter().any(|attr| attr.name == "production"),
            recover,
        }
    };
    let unit = crate::release::Unit {
        description: item.doc.clone().unwrap_or_default(),
        environment,
        checks,
        version,
        steps: steps
            .iter()
            .map(|decl| {
                step(
                    decl,
                    format!("step:{}:{}", dash(&item.name), dash(&decl.name)),
                )
            })
            .collect(),
        rollback: rollback.map(|decl| step(decl, format!("rollback:{}", dash(&item.name)))),
    };
    unit.validate(&dash(&item.name))
        .map_err(|error| Error::at(item.span, format!("{error:#}")))?;
    Ok(unit)
}

fn artifact(compiler: &mut Compiler, item: &Item) -> Compiled<crate::deploy::Artifact> {
    let mut object = serde_json::Map::new();
    object.insert(
        "description".into(),
        json!(item.doc.clone().unwrap_or_default()),
    );
    for attr in &item.attrs {
        match attr.name.as_str() {
            "inputs" => {
                for (_, arg) in &attr.args {
                    if matches!(arg, Expr::Command { .. }) {
                        object.insert("inputs_command".into(), json!(compiler.argv(arg)?));
                    } else {
                        let mut globs = Vec::new();
                        compiler.value(arg)?.globs(&mut globs);
                        let entry = object.entry("inputs").or_insert_with(|| json!([]));
                        if let Json::Array(items) = entry {
                            items.extend(globs.into_iter().map(Json::String));
                        }
                    }
                }
            }
            "dockerfile" => {
                let fields = compiler.attr_object(attr)?;
                object.insert(
                    "dockerfile".into(),
                    json!({"file": fields.get("0").cloned().unwrap_or(Json::Null), "target": fields.get("target").cloned().unwrap_or(Json::Null)}),
                );
            }
            // How it is built and published: provider settings, part of its key.
            "build" | "publish" => {
                let fields = compiler.attr_object(attr)?;
                object.insert(attr.name.clone(), Json::Object(fields));
            }
            other => {
                return Err(Error::at(
                    attr.span,
                    format!("`#[{other}]` does not apply to an artifact"),
                )
                .help(
                    "artifacts take #[inputs(…)], #[dockerfile(…)], #[build(…)] and #[publish(…)]",
                ));
            }
        }
    }
    serde_json::from_value(Json::Object(object))
        .map_err(|error| Error::at(item.span, format!("artifact {}: {error}", item.name)))
}

fn environment(
    compiler: &mut Compiler,
    item: &Item,
) -> Compiled<Option<crate::deploy::Environment>> {
    let Some(provider) = item.attrs.iter().find(|attr| attr.name == "kubernetes") else {
        // Without a provider an environment is only a release lock.
        return Ok(None);
    };
    let mut object = serde_json::Map::new();
    object.insert("provider".into(), json!("kubernetes"));
    object.insert(
        "description".into(),
        json!(item.doc.clone().unwrap_or_default()),
    );
    let connection: serde_json::Map<String, Json> = compiler
        .attr_object(provider)?
        .into_iter()
        .map(|(key, value)| {
            (
                key,
                json!(
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                ),
            )
        })
        .collect();
    object.insert("connection".into(), Json::Object(connection));
    let mut workloads = serde_json::Map::new();
    for attr in &item.attrs {
        match attr.name.as_str() {
            "kubernetes" => {}
            "deploy" => {
                let mut fields = compiler.attr_object(attr)?;
                let name = fields
                    .remove("0")
                    .and_then(|value| value.as_str().map(str::to_owned));
                let artifact = fields.remove("1");
                let (Some(name), Some(artifact)) = (name, artifact) else {
                    return Err(Error::at(attr.span, "`#[deploy(\"workload\", artifact)]`"));
                };
                fields.insert("artifact".into(), artifact);
                workloads.insert(name, Json::Object(fields));
            }
            "record" | "migrations" | "verify" => {
                let mut fields = compiler.attr_object(attr)?;
                if attr.name == "record"
                    && let Some((_, arg)) = attr
                        .args
                        .iter()
                        .find(|(key, _)| key.as_deref() == Some("resolve"))
                {
                    fields.insert("resolve".into(), json!(compiler.argv(arg)?));
                }
                if attr.name == "verify"
                    && let Some((_, Expr::List(items, _))) = attr
                        .args
                        .iter()
                        .find(|(key, _)| key.as_deref() == Some("commands"))
                {
                    let commands = items
                        .iter()
                        .map(|item| compiler.argv(item))
                        .collect::<Compiled<Vec<_>>>()?;
                    fields.insert("commands".into(), json!(commands));
                }
                object.insert(attr.name.clone(), Json::Object(fields));
            }
            "prepare" => {
                let (_, arg) = attr
                    .args
                    .first()
                    .ok_or_else(|| Error::at(attr.span, "`#[prepare(cmd!(\"…\"))]`"))?;
                object.insert("prepare".into(), json!(compiler.argv(arg)?));
            }
            "manifests" => {
                let fields = compiler.attr_object(attr)?;
                object.insert(
                    "manifests".into(),
                    fields.get("0").cloned().unwrap_or(Json::Null),
                );
            }
            "approval" | "checks" | "release_name" => {
                let fields = compiler.attr_object(attr)?;
                object.insert(
                    attr.name.clone(),
                    fields.get("0").cloned().unwrap_or(Json::Null),
                );
            }
            other => {
                return Err(Error::at(
                    attr.span,
                    format!("`#[{other}]` does not apply to an environment"),
                ));
            }
        }
    }
    object.insert("workloads".into(), Json::Object(workloads));
    serde_json::from_value(Json::Object(object))
        .map(Some)
        .map_err(|error| Error::at(item.span, format!("environment {}: {error}", item.name)))
}

pub fn compile(
    program: &Program,
    sources: &Sources,
    root: &Path,
    revision: Option<&str>,
) -> Compiled<Project> {
    let mut interp = Interp::new(program, root);
    interp.revision = revision.map(str::to_owned);
    interp.load_consts(program).map_err(failure)?;
    let shared = program
        .items
        .iter()
        .filter(|item| {
            matches!(
                item.kind,
                ItemKind::Fn(_) | ItemKind::Const { .. } | ItemKind::Struct { .. }
            )
        })
        .map(|item| slice(sources, item.span))
        .collect::<Vec<_>>()
        .join("\n");
    let mut compiler = Compiler {
        program,
        sources,
        interp,
        files: tools::RepoFiles::new(root, revision),
        shared,
        tools: Vec::new(),
        make: std::cell::OnceCell::new(),
    };
    let mut project = Project::default();
    let mut default_cache = true;
    for attr in &program.inner {
        let first = |compiler: &mut Compiler| -> Compiled<Value> {
            let (_, arg) = attr.args.first().ok_or_else(|| {
                Error::at(attr.span, format!("`#![{}(…)]` needs a value", attr.name))
            })?;
            compiler.value(arg)
        };
        match attr.name.as_str() {
            "citrus" => {}
            // Read by every `citrus` before it loads anything (src/pin.rs).
            "pin" => {
                let commit = first(&mut compiler)?.as_text();
                if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(Error::at(
                        attr.span,
                        "`#![pin(…)]` takes a full 40-character commit",
                    ));
                }
            }
            "main" => project.base = Some(first(&mut compiler)?.as_text()),
            "logs" => project.logs = Some(first(&mut compiler)?.as_text()),
            "receipts" => project.receipts = Some(first(&mut compiler)?.as_text()),
            "cache" => default_cache = matches!(first(&mut compiler)?, Value::Bool(true)),
            "toolchain" => {
                for (_, arg) in &attr.args {
                    compiler.value(arg)?.globs(&mut project.toolchain);
                }
            }
            "signals" | "free_version" | "after_merge" | "prepare" => {
                let (_, arg) = attr.args.first().ok_or_else(|| {
                    Error::at(attr.span, format!("`#![{}(cmd!(\"…\"))]`", attr.name))
                })?;
                let argv = compiler.argv(arg)?;
                match attr.name.as_str() {
                    "signals" => project.signals = argv,
                    "prepare" => project.prepare = argv,
                    "free_version" => project.free_version = argv,
                    _ => project.after_merge = argv,
                }
            }
            "tool" => {
                let [(None, program), (None, command)] = attr.args.as_slice() else {
                    return Err(Error::at(
                        attr.span,
                        "`#![tool(\"path/to/wrapper\", cmd!(\"cargo test\"))]`",
                    ));
                };
                let program = compiler.value(program)?.as_text();
                let program = program.trim_start_matches("./").to_owned();
                if !crate::lang::cargo::Files::list(&compiler.files).contains(&program) {
                    return Err(Error::at(
                        attr.span,
                        format!("no file {program} in the repository"),
                    ));
                }
                let argv = compiler.argv(command)?;
                compiler.tools.push((program, argv));
            }
            "runner" => {
                let mut pool = Pool::default();
                for (key, arg) in &attr.args {
                    match key.as_deref() {
                        None => pool.argv = compiler.argv(arg)?,
                        Some("status") => pool.status = compiler.argv(arg)?,
                        Some(other) => {
                            return Err(Error::at(
                                arg.span(),
                                format!("`#![runner]` has no `{other}`"),
                            )
                            .help("#![runner(cmd!(\"…\"), status = cmd!(\"…\"))]"));
                        }
                    }
                }
                project.pool = Some(pool);
            }
            "image" => {
                let fields = compiler.attr_object(attr)?;
                let text = |key: &str| fields.get(key).and_then(Json::as_str).map(str::to_owned);
                let Some(dockerfile) = text("dockerfile").or_else(|| text("0")) else {
                    return Err(
                        Error::at(attr.span, "`#![image(dockerfile = \"…\")]`").help(
                            "#![image(dockerfile = \"ci/runner.Dockerfile\", target = \"runner\")]",
                        ),
                    );
                };
                if let Some(other) = fields
                    .keys()
                    .find(|key| !matches!(key.as_str(), "0" | "dockerfile" | "target" | "context"))
                {
                    return Err(Error::at(
                        attr.span,
                        format!("`#![image]` has no `{other}`"),
                    ));
                }
                if !crate::lang::cargo::Files::list(&compiler.files).contains(&dockerfile) {
                    return Err(Error::at(
                        attr.span,
                        format!("no file {dockerfile} in the repository"),
                    ));
                }
                project.image = Some(crate::model::Image {
                    dockerfile,
                    target: text("target"),
                    context: text("context").unwrap_or_else(|| ".".into()),
                });
            }
            "private" => {
                for (_, arg) in &attr.args {
                    compiler.value(arg)?.globs(&mut project.private);
                }
            }
            "command" => {
                let fields = compiler.attr_object(attr)?;
                let text = |key: &str| {
                    fields
                        .get(key)
                        .and_then(Json::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                project.commands.push((text("1"), text("2"), text("0")));
            }
            "label" => {
                let [(_, name), (_, when)] = attr.args.as_slice() else {
                    return Err(Error::at(attr.span, "`#![label(\"name\", condition)]`"));
                };
                let name = compiler.value(name)?.as_text();
                let when = condition(when, &mut compiler)?;
                project.labels.push((name, when));
            }
            other => {
                return Err(Error::at(attr.span, format!("unknown project attribute `#![{other}]`")).help(
                    "project attributes: citrus, main, toolchain, logs, receipts, cache, signals, free_version, after_merge, runner, image, private, prepare, tool, command, label",
                ));
            }
        }
    }
    for item in &program.items {
        match &item.kind {
            ItemKind::Profile => {
                project.profiles.push(dash(&item.name));
                let env = compiler.env(&item.attrs)?;
                project.profile_env.push((dash(&item.name), env));
            }
            ItemKind::Service { start, ready, stop } => {
                let limit = match item.attr("limit").and_then(|attr| attr.args.first()) {
                    Some((_, arg)) => match compiler.value(arg)? {
                        Value::Int(limit) => Some(limit),
                        _ => return Err(Error::at(arg.span(), "`#[limit(n)]` takes a number")),
                    },
                    None => None,
                };
                let start = start
                    .as_ref()
                    .map(|body| {
                        vec![compiler.script(
                            format!("service-start:{}", dash(&item.name)),
                            body.span,
                            Vec::new(),
                            Vec::new(),
                        )]
                    })
                    .unwrap_or_default();
                let ready = ready
                    .as_ref()
                    .map(|body| {
                        vec![compiler.script(
                            format!("service-ready:{}", dash(&item.name)),
                            body.span,
                            Vec::new(),
                            Vec::new(),
                        )]
                    })
                    .unwrap_or_default();
                let stop = stop
                    .as_ref()
                    .map(|body| {
                        vec![compiler.script(
                            format!("service-stop:{}", dash(&item.name)),
                            body.span,
                            Vec::new(),
                            Vec::new(),
                        )]
                    })
                    .unwrap_or_default();
                project.services.push(Service {
                    name: dash(&item.name),
                    description: item.doc.clone(),
                    start,
                    ready,
                    stop,
                    limit,
                });
            }
            ItemKind::Group { items } => {
                let (paths, _) = compiler.globs(&item.attrs, "paths")?;
                let (reads, _) = compiler.globs(&item.attrs, "reads")?;
                let mut when = None;
                for attr in item.attrs.iter().filter(|attr| attr.name == "when") {
                    for (_, arg) in &attr.args {
                        when = Some(condition(arg, &mut compiler)?);
                    }
                }
                let inherited = Inherited {
                    group: Some(dash(&item.name)),
                    paths: paths.clone(),
                    reads,
                    needs: Compiler::names(&item.attrs, "needs"),
                    env: compiler.env(&item.attrs)?,
                    profiles: Compiler::names(&item.attrs, "profile"),
                    cache: compiler.flag(&item.attrs, "cache")?,
                    when,
                };
                project.groups.push(Group {
                    name: dash(&item.name),
                    owns: paths,
                    span: item.span,
                });
                for inner in items {
                    let name = format!("{}.{}", dash(&item.name), dash(&inner.name));
                    let compiled = check(&mut compiler, inner, &name, &inherited)?;
                    project.checks.push(compiled);
                }
            }
            ItemKind::Check { .. } => {
                let compiled = check(
                    &mut compiler,
                    item,
                    &dash(&item.name),
                    &Inherited::default(),
                )?;
                project.checks.push(compiled);
            }
            ItemKind::Task { body } => {
                let step = lowered(body, &[], item.span).unwrap_or_else(|| {
                    compiler.script(
                        format!("task:{}", dash(&item.name)),
                        item.span,
                        Vec::new(),
                        Vec::new(),
                    )
                });
                project.tasks.push(Task {
                    name: dash(&item.name),
                    about: item.doc.clone().unwrap_or_default(),
                    steps: vec![step],
                    span: item.span,
                });
            }
            ItemKind::Release { steps, rollback } => {
                let unit = release(&mut compiler, item, steps, rollback.as_ref())?;
                project.releases.insert(dash(&item.name), unit);
            }
            ItemKind::Artifact => {
                let built = artifact(&mut compiler, item)?;
                project.artifacts.insert(dash(&item.name), built);
            }
            ItemKind::Environment => {
                if let Some(env) = environment(&mut compiler, item)? {
                    project.environments.insert(dash(&item.name), env);
                }
            }
            ItemKind::Const { .. } | ItemKind::Fn(_) | ItemKind::Struct { .. } => {}
        }
    }
    // Checks that cover others; groups named in paths are part of the inputs.
    let covers: Vec<(String, Vec<String>)> = program
        .items
        .iter()
        .flat_map(|item| match &item.kind {
            ItemKind::Group { items } => items
                .iter()
                .map(|inner| (format!("{}.{}", dash(&item.name), dash(&inner.name)), inner))
                .collect::<Vec<_>>(),
            _ => vec![(dash(&item.name), item)],
        })
        .map(|(name, item)| {
            let group = name.split_once('.').map(|(group, _)| group.to_owned());
            (
                name,
                qualify(Compiler::names(&item.attrs, "covers"), &group),
            )
        })
        .collect();
    for (by, names) in covers {
        for name in names {
            for check in project.checks.iter_mut().filter(|check| check.name == name) {
                check.covered_by.push(by.clone());
            }
        }
    }
    let group_paths: BTreeMap<String, Vec<String>> = project
        .groups
        .iter()
        .map(|group| (group.name.clone(), group.owns.clone()))
        .collect();
    for check in &mut project.checks {
        let mut reads: Vec<String> = check
            .via
            .iter()
            .chain(&check.reads_via)
            .flat_map(|name| group_paths.get(name).into_iter().flatten().cloned())
            .collect();
        if !reads.is_empty() {
            reads.append(&mut check.reads);
            check.reads = reads;
        }
        check.cache = check.cache_set.unwrap_or(default_cache) && check.known_inputs;
    }
    project.files = sources
        .files
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    Ok(project)
}
