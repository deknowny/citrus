//! Releases: named units of steps (version → build → publish → deploy →
//! postcheck) that Citrus runs with recorded state, an environment lock,
//! recovery of interrupted steps and rollback to the previous release.
//!
//! The steps are the project's own commands; Citrus owns the order, the
//! gates, the state and what happens after an interruption.

use std::collections::BTreeMap;
use std::fs::OpenOptions;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;

use crate::exec::{Context, agent, read_log, segment};
use crate::manifest::now;
use crate::model::Work;
use crate::report::{self, compact_utc};
use crate::state::{Release, ReleaseStep};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    #[serde(default)]
    pub description: String,
    /// One release at a time per environment, across worktrees.
    pub environment: String,
    /// `proven` (default): every check the plan selects must be proven for the
    /// release commit. `none`: no check gate (the steps validate themselves).
    #[serde(default = "proven")]
    pub checks: String,
    #[serde(default)]
    pub version: Option<Version>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub rollback: Option<Step>,
}

fn proven() -> String {
    "proven".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Version {
    /// Version to start from when Citrus has no passed release of this unit yet.
    pub initial: String,
    /// What the version is for (the images it names); default: the unit's name.
    #[serde(default)]
    pub scope: Vec<String>,
}

impl Version {
    pub fn scope(&self, unit: &str) -> Result<Vec<String>> {
        if self.scope.is_empty() {
            crate::versions::scope(&[unit.to_owned()])
        } else {
            crate::versions::scope(&self.scope)
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub name: String,
    /// Commands and built-in steps (`wait.http`, `copy`, …), in order.
    pub run: Vec<Work>,
    /// Changes production: the release needs `--approve`.
    #[serde(default)]
    pub production: bool,
    /// Run instead of repeating this step when its outcome is unknown.
    #[serde(default)]
    pub recover: Vec<Work>,
}

/// Runtime values in every text of `works`.
fn substitute_works(works: &[Work], values: &BTreeMap<&str, String>) -> Vec<Work> {
    works
        .iter()
        .map(|work| {
            work.map_text(|text| {
                values.iter().fold(text.to_owned(), |text, (key, value)| {
                    text.replace(&format!("{{{key}}}"), value)
                })
            })
        })
        .collect()
}

fn describe(works: &[Work]) -> String {
    works
        .iter()
        .map(Work::label)
        .collect::<Vec<_>>()
        .join(" && ")
}

#[derive(Debug, Default)]
pub struct Releases {
    pub releases: BTreeMap<String, Unit>,
}

impl Unit {
    pub fn validate(&self, name: &str) -> Result<()> {
        if self.steps.is_empty() {
            bail!("release {name}: no steps");
        }
        let mut names: Vec<&str> = self.steps.iter().map(|step| step.name.as_str()).collect();
        names.sort_unstable();
        if names.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("release {name}: step names must be unique");
        }
        if !matches!(self.checks.as_str(), "proven" | "none") {
            bail!("release {name}: checks must be proven or none");
        }
        Ok(())
    }
}

impl Releases {
    /// Release units declared in `citrus.ci`.
    pub fn load(context: &Context) -> Result<Releases> {
        Ok(Releases {
            releases: context
                .project
                .as_ref()
                .map(|project| project.releases.clone())
                .unwrap_or_default(),
        })
    }

    pub fn unit(&self, name: &str) -> Result<&Unit> {
        self.releases.get(name).with_context(|| {
            let known: Vec<&String> = self.releases.keys().collect();
            format!("no release unit {name}; declared: {known:?}")
        })
    }
}

/// `1.2.3-suffix` → `1.2.4-suffix`.
pub fn bump(version: &str) -> Option<String> {
    let (core, suffix) = match version.split_once('-') {
        Some((core, suffix)) => (core, format!("-{suffix}")),
        None => (version, String::new()),
    };
    let mut parts: Vec<u64> = core
        .split('.')
        .map(|part| part.parse().ok())
        .collect::<Option<_>>()?;
    if parts.len() != 3 {
        return None;
    }
    parts[2] += 1;
    Some(format!("{}.{}.{}{suffix}", parts[0], parts[1], parts[2]))
}

#[derive(Debug)]
pub struct Start {
    pub unit: String,
    pub approve: bool,
    pub unchecked: bool,
    pub rollback: bool,
    /// Given by hand: no `version` step.
    pub version: Option<String>,
}

/// What `start` would do, without doing it: gates, version and exact commands.
pub fn dry_run(
    context: &mut Context,
    unit_name: &str,
    given: Option<&str>,
) -> Result<serde_json::Value> {
    let releases = Releases::load(context)?;
    let unit = releases.unit(unit_name)?;
    let repo = &context.repo;
    let clean = repo
        .git(&["status", "--porcelain", "--untracked-files=no"])?
        .is_empty();
    let previous = previous_version(context, unit_name, unit)?;
    let next = match (given, &unit.version) {
        (Some(version), _) => version.to_owned(),
        (None, Some(spec)) if previous.is_empty() => spec.initial.clone(),
        (None, Some(_)) => bump(&previous).unwrap_or_default(),
        (None, None) => String::new(),
    };
    let plan = crate::plan::compute(repo, &context.manifest, None)?;
    let files = repo.files()?;
    let snapshot = repo.snapshot()?;
    let mut needed = Vec::new();
    for name in &plan.targets {
        if context.decide(&files, &snapshot, name, false)?.result != "reused" {
            needed.push(name.clone());
        }
    }
    let values = BTreeMap::from([
        (
            "version",
            if next.is_empty() {
                "{version}".to_owned()
            } else {
                next.clone()
            },
        ),
        ("previous", previous.clone()),
        ("commit", repo.git(&["rev-parse", "HEAD"])?),
        ("unit", unit_name.to_owned()),
    ]);
    let mut commands = Vec::new();
    if let (None, Some(spec)) = (given, &unit.version) {
        commands.push(serde_json::json!({"step": "version", "run": format!("reserve {} or the next free version for {}", values["version"], spec.scope(unit_name)?.join(","))}));
    }
    for step in &unit.steps {
        commands.push(serde_json::json!({
            "step": step.name, "production": step.production,
            "run": describe(&substitute_works(&step.run, &values)),
            "recover": if step.recover.is_empty() { None } else { Some(describe(&substitute_works(&step.recover, &values))) },
        }));
    }
    let holder = context.store.environment_holder(&unit.environment)?;
    Ok(serde_json::json!({
        "unit": unit_name, "environment": unit.environment, "environment_holder": holder,
        "clean": clean, "checks_gate": unit.checks, "checks_needed": needed,
        "previous": previous, "next_version": next, "steps": commands,
    }))
}

/// The version the environment runs now when Citrus can see it (releases
/// may also be made by other means), otherwise the last release Citrus passed.
fn previous_version(context: &Context, unit_name: &str, unit: &Unit) -> Result<String> {
    if let Some(running) = crate::deploy::running_release(context, &unit.environment) {
        return Ok(running);
    }
    Ok(context
        .store
        .last_passed_release(unit_name)?
        .map(|release| release.version)
        .unwrap_or_default())
}

/// Validate the gates, reserve the version and start the release worker.
pub fn start(context: &mut Context, request: &Start) -> Result<Release> {
    let releases = Releases::load(context)?;
    let unit = releases.unit(&request.unit)?;
    let repo = &context.repo.clone();
    if !repo
        .git(&["status", "--porcelain", "--untracked-files=no"])?
        .is_empty()
    {
        bail!("commit the source first: a release is built from a commit");
    }
    let commit = repo.git(&["rev-parse", "HEAD"])?;
    let previous_release = context.store.last_passed_release(&request.unit)?;
    let (kind, steps): (&str, Vec<Step>) = if request.rollback {
        let rollback = unit
            .rollback
            .clone()
            .context("this unit declares no rollback")?;
        if previous_release.is_none() {
            bail!(
                "no passed release of {} recorded to roll back from",
                request.unit
            );
        }
        (
            "rollback",
            vec![Step {
                name: "rollback".into(),
                ..rollback
            }],
        )
    } else {
        ("release", unit.steps.clone())
    };
    let production: Vec<&str> = steps
        .iter()
        .filter(|step| step.production)
        .map(|step| step.name.as_str())
        .collect();
    if !production.is_empty() && !request.approve {
        bail!(
            "steps {} change production: pass --approve",
            production.join(", ")
        );
    }
    if kind == "release" && unit.checks == "proven" && !request.unchecked {
        let needed = unproven_checks(&mut *context)?;
        if !needed.is_empty() {
            bail!(
                "checks not proven for this commit: {} — run `citrus run` first (or --unchecked to release anyway)",
                needed.join(", ")
            );
        }
    }
    let started = now();
    let id = format!(
        "rel-{}-{}-{:04x}",
        request.unit,
        compact_utc(started),
        crate::exec::random16()
    );
    let log = repo.log_dir().join(format!("{id}.log"));
    crate::repo::private_dir(&repo.log_dir())?;
    let (version, previous) = if request.rollback {
        // Roll back to the release before the last passed one.
        let history = context.store.releases_of(&request.unit, 50)?;
        let current = previous_release
            .as_ref()
            .map(|release| release.version.clone())
            .unwrap_or_default();
        let target = history
            .iter()
            .filter(|release| {
                release.kind == "release" && release.state == "passed" && release.version != current
            })
            .map(|release| release.version.clone())
            .next()
            .context("no earlier passed release to roll back to")?;
        (target, current)
    } else {
        (
            request.version.clone().unwrap_or_default(),
            previous_version(context, &request.unit, unit)?,
        )
    };
    if request.version.is_none() && unit.version.is_none() && !request.rollback {
        bail!(
            "release {}: no `version` to reserve one; pass --version",
            request.unit
        );
    }
    let mut names: Vec<String> = Vec::new();
    if !request.rollback && request.version.is_none() && unit.version.is_some() {
        names.push("version".into());
    }
    names.extend(steps.iter().map(|step| step.name.clone()));
    let release = Release {
        id: id.clone(),
        unit: request.unit.clone(),
        kind: kind.into(),
        environment: unit.environment.clone(),
        version,
        previous,
        commit,
        worktree: context.worktree(),
        agent: agent(),
        state: "queued".into(),
        note: if request.unchecked {
            "released without the check gate (--unchecked)".into()
        } else {
            String::new()
        },
        pid: None,
        started: started as i64,
        ended: None,
        log: log.display().to_string(),
    };
    context.store.insert_release(&release, &names)?;
    if let Some(holder) = context.store.lock_environment(&unit.environment, &id)? {
        context.store.finish_release(
            &id,
            "cancelled",
            &format!("{} is held by {holder}", unit.environment),
        )?;
        bail!(
            "{} is held by {holder}: wait for it, or `citrus release resume/abandon {holder}`",
            unit.environment
        );
    }
    spawn(context, &release)?;
    context.store.release(&id)?.context("release disappeared")
}

/// Checks the plan selects for HEAD that are not proven yet.
pub fn unproven_checks(context: &mut Context) -> Result<Vec<String>> {
    let repo = &context.repo;
    let plan = crate::plan::compute(repo, &context.manifest, None)?;
    let files = repo.files()?;
    let snapshot = repo.snapshot()?;
    Ok(plan
        .targets
        .iter()
        .map(|name| context.decide(&files, &snapshot, name, false))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|decision| decision.result != "reused")
        .map(|decision| decision.target)
        .collect())
}

pub fn spawn(context: &Context, release: &Release) -> Result<()> {
    let output = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&release.log)?;
    let worker = if release.kind == "apply" {
        "apply-worker"
    } else {
        "release-worker"
    };
    let pid = context.spawn_detached(&[worker, &release.id], output)?;
    context.store.set_release_pid(&release.id, pid)?;
    context
        .store
        .set_release(&release.id, "running", &release.note)
}

/// Continue a failed or unknown release from its first unfinished step.
pub fn resume(context: &Context, release: &Release, approve: bool) -> Result<Release> {
    let release = reconcile(context, release.clone())?;
    if !matches!(release.state.as_str(), "failed" | "unknown") {
        bail!(
            "{} is {}; only failed or unknown releases resume",
            release.id,
            release.state
        );
    }
    let needs_approval = if release.kind == "apply" {
        crate::deploy::environments(context)?
            .get(&release.unit)
            .is_none_or(|environment| environment.approval != "none")
    } else {
        Releases::load(context)?
            .unit(&release.unit)?
            .steps
            .iter()
            .any(|step| step.production)
    };
    if needs_approval && !approve {
        bail!("this release changes production: pass --approve");
    }
    if context.repo.git(&["rev-parse", "HEAD"])? != release.commit {
        bail!(
            "HEAD moved since {} started from {}; resume from that commit",
            release.id,
            &release.commit[..12]
        );
    }
    if let Some(holder) = context
        .store
        .lock_environment(&release.environment, &release.id)?
    {
        bail!("{} is held by {holder}", release.environment);
    }
    spawn(context, &release)?;
    context
        .store
        .release(&release.id)?
        .context("release disappeared")
}

/// Give up an unfinished release (after checking the environment by hand) and free its environment.
pub fn abandon(context: &Context, release: &Release, reason: &str) -> Result<()> {
    if matches!(release.state.as_str(), "passed" | "abandoned" | "cancelled") {
        bail!("{} is already {}", release.id, release.state);
    }
    if let Some(pid) = release.pid.filter(|pid| crate::exec::alive(*pid)) {
        bail!(
            "{} is still running (pid {pid}); wait or cancel it first",
            release.id
        );
    }
    context
        .store
        .finish_release(&release.id, "abandoned", reason)?;
    context
        .store
        .unlock_environment(&release.environment, &release.id)
}

/// A running release whose worker vanished becomes `unknown`; its environment stays locked.
pub fn reconcile(context: &Context, release: Release) -> Result<Release> {
    if release.state != "running" && release.state != "queued" {
        return Ok(release);
    }
    let lost = match release.pid {
        Some(pid) => !crate::exec::alive(pid),
        None => now() as i64 - release.started > 60,
    };
    if !lost {
        return Ok(release);
    }
    for mut step in context.store.release_steps(&release.id)? {
        if step.state == "running" {
            step.state = "unknown".into();
            context.store.update_step(&release.id, &step)?;
        }
    }
    context.store.finish_release(&release.id, "unknown", "worker disappeared mid-step; resume runs the step's recovery, or abandon after checking by hand")?;
    context
        .store
        .release(&release.id)?
        .context("release disappeared")
}

/// Runs in the detached worker; stdout and stderr are the release log.
pub fn work(context: &Context, id: &str) -> Result<()> {
    let release = context.store.release(id)?.context("unknown release")?;
    let units = Releases::load(context)?;
    let unit = units.unit(&release.unit)?;
    let mut version = release.version.clone();
    let steps: Vec<Step> = if release.kind == "rollback" {
        vec![Step {
            name: "rollback".into(),
            ..unit.rollback.clone().context("no rollback declared")?
        }]
    } else {
        unit.steps.clone()
    };
    for mut state in context.store.release_steps(id)? {
        if matches!(state.state.as_str(), "passed" | "recovered") {
            continue;
        }
        let values = |version: &str| -> BTreeMap<&str, String> {
            BTreeMap::from([
                ("version", version.to_owned()),
                ("previous", release.previous.clone()),
                ("commit", release.commit.clone()),
                ("unit", release.unit.clone()),
            ])
        };
        let (works, recovering) = if state.name == "version" {
            (Vec::new(), false)
        } else {
            let step = steps
                .iter()
                .find(|step| step.name == state.name)
                .context("step no longer declared")?;
            if state.state == "unknown" && !step.recover.is_empty() {
                (substitute_works(&step.recover, &values(&version)), true)
            } else {
                (substitute_works(&step.run, &values(&version)), false)
            }
        };
        println!(
            "CITRUS_STEP target={} status=START{}",
            state.name,
            if recovering { " recover=1" } else { "" }
        );
        state.state = "running".into();
        context.store.update_step(id, &state)?;
        let started = now();
        let mut code = 0;
        if state.name == "version" {
            let spec = unit
                .version
                .as_ref()
                .context("version step without `version`")?;
            let start = if release.previous.is_empty() {
                spec.initial.clone()
            } else {
                bump(&release.previous)
                    .with_context(|| format!("cannot bump version {}", release.previous))?
            };
            let free_version = context
                .project
                .as_ref()
                .map(|project| project.free_version.clone())
                .unwrap_or_default();
            match crate::versions::reserve(
                &context.store,
                &context.repo.root,
                &free_version,
                &start,
                &spec.scope(&release.unit)?,
                &crate::versions::Holder {
                    owner: &release.worktree,
                    source: &release.commit,
                    agent: &release.agent,
                },
            ) {
                Ok(reserved) => {
                    println!("reserved {reserved}");
                    version = reserved;
                    context.store.set_release_version(id, &version)?;
                }
                Err(error) => {
                    println!("error: {error:#}");
                    code = 1;
                }
            }
        } else {
            for work in &works {
                let step = crate::model::Step {
                    span: Default::default(),
                    label: work.label(),
                    work: work.clone(),
                };
                code = i64::from(crate::model::execute(&step, &context.repo.root, false)?);
                if code != 0 {
                    break;
                }
            }
        }
        state.seconds = Some((now() - started) as i64);
        state.exit = Some(code);
        let passed = code == 0 && (state.name != "version" || !version.is_empty());
        println!(
            "CITRUS_STEP target={} status={} exit={code} seconds={}",
            state.name,
            if passed { "PASS" } else { "FAIL" },
            state.seconds.unwrap_or_default()
        );
        if passed {
            state.state = if recovering {
                "recovered".into()
            } else {
                "passed".into()
            };
            state.first_error = None;
            context.store.update_step(id, &state)?;
            continue;
        }
        state.state = "failed".into();
        let prefixes = vec!["CITRUS_STEP".to_owned()];
        state.first_error =
            report::first_error(&segment(&read_log(&release.log), &state.name, &prefixes));
        context.store.update_step(id, &state)?;
        context
            .store
            .finish_release(id, "failed", &format!("step {} failed", state.name))?;
        context.store.unlock_environment(&release.environment, id)?;
        return Ok(());
    }
    context.store.finish_release(id, "passed", "")?;
    context.store.unlock_environment(&release.environment, id)?;
    Ok(())
}

pub fn steps_of(context: &Context, id: &str) -> Result<Vec<ReleaseStep>> {
    context.store.release_steps(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_patch_and_keeps_suffix() {
        assert_eq!(bump("0.49.258-clyer").as_deref(), Some("0.49.259-clyer"));
        assert_eq!(bump("1.2.9").as_deref(), Some("1.2.10"));
        assert_eq!(bump("v1"), None);
    }
}
