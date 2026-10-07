//! Every worktree of the clone as a task: where it is, what it runs, what its
//! owner wants others to know.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::exec::Context;
use crate::manifest::now;

#[derive(Debug, Serialize)]
pub struct Task {
    pub worktree: String,
    pub name: String,
    pub branch: String,
    pub head: String,
    /// Commits on this task that the base does not have, and the reverse.
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    /// Seconds since the last commit.
    pub idle: Option<i64>,
    pub note: Option<String>,
    pub note_agent: Option<String>,
    pub last_run: Option<String>,
    pub last_run_state: Option<String>,
    pub running: bool,
    pub missing: bool,
    pub current: bool,
}

pub fn list(context: &Context, base: &str) -> Result<Vec<Task>> {
    let repo = &context.repo;
    let listing = repo.git(&["worktree", "list", "--porcelain"])?;
    let notes = context.store.task_notes()?;
    let runs = context.store.latest_runs()?;
    let here = context.worktree();
    let base_commit = repo
        .git(&["rev-parse", "--verify", "--quiet", base])
        .unwrap_or_default();
    let mut tasks = Vec::new();
    for block in listing.split("\n\n") {
        let mut path = None;
        let mut head = String::new();
        let mut branch = String::from("(detached)");
        for line in block.lines() {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(value.to_owned());
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                head = value.to_owned();
            } else if let Some(value) = line.strip_prefix("branch ") {
                branch = value.trim_start_matches("refs/heads/").to_owned();
            } else if line == "bare" {
                path = None;
            }
        }
        let Some(path) = path else { continue };
        let (mut ahead, mut behind) = (None, None);
        if !base_commit.is_empty()
            && !head.is_empty()
            && let Ok(counts) = repo.git(&[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{head}...{base_commit}"),
            ])
        {
            let mut parts = counts
                .split_whitespace()
                .map(|part| part.parse::<u64>().ok());
            ahead = parts.next().flatten();
            behind = parts.next().flatten();
        }
        let idle = repo
            .git(&["log", "-1", "--format=%ct", &head])
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .map(|time| now() as i64 - time);
        let note = notes.iter().find(|(worktree, ..)| *worktree == path);
        let run = runs.iter().find(|run| run.worktree == path);
        tasks.push(Task {
            name: Path::new(&path)
                .file_name()
                .map_or(path.clone(), |name| name.to_string_lossy().into_owned()),
            missing: !Path::new(&path).exists(),
            current: path == here,
            branch,
            head: head.chars().take(10).collect(),
            ahead,
            behind,
            idle,
            note: note.map(|(_, text, ..)| text.clone()),
            note_agent: note.map(|(_, _, agent, _)| agent.clone()),
            last_run: run.map(|run| run.id.clone()),
            last_run_state: run.map(|run| run.state.clone()),
            running: run.is_some_and(|run| !run.finished()),
            worktree: path,
        });
    }
    // Most relevant first: running, noted, unmerged work, then by recency.
    tasks.sort_by_key(|task| {
        (
            !task.current,
            !task.running,
            task.note.is_none(),
            task.ahead.unwrap_or(0) == 0,
            task.idle.unwrap_or(i64::MAX),
        )
    });
    Ok(tasks)
}

/// Worth showing without `--all`: something is happening or unmerged there.
pub fn active(task: &Task) -> bool {
    task.current
        || task.running
        || task.note.is_some()
        || (task.ahead.unwrap_or(0) > 0 && task.idle.unwrap_or(i64::MAX) < 3 * 86400)
}
