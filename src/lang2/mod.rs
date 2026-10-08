//! Language v2 (prototype, docs/design/language-v2.md): Rust-like items with
//! typed bodies. A file whose first line is `#![citrus(2)]` is read by this
//! module; it compiles into the same project model as the v1 language, and a
//! check's body runs through `Work::Script` in the check's worker.

pub mod ast;
pub mod check;
pub mod interp;
pub mod lexer;
pub mod parser;

use std::collections::BTreeMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::lang::compile::{Check, Group, Project, Step, Task, Work};
use crate::lang::{Error, Sources, Span};
use ast::{Attr, Expr, Item, ItemKind, Program};
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
    compile(&program, &sources, root)
        .map(|project| (project, sources.clone()))
        .map_err(|error| sources.render(&error))
}

fn attr_names(attr: &Attr) -> Vec<String> {
    attr.args
        .iter()
        .filter_map(|(_, arg)| match arg {
            Expr::Path(segments, _) => Some(segments.join("::")),
            _ => None,
        })
        .collect()
}

fn globs(interp: &mut Interp, item: &Item, name: &str) -> Result<Vec<String>, Error> {
    let mut out = Vec::new();
    for attr in item.attrs.iter().filter(|attr| attr.name == name) {
        for (_, arg) in &attr.args {
            let value = interp
                .value(arg)
                .map_err(|failure| Error::at(failure.span, failure.message))?;
            value.globs(&mut out);
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    out.retain(|glob| seen.insert(glob.clone()));
    Ok(out)
}

fn not_yet(item: &Item, names: &[&str]) -> Result<(), Error> {
    for attr in &item.attrs {
        if names.contains(&attr.name.as_str()) {
            return Err(Error::at(
                attr.span,
                format!("`#[{}]` is not in the v2 prototype yet", attr.name),
            ));
        }
    }
    Ok(())
}

fn cache(item: &Item, interp: &mut Interp) -> Result<Option<bool>, Error> {
    let Some(attr) = item.attr("cache") else {
        return Ok(None);
    };
    match attr.args.first() {
        None => Ok(Some(true)),
        Some((_, arg)) => match interp.value(arg) {
            Ok(Value::Bool(flag)) => Ok(Some(flag)),
            _ => Err(Error::at(attr.span, "`#[cache]` or `#[cache(false)]`")),
        },
    }
}

/// What a body's result depends on besides itself: functions, constants, structs.
fn shared_source(program: &Program, sources: &Sources) -> String {
    let mut out = String::new();
    for item in &program.items {
        if matches!(
            item.kind,
            ItemKind::Fn(_) | ItemKind::Const { .. } | ItemKind::Struct { .. }
        ) {
            out.push_str(slice(sources, item.span));
            out.push('\n');
        }
    }
    out
}

fn slice(sources: &Sources, span: Span) -> &str {
    sources
        .files
        .get(span.file)
        .and_then(|(_, text)| text.get(span.start..span.end))
        .unwrap_or_default()
}

fn script(item: String, body: &str, shared: &str, args: Vec<(String, String)>, span: Span) -> Step {
    let digest = hex::encode(Sha256::digest(format!("{body}\n{shared}")));
    Step {
        span,
        label: item.clone(),
        work: Work::Script {
            item,
            digest: digest[..16].to_owned(),
            args,
        },
    }
}

fn release_args() -> Vec<(String, String)> {
    ["version", "previous", "commit", "unit"]
        .into_iter()
        .map(|name| (name.to_owned(), format!("{{{name}}}")))
        .collect()
}

pub fn compile(program: &Program, sources: &Sources, root: &Path) -> Result<Project, Error> {
    let mut project = Project::default();
    let mut interp = Interp::new(program, root);
    interp
        .load_consts(program)
        .map_err(|failure| Error::at(failure.span, failure.message))?;
    for attr in &program.inner {
        let values: Vec<Value> = attr
            .args
            .iter()
            .map(|(_, arg)| interp.value(arg))
            .collect::<Result<_, _>>()
            .map_err(|failure| Error::at(failure.span, failure.message))?;
        let text = |index: usize| match values.get(index) {
            Some(Value::Str(text)) => Ok(text.to_string()),
            _ => Err(Error::at(
                attr.span,
                format!("`#![{}(\"…\")]` takes text", attr.name),
            )),
        };
        match attr.name.as_str() {
            "citrus" => {}
            "main" => project.base = Some(text(0)?),
            "toolchain" => {
                for value in &values {
                    value.globs(&mut project.toolchain);
                }
            }
            other => {
                return Err(Error::at(
                    attr.span,
                    format!("unknown project attribute `#![{other}]`"),
                )
                .help("project attributes: citrus, main, toolchain"));
            }
        }
    }
    let shared = shared_source(program, sources);
    for item in &program.items {
        match &item.kind {
            ItemKind::Group { items } => {
                not_yet(item, &["needs", "after"])?;
                let paths = globs(&mut interp, item, "paths")?;
                let reads = globs(&mut interp, item, "reads")?;
                let group_cache = cache(item, &mut interp)?;
                project.groups.push(Group {
                    name: item.name.clone(),
                    owns: paths.clone(),
                    span: item.span,
                });
                for inner in items {
                    let name = format!("{}.{}", item.name, inner.name);
                    let mut check = make_check(inner, &name, &mut interp, sources, &shared)?;
                    check.group = Some(item.name.clone());
                    if check.owns.is_empty() {
                        check.owns = paths.clone();
                    } else {
                        check.narrows = true;
                    }
                    check.reads.extend(reads.iter().cloned());
                    check.cache_set = check.cache_set.or(group_cache);
                    project.checks.push(check);
                }
            }
            ItemKind::Check { .. } => {
                let check = make_check(item, &item.name, &mut interp, sources, &shared)?;
                if check.owns.is_empty() {
                    return Err(
                        Error::at(item.span, format!("check {} has no paths", item.name))
                            .help("put it in a group, or give it `#[paths(\"…\")]`"),
                    );
                }
                project.checks.push(check);
            }
            ItemKind::Task { .. } => {
                project.tasks.push(Task {
                    name: item.name.clone(),
                    about: item.doc.clone().unwrap_or_default(),
                    steps: vec![script(
                        format!("task:{}", item.name),
                        slice(sources, item.span),
                        &shared,
                        Vec::new(),
                        item.span,
                    )],
                    span: item.span,
                });
            }
            ItemKind::Release { steps, rollback } => {
                let environment = item
                    .attr("environment")
                    .map(attr_names)
                    .and_then(|names| names.first().cloned())
                    .ok_or_else(|| {
                        Error::at(
                            item.span,
                            format!("release {} needs `#[environment(…)]`", item.name),
                        )
                    })?;
                let version = match item.attr("version") {
                    None => None,
                    Some(attr) => {
                        let mut initial = String::new();
                        let mut scope = Vec::new();
                        for (key, arg) in &attr.args {
                            let value = interp
                                .value(arg)
                                .map_err(|failure| Error::at(failure.span, failure.message))?;
                            match key.as_deref() {
                                Some("initial") => initial = value.as_text(),
                                _ => value.globs(&mut scope),
                            }
                        }
                        Some(crate::release::Version { initial, scope })
                    }
                };
                let step = |decl: &ast::StepDecl, item_name: String| -> crate::release::Step {
                    let recover = decl
                        .attrs
                        .iter()
                        .find(|attr| attr.name == "recover")
                        .and_then(|attr| attr_names(attr).first().cloned())
                        .map(|name| {
                            vec![
                                script(
                                    format!("fn:{name}"),
                                    slice(sources, decl.span),
                                    &shared,
                                    release_args(),
                                    decl.span,
                                )
                                .work,
                            ]
                        })
                        .unwrap_or_default();
                    crate::release::Step {
                        name: decl.name.clone(),
                        run: vec![
                            script(
                                item_name,
                                slice(sources, decl.span),
                                &shared,
                                release_args(),
                                decl.span,
                            )
                            .work,
                        ],
                        production: decl.attrs.iter().any(|attr| attr.name == "production"),
                        recover,
                    }
                };
                let unit = crate::release::Unit {
                    description: item.doc.clone().unwrap_or_default(),
                    environment,
                    checks: "proven".into(),
                    version,
                    steps: steps
                        .iter()
                        .map(|decl| step(decl, format!("step:{}:{}", item.name, decl.name)))
                        .collect(),
                    rollback: rollback
                        .as_ref()
                        .map(|decl| step(decl, format!("rollback:{}", item.name))),
                };
                unit.validate(&item.name)
                    .map_err(|error| Error::at(item.span, format!("{error:#}")))?;
                project.releases.insert(item.name.clone(), unit);
            }
            ItemKind::Const { .. }
            | ItemKind::Fn(_)
            | ItemKind::Struct { .. }
            | ItemKind::Environment => {}
        }
    }
    project.files = sources
        .files
        .iter()
        .map(|(path, _)| path.display().to_string())
        .collect();
    Ok(project)
}

fn make_check(
    item: &Item,
    name: &str,
    interp: &mut Interp,
    sources: &Sources,
    shared: &str,
) -> Result<Check, Error> {
    not_yet(item, &["needs", "after"])?;
    if !crate::manifest::valid_name(name) {
        return Err(Error::at(
            item.span,
            format!("check name {name} must be lowercase letters, digits, `_` or `-`"),
        ));
    }
    Ok(Check {
        name: name.to_owned(),
        description: item.doc.clone(),
        owns: globs(interp, item, "paths")?,
        reads: globs(interp, item, "reads")?,
        cache: true,
        cache_set: cache(item, interp)?,
        resources: Vec::new(),
        meta: BTreeMap::new(),
        env: Vec::new(),
        steps: vec![script(
            format!("check:{name}"),
            slice(sources, item.span),
            shared,
            Vec::new(),
            item.span,
        )],
        group: None,
        narrows: false,
        via: Vec::new(),
        arms: Vec::new(),
        profiles: Vec::new(),
        covered_by: Vec::new(),
        when: None,
        replaces: Vec::new(),
        span: item.span,
    })
}

impl Value {
    fn as_text(&self) -> String {
        let mut out = Vec::new();
        self.globs(&mut out);
        out.join("")
    }
}

/// Run one body in the working tree at `root`: `check:name`, `task:name`,
/// `step:release:step`, `rollback:release` or `fn:name`. Exit code 0 or 1;
/// a failure is printed as `error: …` with its place.
pub fn run_item(root: &Path, item: &str, args: &[(String, String)]) -> i32 {
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
