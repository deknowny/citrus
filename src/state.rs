//! Shared SQLite state: runs, their targets and the evidence they produced.

use std::fs;
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
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
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
