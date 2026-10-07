//! Shared SQLite state: runs, their targets and the evidence they produced.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::manifest::now;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    id TEXT PRIMARY KEY,
    key TEXT UNIQUE,
    worktree TEXT NOT NULL,
    branch TEXT NOT NULL,
    agent TEXT NOT NULL,
    mode TEXT NOT NULL,
    state TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    snapshot TEXT NOT NULL,
    base TEXT,
    pid INTEGER,
    started INTEGER NOT NULL,
    ended INTEGER,
    exit INTEGER,
    log TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS run_targets (
    run TEXT NOT NULL,
    target TEXT NOT NULL,
    position INTEGER NOT NULL,
    result TEXT NOT NULL,
    reason TEXT NOT NULL,
    fingerprint TEXT,
    evidence_run TEXT,
    seconds INTEGER,
    exit INTEGER,
    first_error TEXT,
    PRIMARY KEY (run, target)
);
CREATE TABLE IF NOT EXISTS evidence (
    target TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    result TEXT NOT NULL,
    run TEXT NOT NULL,
    created INTEGER NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (target, kind, key)
);
CREATE TABLE IF NOT EXISTS task_notes (
    worktree TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    agent TEXT NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS releases (
    id TEXT PRIMARY KEY,
    unit TEXT NOT NULL,
    kind TEXT NOT NULL,
    environment TEXT NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    previous TEXT NOT NULL DEFAULT '',
    commit_id TEXT NOT NULL,
    worktree TEXT NOT NULL,
    agent TEXT NOT NULL,
    state TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    pid INTEGER,
    started INTEGER NOT NULL,
    ended INTEGER,
    log TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS release_steps (
    release TEXT NOT NULL,
    position INTEGER NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL,
    seconds INTEGER,
    exit INTEGER,
    first_error TEXT,
    PRIMARY KEY (release, name)
);
CREATE TABLE IF NOT EXISTS environment_locks (
    environment TEXT PRIMARY KEY,
    release TEXT NOT NULL,
    acquired INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS facts (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_latest ON evidence (target, created);
CREATE INDEX IF NOT EXISTS runs_worktree ON runs (worktree, started);
";

#[derive(Debug, Clone, Serialize)]
pub struct Run {
    pub id: String,
    pub key: Option<String>,
    pub worktree: String,
    pub branch: String,
    pub agent: String,
    pub mode: String,
    pub state: String,
    pub note: String,
    pub snapshot: String,
    pub base: Option<String>,
    pub pid: Option<i64>,
    pub started: i64,
    pub ended: Option<i64>,
    pub exit: Option<i64>,
    pub log: String,
    pub linked_log: Option<String>,
}

impl Run {
    pub fn finished(&self) -> bool {
        matches!(
            self.state.as_str(),
            "passed" | "failed" | "cancelled" | "unknown"
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunTarget {
    pub target: String,
    pub result: String,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub id: String,
    pub unit: String,
    /// `release` or `rollback`.
    pub kind: String,
    pub environment: String,
    pub version: String,
    pub previous: String,
    pub commit: String,
    pub worktree: String,
    pub agent: String,
    pub state: String,
    pub note: String,
    pub pid: Option<i64>,
    pub started: i64,
    pub ended: Option<i64>,
    pub log: String,
}

impl Release {
    pub fn finished(&self) -> bool {
        matches!(
            self.state.as_str(),
            "passed" | "failed" | "cancelled" | "abandoned" | "unknown"
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ReleaseStep {
    pub name: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Stats {
    pub runs_by_state: Vec<(String, i64)>,
    pub runs_by_agent: Vec<(String, i64)>,
    pub runs_by_mode: Vec<(String, i64)>,
    /// (result, targets, seconds spent)
    pub outcomes: Vec<(String, i64, i64)>,
    /// Seconds the reused targets took when they were proven.
    pub saved_seconds: i64,
    /// Times a `run` joined an already running run instead of starting a duplicate.
    pub joined_runs: i64,
}

#[derive(Debug, Clone)]
pub struct Evidence {
    pub key: String,
    pub result: String,
    pub run: String,
    pub created: i64,
    pub detail: String,
}

#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Store> {
        crate::repo::private_dir(dir)?;
        let conn = Connection::open(dir.join("state.db"))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        if conn.prepare("SELECT linked_log FROM runs LIMIT 0").is_err() {
            conn.execute_batch("ALTER TABLE runs ADD COLUMN linked_log TEXT")?;
        }
        Ok(Store { conn })
    }

    pub fn insert_run(&self, run: &Run, targets: &[RunTarget]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO runs (id, key, worktree, branch, agent, mode, state, note, snapshot, base, pid, started, ended, exit, log)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![run.id, run.key, run.worktree, run.branch, run.agent, run.mode, run.state, run.note, run.snapshot,
                    run.base, run.pid, run.started, run.ended, run.exit, run.log],
        )?;
        for (position, target) in targets.iter().enumerate() {
            tx.execute(
                "INSERT INTO run_targets (run, target, position, result, reason, fingerprint, evidence_run, seconds, exit, first_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![run.id, target.target, position as i64, target.result, target.reason, target.fingerprint,
                        target.evidence_run, target.seconds, target.exit, target.first_error],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_pid(&self, run: &str, pid: i64) -> Result<()> {
        self.conn
            .execute("UPDATE runs SET pid = ?2 WHERE id = ?1", params![run, pid])?;
        Ok(())
    }

    pub fn set_state(&self, run: &str, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state = ?2 WHERE id = ?1",
            params![run, state],
        )?;
        Ok(())
    }

    pub fn insert_release(&self, release: &Release, steps: &[String]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO releases (id, unit, kind, environment, version, previous, commit_id, worktree, agent, state, note, pid, started, ended, log)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![release.id, release.unit, release.kind, release.environment, release.version, release.previous,
                    release.commit, release.worktree, release.agent, release.state, release.note, release.pid,
                    release.started, release.ended, release.log],
        )?;
        for (position, name) in steps.iter().enumerate() {
            tx.execute(
                "INSERT INTO release_steps (release, position, name, state) VALUES (?1, ?2, ?3, 'pending')",
                params![release.id, position as i64, name],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Take the environment for `release` unless an unfinished (or unknown) release holds it.
    /// Returns the holder when the lock is taken by someone else.
    pub fn lock_environment(&self, environment: &str, release: &str) -> Result<Option<String>> {
        let tx = self.conn.unchecked_transaction()?;
        let holder: Option<(String, String)> = tx
            .query_row(
                "SELECT l.release, COALESCE(r.state, 'gone') FROM environment_locks l LEFT JOIN releases r ON r.id = l.release
                 WHERE l.environment = ?1",
                params![environment],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((holder, state)) = holder
            && holder != release
            && matches!(state.as_str(), "queued" | "running" | "unknown")
        {
            return Ok(Some(holder));
        }
        tx.execute(
            "INSERT INTO environment_locks (environment, release, acquired) VALUES (?1, ?2, ?3)
             ON CONFLICT (environment) DO UPDATE SET release = ?2, acquired = ?3",
            params![environment, release, now() as i64],
        )?;
        tx.commit()?;
        Ok(None)
    }

    pub fn unlock_environment(&self, environment: &str, release: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM environment_locks WHERE environment = ?1 AND release = ?2",
            params![environment, release],
        )?;
        Ok(())
    }

    pub fn environment_holder(&self, environment: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT release FROM environment_locks WHERE environment = ?1",
                params![environment],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn release(&self, id: &str) -> Result<Option<Release>> {
        Ok(self
            .conn
            .query_row(
                &format!("{RELEASE_SELECT} WHERE id = ?1"),
                params![id],
                release_row,
            )
            .optional()?)
    }

    /// Full id, unique prefix, or `last` (latest release started from this worktree).
    pub fn resolve_release(&self, reference: &str, worktree: &str) -> Result<Release> {
        if reference == "last" {
            return self
                .conn
                .query_row(&format!("{RELEASE_SELECT} WHERE worktree = ?1 ORDER BY started DESC, id DESC LIMIT 1"), params![worktree], release_row)
                .optional()?
                .context("no releases from this worktree yet");
        }
        if let Some(release) = self.release(reference)? {
            return Ok(release);
        }
        let mut statement = self
            .conn
            .prepare(&format!("{RELEASE_SELECT} WHERE id LIKE ?1 || '%' LIMIT 2"))?;
        let found = statement
            .query_map(params![reference], release_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match found.len() {
            1 => Ok(found.into_iter().next().unwrap_or_else(|| unreachable!())),
            0 => bail!("no release {reference}"),
            _ => bail!("release prefix {reference} is ambiguous"),
        }
    }

    pub fn releases_of(&self, unit: &str, limit: i64) -> Result<Vec<Release>> {
        let mut statement = self.conn.prepare(&format!(
            "{RELEASE_SELECT} WHERE unit = ?1 ORDER BY started DESC, id DESC LIMIT ?2"
        ))?;
        Ok(statement
            .query_map(params![unit, limit], release_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The latest passed release (not rollback) of a unit.
    pub fn last_passed_release(&self, unit: &str) -> Result<Option<Release>> {
        Ok(self
            .conn
            .query_row(
                &format!("{RELEASE_SELECT} WHERE unit = ?1 AND state = 'passed' AND kind = 'release' ORDER BY started DESC LIMIT 1"),
                params![unit],
                release_row,
            )
            .optional()?)
    }

    pub fn set_release(&self, id: &str, state: &str, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE releases SET state = ?2, note = ?3 WHERE id = ?1",
            params![id, state, note],
        )?;
        Ok(())
    }

    pub fn set_release_version(&self, id: &str, version: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE releases SET version = ?2 WHERE id = ?1",
            params![id, version],
        )?;
        Ok(())
    }

    pub fn set_release_pid(&self, id: &str, pid: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE releases SET pid = ?2 WHERE id = ?1",
            params![id, pid],
        )?;
        Ok(())
    }

    pub fn finish_release(&self, id: &str, state: &str, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE releases SET state = ?2, note = ?3, ended = ?4 WHERE id = ?1",
            params![id, state, note, now() as i64],
        )?;
        Ok(())
    }

    pub fn release_steps(&self, id: &str) -> Result<Vec<ReleaseStep>> {
        let mut statement = self.conn.prepare(
            "SELECT name, state, seconds, exit, first_error FROM release_steps WHERE release = ?1 ORDER BY position",
        )?;
        Ok(statement
            .query_map(params![id], |row| {
                Ok(ReleaseStep {
                    name: row.get(0)?,
                    state: row.get(1)?,
                    seconds: row.get(2)?,
                    exit: row.get(3)?,
                    first_error: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_step(&self, id: &str, step: &ReleaseStep) -> Result<()> {
        self.conn.execute(
            "UPDATE release_steps SET state = ?3, seconds = ?4, exit = ?5, first_error = ?6 WHERE release = ?1 AND name = ?2",
            params![id, step.name, step.state, step.seconds, step.exit, step.first_error],
        )?;
        Ok(())
    }

    /// What a worktree's owner wants others to know (intent, blocker); empty clears it.
    pub fn set_task_note(&self, worktree: &str, text: &str, agent: &str) -> Result<()> {
        if text.trim().is_empty() {
            self.conn.execute(
                "DELETE FROM task_notes WHERE worktree = ?1",
                params![worktree],
            )?;
        } else {
            self.conn.execute(
                "INSERT INTO task_notes (worktree, text, agent, updated) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (worktree) DO UPDATE SET text = ?2, agent = ?3, updated = ?4",
                params![worktree, text.trim(), agent, now() as i64],
            )?;
        }
        Ok(())
    }

    /// (worktree, text, agent, updated)
    pub fn task_notes(&self) -> Result<Vec<(String, String, String, i64)>> {
        let mut statement = self.conn.prepare(
            "SELECT worktree, text, agent, updated FROM task_notes ORDER BY updated DESC",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The latest run of every worktree (one row each).
    pub fn latest_runs(&self) -> Result<Vec<Run>> {
        let mut statement = self.conn.prepare(&format!(
            "{RUN_SELECT} WHERE id IN (SELECT id FROM runs r WHERE started = (SELECT MAX(started) FROM runs WHERE worktree = r.worktree))"
        ))?;
        Ok(statement
            .query_map([], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A small named value with its update time (cached observations).
    pub fn fact(&self, name: &str) -> Result<Option<(String, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value, updated FROM facts WHERE name = ?1",
                params![name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }

    pub fn set_fact(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO facts (name, value, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET value = ?2, updated = ?3",
            params![name, value, now() as i64],
        )?;
        Ok(())
    }

    /// Usage since `since`: runs by state/agent, target outcomes, time saved by reuse.
    pub fn stats(&self, since: i64) -> Result<Stats> {
        let count_by = |column: &str| -> Result<Vec<(String, i64)>> {
            let mut statement = self.conn.prepare(&format!(
                "SELECT {column}, COUNT(*) FROM runs WHERE started >= ?1 GROUP BY {column} ORDER BY COUNT(*) DESC"
            ))?;
            Ok(statement
                .query_map(params![since], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let mut outcomes = self.conn.prepare(
            "SELECT t.result, COUNT(*), COALESCE(SUM(t.seconds), 0) FROM run_targets t JOIN runs r ON r.id = t.run
             WHERE r.started >= ?1 GROUP BY t.result ORDER BY COUNT(*) DESC",
        )?;
        let outcomes = outcomes
            .query_map(params![since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let saved_seconds: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(e.seconds), 0) FROM run_targets t JOIN runs r ON r.id = t.run
             JOIN run_targets e ON e.run = t.evidence_run AND e.target = t.target
             WHERE r.started >= ?1 AND t.result = 'reused'",
            params![since],
            |row| row.get(0),
        )?;
        let joined: i64 = self
            .fact("joined_runs")?
            .and_then(|(value, _)| value.parse().ok())
            .unwrap_or_default();
        Ok(Stats {
            runs_by_state: count_by("state")?,
            runs_by_agent: count_by("agent")?,
            runs_by_mode: count_by("mode")?,
            outcomes,
            saved_seconds,
            joined_runs: joined,
        })
    }

    pub fn set_linked_log(&self, run: &str, path: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET linked_log = ?2 WHERE id = ?1",
            params![run, path],
        )?;
        Ok(())
    }

    pub fn set_note(&self, run: &str, note: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET note = ?2 WHERE id = ?1",
            params![run, note],
        )?;
        Ok(())
    }

    pub fn finish(&self, run: &str, state: &str, exit: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state = ?2, exit = ?3, ended = ?4 WHERE id = ?1 AND ended IS NULL",
            params![run, state, exit, now() as i64],
        )?;
        Ok(())
    }

    pub fn update_target(&self, run: &str, target: &RunTarget) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE run_targets SET result = ?3, reason = ?4, fingerprint = COALESCE(?5, fingerprint),
                 evidence_run = COALESCE(?6, evidence_run), seconds = ?7, exit = ?8, first_error = ?9
             WHERE run = ?1 AND target = ?2",
            params![run, target.target, target.result, target.reason, target.fingerprint, target.evidence_run,
                    target.seconds, target.exit, target.first_error],
        )?;
        if changed == 0 {
            let position: i64 = self.conn.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM run_targets WHERE run = ?1",
                params![run],
                |row| row.get(0),
            )?;
            self.conn.execute(
                "INSERT INTO run_targets (run, target, position, result, reason, fingerprint, evidence_run, seconds, exit, first_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![run, target.target, position, target.result, target.reason, target.fingerprint,
                        target.evidence_run, target.seconds, target.exit, target.first_error],
            )?;
        }
        Ok(())
    }

    pub fn run(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row(&format!("{RUN_SELECT} WHERE id = ?1"), params![id], run_row)
            .optional()?)
    }

    pub fn run_by_key(&self, key: &str) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row(
                &format!("{RUN_SELECT} WHERE key = ?1"),
                params![key],
                run_row,
            )
            .optional()?)
    }

    /// Accepts a full id, a unique prefix, or `last` for this worktree.
    pub fn resolve(&self, reference: &str, worktree: &str) -> Result<Run> {
        if reference == "last" {
            return self
                .conn
                .query_row(
                    &format!(
                        "{RUN_SELECT} WHERE worktree = ?1 ORDER BY started DESC, id DESC LIMIT 1"
                    ),
                    params![worktree],
                    run_row,
                )
                .optional()?
                .context("no runs in this worktree yet");
        }
        if let Some(run) = self.run(reference)? {
            return Ok(run);
        }
        let mut statement = self
            .conn
            .prepare(&format!("{RUN_SELECT} WHERE id LIKE ?1 || '%' LIMIT 2"))?;
        let matches = statement
            .query_map(params![reference], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match matches.len() {
            1 => Ok(matches.into_iter().next().unwrap_or_else(|| unreachable!())),
            0 => bail!("no run {reference}"),
            _ => bail!("run prefix {reference} is ambiguous"),
        }
    }

    pub fn targets(&self, run: &str) -> Result<Vec<RunTarget>> {
        let mut statement = self.conn.prepare(
            "SELECT target, result, reason, fingerprint, evidence_run, seconds, exit, first_error
             FROM run_targets WHERE run = ?1 ORDER BY position",
        )?;
        let rows = statement.query_map(params![run], |row| {
            Ok(RunTarget {
                target: row.get(0)?,
                result: row.get(1)?,
                reason: row.get(2)?,
                fingerprint: row.get(3)?,
                evidence_run: row.get(4)?,
                seconds: row.get(5)?,
                exit: row.get(6)?,
                first_error: row.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Runs not finished yet, across all worktrees.
    pub fn active(&self) -> Result<Vec<Run>> {
        let mut statement = self.conn.prepare(&format!(
            "{RUN_SELECT} WHERE state IN ('queued', 'waiting', 'running') ORDER BY started"
        ))?;
        Ok(statement
            .query_map([], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn recent(&self, worktree: &str, limit: i64) -> Result<Vec<Run>> {
        let mut statement = self.conn.prepare(&format!(
            "{RUN_SELECT} WHERE worktree = ?1 ORDER BY started DESC, id DESC LIMIT ?2"
        ))?;
        Ok(statement
            .query_map(params![worktree, limit], run_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn put_evidence(
        &self,
        target: &str,
        kind: &str,
        key: &str,
        result: &str,
        run: &str,
        detail: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO evidence (target, kind, key, result, run, created, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (target, kind, key) DO UPDATE SET result = ?4, run = ?5, created = ?6, detail = ?7",
            params![target, kind, key, result, run, now() as i64, detail],
        )?;
        Ok(())
    }

    pub fn evidence(&self, target: &str, kind: &str, key: &str) -> Result<Option<Evidence>> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, result, run, created, detail FROM evidence WHERE target = ?1 AND kind = ?2 AND key = ?3",
                params![target, kind, key],
                evidence_row,
            )
            .optional()?)
    }

    pub fn latest_evidence(&self, target: &str, kind: &str) -> Result<Option<Evidence>> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, result, run, created, detail FROM evidence WHERE target = ?1 AND kind = ?2
                 ORDER BY created DESC LIMIT 1",
                params![target, kind],
                evidence_row,
            )
            .optional()?)
    }
}

const RELEASE_SELECT: &str = "SELECT id, unit, kind, environment, version, previous, commit_id, worktree, agent, state, note, pid, started, ended, log FROM releases";

fn release_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Release> {
    Ok(Release {
        id: row.get(0)?,
        unit: row.get(1)?,
        kind: row.get(2)?,
        environment: row.get(3)?,
        version: row.get(4)?,
        previous: row.get(5)?,
        commit: row.get(6)?,
        worktree: row.get(7)?,
        agent: row.get(8)?,
        state: row.get(9)?,
        note: row.get(10)?,
        pid: row.get(11)?,
        started: row.get(12)?,
        ended: row.get(13)?,
        log: row.get(14)?,
    })
}

const RUN_SELECT: &str = "SELECT id, key, worktree, branch, agent, mode, state, note, snapshot, base, pid, started, ended, exit, log, linked_log FROM runs";

fn run_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: row.get(0)?,
        key: row.get(1)?,
        worktree: row.get(2)?,
        branch: row.get(3)?,
        agent: row.get(4)?,
        mode: row.get(5)?,
        state: row.get(6)?,
        note: row.get(7)?,
        snapshot: row.get(8)?,
        base: row.get(9)?,
        pid: row.get(10)?,
        started: row.get(11)?,
        ended: row.get(12)?,
        exit: row.get(13)?,
        log: row.get(14)?,
        linked_log: row.get(15)?,
    })
}

fn evidence_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Evidence> {
    Ok(Evidence {
        key: row.get(0)?,
        result: row.get(1)?,
        run: row.get(2)?,
        created: row.get(3)?,
        detail: row.get(4)?,
    })
}
