//! Tracked work (`citrus work new/claim/report/link/verify/close/show`,
//! `citrus works`): one piece of agent work from an owner-hub card to a
//! verified release.
//!
//! A task holds a step (open → claimed → in-queue → merged → released →
//! verified → closed) and whose turn it is (owner, lead, lead2, agent,
//! citrus). People move it with commands, each under a row lock; Citrus moves
//! it forward on what it already knows — the queue entries (`citrus queue add
//! --task`) and the releases linked to it — only if nobody moved it since it
//! was read. `citrus works --stuck` names the work nobody moves.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};

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
    let wrong = || anyhow::anyhow!("a duration is a number with s, m, h or d: {text}");
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(at) => text.split_at(at),
        None => (text, "s"),
    };
    let value: i64 = number.parse().map_err(|_| wrong())?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!(wrong()),
    };
    value.checked_mul(scale).ok_or_else(wrong)
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

/// A title is one line.
pub fn check_title(value: &str) -> Result<()> {
    ensure!(!value.trim().is_empty(), "a task has a title");
    ensure!(
        value.chars().count() <= 200 && !value.chars().any(char::is_control),
        "a task's title is one line of at most 200 characters"
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
    /// Linked entries, none landed and none waiting: each failed, conflicted
    /// or was cancelled (`entry state`).
    pub queue_failed: Vec<String>,
    /// A linked entry asked for releases the queue still runs.
    pub releasing: bool,
    /// Units with a passed release of the change.
    pub released: BTreeSet<String>,
    /// Units whose passed release ran a verify or postcheck step.
    pub verified: BTreeSet<String>,
    /// Releases that ended without passing (and no later one passed).
    pub failed: Vec<String>,
    /// A linked release has not ended.
    pub release_running: bool,
    /// When the last release passed.
    pub released_at: Option<i64>,
    /// Units the task must reach: its own and those its entries asked for.
    pub due: Vec<String>,
    /// Its links, read once (with the releases linked while reading).
    #[serde(skip)]
    pub links: Vec<WorkLink>,
}

/// Answers of `git merge-base --is-ancestor` within one command: many tasks
/// ask about the same merges and releases.
pub type Ancestry = RefCell<HashMap<(String, String), bool>>;

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

/// A release that counts for its unit: passed, or still on its way. Its
/// state is read as stored: whether its worker lives is known only on the
/// machine that runs it (`citrus release show` there settles it).
fn covers(release: &Release) -> bool {
    matches!(release.state.as_str(), "passed" | "running" | "queued")
}

/// What the task's links prove; links the releases its merges led to.
pub fn progress(context: &Context, task: &WorkTask, ancestry: &Ancestry) -> Result<Progress> {
    let store = &context.store;
    let mut progress = Progress {
        due: units_of(task),
        ..Progress::default()
    };
    let mut links = store.work_links(&task.id)?;
    let link_release = |links: &mut Vec<WorkLink>, id: &str| -> Result<()> {
        if !links
            .iter()
            .any(|link| link.kind == "release" && link.value == id)
        {
            store.work_link(&task.id, "release", id)?;
            links.push(WorkLink {
                kind: "release".into(),
                value: id.to_owned(),
                added: now() as i64,
            });
        }
        Ok(())
    };
    let queue: Vec<String> = links
        .iter()
        .filter(|link| link.kind == "queue")
        .map(|link| link.value.clone())
        .collect();
    let mut entries = 0;
    let mut failed_entries = Vec::new();
    for id in &queue {
        let Some(entry) = store.queue_entry(id)? else {
            continue;
        };
        entries += 1;
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
                    link_release(&mut links, &id)?;
                }
            }
        } else if matches!(entry.state.as_str(), "queued" | "checking") {
            progress.queued = true;
        } else {
            failed_entries.push(format!("{} {}", entry.id, entry.state));
        }
        if entry.state == "releasing" {
            progress.releasing = true;
        }
    }
    if entries > 0 && progress.merged.is_empty() && !progress.queued {
        progress.queue_failed = failed_entries;
    }
    // A due unit released later (by hand, from a newer base) still counts:
    // a release whose commit carries every merge of the task.
    if !progress.merged.is_empty() && !progress.due.is_empty() {
        let linked: Vec<Release> = links
            .iter()
            .filter(|link| link.kind == "release")
            .filter_map(|link| store.release(&link.value).ok().flatten())
            .collect();
        for unit in &progress.due {
            if linked
                .iter()
                .any(|release| release.unit == *unit && covers(release))
            {
                continue;
            }
            for release in store.releases_of(unit, 20)? {
                if release.kind == "release"
                    && covers(&release)
                    && carries(context, ancestry, &release.commit, &progress.merged)
                {
                    link_release(&mut links, &release.id)?;
                    break;
                }
            }
        }
    }
    let mut failed = Vec::new();
    for link in links.iter().filter(|link| link.kind == "release") {
        let Some(release) = store.release(&link.value)? else {
            continue;
        };
        if release.state == "passed" {
            let ended = release.ended.unwrap_or(release.started);
            progress.released.insert(release.unit.clone());
            progress.released_at = progress.released_at.max(Some(ended));
            if verifies(context, &release)? {
                progress.verified.insert(release.unit.clone());
            }
        } else if release.finished() {
            failed.push((release.unit.clone(), release.id.clone(), release.state));
        } else {
            progress.release_running = true;
        }
    }
    progress.failed = failed
        .into_iter()
        .filter(|(unit, _, _)| !progress.released.contains(unit))
        .map(|(unit, id, state)| format!("{unit} {id} {state}"))
        .collect();
    progress.links = links;
    Ok(progress)
}

/// The step the links prove, with the turn and the event that go with
/// reaching it.
fn derive(progress: &Progress, verified_by_hand: bool) -> (&'static str, &'static str, String) {
    let names = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
    if !progress.merged.is_empty() || !progress.released.is_empty() {
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
            return (
                "verified",
                "lead",
                format!("verified {}", names(&progress.verified)),
            );
        }
        if all_released {
            return (
                "released",
                "lead",
                format!(
                    "released {}; no verify or postcheck step proved it (citrus work verify)",
                    names(&progress.released)
                ),
            );
        }
        if progress.merged.is_empty() {
            return ("open", "lead", String::new());
        }
        if progress.releasing || progress.release_running {
            return (
                "merged",
                "citrus",
                format!("merged; Citrus releases {}", progress.due.join(", ")),
            );
        }
        if progress.due.is_empty() {
            return (
                "merged",
                "lead",
                "merged; no release asked (close it, or link the release)".into(),
            );
        }
        return (
            "merged",
            "lead",
            format!("merged; release {} is due", progress.due.join(", ")),
        );
    }
    if progress.queued {
        return ("in-queue", "citrus", "queued: Citrus lands it".into());
    }
    ("open", "lead", String::new())
}

/// Bring a task up to what its links prove: the step moves forward (never
/// back); from `merged` on the claim is released, the claimant's part being
/// done; a queue that failed every entry gives the turn back. Written only if
/// nobody moved the task since it was read. Returns the task as stored.
pub fn refresh(
    context: &Context,
    task: WorkTask,
    ancestry: &Ancestry,
) -> Result<(WorkTask, Progress)> {
    let progress = progress(context, &task, ancestry)?;
    if task.closed.is_some() {
        return Ok((task, progress));
    }
    let verified_by_hand = progress.links.iter().any(|link| link.kind == "verified");
    let (step, turn, text) = derive(&progress, verified_by_hand);
    let mut next = task.clone();
    let mut text = text;
    if step_rank(step) > step_rank(&task.step) {
        next.step = step.to_owned();
        next.turn = turn.to_owned();
        if step_rank(step) >= step_rank("merged") {
            next.claimed_by.clear();
        }
    } else if task.step == "in-queue" && task.turn == "citrus" && !progress.queue_failed.is_empty()
    {
        next.turn = if task.claimed_by.is_empty() {
            "lead".into()
        } else {
            turn_of(&task.claimed_by).to_owned()
        };
        text = format!(
            "the queue did not land it: {}",
            progress.queue_failed.join(", ")
        );
    }
    if next.step == task.step && next.turn == task.turn && next.claimed_by == task.claimed_by {
        return Ok((task, progress));
    }
    if context.store.work_advance(&task, &next, &text)? {
        let stored = context.store.work_task(&task.id)?.unwrap_or(next);
        return Ok((stored, progress));
    }
    // Moved meanwhile (or closed): what is stored wins.
    let stored = context.store.work_task(&task.id)?.unwrap_or(task);
    Ok((stored, progress))
}

/// Whether `commit` carries every one of `merged`.
fn carries(context: &Context, ancestry: &Ancestry, commit: &str, merged: &[(String, i64)]) -> bool {
    merged.iter().all(|(merge, _)| {
        if merge == commit {
            return true;
        }
        let key = (merge.clone(), commit.to_owned());
        if let Some(known) = ancestry.borrow().get(&key) {
            return *known;
        }
        let carried = context
            .repo
            .git(&["merge-base", "--is-ancestor", merge, commit])
            .is_ok();
        ancestry.borrow_mut().insert(key, carried);
        carried
    })
}

/// Thresholds of `citrus works --stuck`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Merged but not released this long.
    pub unreleased: i64,
    /// Released but not verified this long.
    pub unverified: i64,
    /// No event, commit or run on an open task this long.
    pub idle: i64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            unreleased: 15 * 60,
            unverified: 2 * 3600,
            idle: 24 * 3600,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Stuck {
    pub task: String,
    pub card: String,
    /// silent, queue_failed, release_failed, unreleased, unverified or idle.
    pub kind: String,
    /// Who should act.
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
    // Whoever holds the turn, unless it is Citrus's: then the lead.
    let holder = match task.turn.as_str() {
        "citrus" => "lead".to_owned(),
        turn => turn.to_owned(),
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
    let active = last_activity(context, task, &progress.links);
    if !task.claimed_by.is_empty() && task.turn == turn_of(&task.claimed_by) {
        let silent = at - active;
        if silent > task.lease {
            push(
                "silent",
                "lead".into(),
                silent,
                format!(
                    "{} claimed it and is silent for {} (lease {})",
                    task.claimed_by,
                    crate::report::age(silent),
                    crate::report::age(task.lease)
                ),
            );
        }
    }
    if !progress.queue_failed.is_empty() {
        push(
            "queue_failed",
            holder.clone(),
            0,
            format!(
                "the queue did not land it: {} (fix, then citrus queue add)",
                progress.queue_failed.join(", ")
            ),
        );
    }
    if !progress.failed.is_empty() {
        push(
            "release_failed",
            holder.clone(),
            0,
            format!(
                "release failed: {} (citrus release log/resume)",
                progress.failed.join("; ")
            ),
        );
    }
    let landed = progress.merged.iter().map(|(_, time)| *time).min();
    let missing: Vec<&str> = progress
        .due
        .iter()
        .filter(|unit| !progress.released.contains(*unit))
        .map(String::as_str)
        .collect();
    if let Some(landed) = landed
        && !missing.is_empty()
        && progress.failed.is_empty()
        && !progress.releasing
        && !progress.release_running
        && at - landed > limits.unreleased
    {
        push(
            "unreleased",
            holder.clone(),
            at - landed,
            format!(
                "merged {} ago, {} not released",
                crate::report::age(at - landed),
                missing.join(", ")
            ),
        );
    }
    if task.step == "released"
        && let Some(released) = progress.released_at
        && at - released > limits.unverified
    {
        push(
            "unverified",
            holder.clone(),
            at - released,
            format!(
                "released {} ago and not verified: the unit has no verify or postcheck step (citrus work verify {} \"what was checked\")",
                crate::report::age(at - released),
                task.id
            ),
        );
    }
    let idle = at - active;
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
        // Too large for seconds: refused, not wrapped.
        assert!(parse_duration("9223372036854775807d").is_err());
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
    fn ids_and_titles() {
        assert_eq!(new_id(Some("weekly-left")).unwrap(), "t-weekly-left");
        assert_eq!(new_id(Some("t-x1")).unwrap(), "t-x1");
        assert!(new_id(Some("Bad Id")).is_err());
        assert!(new_id(None).unwrap().starts_with("t-"));
        assert_eq!(turn_of("lead2"), "lead2");
        assert_eq!(turn_of("claude-hub"), "agent");
        assert!(check_title("one line").is_ok());
        assert!(check_title("two\nlines").is_err());
        assert!(check_title("  ").is_err());
    }

    #[test]
    fn releases_cover_only_while_they_may_pass() {
        let release = |state: &str| Release {
            id: "rel-app-1".into(),
            unit: "app".into(),
            kind: "release".into(),
            environment: String::new(),
            version: String::new(),
            previous: String::new(),
            commit: String::new(),
            worktree: String::new(),
            agent: String::new(),
            state: state.into(),
            note: String::new(),
            pid: None,
            started: 0,
            ended: None,
            log: String::new(),
        };
        for state in ["passed", "running", "queued"] {
            assert!(covers(&release(state)), "{state}");
        }
        for state in ["failed", "unknown", "cancelled", "abandoned"] {
            assert!(!covers(&release(state)), "{state}");
        }
    }

    #[test]
    fn a_failed_queue_gives_the_turn_back_and_merges_free_the_claim() {
        let failed = Progress {
            queue_failed: vec!["q-1 failed".into()],
            ..Progress::default()
        };
        assert_eq!(derive(&failed, false).0, "open");
        let merged = Progress {
            merged: vec![("abc".into(), 1)],
            due: vec!["app".into()],
            ..Progress::default()
        };
        assert_eq!(derive(&merged, false).0, "merged");
        assert_eq!(derive(&merged, false).1, "lead");
        let releasing = Progress {
            releasing: true,
            ..merged
        };
        assert_eq!(derive(&releasing, false).1, "citrus");
    }

    /// A schema set up before the newest index (an earlier build) gets it
    /// on the next open (needs CITRUS_TEST_STATE).
    #[test]
    fn a_schema_without_the_newest_index_is_set_up_again() {
        let Ok(url) = std::env::var("CITRUS_TEST_STATE") else {
            return;
        };
        let schema = format!(
            "citrus_unit_index_{}",
            crate::exec::unique().replace('-', "_")
        );
        drop(crate::state::Store::open_schema(&url, &schema).unwrap());
        let mut client = crate::pool::connect(&url).unwrap();
        let exists = |client: &mut postgres::Client| -> bool {
            client
                .query_one(
                    "select exists (select 1 from pg_indexes where schemaname = $1 and indexname = 'releases_environment_commit')",
                    &[&schema],
                )
                .unwrap()
                .get(0)
        };
        client
            .batch_execute(&format!("drop index {schema}.releases_environment_commit"))
            .unwrap();
        assert!(!exists(&mut client));
        drop(crate::state::Store::open_schema(&url, &schema).unwrap());
        assert!(exists(&mut client));
        crate::state::Store::drop_schema(&url, &schema).unwrap();
    }

    /// A refresh that read the task before it was closed does not reopen it
    /// (needs CITRUS_TEST_STATE, as the CLI tests).
    #[test]
    fn a_stale_refresh_does_not_reopen_a_closed_task() {
        let Ok(url) = std::env::var("CITRUS_TEST_STATE") else {
            return;
        };
        let schema = format!(
            "citrus_unit_work_{}",
            crate::exec::unique().replace('-', "_")
        );
        let store = crate::state::Store::open_schema(&url, &schema).unwrap();
        let task = WorkTask {
            id: "t-race".into(),
            title: "race".into(),
            card: String::new(),
            step: "in-queue".into(),
            turn: "citrus".into(),
            claimed_by: "agent-a".into(),
            lease: DEFAULT_LEASE,
            units: String::new(),
            agent: "test".into(),
            created: 1,
            updated: 1,
            closed: None,
        };
        assert!(store.work_insert(&task, "opened").unwrap());
        // A refresh reads it…
        let seen = store.work_task("t-race").unwrap().unwrap();
        // …the lead closes it…
        store
            .work_modify("t-race", "lead", |task| {
                task.step = "closed".into();
                task.closed = Some(2);
                Ok("closed".into())
            })
            .unwrap();
        // …and the refresh writes what it saw the links prove.
        let mut next = seen.clone();
        next.step = "merged".into();
        next.turn = "lead".into();
        assert!(!store.work_advance(&seen, &next, "merged").unwrap());
        let stored = store.work_task("t-race").unwrap().unwrap();
        assert_eq!(stored.step, "closed");
        assert!(stored.closed.is_some());
        // A command on the closed task sees it closed (under the lock).
        let refused = store.work_modify("t-race", "agent-a", |task| {
            ensure!(task.closed.is_none(), "{} is closed", task.id);
            Ok("report".into())
        });
        assert!(refused.is_err());
        crate::state::Store::drop_schema(&url, &schema).unwrap();
    }
}
