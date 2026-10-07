//! From evaluated declarations to what Citrus runs: project settings, checks
//! with their steps, tasks. Validation that needs the meaning of a block
//! (known kinds and fields, portable actions, globs that match) lives here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use super::eval::{Action, Decl, Graph, Value};
use super::{Error, Sources, Span};

/// One unit of work Citrus executes, with where it was declared.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Step {
    pub span: Span,
    /// Human label, e.g. `make test-api` or `wait.tcp localhost:5432`.
    pub label: String,
    pub work: Work,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Work {
    Process {
        argv: Vec<String>,
        env: Vec<(String, String)>,
        portable: bool,
    },
    WaitTcp {
        address: String,
        timeout: u64,
    },
    WaitHttp {
        url: String,
        timeout: u64,
    },
    WaitFile {
        path: String,
        timeout: u64,
    },
    Copy {
        from: String,
        to: String,
    },
    /// Relative links in Markdown files matching `pattern` point at existing files.
    LinksCheck {
        pattern: String,
    },
}

impl Work {
    /// Short human form, e.g. `make test-api` or `wait.tcp localhost:5432`.
    pub fn label(&self) -> String {
        match self {
            Work::Process { argv, .. } => argv
                .iter()
                .filter(|part| *part != "--no-print-directory")
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
            Work::WaitTcp { address, .. } => format!("wait.tcp {address}"),
            Work::WaitHttp { url, .. } => format!("wait.http {url}"),
            Work::WaitFile { path, .. } => format!("wait.file {path}"),
            Work::Copy { from, to } => format!("copy {from} → {to}"),
            Work::LinksCheck { pattern } => format!("links.check {pattern}"),
        }
    }

    /// The same work with every text passed through `apply` (runtime values).
    pub fn map_text(&self, apply: impl Fn(&str) -> String) -> Work {
        match self {
            Work::Process {
                argv,
                env,
                portable,
            } => Work::Process {
                argv: argv.iter().map(|part| apply(part)).collect(),
                env: env
                    .iter()
                    .map(|(key, value)| (key.clone(), apply(value)))
                    .collect(),
                portable: *portable,
            },
            Work::WaitTcp { address, timeout } => Work::WaitTcp {
                address: apply(address),
                timeout: *timeout,
            },
            Work::WaitHttp { url, timeout } => Work::WaitHttp {
                url: apply(url),
                timeout: *timeout,
            },
            Work::WaitFile { path, timeout } => Work::WaitFile {
                path: apply(path),
                timeout: *timeout,
            },
            Work::Copy { from, to } => Work::Copy {
                from: apply(from),
                to: apply(to),
            },
            Work::LinksCheck { pattern } => Work::LinksCheck {
                pattern: apply(pattern),
            },
        }
    }

    /// `[kind, arguments…]`, the documented form hashed into fingerprints.
    pub fn canonical(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self {
            Work::Process { argv, .. } => {
                out.push("run".to_owned());
                out.extend(argv.iter().cloned());
            }
            Work::WaitTcp { address, timeout } => {
                out.extend(["wait.tcp".into(), address.clone(), timeout.to_string()])
            }
            Work::WaitHttp { url, timeout } => {
                out.extend(["wait.http".into(), url.clone(), timeout.to_string()])
            }
            Work::WaitFile { path, timeout } => {
                out.extend(["wait.file".into(), path.clone(), timeout.to_string()])
            }
            Work::Copy { from, to } => out.extend(["copy".into(), from.clone(), to.clone()]),
            Work::LinksCheck { pattern } => out.extend(["links.check".into(), pattern.clone()]),
        }
        out
    }
}

/// What `touched`, `only` and `without` look at: a group or check by name,
/// or a list of path globs.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Paths {
    Name(String),
    Globs(Vec<String>),
}

/// A plan-time condition (`when`), evaluated once the changed paths are known.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cond {
    Always(bool),
    /// A changed path is in this group, check or glob list.
    Touched(Paths),
    /// This check is in the plan.
    Selected(String),
    /// The project's signal command printed this signal.
    Signal(String),
    /// The plan is for this profile.
    Profile(String),
    /// Every changed path is in it.
    Only(Paths),
    /// No changed path is in it.
    Without(Paths),
    And(Box<Cond>, Box<Cond>),
    Or(Box<Cond>, Box<Cond>),
    Not(Box<Cond>),
}

impl Cond {
    pub fn eval(&self, facts: &dyn Fn(&Cond) -> bool) -> bool {
        match self {
            Cond::Always(value) => *value,
            Cond::And(left, right) => left.eval(facts) && right.eval(facts),
            Cond::Or(left, right) => left.eval(facts) || right.eval(facts),
            Cond::Not(inner) => !inner.eval(facts),
            leaf => facts(leaf),
        }
    }
}

fn cond(value: &Value, span: Span) -> Result<Cond, Error> {
    let name = |action: &Action| match action.args.first() {
        Some(Value::Ref(name)) => Ok(name.clone()),
        _ => text(action, 0),
    };
    let paths = |action: &Action| match action.args.first() {
        Some(Value::List(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::at(
                        action.span,
                        format!("`{}` takes a name or a list of paths", action.kind),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Paths::Globs),
        _ => name(action).map(Paths::Name),
    };
    Ok(match value {
        Value::Bool(value) => Cond::Always(*value),
        Value::Action(action) => match action.kind.as_str() {
            "touched" => Cond::Touched(paths(action)?),
            "selected" => Cond::Selected(name(action)?),
            "signal" => Cond::Signal(name(action)?),
            "profile" => Cond::Profile(name(action)?),
            "only" => Cond::Only(paths(action)?),
            "without" => Cond::Without(paths(action)?),
            "and" | "or" => {
                let left = Box::new(cond(&action.args[0], span)?);
                let right = Box::new(cond(&action.args[1], span)?);
                if action.kind == "and" {
                    Cond::And(left, right)
                } else {
                    Cond::Or(left, right)
                }
            }
            "not" => Cond::Not(Box::new(cond(&action.args[0], span)?)),
            other => {
                return Err(Error::at(
                    action.span,
                    format!(
                        "`when` takes touched(...), selected(...), signal(...), and, or, not — not `{other}`"
                    ),
                ));
            }
        },
        other => {
            return Err(Error::at(
                span,
                format!("`when` must be a condition, not a {}", other.type_name()),
            ));
        }
    })
}

/// A set of paths and the checks that protect it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Group {
    pub name: String,
    pub owns: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Check {
    /// Its qualified name: `clyer.bot`.
    pub name: String,
    pub description: Option<String>,
    pub owns: Vec<String>,
    pub reads: Vec<String>,
    pub cache: bool,
    /// `cache` as the check or its group says; otherwise the project's default.
    #[serde(skip)]
    pub cache_set: Option<bool>,
    pub resources: Vec<String>,
    /// Project-specific data for the project's own tools (`meta = { … }`).
    pub meta: BTreeMap<String, serde_json::Value>,
    pub env: Vec<(String, String)>,
    pub steps: Vec<Step>,
    /// The group it is declared in; `narrows`: it has its own `paths`.
    pub group: Option<String>,
    pub narrows: bool,
    /// Groups whose paths select it too (`paths = [platform, …]`).
    pub via: Vec<String>,
    /// `match changed` arms: the first whose condition holds replaces `steps`.
    pub arms: Vec<(Cond, Vec<Step>)>,
    /// Profiles the check belongs to; empty: every profile.
    pub profiles: Vec<String>,
    /// Checks that already run this one: with one of them in a plan, this one is dropped.
    pub covered_by: Vec<String>,
    /// Selected only when this holds (and, with `owns`, a path it owns changed).
    pub when: Option<Cond>,
    /// Checks it runs instead of when the change goes beyond one of them.
    pub replaces: Vec<String>,
    pub span: Span,
}

/// A service checks need: started by Citrus when it has an action,
/// otherwise a resource the runner provides.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Service {
    pub name: String,
    pub description: Option<String>,
    pub start: Vec<Step>,
    pub ready: Vec<Step>,
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub name: String,
    pub about: String,
    pub steps: Vec<Step>,
    pub span: Span,
}

/// A runner: runs the planned checks elsewhere and reports them in the
/// runner protocol (docs/protocol.md).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Pool {
    pub argv: Vec<String>,
    /// Prints `CITRUS_RESOURCE key=value…` lines describing the runner's machines.
    pub status: Vec<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Project {
    pub base: Option<String>,
    /// Profiles checks may belong to; the first one is the default.
    pub profiles: Vec<String>,
    pub logs: Option<String>,
    pub toolchain: Vec<String>,
    pub checks: Vec<Check>,
    pub tasks: Vec<Task>,
    pub receipts: Option<String>,
    /// Environment of each profile's checks.
    pub profile_env: Vec<(String, Vec<(String, String)>)>,
    pub services: Vec<Service>,
    pub pool: Option<Pool>,
    pub after_merge: Vec<String>,
    pub commands: Vec<(String, String, String)>,
    pub groups: Vec<Group>,
    /// Named conditions reported with the plan (`label "scope:main" { when = … }`).
    pub labels: Vec<(String, Cond)>,
    /// Prints `SIGNAL <name>` lines for the changed paths (in CITRUS_PATHS).
    pub signals: Vec<String>,
    #[serde(skip)]
    pub releases: BTreeMap<String, crate::release::Unit>,
    #[serde(skip)]
    pub artifacts: BTreeMap<String, crate::deploy::Artifact>,
    #[serde(skip)]
    pub environments: BTreeMap<String, crate::deploy::Environment>,
    /// Repository-relative `.ci` files this project was read from.
    pub files: Vec<String>,
    /// Declarations Citrus reads but does not execute from `.ci` yet.
    pub pending: Vec<(String, String, Span)>,
    pub warnings: Vec<Error>,
}

const KINDS: &[&str] = &[
    "project",
    "profile",
    "group",
    "check",
    "service",
    "runner",
    "task",
    "commands",
    "label",
    "release",
    "artifact",
    "environment",
];

/// A group's paths and environment, given to the checks inside it.
struct Scope<'a> {
    name: &'a str,
    paths: &'a [String],
    env: &'a [(String, String)],
    needs: &'a [String],
    cache: Option<bool>,
}

pub fn compile(graph: &Graph) -> Result<Project, Error> {
    let mut project = Project::default();
    let mut runners: Vec<(String, Pool, Span)> = Vec::new();
    let mut chosen_runner: Option<(String, Span)> = None;
    // `project { cache = false }`: checks reuse a pass only when they say so.
    let mut default_cache = true;
    // (check index, field, names, span): references resolved after all checks are known.
    let mut covers: Vec<(String, Vec<String>, Span)> = Vec::new();
    let mut needs: Vec<(usize, Vec<String>, Span)> = Vec::new();
    for decl in &graph.decls {
        match decl.kind.as_str() {
            "project" => {
                known_fields(
                    decl,
                    &[
                        "main",
                        "runner",
                        "logs",
                        "toolchain",
                        "receipts",
                        "after_merge",
                        "signals",
                        "cache",
                    ],
                )?;
                if let Some(Value::Bool(flag)) = decl.field("cache") {
                    default_cache = *flag;
                }
                known_children(decl, &[])?;
                project.base = optional_string(decl, "main")?;
                if let Some(value) = decl.field("runner") {
                    chosen_runner = Some((
                        reference(value, decl.field_span("runner"))?,
                        decl.field_span("runner"),
                    ));
                }
                if let Some(Value::Action(action)) = decl.field("signals") {
                    project.signals = argv(action)?;
                }
                project.logs = optional_string(decl, "logs")?;
                project.toolchain = strings(decl, "toolchain")?;
                project.receipts = optional_string(decl, "receipts")?;
                if let Some(Value::Action(action)) = decl.field("after_merge") {
                    project.after_merge = argv(action)?;
                }
            }
            "profile" => {
                known_children(decl, &["env"])?;
                known_fields(decl, &[])?;
                let name = label(decl)?;
                if project.profiles.contains(&name) {
                    return Err(Error::at(
                        decl.span,
                        format!("profile {name} is declared twice"),
                    ));
                }
                project.profile_env.push((name.clone(), env_of(decl)?));
                project.profiles.push(name);
            }
            "runner" => {
                known_fields(decl, &["status"])?;
                known_children(decl, &[])?;
                let name = label(decl)?;
                let Some(Value::Action(action)) = &decl.value else {
                    return Err(
                        Error::at(decl.span, format!("runner {name} needs a command"))
                            .help(format!("runner {name} = make(\"remote-check\")")),
                    );
                };
                let status = match decl.field("status") {
                    Some(Value::Action(action)) => argv(action)?,
                    _ => Vec::new(),
                };
                runners.push((
                    name,
                    Pool {
                        argv: argv(action)?,
                        status,
                    },
                    decl.span,
                ));
            }
            "service" => {
                known_fields(decl, &["ready", "limit"])?;
                known_children(decl, &[])?;
                let name = label(decl)?;
                let start = match &decl.value {
                    Some(Value::Action(action)) => vec![step(action, &[])?],
                    _ => Vec::new(),
                };
                let ready = match decl.field("ready") {
                    Some(Value::Action(action)) => vec![step(action, &[])?],
                    _ => Vec::new(),
                };
                project.services.push(Service {
                    name,
                    description: decl.doc.clone(),
                    start,
                    ready,
                    limit: match decl.field("limit") {
                        Some(Value::Int(limit)) => Some(*limit),
                        _ => None,
                    },
                });
            }
            "commands" => {
                for (command, value, span) in &decl.fields {
                    let Value::Str(about) = value else {
                        return Err(Error::at(*span, "a command's description is a string"));
                    };
                    // `commands release { … }`: listed under that heading.
                    project.commands.push((
                        command.clone(),
                        about.clone(),
                        decl.name.clone().unwrap_or_default(),
                    ));
                }
            }
            "label" => {
                known_fields(decl, &["when"])?;
                known_children(decl, &[])?;
                let name = label(decl)?;
                let Some(value) = decl.field("when") else {
                    return Err(Error::at(
                        decl.span,
                        format!("label {name} needs `when = …`"),
                    ));
                };
                project
                    .labels
                    .push((name, cond(value, decl.field_span("when"))?));
            }
            "group" => {
                known_fields(decl, &["paths", "needs", "cache"])?;
                known_children(decl, &["check", "env"])?;
                let name = label(decl)?;
                if project.groups.iter().any(|group| group.name == name) {
                    return Err(
                        Error::at(decl.span, format!("group {name} is declared twice")).help(
                            "one group, one place: move these paths and checks into the first one",
                        ),
                    );
                }
                let paths = unique(strings(decl, "paths")?);
                let env = env_of(decl)?;
                project.groups.push(Group {
                    owns: paths.clone(),
                    name: name.clone(),
                    span: decl.span,
                });
                let group_needs = references(decl, "needs")?;
                let scope = Scope {
                    name: &name,
                    paths: &paths,
                    env: &env,
                    needs: &group_needs,
                    cache: match decl.field("cache") {
                        Some(Value::Bool(flag)) => Some(*flag),
                        _ => None,
                    },
                };
                for child in decl.children.iter().filter(|child| child.kind == "check") {
                    compile_check(child, Some(&scope), &mut project, &mut covers, &mut needs)?;
                }
            }
            "check" => compile_check(decl, None, &mut project, &mut covers, &mut needs)?,
            "task" => {
                known_fields(decl, &[])?;
                known_children(decl, &["env"])?;
                let name = label(decl)?;
                let env = env_of(decl)?;
                let steps = match &decl.value {
                    Some(value) => actions(value, decl.span, &env, &mut project.warnings)?,
                    None => Vec::new(),
                };
                if steps.is_empty() {
                    return Err(
                        Error::at(decl.span, format!("task {name} has nothing to run"))
                            .help(format!("task {name} = [make(\"…\"), wait.tcp(\"…\")]")),
                    );
                }
                project.tasks.push(Task {
                    about: decl.doc.clone().unwrap_or_default(),
                    steps,
                    name,
                    span: decl.span,
                });
            }
            "release" => {
                let name = label(decl)?;
                let unit: crate::release::Unit = typed(decl, release_json(decl)?)?;
                unit.validate(&name)
                    .map_err(|error| Error::at(decl.span, error.to_string()))?;
                project.releases.insert(name, unit);
            }
            "artifact" => {
                let name = label(decl)?;
                project
                    .artifacts
                    .insert(name, typed(decl, artifact_json(decl)?)?);
            }
            "environment" => {
                let name = label(decl)?;
                project
                    .environments
                    .insert(name, typed(decl, environment_json(decl)?)?);
            }
            other => {
                let error = Error::at(decl.span, format!("unknown declaration `{other}`"));
                return Err(match super::suggest(other, KINDS.iter().copied()) {
                    Some(close) => error.help(format!("did you mean `{close}`?")),
                    None => error.help(format!("declarations are: {}", KINDS.join(", "))),
                });
            }
        }
    }
    // The runner: the one `project { runner = … }` names, or the only one.
    project.pool = match (chosen_runner, runners.len()) {
        (Some((name, span)), _) => {
            let Some((_, pool, _)) = runners.iter().find(|(known, ..)| *known == name) else {
                return Err(unknown(
                    span,
                    "runner",
                    &name,
                    runners.iter().map(|(known, ..)| known.as_str()),
                ));
            };
            Some(pool.clone())
        }
        (None, 1) => Some(runners[0].1.clone()),
        (None, 0) => None,
        (None, _) => {
            return Err(Error::at(
                runners[1].2,
                "several runners: name one in `project { runner = … }`",
            ));
        }
    };
    // References: profiles, services, covered checks.
    let profiles = project.profiles.clone();
    let services: Vec<String> = project
        .services
        .iter()
        .map(|service| service.name.clone())
        .collect();
    for check in &project.checks {
        for profile in &check.profiles {
            if !profiles.contains(profile) {
                return Err(unknown(
                    check.span,
                    "profile",
                    profile,
                    profiles.iter().map(String::as_str),
                )
                .or_help(format!("declare it: profile {profile}")));
            }
        }
    }
    for (index, names, span) in needs {
        for name in &names {
            if !services.contains(name) {
                return Err(
                    unknown(span, "service", name, services.iter().map(String::as_str))
                        .or_help(format!("declare it: service {name}")),
                );
            }
        }
        project.checks[index].resources.extend(names);
    }
    for (by, names, span) in covers {
        for name in names {
            let covered: Vec<usize> = project
                .checks
                .iter()
                .enumerate()
                .filter(|(_, check)| check.name == name)
                .map(|(index, _)| index)
                .collect();
            if covered.is_empty() {
                return Err(unknown(
                    span,
                    "check",
                    &name,
                    project.checks.iter().map(|check| check.name.as_str()),
                ));
            }
            for index in covered {
                project.checks[index].covered_by.push(by.clone());
            }
        }
    }
    // A group a check names in its paths is part of its inputs; a check with
    // no known inputs is never reused by them.
    let group_paths: BTreeMap<String, Vec<String>> = project
        .groups
        .iter()
        .map(|group| (group.name.clone(), group.owns.clone()))
        .collect();
    for check in &mut project.checks {
        // The groups' globs first: a check's own reads may re-include what a
        // group excludes, never the other way round.
        let mut reads: Vec<String> = Vec::new();
        for name in &check.via {
            reads.extend(group_paths.get(name).into_iter().flatten().cloned());
        }
        if !reads.is_empty() {
            reads.append(&mut check.reads);
            check.reads = reads;
        }
        check.cache = check.cache_set.unwrap_or(default_cache)
            && !(check.owns.is_empty() && check.reads.is_empty());
    }
    for check in &project.checks {
        for name in &check.via {
            if !project.groups.iter().any(|group| group.name == *name) {
                return Err(unknown(
                    check.span,
                    "group",
                    name,
                    project.groups.iter().map(|group| group.name.as_str()),
                ));
            }
        }
        for name in &check.replaces {
            if !project.checks.iter().any(|other| other.name == *name) {
                return Err(unknown(
                    check.span,
                    "check",
                    name,
                    project.checks.iter().map(|other| other.name.as_str()),
                ));
            }
        }
    }
    Ok(project)
}

/// Globs that match no file: almost always typos. Each is reported once,
/// where it is written (a group's paths, a check's own paths and reads).
/// Slow on large repositories, so only `citrus check` asks.
pub fn dead_globs(project: &Project, root: &Path) -> Vec<Error> {
    let mut dead_globs = Vec::new();
    let files = crate::repo::Repo::discover_at(root)
        .and_then(|repo| repo.paths())
        .unwrap_or_default();
    if !files.is_empty() {
        let dead = |pattern: &String| {
            !pattern.starts_with('!')
                && !crate::manifest::pattern_matches_any(pattern, &files).unwrap_or(true)
        };
        for group in &project.groups {
            for pattern in group.owns.iter().filter(|pattern| dead(pattern)) {
                dead_globs.push(Error::at(
                    group.span,
                    format!("group {}: `{pattern}` matches no file", group.name),
                ));
            }
        }
        for check in &project.checks {
            let own = if check.narrows || check.group.is_none() {
                check.owns.as_slice()
            } else {
                &[]
            };
            for pattern in own
                .iter()
                .chain(&check.reads)
                .filter(|pattern| dead(pattern))
            {
                dead_globs.push(Error::at(
                    check.span,
                    format!("check {}: `{pattern}` matches no file", check.name),
                ));
            }
        }
    }
    dead_globs
}

/// First occurrence of each entry, in order (input lists are often joined).
/// Lists with exclusions keep their order exactly: a later entry may re-include.
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

/// `kind name` is not declared: the message names the closest one.
fn unknown<'a>(span: Span, kind: &str, name: &str, known: impl Iterator<Item = &'a str>) -> Error {
    let error = Error::at(span, format!("no {kind} `{name}`"));
    match super::suggest(name, known) {
        Some(close) => error.help(format!("did you mean `{close}`?")),
        None => error,
    }
}

/// A bare name (or a quoted one) naming a declaration.
fn reference(value: &Value, span: Span) -> Result<String, Error> {
    match value {
        Value::Ref(name) | Value::Str(name) => Ok(name.clone()),
        other => Err(Error::at(
            span,
            format!("expected a name, not a {}", other.type_name()),
        )),
    }
}

fn references(decl: &Decl, field: &str) -> Result<Vec<String>, Error> {
    match decl.field(field) {
        None => Ok(Vec::new()),
        Some(Value::List(items)) => items
            .iter()
            .map(|item| reference(item, decl.field_span(field)))
            .collect(),
        Some(value) => Ok(vec![reference(value, decl.field_span(field))?]),
    }
}

/// `env { NAME = "value" }` inside a declaration.
fn env_of(decl: &Decl) -> Result<Vec<(String, String)>, Error> {
    let mut env = Vec::new();
    for child in decl.children.iter().filter(|child| child.kind == "env") {
        for (name, value, span) in &child.fields {
            match value {
                Value::Str(text) => env.push((name.clone(), text.clone())),
                other => {
                    return Err(Error::at(
                        *span,
                        format!("`{name}` must be a string, not a {}", other.type_name()),
                    ));
                }
            }
        }
    }
    Ok(env)
}

fn known_children(decl: &Decl, known: &[&str]) -> Result<(), Error> {
    for child in &decl.children {
        if !known.contains(&child.kind.as_str()) {
            return Err(Error::at(
                child.span,
                format!("`{}` cannot contain `{}`", decl.kind, child.kind),
            ));
        }
    }
    Ok(())
}

/// Steps of an action or a list of actions.
fn actions(
    value: &Value,
    span: Span,
    env: &[(String, String)],
    warnings: &mut Vec<Error>,
) -> Result<Vec<Step>, Error> {
    let items = match value {
        Value::List(items) => items.clone(),
        other => vec![other.clone()],
    };
    items
        .iter()
        .map(|item| match item {
            Value::Action(action) => {
                let step = step(action, env)?;
                if let Work::Process {
                    portable: false, ..
                } = step.work
                {
                    warnings.push(
                        Error::at(
                            action.span,
                            format!(
                                "`{}` runs a shell command: it will not work on Windows",
                                action.kind
                            ),
                        )
                        .help("prefer run(...) or a built-in action"),
                    );
                }
                if let Some(builtin) = builtin_for(action) {
                    warnings.push(
                        Error::at(
                            action.span,
                            format!("`run(\"cargo\", …)` has a built-in: use {builtin}"),
                        )
                        .help("built-in steps know their flags and read the same everywhere"),
                    );
                }
                Ok(step)
            }
            other => Err(Error::at(
                span,
                format!(
                    "expected actions such as make(\"…\"), not a {}",
                    other.type_name()
                ),
            )),
        })
        .collect()
}

/// `check name = action { … }`, alone or inside a group.
fn compile_check(
    decl: &Decl,
    group: Option<&Scope>,
    project: &mut Project,
    covers: &mut Vec<(String, Vec<String>, Span)>,
    needs: &mut Vec<(usize, Vec<String>, Span)>,
) -> Result<(), Error> {
    known_fields(
        decl,
        &[
            "paths", "reads", "profile", "needs", "covers", "replaces", "cache", "when", "meta",
        ],
    )?;
    known_children(decl, &["env"])?;
    let short = label(decl)?;
    let title = match group {
        Some(group) => format!("{}.{short}", group.name),
        None => short.clone(),
    };
    let mut env: Vec<(String, String)> = group.map(|group| group.env.to_vec()).unwrap_or_default();
    for (name, value) in env_of(decl)? {
        env.retain(|(known, _)| *known != name);
        env.push((name, value));
    }
    // `paths = [platform, "x/**"]`: a group's paths (shared) and globs (its own).
    let mut via = Vec::new();
    let mut globs = Vec::new();
    match decl.field("paths") {
        None | Some(Value::None) => {}
        Some(Value::List(items)) => {
            for item in items {
                match item {
                    Value::Str(glob) => globs.push(glob.clone()),
                    Value::Ref(name) => via.push(name.clone()),
                    other => {
                        return Err(Error::at(
                            decl.field_span("paths"),
                            format!(
                                "`paths` takes globs and group names, not a {}",
                                other.type_name()
                            ),
                        ));
                    }
                }
            }
        }
        Some(Value::Str(glob)) => globs.push(glob.clone()),
        Some(Value::Ref(name)) => via.push(name.clone()),
        Some(other) => {
            return Err(Error::at(
                decl.field_span("paths"),
                format!(
                    "`paths` takes globs and group names, not a {}",
                    other.type_name()
                ),
            ));
        }
    }
    let own_paths = unique(globs);
    let narrows = !own_paths.is_empty() || !via.is_empty();
    let owns = if narrows {
        own_paths
    } else {
        group.map(|group| group.paths.to_vec()).unwrap_or_default()
    };
    let when = match decl.field("when") {
        None => None,
        Some(value) => Some(cond(value, decl.field_span("when"))?),
    };
    if owns.is_empty() && via.is_empty() && when.is_none() {
        return Err(Error::at(decl.span, format!("check {title} has no paths"))
            .help("put it in a group, or give it `paths = [\"…\"]`"));
    }
    let Some(value) = &decl.value else {
        return Err(
            Error::at(decl.span, format!("check {title} has nothing to run"))
                .help(format!("check {short} = make(\"…\")")),
        );
    };
    // `match changed { … }`: the arms before `_` are tried in order when the
    // plan is made; `_` is the command otherwise.
    let (run, arms) = match value {
        Value::Action(action) if action.kind == "match" => {
            let pairs: Vec<&[Value]> = action.args.chunks(2).collect();
            let Some((last, rest)) = pairs.split_last() else {
                return Err(Error::at(action.span, "`match changed` needs arms"));
            };
            if last[0] != Value::Bool(true) {
                return Err(Error::at(
                    action.span,
                    format!("check {title}: `match changed` needs a last `_ => …` arm"),
                )
                .help("the `_` arm runs when no other arm holds"));
            }
            let mut arms = Vec::new();
            for pair in rest {
                arms.push((
                    cond(&pair[0], action.span)?,
                    actions(&pair[1], decl.span, &env, &mut project.warnings)?,
                ));
            }
            (last[1].clone(), arms)
        }
        other => (other.clone(), Vec::new()),
    };
    let profiles = references(decl, "profile")?;
    // Names inside a group may leave the group out: `covers = [bot]`.
    let qualify = |names: Vec<String>| -> Vec<String> {
        names
            .into_iter()
            .map(|name| match group {
                Some(group) if !name.contains('.') => format!("{}.{name}", group.name),
                _ => name,
            })
            .collect()
    };
    let covered = qualify(references(decl, "covers")?);
    let replaces = qualify(references(decl, "replaces")?);
    let mut required: Vec<String> = group.map(|group| group.needs.to_vec()).unwrap_or_default();
    for name in references(decl, "needs")? {
        if !required.contains(&name) {
            required.push(name);
        }
    }
    let meta = match decl.field("meta") {
        None => BTreeMap::new(),
        Some(value @ Value::Map(_)) => match to_json(value, decl.field_span("meta"))? {
            serde_json::Value::Object(map) => map.into_iter().collect(),
            _ => BTreeMap::new(),
        },
        Some(other) => {
            return Err(Error::at(
                decl.field_span("meta"),
                format!("`meta` must be a map, not a {}", other.type_name()),
            ));
        }
    };
    let steps = actions(&run, decl.span, &env, &mut project.warnings)?;
    let name = title.clone();
    if !crate::manifest::valid_name(&name) {
        return Err(Error::at(
            decl.span,
            format!("check name {name} must be lowercase letters, digits, `.`, `_` or `-`"),
        ));
    }
    if project.checks.iter().any(|check| check.name == name) {
        return Err(Error::at(
            decl.span,
            format!("check {name} is declared twice"),
        ));
    }
    if !required.is_empty() {
        needs.push((project.checks.len(), required, decl.field_span("needs")));
    }
    project.checks.push(Check {
        name,
        description: decl.doc.clone(),
        owns,
        reads: unique(strings(decl, "reads")?),
        cache: true,
        cache_set: match decl.field("cache") {
            Some(Value::Bool(flag)) => Some(*flag),
            _ => group.and_then(|group| group.cache),
        },
        resources: Vec::new(),
        profiles,
        covered_by: Vec::new(),
        when,
        replaces,
        meta,
        env,
        steps,
        arms,
        group: group.map(|group| group.name.to_owned()),
        narrows,
        via,
        span: decl.span,
    });
    if !covered.is_empty() {
        covers.push((title, covered, decl.field_span("covers")));
    }
    Ok(())
}

fn known_fields(decl: &Decl, known: &[&str]) -> Result<(), Error> {
    for (name, _, span) in &decl.fields {
        if !known.contains(&name.as_str()) {
            let error = Error::at(
                *span,
                format!("`{}` blocks have no field `{name}`", decl.kind),
            );
            return Err(match super::suggest(name, known.iter().copied()) {
                Some(close) => error.help(format!("did you mean `{close}`?")),
                None => error.help(format!("fields are: {}", known.join(", "))),
            });
        }
    }
    Ok(())
}

fn label(decl: &Decl) -> Result<String, Error> {
    decl.name.clone().ok_or_else(|| {
        Error::at(decl.span, format!("a `{}` block needs a name", decl.kind))
            .help(format!("{} name {{ … }}", decl.kind))
    })
}

fn optional_string(decl: &Decl, field: &str) -> Result<Option<String>, Error> {
    match decl.field(field) {
        None | Some(Value::None) => Ok(None),
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(other) => Err(Error::at(
            decl.field_span(field),
            format!("`{field}` must be a string, not a {}", other.type_name()),
        )),
    }
}

fn strings(decl: &Decl, field: &str) -> Result<Vec<String>, Error> {
    match decl.field(field) {
        None | Some(Value::None) => Ok(Vec::new()),
        Some(Value::Str(value)) => Ok(vec![value.clone()]),
        Some(Value::List(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::at(
                        decl.field_span(field),
                        format!(
                            "`{field}` must be a list of strings; found a {}",
                            item.type_name()
                        ),
                    )
                })
            })
            .collect(),
        Some(other) => Err(Error::at(
            decl.field_span(field),
            format!(
                "`{field}` must be a list of strings, not a {}",
                other.type_name()
            ),
        )),
    }
}

/// The built-in action a `run("cargo", "<sub>", …)` step should be.
fn builtin_for(action: &Action) -> Option<String> {
    if action.kind != "run" || action.args.first().and_then(Value::as_str) != Some("cargo") {
        return None;
    }
    let sub = action.args.get(1).and_then(Value::as_str)?;
    ["fmt", "test", "build", "clippy", "run"]
        .contains(&sub)
        .then(|| format!("cargo.{sub}(…)"))
}

fn text(action: &Action, index: usize) -> Result<String, Error> {
    match action.args.get(index) {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(Value::Int(value)) => Ok(value.to_string()),
        Some(other) => Err(Error::at(
            action.span,
            format!(
                "`{}` argument {} must be a string, not a {}",
                action.kind,
                index + 1,
                other.type_name()
            ),
        )),
        None => Err(Error::at(
            action.span,
            format!("`{}` needs argument {}", action.kind, index + 1),
        )),
    }
}

fn named<'a>(action: &'a Action, name: &str) -> Option<&'a Value> {
    action
        .named
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
}

fn timeout(action: &Action) -> u64 {
    match named(action, "timeout") {
        Some(Value::Duration(seconds)) => *seconds,
        Some(Value::Int(seconds)) => *seconds as u64,
        _ => 60,
    }
}

fn step(action: &Action, env: &[(String, String)]) -> Result<Step, Error> {
    let process = |argv: Vec<String>, portable: bool| Work::Process {
        argv,
        env: env.to_vec(),
        portable,
    };
    let all_text = |action: &Action| -> Result<Vec<String>, Error> {
        (0..action.args.len())
            .map(|index| text(action, index))
            .collect()
    };
    let work = match action.kind.as_str() {
        "run" => {
            let argv = all_text(action)?;
            if argv.is_empty() {
                return Err(Error::at(action.span, "`run` needs a program"));
            }
            process(argv, true)
        }
        "make" => {
            let mut argv = vec![
                "make".to_owned(),
                "--no-print-directory".to_owned(),
                text(action, 0)?,
            ];
            for (key, value) in &action.named {
                argv.push(format!("{key}={}", value_text(value)));
            }
            process(argv, true)
        }
        "sh" => process(vec!["sh".into(), "-c".into(), text(action, 0)?], false),
        "cargo.test" => {
            let mut argv = vec!["cargo".into(), "test".into(), "--locked".into()];
            if !action.args.is_empty() {
                argv.extend(["-p".into(), text(action, 0)?]);
            }
            process(argv, true)
        }
        "cargo.build" => {
            let mut argv = vec!["cargo".into(), "build".into(), "--locked".into()];
            if matches!(named(action, "release"), Some(Value::Bool(true))) {
                argv.push("--release".into());
            }
            if let Some(Value::Str(target)) = named(action, "target") {
                argv.extend(["--target".into(), target.clone()]);
            }
            process(argv, true)
        }
        "cargo.fmt" => {
            let mut argv = vec!["cargo".into(), "fmt".into()];
            if matches!(named(action, "check"), Some(Value::Bool(true))) {
                argv.push("--check".into());
            }
            process(argv, true)
        }
        "pnpm.test" | "pnpm.build" => {
            let script = action.kind.trim_start_matches("pnpm.").to_owned();
            let dir = match named(action, "dir") {
                Some(Value::Str(dir)) => dir.clone(),
                _ => ".".into(),
            };
            process(
                vec![
                    "pnpm".into(),
                    "--dir".into(),
                    dir,
                    "--filter".into(),
                    text(action, 0)?,
                    "run".into(),
                    script,
                ],
                true,
            )
        }
        "cargo.run" => {
            // `cargo run` of this package with the given arguments.
            let mut argv = vec![
                "cargo".to_owned(),
                "run".to_owned(),
                "--quiet".to_owned(),
                "--locked".to_owned(),
                "--".to_owned(),
            ];
            argv.extend(all_text(action)?);
            process(argv, true)
        }
        "cargo.clippy" => {
            let mut argv = vec![
                "cargo".into(),
                "clippy".into(),
                "--all-targets".into(),
                "--locked".into(),
            ];
            if let Some(Value::Str(level)) = named(action, "deny") {
                argv.extend(["--".into(), "-D".into(), level.clone()]);
            }
            process(argv, true)
        }
        "compose.up" => process(
            ["docker", "compose", "up", "-d"]
                .into_iter()
                .map(str::to_owned)
                .chain(all_text(action)?)
                .collect(),
            true,
        ),
        "compose.down" => process(
            ["docker", "compose", "down"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            true,
        ),
        "wait.tcp" => Work::WaitTcp {
            address: text(action, 0)?,
            timeout: timeout(action),
        },
        "wait.http" => Work::WaitHttp {
            url: text(action, 0)?,
            timeout: timeout(action),
        },
        "wait.file" => Work::WaitFile {
            path: text(action, 0)?,
            timeout: timeout(action),
        },
        "copy" => Work::Copy {
            from: text(action, 0)?,
            to: text(action, 1)?,
        },
        "links.check" => Work::LinksCheck {
            pattern: if action.args.is_empty() {
                "**/*.md".into()
            } else {
                text(action, 0)?
            },
        },
        other => {
            return Err(
                Error::at(action.span, format!("`{other}` cannot be a step here yet"))
                    .help("steps are run, make, sh, cargo.*, pnpm.*, compose.*, wait.*, copy, links.check"),
            );
        }
    };
    let label = work.label();
    Ok(Step {
        span: action.span,
        label,
        work,
    })
}

/// The program and arguments of a process action.
fn argv(action: &Action) -> Result<Vec<String>, Error> {
    match step(action, &[])?.work {
        Work::Process { argv, .. } => Ok(argv),
        _ => Err(Error::at(
            action.span,
            format!("`{}` is not a command here", action.kind),
        )),
    }
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        Value::Int(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => format!("{other:?}"),
    }
}

/// Execute one step in `root`; the process inherits stdout/stderr.
/// `stdout_to_stderr` keeps a JSON answer on stdout clean while steps print.
pub fn execute(step: &Step, root: &Path, stdout_to_stderr: bool) -> anyhow::Result<i32> {
    let deadline = |seconds: u64| Instant::now() + Duration::from_secs(seconds);
    match &step.work {
        Work::Process { argv, env, .. } => {
            let (program, args) = argv
                .split_first()
                .ok_or_else(|| anyhow::anyhow!("empty command"))?;
            let mut command = std::process::Command::new(program);
            command
                .args(args)
                .envs(env.iter().cloned())
                .current_dir(root)
                .stdin(std::process::Stdio::null());
            if stdout_to_stderr {
                use std::os::fd::AsFd;
                command.stdout(std::process::Stdio::from(
                    std::io::stderr().as_fd().try_clone_to_owned()?,
                ));
            }
            let status = command.status()?;
            Ok(status.code().unwrap_or(-1))
        }
        Work::WaitTcp { address, timeout } => {
            let until = deadline(*timeout);
            loop {
                if std::net::TcpStream::connect(address.as_str()).is_ok() {
                    return Ok(0);
                }
                if Instant::now() > until {
                    eprintln!("{address} did not accept connections within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Work::WaitHttp { url, timeout } => {
            let until = deadline(*timeout);
            loop {
                let ok = std::process::Command::new("curl")
                    .args([
                        "-fsS",
                        "--max-time",
                        "5",
                        "-o",
                        if cfg!(windows) { "NUL" } else { "/dev/null" },
                        url,
                    ])
                    .status()
                    .is_ok_and(|status| status.success());
                if ok {
                    return Ok(0);
                }
                if Instant::now() > until {
                    eprintln!("{url} did not answer within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Work::WaitFile { path, timeout } => {
            let until = deadline(*timeout);
            while !root.join(path).exists() {
                if Instant::now() > until {
                    eprintln!("{path} did not appear within {timeout}s");
                    return Ok(1);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            Ok(0)
        }
        Work::LinksCheck { pattern } => {
            let files = crate::repo::Repo::discover_at(root)?.files()?;
            let mut broken = 0;
            let mut checked = 0;
            for file in &files {
                if !crate::manifest::pattern_matches_any(pattern, std::slice::from_ref(file))? {
                    continue;
                }
                checked += 1;
                let text = std::fs::read_to_string(root.join(file)).unwrap_or_default();
                let dir = Path::new(file).parent().unwrap_or(Path::new(""));
                for link in markdown_links(&text) {
                    let target = link.split('#').next().unwrap_or_default();
                    if target.is_empty() || target.contains("://") || target.starts_with("mailto:")
                    {
                        continue;
                    }
                    if !root.join(dir).join(target).exists() {
                        eprintln!("{file}: broken link {link}");
                        broken += 1;
                    }
                }
            }
            if broken == 0 {
                eprintln!("links ok: {checked} files");
            }
            Ok(i32::from(broken > 0))
        }
        Work::Copy { from, to } => {
            let target = root.join(to);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(root.join(from), target)?;
            Ok(0)
        }
    }
}

/// Targets of `[text](target)` links.
fn markdown_links(text: &str) -> Vec<&str> {
    let mut links = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("](") {
        rest = &rest[start + 2..];
        if let Some(end) = rest.find(')') {
            let link = &rest[..end];
            if !link.contains(char::is_whitespace) {
                links.push(link);
            }
            rest = &rest[end..];
        }
    }
    links
}

/// `citrus.ci` of a repository, compiled; None when there is no such file.
pub fn load(root: &Path) -> Result<Option<(Project, Sources)>, String> {
    let file = root.join("citrus.ci").exists();
    let directory = root.join(".citrus").is_dir()
        && std::fs::read_dir(root.join(".citrus"))
            .map(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().extension().is_some_and(|ext| ext == "ci"))
            })
            .unwrap_or(false);
    match (file, directory) {
        (true, true) => Err(
            "error: both citrus.ci and .citrus/ hold a configuration; keep one\nhelp: a small project uses citrus.ci, a larger one .citrus/*.ci\n".into(),
        ),
        (false, false) => Ok(None),
        _ => load_at(root, None),
    }
}

/// The project as committed at `revision` (or the working tree), compiled.
pub fn load_at(root: &Path, revision: Option<&str>) -> Result<Option<(Project, Sources)>, String> {
    let entry = match revision {
        None if root.join("citrus.ci").exists() => "citrus.ci",
        None => ".citrus",
        Some(revision) => {
            let listed = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["cat-file", "-e", &format!("{revision}:citrus.ci")])
                .status()
                .is_ok_and(|status| status.success());
            if listed { "citrus.ci" } else { ".citrus" }
        }
    };
    match super::load_at(root, entry, revision) {
        Ok((graph, sources)) => match compile(&graph) {
            Ok(mut project) => {
                project.files = sources
                    .files
                    .iter()
                    .map(|(path, _)| path.display().to_string())
                    .collect();
                Ok(Some((project, sources)))
            }
            Err(error) => Err(sources.render(&error)),
        },
        Err((error, sources)) => Err(sources.render(&error)),
    }
}

/// A `.ci` value as JSON: durations become seconds, commands their argv.
fn to_json(value: &Value, span: Span) -> Result<serde_json::Value, Error> {
    use serde_json::Value as Json;
    Ok(match value {
        Value::None => Json::Null,
        Value::Bool(flag) => Json::Bool(*flag),
        Value::Int(number) => Json::from(*number),
        Value::Duration(seconds) => Json::from(*seconds),
        Value::Str(text) | Value::Ref(text) => Json::String(text.clone()),
        Value::List(items) => Json::Array(
            items
                .iter()
                .map(|item| to_json(item, span))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(entries) => Json::Object(
            entries
                .iter()
                .map(|(key, value)| Ok((key.clone(), to_json(value, span)?)))
                .collect::<Result<_, Error>>()?,
        ),
        Value::Action(action) => Json::from(argv(action)?),
        other => {
            return Err(Error::at(
                span,
                format!("a {} cannot be used as data", other.type_name()),
            ));
        }
    })
}

/// Fields of a block as a JSON object.
fn fields_json(
    decl: &Decl,
    skip: &[&str],
) -> Result<serde_json::Map<String, serde_json::Value>, Error> {
    let mut object = serde_json::Map::new();
    for (name, value, span) in &decl.fields {
        if !skip.contains(&name.as_str()) {
            object.insert(name.clone(), to_json(value, *span)?);
        }
    }
    Ok(object)
}

/// `checks = none` / `approval = none` read as the word, not as absence.
fn none_as_word(object: &mut serde_json::Map<String, serde_json::Value>, fields: &[&str]) {
    for field in fields {
        if object.get(*field).is_some_and(serde_json::Value::is_null) {
            object.insert((*field).to_owned(), "none".into());
        }
    }
}

/// Actions as JSON `Work` values: commands and built-in steps.
fn works_json(value: &Value, span: Span) -> Result<serde_json::Value, Error> {
    let items = match value {
        Value::List(items) => items.clone(),
        other => vec![other.clone()],
    };
    let works = items
        .iter()
        .map(|item| match item {
            Value::Action(action) => Ok(step(action, &[])?.work),
            other => Err(Error::at(
                span,
                format!(
                    "expected actions such as make(\"…\"), not a {}",
                    other.type_name()
                ),
            )),
        })
        .collect::<Result<Vec<_>, Error>>()?;
    serde_json::to_value(works).map_err(|error| Error::at(span, error.to_string()))
}

/// `step name = actions { production = true; recover = actions }`, also
/// `rollback = actions { … }`.
fn release_step_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    known_fields(decl, &["production", "recover"])?;
    known_children(decl, &[])?;
    let Some(run) = &decl.value else {
        return Err(
            Error::at(decl.span, format!("`{}` needs what it runs", decl.kind))
                .help(format!("{} = make(\"…\")", decl.kind)),
        );
    };
    let mut object = serde_json::Map::new();
    if let Some(name) = &decl.name {
        object.insert("name".into(), name.clone().into());
    }
    object.insert("run".into(), works_json(run, decl.span)?);
    if let Some(recover) = decl.field("recover") {
        object.insert(
            "recover".into(),
            works_json(recover, decl.field_span("recover"))?,
        );
    }
    if let Some(production) = decl.field("production") {
        object.insert(
            "production".into(),
            to_json(production, decl.field_span("production"))?,
        );
    }
    Ok(object.into())
}

/// A field given as `name = value` or as `name = value { … }` (a child block).
fn value_block(decl: &Decl, kind: &str) -> Option<Decl> {
    decl.children
        .iter()
        .find(|child| child.kind == kind && child.name.is_none())
        .cloned()
        .or_else(|| {
            decl.fields
                .iter()
                .find(|(name, ..)| name == kind)
                .map(|(_, value, span)| Decl {
                    kind: kind.to_owned(),
                    name: None,
                    value: Some(value.clone()),
                    doc: None,
                    fields: Vec::new(),
                    children: Vec::new(),
                    span: *span,
                    instance: Vec::new(),
                })
        })
}

fn release_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    known_fields(decl, &["environment", "checks", "version", "rollback"])?;
    known_children(decl, &["step", "version", "rollback"])?;
    let mut object = fields_json(decl, &["version", "rollback"])?;
    if let Some(doc) = &decl.doc {
        object.insert("description".into(), doc.clone().into());
    }
    none_as_word(&mut object, &["checks"]);
    if let Some(version) = value_block(decl, "version") {
        known_fields(&version, &["initial", "prefix"])?;
        let Some(Value::Action(reserve)) = &version.value else {
            return Err(Error::at(
                version.span,
                "`version` is the command that reserves a version",
            )
            .help(
                "version = make(\"reserve-version\", VERSION: version) { initial = \"1.0.0\" }",
            ));
        };
        let mut spec = fields_json(&version, &[])?;
        spec.insert("reserve".into(), argv(reserve)?.into());
        object.insert("version".into(), spec.into());
    }
    if let Some(rollback) = value_block(decl, "rollback") {
        object.insert("rollback".into(), release_step_json(&rollback)?);
    }
    let steps = decl
        .children
        .iter()
        .filter(|child| child.kind == "step")
        .map(|step| {
            label(step)?;
            release_step_json(step)
        })
        .collect::<Result<Vec<_>, Error>>()?;
    object.insert("steps".into(), steps.into());
    Ok(object.into())
}

fn artifact_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    known_children(decl, &[])?;
    let mut object = fields_json(decl, &["inputs"])?;
    if let Some(doc) = &decl.doc {
        object.insert("description".into(), doc.clone().into());
    }
    match decl.field("inputs") {
        Some(Value::Action(action)) if action.kind == "inputs_of" => {
            let Some(Value::Action(command)) = action.args.first() else {
                return Err(Error::at(
                    action.span,
                    "`inputs_of` takes a command, e.g. inputs_of(run(\"…\"))",
                ));
            };
            object.insert("inputs_command".into(), argv(command)?.into());
        }
        Some(value) => {
            object.insert("inputs".into(), to_json(value, decl.field_span("inputs"))?);
        }
        None => {}
    }
    Ok(object.into())
}

/// `environment name = kubernetes(…) { record = …; deploy workload = artifact }`.
fn environment_json(decl: &Decl) -> Result<serde_json::Value, Error> {
    known_children(decl, &["deploy"])?;
    let mut object = fields_json(decl, &[])?;
    if let Some(doc) = &decl.doc {
        object.insert("description".into(), doc.clone().into());
    }
    none_as_word(&mut object, &["checks", "approval"]);
    let Some(Value::Action(on)) = &decl.value else {
        return Err(Error::at(
            decl.span,
            format!(
                "environment {} needs where it is",
                decl.name.clone().unwrap_or_default()
            ),
        )
        .help("environment production = kubernetes(context: \"…\", namespace: \"…\")"));
    };
    object.insert("provider".into(), on.kind.clone().into());
    let connection: serde_json::Map<String, serde_json::Value> = on
        .named
        .iter()
        .map(|(key, value)| (key.clone(), value_text(value).into()))
        .collect();
    object.insert("connection".into(), connection.into());
    let mut workloads = serde_json::Map::new();
    for deploy in &decl.children {
        known_children(deploy, &[])?;
        let mut workload = fields_json(deploy, &[])?;
        match &deploy.value {
            Some(value) => {
                workload.insert("artifact".into(), reference(value, deploy.span)?.into());
            }
            None => {
                return Err(
                    Error::at(deploy.span, "`deploy` names the artifact it runs")
                        .help("deploy backend = backend-image"),
                );
            }
        }
        workloads.insert(label(deploy)?, workload.into());
    }
    object.insert("workloads".into(), workloads.into());
    Ok(object.into())
}

/// Read a block's JSON form into Citrus's own type; errors point at the block.
fn typed<T: serde::de::DeserializeOwned>(decl: &Decl, json: serde_json::Value) -> Result<T, Error> {
    serde_json::from_value(json).map_err(|error| {
        Error::at(
            decl.span,
            format!(
                "{} \"{}\": {}",
                decl.kind,
                decl.name.clone().unwrap_or_default(),
                error
            ),
        )
    })
}
