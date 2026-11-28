//! Tracked tasks (`citrus task new/claim/report/link/verify/close`): one unit
//! of agent work from an owner-hub card to a verified release.
//!
//! A task holds a step (open → claimed → in-queue → merged → released →
//! verified → closed) and whose turn it is (owner, lead, lead2, agent,
//! citrus). People move it with commands; Citrus moves it on what it already
//! knows: the queue entries (`citrus queue add --task`) and releases linked to
//! it. `citrus tasks --stuck` names the tasks nobody moves.

use std::collections::BTreeSet;

use anyhow::{Result, bail, ensure};
use serde::Serialize;

use crate::exec::Context;
use crate::manifest::now;
use crate::state::{Release, WorkLink, WorkTask};

pub const STEPS: [&str; 7] = [
    "open", "claimed", "in-queue", "merged", "released", "verified", "closed",
];
pub const TURNS: [&str; 5] = ["owner", "lead", "lead2", "agent", "citrus"];
/// A claim survives this much silence unless `--lease` says otherwise.
pub const DEFAULT_LEASE: i64 = 2 * 3600;

pub fn step_rank(step: &str) -> usize {
    STEPS.iter().position(|name| *name == step).unwrap_or(0)
}

/// `90`, `90s`, `15m`, `2h`, `1d` in seconds.
pub fn parse_duration(text: &str) -> Result<i64> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let value: i64 = number
        .parse()
        .map_err(|_| anyhow::anyhow!("a duration is a number with s, m, h or d: {text}"))?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("a duration is a number with s, m, h or d: {text}"),
    };
    Ok(value * scale)
}

pub fn check_turn(turn: &str) -> Result<()> {
    ensure!(
        TURNS.contains(&turn),
        "a turn is one of {}, not {turn}",
        TURNS.join(", ")
    );
    Ok(())
}

/// Whose turn a claim by `who` makes it: a lead or the owner by name, else an agent.
pub fn turn_of(who: &str) -> &str {
    match who {
        "owner" | "lead" | "lead2" => who,
        _ => "agent",
    }
}

/// A short single-line name: a claimant, a card, a branch.
pub fn check_name(what: &str, value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.chars().count() <= 64
            && !value.chars().any(|c| c.is_whitespace() || c.is_control()),
        "{what} is a short name without spaces: {value:?}"
    );
    Ok(())
}

pub fn check_text(value: &str) -> Result<()> {
    ensure!(
        value.chars().count() <= 2000
            && !value
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "a task's text holds at most 2000 characters"
    );
    Ok(())
}

/// A new id: the given slug (`t-` added) or `t-<utc>-<random>`.
pub fn new_id(given: Option<&str>) -> Result<String> {
    let Some(given) = given else {
        return Ok(format!(
            "t-{}-{:04x}",
            crate::report::compact_utc(now()),
            crate::exec::random16()
        ));
    };
    let id = if given.starts_with("t-") {
        given.to_owned()
    } else {
        format!("t-{given}")
    };
    ensure!(
        id.len() <= 64
            && id.len() > 2
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !id.ends_with('-'),
        "a task id is a short lowercase slug: {id}"
    );
    Ok(id)
}

pub fn units_of(task: &WorkTask) -> Vec<String> {
    task.units
        .split(',')
        .map(str::trim)
        .filter(|unit| !unit.is_empty())
        .map(str::to_owned)
        .collect()
}

pub fn add_units(task: &mut WorkTask, units: impl IntoIterator<Item = String>) {
    let mut all = units_of(task);
    for unit in units {
        if !all.contains(&unit) {
            all.push(unit);
        }
    }
    task.units = all.join(",");
}

/// What Citrus knows of a task from its links: the merges and releases.
#[derive(Debug, Default, Serialize)]
pub struct Progress {
    /// Base commits carrying its queue entries, with the time they landed.
    pub merged: Vec<(String, i64)>,
    /// A linked entry still waits or is checked.
    pub queued: bool,
    /// A linked entry asked for releases the queue still runs.
    pub releasing: bool,
    /// Units with a passed release of the change.
    pub released: BTreeSet<String>,
    /// Units whose passed release ran a verify or postcheck step.
    pub verified: BTreeSet<String>,
    /// Releases that ended without passing (and no later one passed).
    pub failed: Vec<String>,
    /// When the last release passed.
    pub released_at: Option<i64>,
    /// Units the task must reach: its own and those its entries asked for.
    pub due: Vec<String>,
}

/// A release proves its result when one of its steps verifies it, or when it
/// applied a deployed environment (an apply ends with `verify`).
pub fn verifies(context: &Context, release: &Release) -> Result<bool> {
    let steps = context.store.release_steps(&release.id)?;
    if steps
        .iter()
        .any(|step| step.state == "passed" && verify_step(&step.name))
    {
        return Ok(true);
    }
    if release.kind == "release" && !release.environment.is_empty() {
        return Ok(context
            .store
            .passed_apply(&release.environment, &release.commit, release.started)?
            .is_some());
    }
    Ok(false)
}

/// `verify`, `postcheck`, `smoke-postcheck`, `verify:http`…
pub fn verify_step(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.split([':', '-', '_'])
        .any(|part| matches!(part, "verify" | "postcheck"))
}

/// Read the task's links, link the releases its merges led to, and move its
/// step forward to what they prove (never back). Returns the task as stored.
pub fn refresh(context: &Context, mut task: WorkTask) -> Result<(WorkTask, Progress)> {
    let store = &context.store;
    let mut progress = Progress {
        due: units_of(&task),
        ..Progress::default()
    };
    let links = store.work_links(&task.id)?;
    for link in links.iter().filter(|link| link.kind == "queue") {
        let Some(entry) = store.queue_entry(&link.value)? else {
            continue;
        };
        for (unit, _) in entry.release_units() {
            if !progress.due.contains(&unit) {
                progress.due.push(unit);
            }
        }
        if entry.landed() && !entry.merged.is_empty() {
            progress
                .merged
                .push((entry.merged.clone(), entry.ended.unwrap_or(entry.updated)));
            for (_, id) in entry.started() {
                if !id.starts_with("error:") {
                    store.work_link(&task.id, "release", &id)?;
                }
            }
        } else if matches!(entry.state.as_str(), "queued" | "checking") {
            progress.queued = true;
        }
        if entry.state == "releasing" {
            progress.releasing = true;
        }
    }
    // A due unit released later (by hand, from a newer base) still counts:
    // a release whose commit carries every merge of the task.
    if !progress.merged.is_empty() {
        let linked = store.work_links(&task.id)?;
        for unit in &progress.due {
            let covered = linked
                .iter()
                .filter(|link| link.kind == "release")
                .filter_map(|link| store.release(&link.value).ok().flatten())
                .any(|release| release.unit == *unit && release.state != "failed");
            if covered {
                continue;
            }
            for release in store.releases_of(unit, 20)? {
                if release.kind != "release"
                    || !matches!(release.state.as_str(), "passed" | "running" | "queued")
                {
                    continue;
                }
                if carries(context, &release.commit, &progress.merged) {
                    store.work_link(&task.id, "release", &release.id)?;
                    break;
                }
            }
        }
    }
    let mut passed_units: Vec<(String, i64)> = Vec::new();
    let mut failed = Vec::new();
    for link in store
        .work_links(&task.id)?
        .iter()
        .filter(|link| link.kind == "release")
    {
        let Some(release) = store.release(&link.value)? else {
            continue;
        };
        let release = crate::release::reconcile(context, release)?;
        if release.state == "passed" {
            passed_units.push((
                release.unit.clone(),
                release.ended.unwrap_or(release.started),
            ));
            progress.released.insert(release.unit.clone());
            progress.released_at = progress
                .released_at
                .max(Some(release.ended.unwrap_or(release.started)));
            if verifies(context, &release)? {
                progress.verified.insert(release.unit.clone());
            }
        } else if release.finished() {
            failed.push((
                release.unit.clone(),
                release.id.clone(),
                release.state.clone(),
            ));
        }
    }
    progress.failed = failed
        .into_iter()
        .filter(|(unit, _, _)| !progress.released.contains(unit))
        .map(|(unit, id, state)| format!("{unit} {id} {state}"))
        .collect();
    // Manual verification (`citrus task verify`) counts for every unit.
    let verified_by_hand = links.iter().any(|link| link.kind == "verified");

    let derived = if !progress.merged.is_empty() || !progress.released.is_empty() {
        let targets: Vec<&String> = if progress.due.is_empty() {
            progress.released.iter().collect()
        } else {
            progress.due.iter().collect()
        };
        let all_released =
            !targets.is_empty() && targets.iter().all(|unit| progress.released.contains(*unit));
        let all_verified =
            all_released && targets.iter().all(|unit| progress.verified.contains(*unit));
        if all_verified || (all_released && verified_by_hand) {
            "verified"
        } else if all_released {
            "released"
        } else if !progress.merged.is_empty() {
            "merged"
        } else {
            "open"
        }
    } else if progress.queued {
        "in-queue"
    } else {
        "open"
    };
    if task.closed.is_none() && step_rank(derived) > step_rank(&task.step) {
        let (turn, text) = match derived {
            "in-queue" => ("citrus", "queued: Citrus lands it".to_owned()),
            "merged" if progress.releasing => (
                "citrus",
                format!("merged; Citrus releases {}", progress.due.join(", ")),
            ),
            "merged" if progress.due.is_empty() => (
                "lead",
                "merged; no release asked (close it, or link the release)".to_owned(),
            ),
            "merged" => (
                "lead",
                format!("merged; release {} is due", progress.due.join(", ")),
            ),
            "released" => (
                "lead",
                format!(
                    "released {}; verify the result (citrus task verify)",
                    progress
                        .released
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            _ => (
                "lead",
                format!(
                    "verified {}",
                    progress
                        .verified
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
        };
        task.step = derived.to_owned();
        task.turn = turn.to_owned();
        task.updated = now() as i64;
        store.work_update(&task, "citrus", &text)?;
    }
    Ok((task, progress))
}

/// Whether `commit` carries every one of `merged`.
fn carries(context: &Context, commit: &str, merged: &[(String, i64)]) -> bool {
    merged.iter().all(|(merge, _)| {
        merge == commit
            || context
                .repo
                .git(&["merge-base", "--is-ancestor", merge, commit])
                .is_ok()
    })
}

/// Thresholds of `citrus tasks --stuck`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Merged but not released (or released but not verified) this long.
    pub unreleased: i64,
    /// No event on an open task this long.
    pub idle: i64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            unreleased: 15 * 60,
            idle: 24 * 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stuck {
    pub task: String,
    pub card: String,
    /// silent, unreleased, release_failed, unverified or idle.
    pub kind: String,
    /// Who should act: the lead, unless the turn is someone's who can.
    pub to: String,
    /// Seconds it has been so.
    pub since: i64,
    pub text: String,
}

/// The last sign of work on a task: its events, commits on its branches, runs
/// in its worktrees.
pub fn last_activity(context: &Context, task: &WorkTask, links: &[WorkLink]) -> i64 {
    let mut last = task.updated;
    for link in links {
        let seen = match link.kind.as_str() {
            "branch" => context
                .repo
                .git(&[
                    "log",
                    "-1",
                    "--format=%ct",
                    &format!("refs/heads/{}", link.value),
                ])
                .ok()
                .and_then(|time| time.trim().parse::<i64>().ok()),
            "worktree" => context
                .store
                .recent(&link.value, 1)
                .ok()
                .and_then(|runs| runs.into_iter().next())
                .map(|run| run.ended.unwrap_or(run.started).max(run.started)),
            _ => None,
        };
        last = last.max(seen.unwrap_or(0));
    }
    last
}

/// Why a task does not move, if it does not.
pub fn stuck(
    context: &Context,
    task: &WorkTask,
    progress: &Progress,
    limits: Limits,
) -> Result<Vec<Stuck>> {
    let mut found = Vec::new();
    if task.closed.is_some() || task.step == "closed" {
        return Ok(found);
    }
    let at = now() as i64;
    let lead_unless_owner = |turn: &str| match turn {
        "lead2" | "agent" | "owner" => turn.to_owned(),
        _ => "lead".to_owned(),
    };
    let mut push = |kind: &str, to: String, since: i64, text: String| {
        found.push(Stuck {
            task: task.id.clone(),
            card: task.card.clone(),
            kind: kind.to_owned(),
            to,
            since,
            text,
        });
    };
    let links = context.store.work_links(&task.id)?;
    if !task.claimed_by.is_empty() && !matches!(task.turn.as_str(), "owner" | "citrus") {
        let silent = at - last_activity(context, task, &links);
        if silent > task.lease {
            push(
                "silent",
                "lead".into(),
                silent,
                format!(
                    "{} claimed it and is silent for {} (lease {}); the turn is the lead's",
                    task.claimed_by,
                    crate::report::age(silent),
                    crate::report::age(task.lease)
                ),
            );
        }
    }
    if !progress.failed.is_empty() {
        push(
            "release_failed",
            lead_unless_owner(&task.turn),
            0,
            format!(
                "release failed: {} (citrus release resume/log)",
                progress.failed.join("; ")
            ),
        );
    }
    let landed = progress.merged.iter().map(|(_, time)| *time).min();
    let missing: Vec<&String> = progress
        .due
        .iter()
        .filter(|unit| !progress.released.contains(*unit))
        .collect();
    if let Some(landed) = landed
        && !missing.is_empty()
        && progress.failed.is_empty()
        && at - landed > limits.unreleased
    {
        let running = context
            .store
            .work_links(&task.id)?
            .iter()
            .filter(|link| link.kind == "release")
            .filter_map(|link| context.store.release(&link.value).ok().flatten())
            .any(|release| !release.finished());
        if !running {
            push(
                "unreleased",
                lead_unless_owner(&task.turn),
                at - landed,
                format!(
                    "merged {} ago, {} not released",
                    crate::report::age(at - landed),
                    missing
                        .iter()
                        .map(|unit| unit.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }
    }
    if task.step == "released"
        && let Some(released) = progress.released_at
        && at - released > limits.unreleased
    {
        push(
            "unverified",
            lead_unless_owner(&task.turn),
            at - released,
            format!(
                "released {} ago and not verified (citrus task verify {} \"what was checked\")",
                crate::report::age(at - released),
                task.id
            ),
        );
    }
    let idle = at - task.updated;
    if idle > limits.idle {
        push(
            "idle",
            "lead".into(),
            idle,
            format!(
                "no movement for {} ({}, turn {})",
                crate::report::age(idle),
                task.step,
                task.turn
            ),
        );
    }
    Ok(found)
}

/// One line about a task for people.
pub fn describe(task: &WorkTask) -> String {
    let mut line = format!("{} {:<9} turn {:<6}", task.id, task.step, task.turn);
    if !task.claimed_by.is_empty() {
        line.push_str(&format!(" · {}", task.claimed_by));
    }
    if !task.card.is_empty() {
        line.push_str(&format!(" · {}", task.card));
    }
    line.push_str(&format!(
        " · “{}” · {} ago",
        task.title,
        crate::report::age(now() as i64 - task.updated)
    ));
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_units() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("15m").unwrap(), 900);
        assert_eq!(parse_duration("2h").unwrap(), 7200);
        assert_eq!(parse_duration("1d").unwrap(), 86400);
        assert!(parse_duration("2w").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn verify_steps_are_named_so() {
        assert!(verify_step("verify"));
        assert!(verify_step("postcheck"));
        assert!(verify_step("smoke-postcheck"));
        assert!(!verify_step("deploy"));
        assert!(!verify_step("verifying"));
    }

    #[test]
    fn ids_are_slugs() {
        assert_eq!(new_id(Some("weekly-left")).unwrap(), "t-weekly-left");
        assert_eq!(new_id(Some("t-x1")).unwrap(), "t-x1");
        assert!(new_id(Some("Bad Id")).is_err());
        assert!(new_id(None).unwrap().starts_with("t-"));
        assert_eq!(turn_of("lead2"), "lead2");
        assert_eq!(turn_of("claude-hub"), "agent");
    }
}
