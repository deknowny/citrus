//! Shared state in Postgres: runs, their targets, the evidence they produced,
//! releases, tasks, agreements, versions and tracked work (`work_*`). Each repository has its own
//! schema in the database the pool uses (or CITRUS_STATE).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
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
    pid BIGINT,
    started BIGINT NOT NULL,
    ended BIGINT,
    exit BIGINT,
    log TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS run_targets (
    run TEXT NOT NULL,
    target TEXT NOT NULL,
    position BIGINT NOT NULL,
    result TEXT NOT NULL,
    reason TEXT NOT NULL,
    fingerprint TEXT,
    evidence_run TEXT,
    seconds BIGINT,
    exit BIGINT,
    first_error TEXT,
    PRIMARY KEY (run, target)
);
CREATE TABLE IF NOT EXISTS evidence (
    target TEXT NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL,
    result TEXT NOT NULL,
    run TEXT NOT NULL,
    created BIGINT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (target, kind, key)
);
CREATE TABLE IF NOT EXISTS tasks (
    worktree TEXT PRIMARY KEY,
    title TEXT NOT NULL DEFAULT '',
    scope TEXT NOT NULL DEFAULT '',
    blocked TEXT NOT NULL DEFAULT '',
    needs TEXT NOT NULL DEFAULT '',
    evidence TEXT NOT NULL DEFAULT '',
    agent TEXT NOT NULL,
    updated BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS agreements (
    key TEXT PRIMARY KEY,
    terms TEXT NOT NULL,
    reopen TEXT NOT NULL,
    evidence TEXT NOT NULL,
    revision BIGINT NOT NULL,
    owner TEXT NOT NULL,
    updated BIGINT NOT NULL
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
    pid BIGINT,
    started BIGINT NOT NULL,
    ended BIGINT,
    log TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS release_steps (
    release TEXT NOT NULL,
    position BIGINT NOT NULL,
    name TEXT NOT NULL,
    state TEXT NOT NULL,
    seconds BIGINT,
    exit BIGINT,
    first_error TEXT,
    PRIMARY KEY (release, name)
);
CREATE TABLE IF NOT EXISTS environment_locks (
    environment TEXT PRIMARY KEY,
    release TEXT NOT NULL,
    acquired BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS artifact_builds (
    artifact TEXT NOT NULL,
    key TEXT NOT NULL,
    reference TEXT NOT NULL,
    created BIGINT NOT NULL,
    PRIMARY KEY (artifact, key)
);
CREATE TABLE IF NOT EXISTS versions (
    version TEXT NOT NULL,
    item TEXT NOT NULL,
    scope TEXT NOT NULL,
    start TEXT NOT NULL,
    owner TEXT NOT NULL,
    source TEXT NOT NULL,
    agent TEXT NOT NULL,
    created BIGINT NOT NULL,
    PRIMARY KEY (version, item)
);
CREATE TABLE IF NOT EXISTS facts (
    name TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated BIGINT NOT NULL
);
CREATE INDEX IF NOT EXISTS evidence_latest ON evidence (target, created);
CREATE INDEX IF NOT EXISTS runs_worktree ON runs (worktree, started);
CREATE TABLE IF NOT EXISTS queue (
    id TEXT PRIMARY KEY,
    seq BIGSERIAL,
    worktree TEXT NOT NULL,
    branch TEXT NOT NULL,
    agent TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    base TEXT NOT NULL,
    releases TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    batch TEXT NOT NULL DEFAULT '',
    run TEXT NOT NULL DEFAULT '',
    merged TEXT NOT NULL DEFAULT '',
    started_releases TEXT NOT NULL DEFAULT '',
    added BIGINT NOT NULL,
    updated BIGINT NOT NULL,
    ended BIGINT,
    UNIQUE (worktree, commit_sha, base)
);
CREATE INDEX IF NOT EXISTS queue_state ON queue (state, added);
CREATE TABLE IF NOT EXISTS work_tasks (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    card TEXT NOT NULL DEFAULT '',
    step TEXT NOT NULL,
    turn TEXT NOT NULL,
    claimed_by TEXT NOT NULL DEFAULT '',
    lease BIGINT NOT NULL DEFAULT 7200,
    units TEXT NOT NULL DEFAULT '',
    agent TEXT NOT NULL,
    created BIGINT NOT NULL,
    updated BIGINT NOT NULL,
    closed BIGINT
);
CREATE TABLE IF NOT EXISTS work_links (
    task TEXT NOT NULL,
    kind TEXT NOT NULL,
    value TEXT NOT NULL,
    added BIGINT NOT NULL,
    PRIMARY KEY (task, kind, value)
);
CREATE TABLE IF NOT EXISTS work_events (
    seq BIGSERIAL PRIMARY KEY,
    task TEXT NOT NULL,
    at BIGINT NOT NULL,
    agent TEXT NOT NULL,
    step TEXT NOT NULL,
    turn TEXT NOT NULL,
    text TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS work_events_task ON work_events (task, seq);
CREATE INDEX IF NOT EXISTS work_links_value ON work_links (kind, value);
CREATE INDEX IF NOT EXISTS releases_unit ON releases (unit, started);
CREATE INDEX IF NOT EXISTS releases_environment_commit ON releases (environment, commit_id);
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

/// What a task is doing and what blocks it (`citrus task`).
#[derive(Debug, Clone, Default, Serialize)]
pub struct TaskInfo {
    pub worktree: String,
    pub title: String,
    pub scope: String,
    pub blocked: String,
    pub needs: String,
    pub evidence: String,
    pub agent: String,
    pub updated: i64,
}

/// Fields `citrus task` sets; `None` keeps the stored value.
#[derive(Debug, Default)]
pub struct TaskChange {
    pub title: Option<String>,
    pub scope: Option<String>,
    pub blocked: Option<String>,
    pub needs: Option<String>,
    pub evidence: Option<String>,
    pub clear_blocker: bool,
    pub clear: bool,
}

/// A decision between tasks: who does what, and when it is reopened.
#[derive(Debug, Clone, Serialize)]
pub struct Agreement {
    pub key: String,
    pub terms: String,
    pub reopen: String,
    pub evidence: String,
    pub revision: i64,
    pub owner: String,
    pub updated: i64,
}

/// A version held by one task's committed source for a set of names (the
/// images or packages it publishes); another set may hold the same version.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reservation {
    pub version: String,
    pub scope: Vec<String>,
    /// The version asked for; a retry with the same start returns this one.
    pub start: String,
    /// The worktree whose task holds it.
    pub owner: String,
    pub source: String,
    pub agent: String,
    pub created: i64,
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

type Param<'a> = &'a (dyn postgres::types::ToSql + Sync);

macro_rules! params {
    ($($value:expr),* $(,)?) => {
        &[$(&$value as Param),*] as &[Param]
    };
}

/// A row read back, with fallible getters.
struct Row<'a>(&'a postgres::Row);

impl Row<'_> {
    fn get<I, T>(&self, index: I) -> Result<T>
    where
        I: postgres::row::RowIndex + std::fmt::Display,
        T: for<'b> postgres::types::FromSql<'b>,
    {
        Ok(self.0.try_get(index)?)
    }
}

/// `?N` placeholders in Postgres' `$N` (CITRUS_TRACE_SQL prints each).
fn sql(text: &str) -> String {
    if std::env::var_os("CITRUS_TRACE_SQL").is_some() {
        eprintln!(
            "SQL {}",
            text.split_whitespace().collect::<Vec<_>>().join(" ")
        );
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '?' && chars.peek().is_some_and(char::is_ascii_digit) {
            out.push('$');
        } else {
            out.push(c);
        }
    }
    out
}

struct Conn {
    client: RefCell<postgres::Client>,
    /// Statements prepared once: a cached one costs one round trip, not two.
    prepared: RefCell<HashMap<String, postgres::Statement>>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Conn")
    }
}

impl Conn {
    fn statement(&self, text: &str) -> Result<postgres::Statement> {
        let text = sql(text);
        if let Some(statement) = self.prepared.borrow().get(&text) {
            return Ok(statement.clone());
        }
        let statement = self.client.borrow_mut().prepare(&text)?;
        self.prepared.borrow_mut().insert(text, statement.clone());
        Ok(statement)
    }

    fn execute(&self, text: &str, params: &[Param]) -> Result<usize> {
        let statement = self.statement(text)?;
        Ok(self.client.borrow_mut().execute(&statement, params)? as usize)
    }

    fn query_row<T>(
        &self,
        text: &str,
        params: &[Param],
        f: impl FnOnce(&Row) -> Result<T>,
    ) -> Result<Option<T>> {
        let statement = self.statement(text)?;
        match self.client.borrow_mut().query_opt(&statement, params)? {
            Some(row) => Ok(Some(f(&Row(&row))?)),
            None => Ok(None),
        }
    }

    fn query_map<T>(
        &self,
        text: &str,
        params: &[Param],
        mut f: impl FnMut(&Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        let statement = self.statement(text)?;
        self.client
            .borrow_mut()
            .query(&statement, params)?
            .iter()
            .map(|row| f(&Row(row)))
            .collect()
    }

    fn transaction<T>(
        &self,
        f: impl FnOnce(&mut postgres::Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut client = self.client.borrow_mut();
        let mut transaction = client.transaction()?;
        let value = f(&mut transaction)?;
        transaction.commit()?;
        Ok(value)
    }
}

fn tx_execute(
    transaction: &mut postgres::Transaction<'_>,
    text: &str,
    params: &[Param],
) -> Result<usize> {
    Ok(transaction.execute(&sql(text), params)? as usize)
}

/// Evidence and pass times, read once per process: deciding a plan asks for
/// them for every target, and each question would be a round trip.
#[derive(Debug, Default)]
struct Cache {
    facts: Option<HashMap<String, (String, i64)>>,
    evidence: Option<HashMap<(String, String), Vec<Evidence>>>,
    typical: Option<HashMap<String, i64>>,
}

#[derive(Debug)]
pub struct Store {
    conn: Conn,
    cache: RefCell<Cache>,
}

/// The database holding Citrus's state: CITRUS_STATE, ~/.config/citrus/state,
/// otherwise the pool's.
pub fn database_url() -> Result<String> {
    if let Ok(value) = std::env::var("CITRUS_STATE")
        && !value.trim().is_empty()
    {
        return Ok(value.trim().to_owned());
    }
    // This machine's own Postgres: interactive commands stay as fast as a
    // local file (the pool's database is a round trip away).
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    if let Ok(text) = std::fs::read_to_string(home.join(".config/citrus/state"))
        && let Some(url) = text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with('#'))
    {
        return Ok(url.to_owned());
    }
    crate::pool::url().context(
        "Citrus keeps its state in Postgres: set CITRUS_STATE (or join a pool: CITRUS_POOL, ~/.config/citrus/pool)",
    )
}

/// The schema of a repository: one per origin (every clone and machine
/// shares it), else per Git directory; CITRUS_STATE_SCHEMA overrides it.
fn schema_for(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    if let Ok(name) = std::env::var("CITRUS_STATE_SCHEMA")
        && !name.is_empty()
    {
        return name;
    }
    let repo = dir.parent().unwrap_or(dir);
    let origin = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(repo)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|url| !url.is_empty());
    let identity = origin.unwrap_or_else(|| repo.display().to_string());
    let digest = hex::encode(Sha256::digest(identity.as_bytes()));
    format!("citrus_{}", &digest[..16])
}

impl Store {
    /// The state of the repository whose Git directory holds `dir`
    /// (`<git-common-dir>/citrus`).
    pub fn open(dir: &Path) -> Result<Store> {
        Store::open_schema(&database_url()?, &schema_for(dir))
    }

    /// The state in `schema` of the database at `url`, set up when new.
    pub fn open_schema(url: &str, schema: &str) -> Result<Store> {
        let mut client = crate::pool::connect(url)?;
        let current: bool = client
            .query_one(
                // The newest object of SCHEMA: a schema without it is set up
                // again (every statement there is idempotent).
                "select exists (select 1 from pg_indexes
                 where schemaname = $1 and indexname = 'releases_environment_commit')",
                &[&schema],
            )?
            .get(0);
        if current {
            client.batch_execute(&format!("set search_path to {schema}"))?;
            return Ok(Store {
                conn: Conn {
                    client: RefCell::new(client),
                    prepared: RefCell::default(),
                },
                cache: RefCell::default(),
            });
        }
        // Several processes may set the schema up at once: one does, and the
        // lock is released only after its transaction committed.
        client.execute("select pg_advisory_lock(hashtext($1))", &[&schema])?;
        let setup = client.batch_execute(&format!(
            "begin;
             create schema if not exists {schema};
             set local search_path to {schema};
             set local lock_timeout = '30s';
             {SCHEMA}
             alter table runs add column if not exists linked_log text;
             commit;"
        ));
        client.execute("select pg_advisory_unlock(hashtext($1))", &[&schema])?;
        setup?;
        client.batch_execute(&format!("set search_path to {schema}"))?;
        Ok(Store {
            conn: Conn {
                client: RefCell::new(client),
                prepared: RefCell::default(),
            },
            cache: RefCell::default(),
        })
    }

    /// Drop a throwaway schema (an agent's batch).
    pub fn drop_schema(url: &str, schema: &str) -> Result<()> {
        let mut client = crate::pool::connect(url)?;
        client.batch_execute(&format!("drop schema if exists {schema} cascade"))?;
        Ok(())
    }

    pub fn insert_run(&self, run: &Run, targets: &[RunTarget]) -> Result<()> {
        self.conn.transaction(|tx| {
        tx_execute(tx,
            "INSERT INTO runs (id, key, worktree, branch, agent, mode, state, note, snapshot, base, pid, started, ended, exit, log)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![run.id, run.key, run.worktree, run.branch, run.agent, run.mode, run.state, run.note, run.snapshot,
                    run.base, run.pid, run.started, run.ended, run.exit, run.log],
        )?;
        for (position, target) in targets.iter().enumerate() {
            tx_execute(tx,
                "INSERT INTO run_targets (run, target, position, result, reason, fingerprint, evidence_run, seconds, exit, first_error)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![run.id, target.target, position as i64, target.result, target.reason, target.fingerprint,
                        target.evidence_run, target.seconds, target.exit, target.first_error],
            )?;
        }
        Ok(())
        })
    }

    pub fn set_pid(&self, run: &str, pid: i64) -> Result<()> {
        self.conn
            .execute("UPDATE runs SET pid = ?2 WHERE id = ?1", params![run, pid])?;
        Ok(())
    }

    /// A finished run keeps its result: a late worker (one that was followed
    /// again and lost) cannot make it "running" again.
    pub fn set_state(&self, run: &str, state: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state = ?2 WHERE id = ?1 AND ended IS NULL",
            params![run, state],
        )?;
        Ok(())
    }

    /// A run that ended but still says it runs (written by an older build)
    /// becomes `unknown`, so it can be closed.
    pub fn repair_ended(&self, run: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET state = 'unknown' WHERE id = ?1 AND ended IS NOT NULL
             AND state NOT IN ('passed', 'failed', 'cancelled', 'unknown')",
            params![run],
        )?;
        Ok(())
    }

    pub fn insert_release(&self, release: &Release, steps: &[String]) -> Result<()> {
        self.conn.transaction(|tx| {
        tx_execute(tx,
            "INSERT INTO releases (id, unit, kind, environment, version, previous, commit_id, worktree, agent, state, note, pid, started, ended, log)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![release.id, release.unit, release.kind, release.environment, release.version, release.previous,
                    release.commit, release.worktree, release.agent, release.state, release.note, release.pid,
                    release.started, release.ended, release.log],
        )?;
        for (position, name) in steps.iter().enumerate() {
            tx_execute(tx,
                "INSERT INTO release_steps (release, position, name, state) VALUES (?1, ?2, ?3, 'pending')",
                params![release.id, position as i64, name],
            )?;
        }
        Ok(())
        })
    }

    /// Take the environment for `release` unless an unfinished (or unknown) release holds it.
    /// Returns the holder when the lock is taken by someone else.
    pub fn lock_environment(&self, environment: &str, release: &str) -> Result<Option<String>> {
        self.conn.transaction(|tx| {
            // One lock taker at a time for this environment.
            tx_execute(tx, "SELECT pg_advisory_xact_lock(hashtext(?1))", params![environment])
                .or_else(|_| Ok::<usize, anyhow::Error>(0))?;
            let holder: Option<(String, String)> = tx
                .query_opt(
                    &sql("SELECT l.release, COALESCE(r.state, 'gone') FROM environment_locks l LEFT JOIN releases r ON r.id = l.release
                     WHERE l.environment = ?1"),
                    params![environment],
                )?
                .map(|row| (row.get(0), row.get(1)));
            if let Some((holder, state)) = holder
                && holder != release
                && matches!(state.as_str(), "queued" | "running" | "unknown")
            {
                return Ok(Some(holder));
            }
            tx_execute(tx,
                "INSERT INTO environment_locks (environment, release, acquired) VALUES (?1, ?2, ?3)
                 ON CONFLICT (environment) DO UPDATE SET release = ?2, acquired = ?3",
                params![environment, release, now() as i64],
            )?;
            Ok(None)
        })
    }

    pub fn unlock_environment(&self, environment: &str, release: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM environment_locks WHERE environment = ?1 AND release = ?2",
            params![environment, release],
        )?;
        Ok(())
    }

    pub fn environment_holder(&self, environment: &str) -> Result<Option<String>> {
        self.conn.query_row(
            "SELECT release FROM environment_locks WHERE environment = ?1",
            params![environment],
            |row| row.get(0),
        )
    }

    pub fn release(&self, id: &str) -> Result<Option<Release>> {
        self.conn.query_row(
            &format!("{RELEASE_SELECT} WHERE id = ?1"),
            params![id],
            release_row,
        )
    }

    /// Full id, unique prefix, or `last` (latest release started from this worktree).
    pub fn resolve_release(&self, reference: &str, worktree: &str) -> Result<Release> {
        if reference == "last" {
            return self
                .conn
                .query_row(&format!("{RELEASE_SELECT} WHERE worktree = ?1 ORDER BY started DESC, id DESC LIMIT 1"), params![worktree], release_row)?
                .context("no releases from this worktree yet");
        }
        if let Some(release) = self.release(reference)? {
            return Ok(release);
        }
        let found = self.conn.query_map(
            &format!("{RELEASE_SELECT} WHERE id LIKE ?1::text || '%' LIMIT 2"),
            params![reference],
            release_row,
        )?;
        match found.len() {
            1 => Ok(found.into_iter().next().unwrap_or_else(|| unreachable!())),
            0 => bail!("no release {reference}"),
            _ => bail!("release prefix {reference} is ambiguous"),
        }
    }

    pub fn releases_of(&self, unit: &str, limit: i64) -> Result<Vec<Release>> {
        self.conn.query_map(
            &format!("{RELEASE_SELECT} WHERE unit = ?1 ORDER BY started DESC, id DESC LIMIT ?2"),
            params![unit, limit],
            release_row,
        )
    }

    /// The latest passed release (not rollback) of a unit.
    pub fn last_passed_release(&self, unit: &str) -> Result<Option<Release>> {
        self.conn.query_row(
                &format!("{RELEASE_SELECT} WHERE unit = ?1 AND state = 'passed' AND kind = 'release' ORDER BY started DESC LIMIT 1"),
                params![unit],
                release_row,
            )
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
        self.conn.query_map(
            "SELECT name, state, seconds, exit, first_error FROM release_steps WHERE release = ?1 ORDER BY position", params![id], |row| {
                Ok(ReleaseStep {
                    name: row.get(0)?,
                    state: row.get(1)?,
                    seconds: row.get(2)?,
                    exit: row.get(3)?,
                    first_error: row.get(4)?,
                })
            })
    }

    pub fn update_step(&self, id: &str, step: &ReleaseStep) -> Result<()> {
        self.conn.execute(
            "UPDATE release_steps SET state = ?3, seconds = ?4, exit = ?5, first_error = ?6 WHERE release = ?1 AND name = ?2",
            params![id, step.name, step.state, step.seconds, step.exit, step.first_error],
        )?;
        Ok(())
    }

    /// Image built before from exactly these inputs.
    pub fn artifact_reference(&self, artifact: &str, key: &str) -> Result<Option<String>> {
        self.conn.query_row(
            "SELECT reference FROM artifact_builds WHERE artifact = ?1 AND key = ?2",
            params![artifact, key],
            |row| row.get(0),
        )
    }

    pub fn put_artifact_reference(&self, artifact: &str, key: &str, reference: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO artifact_builds (artifact, key, reference, created) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (artifact, key) DO UPDATE SET reference = ?3, created = ?4",
            params![artifact, key, reference, now() as i64],
        )?;
        Ok(())
    }

    /// What a worktree's task is doing, and what blocks it. Given fields
    /// replace the stored ones; `clear` forgets the task's description.
    pub fn update_task(
        &self,
        worktree: &str,
        change: &TaskChange,
        agent: &str,
    ) -> Result<TaskInfo> {
        let mut info = self
            .task_infos()?
            .into_iter()
            .find(|info| info.worktree == worktree)
            .unwrap_or(TaskInfo {
                worktree: worktree.to_owned(),
                ..TaskInfo::default()
            });
        if change.clear {
            self.conn
                .execute("DELETE FROM tasks WHERE worktree = ?1", params![worktree])?;
            return Ok(TaskInfo {
                worktree: worktree.to_owned(),
                ..TaskInfo::default()
            });
        }
        for (field, value) in [
            (&mut info.title, &change.title),
            (&mut info.scope, &change.scope),
            (&mut info.blocked, &change.blocked),
            (&mut info.needs, &change.needs),
            (&mut info.evidence, &change.evidence),
        ] {
            if let Some(value) = value {
                anyhow::ensure!(
                    value.chars().count() <= 1000
                        && !value
                            .chars()
                            .any(|c| c.is_control() && c != '\n' && c != '\t'),
                    "a task field holds at most 1000 characters of text"
                );
                *field = value.trim().to_owned();
            }
        }
        if change.clear_blocker {
            info.blocked.clear();
            info.needs.clear();
        }
        anyhow::ensure!(
            info.blocked.is_empty() == info.needs.is_empty(),
            "a blocker names both the blocked action (--blocked) and what it needs (--needs)"
        );
        info.agent = agent.to_owned();
        info.updated = now() as i64;
        self.conn.execute(
            "INSERT INTO tasks (worktree, title, scope, blocked, needs, evidence, agent, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (worktree) DO UPDATE SET title = ?2, scope = ?3, blocked = ?4, needs = ?5,
                 evidence = ?6, agent = ?7, updated = ?8",
            params![
                info.worktree,
                info.title,
                info.scope,
                info.blocked,
                info.needs,
                info.evidence,
                info.agent,
                info.updated
            ],
        )?;
        Ok(info)
    }

    /// Every described task, newest first.
    pub fn task_infos(&self) -> Result<Vec<TaskInfo>> {
        self.conn.query_map(
            "SELECT worktree, title, scope, blocked, needs, evidence, agent, updated FROM tasks ORDER BY updated DESC", params![], |row| {
                Ok(TaskInfo {
                    worktree: row.get(0)?,
                    title: row.get(1)?,
                    scope: row.get(2)?,
                    blocked: row.get(3)?,
                    needs: row.get(4)?,
                    evidence: row.get(5)?,
                    agent: row.get(6)?,
                    updated: row.get(7)?,
                })
            })
    }

    /// Record or revise an agreement between tasks. `revision` is the one the
    /// caller read: a concurrent revision is refused, not overwritten; the
    /// same content again changes nothing.
    pub fn agree(
        &self,
        key: &str,
        content: [&str; 3],
        revision: i64,
        owner: &str,
    ) -> Result<Agreement> {
        anyhow::ensure!(
            !key.is_empty()
                && key.len() <= 64
                && key
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !key.starts_with('-'),
            "an agreement key is a short lowercase slug"
        );
        let [terms, reopen, evidence] = content;
        anyhow::ensure!(
            !terms.trim().is_empty() && !reopen.trim().is_empty() && !evidence.trim().is_empty(),
            "an agreement keeps its terms, when to reopen it and the evidence of the decision"
        );
        let previous = self
            .agreements()?
            .into_iter()
            .find(|agreement| agreement.key == key);
        if let Some(previous) = &previous
            && previous.terms == terms.trim()
            && previous.reopen == reopen.trim()
            && previous.evidence == evidence.trim()
        {
            return Ok(previous.clone());
        }
        let current = previous.as_ref().map_or(0, |previous| previous.revision);
        anyhow::ensure!(
            revision == current,
            "agreement {key} is at revision {current}: read it (citrus tasks) and pass --revision {current}"
        );
        let agreement = Agreement {
            key: key.to_owned(),
            terms: terms.trim().to_owned(),
            reopen: reopen.trim().to_owned(),
            evidence: evidence.trim().to_owned(),
            revision: current + 1,
            owner: owner.to_owned(),
            updated: now() as i64,
        };
        let changed = self.conn.execute(
            "INSERT INTO agreements (key, terms, reopen, evidence, revision, owner, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (key) DO UPDATE SET terms = ?2, reopen = ?3, evidence = ?4, revision = ?5,
                 owner = ?6, updated = ?7
             WHERE agreements.revision = ?8",
            params![
                agreement.key,
                agreement.terms,
                agreement.reopen,
                agreement.evidence,
                agreement.revision,
                agreement.owner,
                agreement.updated,
                current
            ],
        )?;
        anyhow::ensure!(
            changed == 1,
            "agreement {key} changed meanwhile: read it again"
        );
        Ok(agreement)
    }

    pub fn agreements(&self) -> Result<Vec<Agreement>> {
        self.conn.query_map(
            "SELECT key, terms, reopen, evidence, revision, owner, updated FROM agreements ORDER BY key", params![], |row| {
                Ok(Agreement {
                    key: row.get(0)?,
                    terms: row.get(1)?,
                    reopen: row.get(2)?,
                    evidence: row.get(3)?,
                    revision: row.get(4)?,
                    owner: row.get(5)?,
                    updated: row.get(6)?,
                })
            })
    }

    /// Hold `reservation.version` for every name of its scope at once;
    /// false when another reservation holds it for one of them.
    pub fn try_reserve(&self, reservation: &Reservation) -> Result<bool> {
        self.conn
            .transaction(|transaction| {
                for item in &reservation.scope {
                    let inserted = tx_execute(transaction,
                "INSERT INTO versions (version, item, scope, start, owner, source, agent, created)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT DO NOTHING",
                params![
                    reservation.version,
                    item,
                    reservation.scope.join(","),
                    reservation.start,
                    reservation.owner,
                    reservation.source,
                    reservation.agent,
                    reservation.created
                ],
            )?;
                    if inserted == 0 {
                        // Nothing of this attempt stays.
                        bail!("version taken");
                    }
                }
                Ok(true)
            })
            .or_else(|error| {
                if error.to_string() == "version taken" {
                    Ok(false)
                } else {
                    Err(error)
                }
            })
    }

    /// Reservations, oldest first; `version` narrows them to one version.
    pub fn reservations(&self, version: Option<&str>) -> Result<Vec<Reservation>> {
        self.conn.query_map(
            "SELECT DISTINCT version, scope, start, owner, source, agent, created FROM versions
             WHERE ?1::text IS NULL OR version = ?1::text ORDER BY created, version",
            params![version],
            |row| {
                Ok(Reservation {
                    version: row.get(0)?,
                    scope: row
                        .get::<_, String>(1)?
                        .split(',')
                        .map(str::to_owned)
                        .collect(),
                    start: row.get(2)?,
                    owner: row.get(3)?,
                    source: row.get(4)?,
                    agent: row.get(5)?,
                    created: row.get(6)?,
                })
            },
        )
    }

    /// The latest run of every worktree (one row each).
    pub fn latest_runs(&self) -> Result<Vec<Run>> {
        self.conn.query_map(&format!(
            "{RUN_SELECT} WHERE id IN (SELECT id FROM runs r WHERE started = (SELECT MAX(started) FROM runs WHERE worktree = r.worktree))"
        ), params![], run_row)
    }

    /// A small named value with its update time (cached observations).
    pub fn fact(&self, name: &str) -> Result<Option<(String, i64)>> {
        if self.cache.borrow().facts.is_none() {
            let rows = self.conn.query_map(
                "SELECT name, value, updated FROM facts",
                params![],
                |row| Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?))),
            )?;
            self.cache.borrow_mut().facts = Some(rows.into_iter().collect());
        }
        Ok(self
            .cache
            .borrow()
            .facts
            .as_ref()
            .and_then(|facts| facts.get(name).cloned()))
    }

    pub fn set_fact(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO facts (name, value, updated) VALUES (?1, ?2, ?3)
             ON CONFLICT (name) DO UPDATE SET value = ?2, updated = ?3",
            params![name, value, now() as i64],
        )?;
        if let Some(facts) = self.cache.borrow_mut().facts.as_mut() {
            facts.insert(name.to_owned(), (value.to_owned(), now() as i64));
        }
        Ok(())
    }

    /// Usage since `since`: runs by state/agent, target outcomes, time saved by reuse.
    pub fn stats(&self, since: i64) -> Result<Stats> {
        let count_by = |column: &str| -> Result<Vec<(String, i64)>> {
            self.conn.query_map(&format!(
                "SELECT {column}, COUNT(*) FROM runs WHERE started >= ?1 GROUP BY {column} ORDER BY COUNT(*) DESC"
            ), params![since], |row| Ok((row.get(0)?, row.get(1)?)))
        };
        let outcomes: Vec<(String, i64, i64)> = self.conn.query_map(
            "SELECT t.result, COUNT(*), COALESCE(SUM(t.seconds), 0)::bigint FROM run_targets t JOIN runs r ON r.id = t.run
             WHERE r.started >= ?1 GROUP BY t.result ORDER BY COUNT(*) DESC", params![since], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        let saved_seconds: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(e.seconds), 0)::bigint FROM run_targets t JOIN runs r ON r.id = t.run
             JOIN run_targets e ON e.run = t.evidence_run AND e.target = t.target
             WHERE r.started >= ?1 AND t.result = 'reused'",
            params![since],
            |row| row.get(0),
        )?
        .unwrap_or(0);
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
            let position: i64 = self
                .conn
                .query_row(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM run_targets WHERE run = ?1",
                    params![run],
                    |row| row.get(0),
                )?
                .unwrap_or(0);
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
        self.conn
            .query_row(&format!("{RUN_SELECT} WHERE id = ?1"), params![id], run_row)
    }

    pub fn run_by_key(&self, key: &str) -> Result<Option<Run>> {
        self.conn.query_row(
            &format!("{RUN_SELECT} WHERE key = ?1"),
            params![key],
            run_row,
        )
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
                )?
                .context("no runs in this worktree yet");
        }
        if let Some(run) = self.run(reference)? {
            return Ok(run);
        }
        let matches = self.conn.query_map(
            &format!("{RUN_SELECT} WHERE id LIKE ?1::text || '%' LIMIT 2"),
            params![reference],
            run_row,
        )?;
        match matches.len() {
            1 => Ok(matches.into_iter().next().unwrap_or_else(|| unreachable!())),
            0 => bail!("no run {reference}"),
            _ => bail!("run prefix {reference} is ambiguous"),
        }
    }

    pub fn targets(&self, run: &str) -> Result<Vec<RunTarget>> {
        self.conn.query_map(
            "SELECT target, result, reason, fingerprint, evidence_run, seconds, exit, first_error
             FROM run_targets WHERE run = ?1 ORDER BY position",
            params![run],
            |row| {
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
            },
        )
    }

    /// Runs not finished yet, across all worktrees.
    pub fn active(&self) -> Result<Vec<Run>> {
        self.conn.query_map(
            &format!(
                "{RUN_SELECT} WHERE state IN ('queued', 'waiting', 'running') ORDER BY started"
            ),
            params![],
            run_row,
        )
    }

    pub fn recent(&self, worktree: &str, limit: i64) -> Result<Vec<Run>> {
        self.conn.query_map(
            &format!("{RUN_SELECT} WHERE worktree = ?1 ORDER BY started DESC, id DESC LIMIT ?2"),
            params![worktree, limit],
            run_row,
        )
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
        let created = now() as i64;
        self.conn.execute(
            "INSERT INTO evidence (target, kind, key, result, run, created, detail) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (target, kind, key) DO UPDATE SET result = ?4, run = ?5, created = ?6, detail = ?7",
            params![target, kind, key, result, run, created, detail],
        )?;
        if let Some(all) = self.cache.borrow_mut().evidence.as_mut() {
            let list = all.entry((target.to_owned(), kind.to_owned())).or_default();
            list.retain(|item| item.key != key);
            list.push(Evidence {
                key: key.to_owned(),
                result: result.to_owned(),
                run: run.to_owned(),
                created,
                detail: detail.to_owned(),
            });
        }
        Ok(())
    }

    /// Every evidence row by (target, kind), read in one query.
    fn with_evidence<T>(
        &self,
        f: impl FnOnce(&HashMap<(String, String), Vec<Evidence>>) -> T,
    ) -> Result<T> {
        if self.cache.borrow().evidence.is_none() {
            let rows = self.conn.query_map(
                "SELECT target, kind, key, result, run, created, detail FROM evidence",
                params![],
                |row| {
                    Ok((
                        (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                        Evidence {
                            key: row.get(2)?,
                            result: row.get(3)?,
                            run: row.get(4)?,
                            created: row.get(5)?,
                            detail: row.get(6)?,
                        },
                    ))
                },
            )?;
            let mut all: HashMap<(String, String), Vec<Evidence>> = HashMap::new();
            for (at, item) in rows {
                all.entry(at).or_default().push(item);
            }
            self.cache.borrow_mut().evidence = Some(all);
        }
        Ok(f(self
            .cache
            .borrow()
            .evidence
            .as_ref()
            .unwrap_or_else(|| unreachable!())))
    }

    pub fn evidence(&self, target: &str, kind: &str, key: &str) -> Result<Option<Evidence>> {
        self.with_evidence(|all| {
            all.get(&(target.to_owned(), kind.to_owned()))
                .and_then(|list| list.iter().find(|item| item.key == key).cloned())
        })
    }

    /// The longest of the last five passes of `target` in seconds, if any.
    pub fn typical_seconds(&self, target: &str) -> Result<Option<i64>> {
        if self.cache.borrow().typical.is_none() {
            let rows = self.conn.query_map(
                "SELECT target, MAX(seconds)::bigint FROM (
                     SELECT run_targets.target, run_targets.seconds,
                            row_number() OVER (PARTITION BY run_targets.target ORDER BY runs.started DESC) AS recent
                     FROM run_targets JOIN runs ON runs.id = run_targets.run
                     WHERE result = 'passed' AND seconds IS NOT NULL) AS passes
                 WHERE recent <= 5 GROUP BY target",
                params![],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?;
            self.cache.borrow_mut().typical = Some(rows.into_iter().collect());
        }
        Ok(self
            .cache
            .borrow()
            .typical
            .as_ref()
            .and_then(|all| all.get(target).copied()))
    }

    pub fn latest_evidence(&self, target: &str, kind: &str) -> Result<Option<Evidence>> {
        self.with_evidence(|all| {
            all.get(&(target.to_owned(), kind.to_owned()))
                .and_then(|list| list.iter().max_by_key(|item| item.created).cloned())
        })
    }
}

const RELEASE_SELECT: &str = "SELECT id, unit, kind, environment, version, previous, commit_id, worktree, agent, state, note, pid, started, ended, log FROM releases";

fn release_row(row: &Row) -> Result<Release> {
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

fn run_row(row: &Row) -> Result<Run> {
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

/// A branch waiting to land on the base (`citrus queue`): one row walks
/// queued → checking → merged → releasing → released (or failed, conflict,
/// cancelled, release_failed). Every transition goes through Citrus.
#[derive(Debug, Clone, Serialize)]
pub struct QueueEntry {
    pub id: String,
    pub worktree: String,
    pub branch: String,
    pub agent: String,
    pub commit: String,
    pub base: String,
    /// Units released after the merge, `unit[:bump]` separated by commas.
    pub releases: String,
    pub state: String,
    pub note: String,
    pub batch: String,
    pub run: String,
    /// The base commit that carries this entry, once merged.
    pub merged: String,
    /// `unit=release-id` separated by commas, for releases started.
    pub started_releases: String,
    pub added: i64,
    pub updated: i64,
    pub ended: Option<i64>,
}

impl QueueEntry {
    /// Nothing more happens to it.
    pub fn settled(&self) -> bool {
        matches!(
            self.state.as_str(),
            "failed" | "conflict" | "cancelled" | "released" | "release_failed"
        ) || (self.state == "merged" && self.releases.is_empty())
    }

    /// Landed on the base (whatever its releases do).
    pub fn landed(&self) -> bool {
        matches!(
            self.state.as_str(),
            "merged" | "releasing" | "released" | "release_failed"
        )
    }

    /// (unit, bump) pairs this entry asks to release.
    pub fn release_units(&self) -> Vec<(String, String)> {
        self.releases
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(|item| match item.split_once(':') {
                Some((unit, bump)) => (unit.to_owned(), bump.to_owned()),
                None => (item.to_owned(), "patch".to_owned()),
            })
            .collect()
    }

    /// (unit, release id) pairs of releases started for this entry.
    pub fn started(&self) -> Vec<(String, String)> {
        self.started_releases
            .split(',')
            .filter_map(|item| item.split_once('='))
            .map(|(unit, id)| (unit.to_owned(), id.to_owned()))
            .collect()
    }
}

const QUEUE_SELECT: &str = "SELECT id, worktree, branch, agent, commit_sha, base, releases, state, note, batch, run, merged, started_releases, added, updated, ended FROM queue";

fn queue_row(row: &Row) -> Result<QueueEntry> {
    Ok(QueueEntry {
        id: row.get(0)?,
        worktree: row.get(1)?,
        branch: row.get(2)?,
        agent: row.get(3)?,
        commit: row.get(4)?,
        base: row.get(5)?,
        releases: row.get(6)?,
        state: row.get(7)?,
        note: row.get(8)?,
        batch: row.get(9)?,
        run: row.get(10)?,
        merged: row.get(11)?,
        started_releases: row.get(12)?,
        added: row.get(13)?,
        updated: row.get(14)?,
        ended: row.get(15)?,
    })
}

impl Store {
    /// Queue `entry` unless this worktree already queued that commit for that
    /// base: then the existing entry comes back, so asking twice (after a lost
    /// terminal or context) never queues a second one. `retry` puts a failed,
    /// conflicting or cancelled entry of the same commit back in the queue.
    pub fn queue_add(&self, entry: &QueueEntry, retry: bool) -> Result<(QueueEntry, bool)> {
        let inserted = self.conn.execute(
            "INSERT INTO queue (id, worktree, branch, agent, commit_sha, base, releases, state, note, added, updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', '', ?8, ?8)
             ON CONFLICT (worktree, commit_sha, base) DO NOTHING",
            params![entry.id, entry.worktree, entry.branch, entry.agent, entry.commit, entry.base, entry.releases, entry.added],
        )? == 1;
        if !inserted && retry {
            self.conn.execute(
                "UPDATE queue SET state = 'queued', note = 'queued again', batch = '', run = '', releases = ?4,
                     updated = ?5, ended = NULL
                 WHERE worktree = ?1 AND commit_sha = ?2 AND base = ?3 AND state IN ('failed', 'conflict', 'cancelled')",
                params![entry.worktree, entry.commit, entry.base, entry.releases, now() as i64],
            )?;
        }
        let stored = self
            .conn
            .query_row(
                &format!("{QUEUE_SELECT} WHERE worktree = ?1 AND commit_sha = ?2 AND base = ?3"),
                params![entry.worktree, entry.commit, entry.base],
                queue_row,
            )?
            .context("the queue entry disappeared")?;
        Ok((stored, inserted))
    }

    pub fn queue_entry(&self, id: &str) -> Result<Option<QueueEntry>> {
        self.conn.query_row(
            &format!("{QUEUE_SELECT} WHERE id = ?1"),
            params![id],
            queue_row,
        )
    }

    /// Entries in `states`, oldest first.
    pub fn queue_in(&self, states: &[&str]) -> Result<Vec<QueueEntry>> {
        let states: Vec<String> = states.iter().map(|state| (*state).to_owned()).collect();
        self.conn.query_map(
            &format!("{QUEUE_SELECT} WHERE state = ANY(?1) ORDER BY seq"),
            params![states],
            queue_row,
        )
    }

    /// Entries changed since `since`, newest first.
    pub fn queue_recent(&self, since: i64) -> Result<Vec<QueueEntry>> {
        self.conn.query_map(
            &format!("{QUEUE_SELECT} WHERE updated >= ?1 OR state IN ('queued', 'checking', 'releasing') ORDER BY seq DESC"),
            params![since],
            queue_row,
        )
    }

    /// The latest entry of a worktree.
    pub fn queue_of(&self, worktree: &str) -> Result<Option<QueueEntry>> {
        self.conn.query_row(
            &format!("{QUEUE_SELECT} WHERE worktree = ?1 ORDER BY seq DESC LIMIT 1"),
            params![worktree],
            queue_row,
        )
    }

    /// Move an entry from one of `from` to `state`; false when it was not in
    /// one of them (someone else moved it), so a transition happens once.
    pub fn queue_move(&self, id: &str, from: &[&str], state: &str, note: &str) -> Result<bool> {
        let from: Vec<String> = from.iter().map(|state| (*state).to_owned()).collect();
        let ended = matches!(
            state,
            "failed" | "conflict" | "cancelled" | "merged" | "released" | "release_failed"
        )
        .then_some(now() as i64);
        Ok(self.conn.execute(
            "UPDATE queue SET state = ?3, note = ?4, updated = ?5, ended = COALESCE(?6, ended)
             WHERE id = ?1 AND state = ANY(?2)",
            params![id, from, state, note, now() as i64, ended],
        )? == 1)
    }

    pub fn queue_set(&self, id: &str, field: &str, value: &str) -> Result<()> {
        anyhow::ensure!(
            matches!(field, "batch" | "run" | "merged" | "started_releases"),
            "not a queue field: {field}"
        );
        self.conn.execute(
            &format!("UPDATE queue SET {field} = ?2, updated = ?3 WHERE id = ?1"),
            params![id, value, now() as i64],
        )?;
        Ok(())
    }
}

/// A unit of agent work followed from an owner-hub card to its verified
/// release (`citrus work new`): its step, whose turn it is, who holds it.
#[derive(Debug, Clone, Serialize)]
pub struct WorkTask {
    pub id: String,
    pub title: String,
    /// The owner-hub card it answers (`Q-123`), if any.
    pub card: String,
    /// open, claimed, in-queue, merged, released, verified or closed.
    pub step: String,
    /// owner, lead, lead2, agent or citrus.
    pub turn: String,
    /// Who claimed it; empty when nobody holds it.
    pub claimed_by: String,
    /// Seconds of silence the claim survives.
    pub lease: i64,
    /// Release units its change must reach, separated by commas.
    pub units: String,
    pub agent: String,
    pub created: i64,
    /// Its last event.
    pub updated: i64,
    pub closed: Option<i64>,
}

/// A branch, queue entry, release or worktree a tracked task is made of.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkLink {
    pub kind: String,
    pub value: String,
    pub added: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkEvent {
    pub at: i64,
    pub agent: String,
    pub step: String,
    pub turn: String,
    pub text: String,
}

const WORK_SELECT: &str = "SELECT id, title, card, step, turn, claimed_by, lease, units, agent, created, updated, closed FROM work_tasks";

fn work_row(row: &Row) -> Result<WorkTask> {
    Ok(WorkTask {
        id: row.get(0)?,
        title: row.get(1)?,
        card: row.get(2)?,
        step: row.get(3)?,
        turn: row.get(4)?,
        claimed_by: row.get(5)?,
        lease: row.get(6)?,
        units: row.get(7)?,
        agent: row.get(8)?,
        created: row.get(9)?,
        updated: row.get(10)?,
        closed: row.get(11)?,
    })
}

impl Store {
    /// Track a new task; false when its id is taken.
    pub fn work_insert(&self, task: &WorkTask, text: &str) -> Result<bool> {
        self.conn.transaction(|tx| {
            let inserted = tx_execute(tx,
                "INSERT INTO work_tasks (id, title, card, step, turn, claimed_by, lease, units, agent, created, updated, closed)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT (id) DO NOTHING",
                params![task.id, task.title, task.card, task.step, task.turn, task.claimed_by, task.lease, task.units,
                        task.agent, task.created, task.updated, task.closed],
            )? == 1;
            if inserted {
                tx_execute(tx,
                    "INSERT INTO work_events (task, at, agent, step, turn, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![task.id, task.updated, task.agent, task.step, task.turn, text],
                )?;
            }
            Ok(inserted)
        })
    }

    /// Change a task under a row lock: `change` sees the stored task and
    /// returns the event explaining what it did (or refuses), so two writers
    /// never undo each other.
    pub fn work_modify(
        &self,
        id: &str,
        agent: &str,
        change: impl FnOnce(&mut WorkTask) -> Result<String>,
    ) -> Result<WorkTask> {
        self.conn.transaction(|tx| {
            let row = tx
                .query_opt(&sql(&format!("{WORK_SELECT} WHERE id = ?1 FOR UPDATE")), params![id])?
                .with_context(|| format!("no task {id}"))?;
            let mut task = work_row(&Row(&row))?;
            let text = change(&mut task)?;
            task.updated = now() as i64;
            tx_execute(tx,
                "UPDATE work_tasks SET title = ?2, card = ?3, step = ?4, turn = ?5, claimed_by = ?6, lease = ?7,
                     units = ?8, updated = ?9, closed = ?10 WHERE id = ?1",
                params![task.id, task.title, task.card, task.step, task.turn, task.claimed_by, task.lease,
                        task.units, task.updated, task.closed],
            )?;
            tx_execute(tx,
                "INSERT INTO work_events (task, at, agent, step, turn, text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![task.id, task.updated, agent, task.step, task.turn, text],
            )?;
            Ok(task)
        })
    }

    /// Citrus moves an open task on what it observed: step, turn and claim
    /// change only if nobody changed them since `seen` was read. False when
    /// the task moved meanwhile (or was closed).
    pub fn work_advance(&self, seen: &WorkTask, next: &WorkTask, text: &str) -> Result<bool> {
        let at = now() as i64;
        self.conn.transaction(|tx| {
            let changed = tx_execute(tx,
                "UPDATE work_tasks SET step = ?2, turn = ?3, claimed_by = ?4, updated = ?5
                 WHERE id = ?1 AND closed IS NULL AND step = ?6 AND turn = ?7 AND claimed_by = ?8",
                params![seen.id, next.step, next.turn, next.claimed_by, at, seen.step, seen.turn, seen.claimed_by],
            )? == 1;
            if changed {
                tx_execute(tx,
                    "INSERT INTO work_events (task, at, agent, step, turn, text) VALUES (?1, ?2, 'citrus', ?3, ?4, ?5)",
                    params![seen.id, at, next.step, next.turn, text],
                )?;
            }
            Ok(changed)
        })
    }

    /// Open tasks with a link of `kind` to `value`.
    pub fn work_linked(&self, kind: &str, value: &str) -> Result<Vec<WorkTask>> {
        self.conn.query_map(
            &format!("{WORK_SELECT} WHERE closed IS NULL AND id IN
                      (SELECT task FROM work_links WHERE kind = ?1 AND value = ?2) ORDER BY created"),
            params![kind, value],
            work_row,
        )
    }

    pub fn work_task(&self, id: &str) -> Result<Option<WorkTask>> {
        self.conn.query_row(
            &format!("{WORK_SELECT} WHERE id = ?1"),
            params![id],
            work_row,
        )
    }

    /// Tracked tasks, newest first; closed ones only with `closed`.
    pub fn work_tasks(&self, closed: bool) -> Result<Vec<WorkTask>> {
        self.conn.query_map(
            &format!("{WORK_SELECT} WHERE ?1 OR closed IS NULL ORDER BY created DESC, id"),
            params![closed],
            work_row,
        )
    }

    /// A full id (`t-` may be left out), a card (`Q-123`: its newest open
    /// task) or a unique prefix.
    pub fn work_resolve(&self, reference: &str) -> Result<WorkTask> {
        let reference = reference.trim();
        anyhow::ensure!(
            !reference.is_empty(),
            "name a task: its id, card or a prefix"
        );
        if let Some(task) = self.work_task(reference)? {
            return Ok(task);
        }
        if !reference.starts_with("t-")
            && let Some(task) = self.work_task(&format!("t-{reference}"))?
        {
            return Ok(task);
        }
        let by_card = self.conn.query_row(
            &format!(
                "{WORK_SELECT} WHERE card <> '' AND card = ?1 ORDER BY closed IS NULL DESC, created DESC LIMIT 1"
            ),
            params![reference],
            work_row,
        )?;
        if let Some(task) = by_card {
            return Ok(task);
        }
        // `weekly` finds `t-weekly-left` as `t-weekly` does.
        let bare = like_prefix(reference);
        let prefixed = like_prefix(&format!("t-{reference}"));
        let found = self.conn.query_map(
            &format!(
                r"{WORK_SELECT} WHERE id LIKE ?1::text || '%' ESCAPE '\' OR id LIKE ?2::text || '%' ESCAPE '\' LIMIT 2"
            ),
            params![bare, prefixed],
            work_row,
        )?;
        match found.len() {
            1 => Ok(found.into_iter().next().unwrap_or_else(|| unreachable!())),
            0 => bail!("no task {reference}: citrus tasks lists them"),
            _ => bail!("task prefix {reference} is ambiguous"),
        }
    }

    /// Link `value` of `kind` to a task; false when it was linked already.
    pub fn work_link(&self, task: &str, kind: &str, value: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT INTO work_links (task, kind, value, added) VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
            params![task, kind, value, now() as i64],
        )? == 1)
    }

    pub fn work_links(&self, task: &str) -> Result<Vec<WorkLink>> {
        self.conn.query_map(
            "SELECT kind, value, added FROM work_links WHERE task = ?1 ORDER BY added, kind, value",
            params![task],
            |row| {
                Ok(WorkLink {
                    kind: row.get(0)?,
                    value: row.get(1)?,
                    added: row.get(2)?,
                })
            },
        )
    }

    /// The last `limit` events of a task, oldest first.
    pub fn work_events(&self, task: &str, limit: i64) -> Result<Vec<WorkEvent>> {
        let mut events = self.conn.query_map(
            "SELECT at, agent, step, turn, text FROM work_events WHERE task = ?1 ORDER BY seq DESC LIMIT ?2",
            params![task, limit],
            |row| {
                Ok(WorkEvent {
                    at: row.get(0)?,
                    agent: row.get(1)?,
                    step: row.get(2)?,
                    turn: row.get(3)?,
                    text: row.get(4)?,
                })
            },
        )?;
        events.reverse();
        Ok(events)
    }

    /// The passed `citrus apply` of `environment` at `commit` started since
    /// `since`: a release unit's apply step, whose last step is `verify`.
    pub fn passed_apply(
        &self,
        environment: &str,
        commit: &str,
        since: i64,
    ) -> Result<Option<Release>> {
        self.conn.query_row(
            &format!(
                "{RELEASE_SELECT} WHERE kind = 'apply' AND environment = ?1 AND commit_id = ?2
                      AND state = 'passed' AND started >= ?3 ORDER BY started DESC LIMIT 1"
            ),
            params![environment, commit, since],
            release_row,
        )
    }
}

/// `text` as a literal LIKE prefix (`%`, `_` and `\` escaped).
fn like_prefix(text: &str) -> String {
    text.replace('\\', r"\\")
        .replace('%', r"\%")
        .replace('_', r"\_")
}
