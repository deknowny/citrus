//! The pool (docs/design/runners.md): every machine that runs `citrus agent`
//! takes checks from a queue in a shared Postgres database. A run records its
//! working tree as a commit, pushes it to `refs/citrus/runs/<run>` of the
//! repository's own remote, queues one row per check, and follows the lines
//! the agents' executors print (the runner protocol).

use anyhow::{Context as _, Result, bail};
use postgres::Client;
use postgres::fallible_iterator::FallibleIterator;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// This build: a run asks agents for an executor of exactly this commit.
pub const VERSION: &str = env!("CITRUS_COMMIT");
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
/// An agent silent this long has stopped; its checks go back to the queue.
const STALE_SECONDS: i64 = 60;
const HEARTBEAT: Duration = Duration::from_secs(10);

/// Set by SIGTERM/SIGINT: the agent hands its checks back and leaves.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// The batch was stopped with the agent; its checks go back to the queue.
#[derive(Debug)]
struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the agent is stopping")
    }
}

impl std::error::Error for Stopped {}

/// The pool this person uses: `CITRUS_POOL`, else the first line of
/// `~/.config/citrus/pool`. Per person, not per repository.
pub fn url() -> Option<String> {
    if let Ok(value) = std::env::var("CITRUS_POOL") {
        let value = value.trim().to_owned();
        return (!value.is_empty()).then_some(value);
    }
    let text = std::fs::read_to_string(home().join(".config/citrus/pool")).ok()?;
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

/// `postgres://user:secret@host/db` without the secret, for messages.
pub fn redact(url: &str) -> String {
    match (url.find("://"), url.rfind('@')) {
        (Some(scheme), Some(at)) if at > scheme => {
            let credentials = &url[scheme + 3..at];
            match credentials.split_once(':') {
                Some((user, _)) => format!("{}{user}:***{}", &url[..scheme + 3], &url[at..]),
                None => url.to_owned(),
            }
        }
        _ => url.to_owned(),
    }
}

pub fn connect(url: &str) -> Result<Client> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // A pool behind its own certificate authority.
    if let Some(ca) = std::env::var_os("CITRUS_POOL_CA").filter(|value| !value.is_empty()) {
        use rustls::pki_types::{CertificateDer, pem::PemObject};
        for cert in CertificateDer::pem_file_iter(&ca)
            .with_context(|| format!("read CITRUS_POOL_CA {}", Path::new(&ca).display()))?
        {
            roots.add(cert?)?;
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tls = tokio_postgres_rustls::MakeRustlsConnect::new(config);
    let mut client = Client::connect(url, tls)
        .with_context(|| format!("connect to the pool {}", redact(url)))?;
    migrate(&mut client)?;
    Ok(client)
}

/// The pool's tables; any client creates them on first use.
/// The schema version this build writes; bump with every change below.
const SCHEMA_VERSION: i32 = 8;

/// The pool's tables. DDL takes exclusive table locks even when it changes
/// nothing, so it runs only when the recorded version is behind; every other
/// connection reads one row.
fn migrate(client: &mut Client) -> Result<()> {
    let current: Option<i32> = client
        .query_opt(
            "select version from citrus.meta where to_regclass('citrus.meta') is not null",
            &[],
        )
        .ok()
        .flatten()
        .map(|row| row.get(0));
    if current.is_some_and(|version| version >= SCHEMA_VERSION) {
        return Ok(());
    }
    client.batch_execute(
        &"begin;
        select pg_advisory_xact_lock(74657);
        create schema if not exists citrus;
        create table if not exists citrus.agents (
            name text primary key,
            labels text[] not null default '{}',
            container text[] not null default '{}',
            cpus integer not null default 0,
            share integer not null default 0,
            slots integer not null default 1,
            version text not null default '',
            state text not null default 'ready',
            running integer not null default 0,
            load real not null default 0,
            started timestamptz not null default now(),
            seen timestamptz not null default now()
        );
        create table if not exists citrus.runs (
            id text primary key,
            repo text not null,
            commit_sha text not null,
            ref_name text not null,
            version text not null,
            profile text not null default '',
            image jsonb,
            requester text not null default '',
            state text not null default 'open',
            created timestamptz not null default now(),
            closed timestamptz
        );
        create table if not exists citrus.jobs (
            run text not null references citrus.runs(id) on delete cascade,
            check_name text not null,
            requires text[] not null default '{}',
            state text not null default 'queued',
            agent text,
            claimed timestamptz,
            finished timestamptz,
            result text,
            seconds real,
            primary key (run, check_name)
        );
        create table if not exists citrus.events (
            id bigserial primary key,
            run text not null references citrus.runs(id) on delete cascade,
            agent text not null default '',
            line text not null,
            at timestamptz not null default now()
        );
        -- Readers follow by transaction, not by id: an id taken by a
        -- transaction that commits later would otherwise be skipped.
        alter table citrus.runs add column if not exists prepare text[] not null default '{}';
        -- What the agent's governor lets the pool use now (CPUs) and why it is below the share.
        alter table citrus.agents add column if not exists budget real not null default 0;
        alter table citrus.agents add column if not exists throttle text not null default '';
        alter table citrus.events add column if not exists tx xid8 not null default pg_current_xact_id();
        create index if not exists events_by_run on citrus.events (run, id);
        create index if not exists jobs_queued on citrus.jobs (state) where state = 'queued';
        -- A check's #[outputs]: the files its passing run brings back (a tar).
        alter table citrus.jobs add column if not exists outputs text[] not null default '{}';
        alter table citrus.jobs add column if not exists output bytea;
        -- Release gates go first, and one slot per agent waits for them.
        alter table citrus.runs add column if not exists priority integer not null default 0;
        -- Citrus builds by commit and platform (`citrus pool publish`): agents
        -- and launchers take them instead of compiling a version each.
        create table if not exists citrus.binaries (
            commit_sha text not null,
            platform text not null,
            sha256 text not null,
            data bytea not null,
            created timestamptz not null default now(),
            primary key (commit_sha, platform)
        );
        -- How long a check usually takes (smoothed over its passes): the longest
        -- checks of a run start first, so a run ends with its longest check.
        create table if not exists citrus.durations (
            repo text not null,
            check_name text not null,
            seconds real not null,
            primary key (repo, check_name)
        );
        insert into citrus.durations (repo, check_name, seconds)
            select r.repo, j.check_name, avg(j.seconds)::real from citrus.jobs j
            join citrus.runs r on r.id = j.run
            where j.result = 'passed' and j.seconds is not null and j.finished > now() - interval '3 days'
            group by 1, 2 on conflict do nothing;
        create table if not exists citrus.meta (version integer not null);
        delete from citrus.meta;
        insert into citrus.meta (version) values (SCHEMA_VERSION);
        commit;"
            .replace("SCHEMA_VERSION", &SCHEMA_VERSION.to_string()),
    )?;
    Ok(())
}

/// Checks of agents that stopped answering go back to the queue.
fn requeue_stale(client: &mut Client) -> Result<Vec<(String, String, String)>> {
    // Several requesters and agents do this at once: rows another one holds
    // are skipped, not waited for, so their locks never cross.
    let rows = retry(|| {
        client.query(
            "with stale as (
                 select j.run, j.check_name, j.agent from citrus.jobs j
                 where j.state = 'claimed' and not exists (
                     select 1 from citrus.agents a
                     where a.name = j.agent and a.seen > now() - make_interval(secs => $1))
                 order by j.run, j.check_name
                 for update of j skip locked)
             update citrus.jobs j set state = 'queued', agent = null, claimed = null
             from stale where j.run = stale.run and j.check_name = stale.check_name
             returning j.run, j.check_name, coalesce(stale.agent, '')",
            &[&(STALE_SECONDS as f64)],
        )
    })?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
}

/// Postgres may abort one side of a lock conflict (deadlock, serialization):
/// that statement is safe to run again.
fn retry<T>(mut statement: impl FnMut() -> Result<T, postgres::Error>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match statement() {
            Ok(value) => return Ok(value),
            Err(error)
                if attempt < 5
                    && error.code().is_some_and(|code| {
                        *code == postgres::error::SqlState::T_R_DEADLOCK_DETECTED
                            || *code == postgres::error::SqlState::T_R_SERIALIZATION_FAILURE
                    }) =>
            {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(50 * attempt));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn notify(client: &mut Client, channel: &str, payload: &str) -> Result<()> {
    client.execute("select pg_notify($1, $2)", &[&channel, &payload])?;
    Ok(())
}

// ---------------------------------------------------------------- requester

/// What one check needs from an agent: `#[meta(linux = true)]` and
/// `#[meta(requires = [...])]`.
pub fn requirements(target: &crate::manifest::Target) -> Vec<String> {
    let mut needs = Vec::new();
    if target.extensions.get("linux") == Some(&serde_json::Value::Bool(true)) {
        needs.push("linux".to_owned());
    }
    if let Some(serde_json::Value::Array(items)) = target.extensions.get("requires") {
        needs.extend(
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned)),
        );
    }
    needs.sort();
    needs.dedup();
    needs
}

/// The working tree as a commit (uncommitted changes included; ignored and
/// `#![private]` paths left out), pushed where the agents fetch it.
pub fn snapshot(repo: &crate::repo::Repo, run: &str) -> Result<(String, String, String)> {
    let remote = std::env::var("CITRUS_POOL_REMOTE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "origin".into());
    let url = repo
        .git(&["remote", "get-url", &remote])
        .with_context(|| format!("the pool fetches snapshots from the `{remote}` remote"))?
        .trim()
        .to_owned();
    let tmp = repo.state_dir().join("tmp");
    crate::repo::private_dir(&tmp)?;
    let index = tmp.join(format!("pool-index-{run}"));
    let _ = std::fs::remove_file(&index);
    let git = |args: &[&str]| -> Result<String> {
        let output = crate::repo::git()
            .args(args)
            .current_dir(&repo.root)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_AUTHOR_NAME", "citrus")
            .env("GIT_AUTHOR_EMAIL", "citrus@invalid")
            .env("GIT_COMMITTER_NAME", "citrus")
            .env("GIT_COMMITTER_EMAIL", "citrus@invalid")
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            bail!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let head = git(&["rev-parse", "--verify", "-q", "HEAD"]).ok();
    // A copy of the worktree's index keeps its stat data: `git add -A` then
    // hashes only what changed, not every file.
    let current = repo.git(&["rev-parse", "--path-format=absolute", "--git-path", "index"]);
    let copied = current
        .ok()
        .map(|path| PathBuf::from(path.trim()))
        .filter(|path| path.is_file())
        .is_some_and(|path| std::fs::copy(path, &index).is_ok());
    if !copied {
        match &head {
            Some(_) => git(&["read-tree", "HEAD"])?,
            None => git(&["read-tree", "--empty"])?,
        };
    }
    git(&["add", "-A", "--", "."])?;
    let private: Vec<String> = repo
        .config
        .run
        .private
        .iter()
        .map(|glob| format!(":(glob){glob}"))
        .collect();
    if !private.is_empty() {
        let mut args = vec!["rm", "-r", "-q", "--cached", "--ignore-unmatch", "--"];
        args.extend(private.iter().map(String::as_str));
        git(&args)?;
    }
    let tree = git(&["write-tree"])?;
    let message = format!("citrus pool run {run}");
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    if let Some(head) = &head {
        args.extend(["-p", head.as_str()]);
    }
    let commit = git(&args)?;
    let _ = std::fs::remove_file(&index);
    let refname = format!("refs/citrus/runs/{run}");
    git(&[
        "push",
        "--quiet",
        "--no-verify",
        &remote,
        &format!("{commit}:{refname}"),
    ])?;
    Ok((url, commit, refname))
}

fn delete_ref(repo: &crate::repo::Repo, refname: &str) {
    let remote = std::env::var("CITRUS_POOL_REMOTE")
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "origin".into());
    let _ = crate::repo::git()
        .args([
            "push",
            "--quiet",
            "--no-verify",
            &remote,
            &format!(":{refname}"),
        ])
        .current_dir(&repo.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Queue `checks` of run `id` and feed the agents' lines to `on_line` until
/// every check has a result. Returns the suite's exit code.
pub fn run(
    context: &crate::exec::Context,
    id: &str,
    checks: &[String],
    on_line: &mut dyn FnMut(String) -> Result<()>,
) -> Result<i64> {
    let url = url().context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    // CITRUS_POOL_EXECUTOR=self: every agent runs its own build (tests, one-machine pools).
    let own_builds = std::env::var("CITRUS_POOL_EXECUTOR").as_deref() == Ok("self");
    if !own_builds && (VERSION == "unknown" || VERSION.ends_with("-dirty")) {
        bail!(
            "this Citrus build ({VERSION}) is not a commit agents can fetch; build Citrus from a clean checkout"
        );
    }
    let mut client = connect(&url)?;
    // Queued before (a worker that stopped): follow it, from its first line.
    if let Some(row) = client.query_opt("select ref_name from citrus.runs where id = $1", &[&id])? {
        let refname: String = row.get(0);
        on_line(format!("CITRUS_STAGE following {id} in the pool again"))?;
        let outcome = follow(&mut client, id, on_line);
        let _ = client.execute(
            "update citrus.runs set state = 'closed', closed = now() where id = $1 and state = 'open'",
            &[&id],
        );
        delete_ref(&context.repo, &refname);
        return outcome;
    }
    on_line(format!(
        "CITRUS_STAGE recording the snapshot for the pool {}",
        redact(&url)
    ))?;
    let (repo, commit, refname) = snapshot(&context.repo, id)?;
    let image = context
        .repo
        .config
        .run
        .image
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    let profile = context.repo.config.plan.profile.clone().unwrap_or_default();
    let prepare = context.repo.config.run.prepare.clone();
    // CITRUS_RUN_PRIORITY: a release's gate (above 0) goes before other runs.
    let priority: i32 = std::env::var("CITRUS_RUN_PRIORITY")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    {
        let mut tx = client.transaction()?;
        tx.execute(
            "insert into citrus.runs (id, repo, commit_sha, ref_name, version, profile, image, requester, prepare, priority)
             values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[&id, &repo, &commit, &refname, &VERSION, &profile, &image, &crate::exec::agent(), &prepare, &priority],
        )?;
        for check in checks {
            let needs = context
                .manifest
                .targets
                .get(check)
                .map(requirements)
                .unwrap_or_default();
            let outputs = context
                .manifest
                .targets
                .get(check)
                .map(|target| target.outputs.clone())
                .unwrap_or_default();
            tx.execute(
                "insert into citrus.jobs (run, check_name, requires, outputs) values ($1, $2, $3, $4)",
                &[&id, check, &needs, &outputs],
            )?;
        }
        tx.execute("select pg_notify('citrus_jobs', $1)", &[&id])?;
        tx.commit()?;
    }
    context.store.set_fact(&format!("pool:{id}"), &refname)?;
    on_line(format!("CITRUS_WAIT pool · {} checks queued", checks.len()))?;
    let outcome = follow(&mut client, id, on_line);
    if outcome.is_ok() {
        bring_back(&mut client, id, &context.repo.root, on_line)?;
    }
    if outcome.is_err() {
        let _ = client.execute(
            "update citrus.runs set state = 'cancelled', closed = now() where id = $1 and state = 'open'",
            &[&id],
        );
    }
    let _ = client.execute(
        "update citrus.runs set state = 'closed', closed = now() where id = $1 and state = 'open'",
        &[&id],
    );
    delete_ref(&context.repo, &refname);
    outcome
}

/// Unpack the #[outputs] of the passed checks of run `id` into `root`.
fn bring_back(
    client: &mut Client,
    id: &str,
    root: &Path,
    on_line: &mut dyn FnMut(String) -> Result<()>,
) -> Result<()> {
    for row in client.query(
        "select check_name, output from citrus.jobs where run = $1 and output is not null and result = 'passed'",
        &[&id],
    )? {
        let (check, data): (String, Vec<u8>) = (row.get(0), row.get(1));
        let mut tar = Command::new("tar")
            .args(["-x", "-f", "-", "-C"])
            .arg(root)
            .stdin(Stdio::piped())
            .spawn()
            .context("tar unpacks the outputs")?;
        std::io::Write::write_all(tar.stdin.as_mut().context("stdin")?, &data)?;
        drop(tar.stdin.take());
        if !tar.wait()?.success() {
            bail!("could not unpack the outputs of {check}");
        }
        on_line(format!("CITRUS_STAGE {check}: its outputs are in the tree"))?;
    }
    Ok(())
}

/// The files of a finished check's #[outputs] in its tree, as a tar.
fn pack_outputs(tree: &Path, globs: &[String]) -> Result<Option<Vec<u8>>> {
    let list = crate::manifest::GlobList::new(globs)?;
    let output = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["ls-files", "-co", "--exclude-standard", "-z"])
        .output()?;
    let files: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|path| !path.is_empty() && list.matches(path) && tree.join(path).is_file())
        .map(str::to_owned)
        .collect();
    if files.is_empty() {
        return Ok(None);
    }
    let output = Command::new("tar")
        .args(["-c", "-f", "-", "-C"])
        .arg(tree)
        .args(&files)
        .output()?;
    if !output.status.success() {
        bail!("tar could not pack the outputs");
    }
    Ok(Some(output.stdout))
}

fn follow(
    client: &mut Client,
    id: &str,
    on_line: &mut dyn FnMut(String) -> Result<()>,
) -> Result<i64> {
    client.batch_execute("listen citrus_events")?;
    // Every transaction below the snapshot's xmin has ended: lines of
    // transactions in [cursor, xmin) are final and read once, in order.
    let mut cursor = "0".to_owned();
    let mut checked = Instant::now() - Duration::from_secs(60);
    loop {
        let horizon: String = client
            .query_one("select pg_snapshot_xmin(pg_current_snapshot())::text", &[])?
            .get(0);
        for row in client.query(
            "select line from citrus.events
             where run = $1 and tx >= $2::text::xid8 and tx < $3::text::xid8 order by tx, id",
            &[&id, &cursor, &horizon],
        )? {
            on_line(row.get(0))?;
        }
        cursor = horizon;
        if checked.elapsed() >= Duration::from_secs(5) {
            checked = Instant::now();
            for (run, check, agent) in requeue_stale(client)? {
                if run == id {
                    on_line(format!(
                        "CITRUS_STAGE {check} back in the queue: agent {agent} stopped answering"
                    ))?;
                }
            }
        }
        let open: i64 = client
            .query_one(
                "select count(*) from citrus.jobs where run = $1 and state in ('queued', 'claimed')",
                &[&id],
            )?
            .get(0);
        if open == 0 {
            // Lines written after the last check settled.
            // Agents settle a check after its lines commit: nothing is in flight.
            for row in client.query(
                "select line from citrus.events where run = $1 and tx >= $2::text::xid8 order by tx, id",
                &[&id, &cursor],
            )? {
                on_line(row.get(0))?;
            }
            let failed: i64 = client
                .query_one(
                    "select count(*) from citrus.jobs where run = $1 and coalesce(result, '') <> 'passed'",
                    &[&id],
                )?
                .get(0);
            return Ok(i64::from(failed > 0));
        }
        // Woken by an agent's lines, or look again in a second.
        let _ = client
            .notifications()
            .timeout_iter(Duration::from_secs(1))
            .next()?;
        while client.notifications().iter().next()?.is_some() {}
    }
}

/// `citrus cancel` of a pool run: queued checks are dropped, agents stop theirs.
pub fn cancel(id: &str) -> Result<()> {
    let Some(url) = url() else { return Ok(()) };
    let mut client = connect(&url)?;
    client.execute(
        "update citrus.runs set state = 'cancelled', closed = now() where id = $1 and state = 'open'",
        &[&id],
    )?;
    client.execute(
        "update citrus.jobs set state = 'done', result = 'cancelled', finished = now()
         where run = $1 and state = 'queued'",
        &[&id],
    )?;
    notify(&mut client, "citrus_jobs", id)
}

// ---------------------------------------------------------------- overview

#[derive(Debug, serde::Serialize)]
pub struct AgentRow {
    pub name: String,
    pub labels: Vec<String>,
    pub container: Vec<String>,
    pub cpus: i32,
    pub share: i32,
    pub slots: i32,
    pub state: String,
    pub running: i32,
    pub load: f32,
    pub budget: f32,
    pub throttle: String,
    pub version: String,
    pub seen_seconds: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct Overview {
    pub pool: String,
    pub agents: Vec<AgentRow>,
    pub queued: i64,
    pub running: Vec<(String, String, String)>,
}

pub fn overview() -> Result<Overview> {
    let url = url().context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    let mut client = connect(&url)?;
    let agents = client
        .query(
            "select name, labels, container, cpus, share, slots, state, running, load, version,
                    extract(epoch from now() - seen)::float8, budget, throttle
             from citrus.agents order by name",
            &[],
        )?
        .iter()
        .map(|row| AgentRow {
            name: row.get(0),
            labels: row.get(1),
            container: row.get(2),
            cpus: row.get(3),
            share: row.get(4),
            slots: row.get(5),
            state: row.get(6),
            running: row.get(7),
            load: row.get(8),
            version: row.get(9),
            seen_seconds: row.get(10),
            budget: row.get(11),
            throttle: row.get(12),
        })
        .collect();
    let queued: i64 = client
        .query_one(
            "select count(*) from citrus.jobs where state = 'queued'",
            &[],
        )?
        .get(0);
    let running = client
        .query(
            "select run, check_name, coalesce(agent, '') from citrus.jobs
             where state = 'claimed' order by claimed",
            &[],
        )?
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect();
    Ok(Overview {
        pool: redact(&url),
        agents,
        queued,
        running,
    })
}

/// `citrus pool drain NAME` / `resume NAME`.
pub fn set_agent_state(name: &str, state: &str) -> Result<()> {
    let url = url().context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    let mut client = connect(&url)?;
    let changed = client.execute(
        "update citrus.agents set state = $2 where name = $1",
        &[&name, &state],
    )?;
    if changed == 0 {
        bail!("no agent `{name}` in the pool (`citrus pool` lists them)");
    }
    Ok(())
}

// ---------------------------------------------------------------- agent

#[derive(Debug, Clone)]
pub struct AgentOptions {
    pub name: Option<String>,
    pub share: Option<usize>,
    pub slots: Option<usize>,
    pub labels: Vec<String>,
    /// Run image runs natively when this machine has no Docker.
    pub native: bool,
    /// Exit after this many seconds without work (tests, one-off helpers).
    pub idle_exit: Option<u64>,
    /// Take no new checks while another process holds a lock on one of
    /// these files (a release build owning this machine).
    pub pause_while_locked: Vec<PathBuf>,
    /// Keep the share fixed instead of adapting to the machine's other load.
    pub fixed: bool,
    /// The least CPUs the pool keeps under pressure (default 1).
    pub min_cpus: Option<f64>,
    /// Write bandwidth cap for the pool's cgroup on the cache disk, MiB/s.
    pub io_mib: Option<f64>,
    /// Fraction of the machine the pool and the rest may use together (default 0.8).
    pub target_util: Option<f64>,
    /// The most disk the Cargo target directories of the slots may use together (GiB, default 200).
    pub cache_gib: Option<u64>,
}

#[derive(Clone)]
struct Machine {
    name: String,
    share: usize,
    slots: usize,
    labels: Vec<String>,
    container: Vec<String>,
    docker: bool,
    native: bool,
    cache: PathBuf,
}

fn arch() -> &'static str {
    std::env::consts::ARCH
}

fn os_label() -> &'static str {
    std::env::consts::OS
}

/// `linux/aarch64` of the Docker engine, if one answers.
fn docker_platform() -> Option<String> {
    let output = Command::new("docker")
        .args(["info", "--format", "{{.OSType}}/{{.Architecture}}"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let (os, arch) = text.split_once('/')?;
    let arch = match arch {
        "arm64" => "aarch64",
        "amd64" => "x86_64",
        other => other,
    };
    Some(format!("{os}/{arch}"))
}

fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most `len` bytes into the buffer.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let end = buffer.iter().position(|byte| *byte == 0).unwrap_or(0);
    if ok && end > 0 {
        String::from_utf8_lossy(&buffer[..end])
            .split('.')
            .next()
            .unwrap_or("agent")
            .to_owned()
    } else {
        "agent".into()
    }
}

fn load_average() -> f32 {
    let mut values = [0f64; 3];
    // SAFETY: getloadavg writes at most 3 doubles into the array.
    let read = unsafe { libc::getloadavg(values.as_mut_ptr(), 3) };
    if read >= 1 { values[0] as f32 } else { 0.0 }
}

/// HOME, or the account's home directory when a service manager leaves it unset.
fn home() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home);
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; the
    // directory string is copied before any other call can overwrite it.
    unsafe {
        let entry = libc::getpwuid(libc::getuid());
        if !entry.is_null() && !(*entry).pw_dir.is_null() {
            let dir = std::ffi::CStr::from_ptr((*entry).pw_dir);
            return PathBuf::from(dir.to_string_lossy().into_owned());
        }
    }
    PathBuf::from("/tmp")
}

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CITRUS_AGENT_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("citrus/agent");
    }
    home().join(".cache/citrus/agent")
}

/// The throwaway state schema of a batch's executor.
fn batch_schema(batch: &str) -> String {
    format!("citrus_batch_{}", short_hash(batch))
}

fn short_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(&Sha256::digest(text.as_bytes())[..8])
}

/// Join the pool and take checks until stopped (or idle for `idle_exit`).
pub fn agent(options: &AgentOptions) -> Result<i32> {
    let url = url().context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let share = options
        .share
        .unwrap_or(cpus.saturating_sub(1))
        .clamp(1, cpus);
    let slots = options.slots.unwrap_or((share / 4).max(1)).max(1);
    let platform = docker_platform();
    let mut labels = vec![os_label().to_owned(), arch().to_owned()];
    if platform.is_some() {
        labels.push("docker".into());
    }
    labels.extend(options.labels.iter().cloned());
    labels.sort();
    labels.dedup();
    let container = match &platform {
        Some(platform) => {
            let (os, arch) = platform.split_once('/').unwrap_or(("linux", ""));
            let mut container = vec![os.to_owned(), arch.to_owned(), "docker".to_owned()];
            container.extend(options.labels.iter().cloned());
            container.sort();
            container.dedup();
            container
        }
        None => Vec::new(),
    };
    let machine = Machine {
        name: options.name.clone().unwrap_or_else(hostname),
        share,
        slots,
        labels,
        container,
        docker: platform.is_some(),
        native: options.native,
        cache: cache_root(),
    };
    std::fs::create_dir_all(&machine.cache)?;
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_stop as *const () as libc::sighandler_t);
    }
    let mut client = connect(&url)?;
    client.execute(
        "insert into citrus.agents (name, labels, container, cpus, share, slots, version, state, running, seen, started)
         values ($1, $2, $3, $4, $5, $6, $7, 'ready', 0, now(), now())
         on conflict (name) do update set labels = $2, container = $3, cpus = $4, share = $5,
             slots = $6, version = $7, running = 0, seen = now(), started = now()",
        &[
            &machine.name,
            &machine.labels,
            &machine.container,
            &(cpus as i32),
            &(share as i32),
            &(slots as i32),
            &VERSION,
        ],
    )?;
    eprintln!(
        "citrus agent {}: {} of {} CPUs, {} checks at once, labels {}{} — pool {}",
        machine.name,
        share,
        cpus,
        slots,
        machine.labels.join(","),
        if machine.docker {
            format!(", containers {}", machine.container.join(","))
        } else {
            String::new()
        },
        redact(&url)
    );
    // The heartbeat keeps the agent in the pool and reads whether to drain.
    let stop = Arc::new(AtomicBool::new(false));
    // Checks a previous process of this agent claimed died with it.
    let orphans = client.execute(
        "update citrus.jobs set state = 'queued', agent = null, claimed = null
         where agent = $1 and state = 'claimed'",
        &[&machine.name],
    )?;
    if orphans > 0 {
        eprintln!(
            "citrus agent {}: {orphans} checks of a previous run of this agent back in the queue",
            machine.name
        );
        notify(&mut client, "citrus_jobs", "")?;
    }
    // A drained agent stays drained across restarts.
    let state: String = client
        .query_one(
            "select state from citrus.agents where name = $1",
            &[&machine.name],
        )?
        .get(0);
    let draining = Arc::new(AtomicBool::new(state == "draining"));
    let running = Arc::new(AtomicUsize::new(0));
    // The governor: how many CPUs the pool may use beside production right now.
    let allowed = Arc::new(AtomicUsize::new(machine.slots));
    let budget = Arc::new(AtomicUsize::new(share * 1000));
    let throttle: Arc<Mutex<&'static str>> = Arc::new(Mutex::new(""));
    let governor = if options.fixed || !cfg!(target_os = "linux") {
        None
    } else {
        let config = crate::governor::Config {
            cores: cpus as f64,
            share: share as f64,
            floor: options
                .min_cpus
                .unwrap_or((share as f64 / 4.0).max(1.0))
                .clamp(0.1, share as f64),
            target: options.target_util.unwrap_or(0.8).clamp(0.1, 1.0),
            slice: std::env::var("CITRUS_AGENT_CGROUP_PARENT")
                .ok()
                .filter(|value| !value.is_empty()),
        };
        let (stop, allowed, budget, throttle) = (
            stop.clone(),
            allowed.clone(),
            budget.clone(),
            throttle.clone(),
        );
        let (name, slots) = (machine.name.clone(), machine.slots);
        let cache = machine.cache.clone();
        let io_mib = options.io_mib.unwrap_or(200.0);
        Some(std::thread::spawn(move || {
            let mut sampler = crate::governor::Sampler::new(config.cores, config.slice.clone());
            let mut probe = crate::governor::DiskProbe::new(&cache);
            let mut write_limit = crate::governor::WriteLimit::new(&cache, io_mib);
            let mut current = config.share;
            let mut announced = current;
            let mut announced_at = Instant::now() - Duration::from_secs(60);
            let mut smoother = crate::governor::Smoother::default();
            while !stop.load(Ordering::SeqCst) {
                let fsync_ms = probe.as_mut().map_or(0.0, |probe| probe.probe());
                if let Some(mut sample) = sampler.sample() {
                    sample.fsync_ms = fsync_ms;
                    let sample = smoother.smooth(sample);
                    let decision = crate::governor::decide(&config, current, &sample);
                    current = decision.budget;
                    allowed.store(
                        crate::governor::allowed_slots(current, config.share, slots),
                        Ordering::SeqCst,
                    );
                    budget.store((current * 1000.0) as usize, Ordering::SeqCst);
                    *throttle.lock().unwrap() = decision.reason;
                    if let Some(dir) = config.slice.as_deref().and_then(crate::governor::slice_dir)
                    {
                        crate::governor::limit_cgroup(&dir, current);
                        if let Some(limit) = write_limit.as_mut() {
                            limit.adjust(fsync_ms);
                            limit.apply(&dir);
                        }
                    }
                    if (current - announced).abs() >= 1.0
                        && announced_at.elapsed() >= Duration::from_secs(30)
                    {
                        announced_at = Instant::now();
                        eprintln!(
                            "citrus agent {name}: {current:.1} of {:.0} CPUs for the pool{}",
                            config.share,
                            if decision.reason.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", decision.reason)
                            }
                        );
                        announced = current;
                    }
                }
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            if let Some(dir) = config.slice.as_deref().and_then(crate::governor::slice_dir) {
                crate::governor::limit_cgroup(&dir, config.share);
                if let Some(limit) = write_limit.as_ref() {
                    limit.lift(&dir);
                }
            }
        }))
    };
    // Caches share the disk with production: trimmed once an hour.
    let hygiene = {
        let stop = stop.clone();
        let cache = machine.cache.clone();
        let total = options.cache_gib.unwrap_or(200).max(10);
        let limits = crate::hygiene::Limits::gib((total / 3).max(5), total);
        let name = machine.name.clone();
        std::thread::spawn(move || {
            let mut wait = Duration::from_secs(120);
            while !stop.load(Ordering::SeqCst) {
                let deadline = Instant::now() + wait;
                while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(500));
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let freed = crate::hygiene::prune(&cache, limits);
                crate::hygiene::docker();
                if freed > (1 << 30) {
                    eprintln!("citrus agent {name}: caches trimmed by {} GiB", freed >> 30);
                }
                wait = Duration::from_secs(3600);
            }
        })
    };
    let heartbeat = {
        let (url, name) = (url.clone(), machine.name.clone());
        let (stop, draining, running) = (stop.clone(), draining.clone(), running.clone());
        let (budget, throttle) = (budget.clone(), throttle.clone());
        std::thread::spawn(move || -> Result<()> {
            let mut client = connect(&url)?;
            while !stop.load(Ordering::SeqCst) {
                let rows = client.query(
                    "update citrus.agents set seen = now(), load = $2, running = $3,
                            budget = $4, throttle = $5
                     where name = $1 returning state",
                    &[
                        &name,
                        &load_average(),
                        &(running.load(Ordering::SeqCst) as i32),
                        &(budget.load(Ordering::SeqCst) as f32 / 1000.0),
                        &throttle.lock().unwrap().to_string(),
                    ],
                )?;
                let state: String = rows.first().map(|row| row.get(0)).unwrap_or_default();
                draining.store(state == "draining", Ordering::SeqCst);
                let deadline = Instant::now() + HEARTBEAT;
                while Instant::now() < deadline && !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            Ok(())
        })
    };
    let result = serve(
        &mut client,
        &url,
        &machine,
        options,
        &draining,
        &running,
        &allowed,
    );
    stop.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();
    let _ = hygiene.join();
    if let Some(governor) = governor {
        let _ = governor.join();
    }
    let _ = client.execute(
        "update citrus.agents set seen = now() - interval '1 hour', running = 0 where name = $1",
        &[&machine.name],
    );
    result
}

/// Claims checks while slots are free and runs each batch on its own thread:
/// a long check holds only its own slot, and every result is reported as it
/// comes, not when the slowest check of its batch ends.
fn serve(
    client: &mut Client,
    url: &str,
    machine: &Machine,
    options: &AgentOptions,
    draining: &AtomicBool,
    running: &AtomicUsize,
    allowed: &AtomicUsize,
) -> Result<i32> {
    client.batch_execute("listen citrus_jobs")?;
    let free = Arc::new(AtomicUsize::new(machine.slots));
    let fatal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let mut workers: Vec<std::thread::JoinHandle<()>> = Vec::new();
    let mut idle_since = Instant::now();
    let mut maintained = Instant::now() - Duration::from_secs(3600);
    let mut paused = false;
    // Batch names must not repeat after a restart of the agent: the state schema
    // and the container of a stopped batch are still being removed while the
    // requeued checks run again under the next process.
    let process = short_hash(&format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos())
    ));
    let mut batches = 0usize;
    let result = loop {
        workers.retain(|worker| !worker.is_finished());
        running.store(
            machine.slots - free.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
        if let Some(error) = fatal.lock().unwrap().take() {
            break Err(anyhow::anyhow!(error));
        }
        if !workers.is_empty() {
            idle_since = Instant::now();
        }
        if maintained.elapsed() >= Duration::from_secs(60) {
            maintained = Instant::now();
            maintain(client, machine)?;
        }
        if STOP.load(Ordering::SeqCst) {
            eprintln!("citrus agent {}: stopped", machine.name);
            break Ok(0);
        }
        // Drained: connected, taking no work until `citrus pool resume`.
        if draining.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        if let Some(path) = options.pause_while_locked.iter().find(|path| held(path)) {
            if !paused {
                eprintln!(
                    "citrus agent {}: paused while {} is held",
                    machine.name,
                    path.display()
                );
                paused = true;
            }
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        paused = false;
        let free_now = free.load(Ordering::SeqCst);
        // The governor lowers how many checks run at once beside production.
        let in_use = machine.slots - free_now;
        let limit = free_now.min(allowed.load(Ordering::SeqCst).saturating_sub(in_use));
        // The last free slot waits for a release's checks.
        let reserved = machine.slots > 1 && free_now == 1;
        let claimed = if limit > 0 {
            claim(client, machine, limit, if reserved { 1 } else { i32::MIN })?
        } else {
            None
        };
        if let Some((run, checks)) = claimed {
            free.fetch_sub(checks.len(), Ordering::SeqCst);
            batches += 1;
            let batch = format!("{}-{}-{batches}", run.id, &process[..6]);
            let (url, machine) = (url.to_owned(), machine.clone());
            let (free, fatal) = (free.clone(), fatal.clone());
            workers.push(std::thread::spawn(move || {
                if let Err(error) = work(&url, &machine, &run, &checks, &batch, &free) {
                    *fatal.lock().unwrap() = Some(format!("{error:#}"));
                }
            }));
            idle_since = Instant::now();
            continue;
        }
        if workers.is_empty()
            && let Some(limit) = options.idle_exit
            && idle_since.elapsed() >= Duration::from_secs(limit)
        {
            break Ok(0);
        }
        let _ = client
            .notifications()
            .timeout_iter(Duration::from_secs(if limit > 0 { 5 } else { 1 }))
            .next()?;
        while client.notifications().iter().next()?.is_some() {}
    };
    // STOP kills the executors; their checks go back to the queue.
    for worker in workers {
        let _ = worker.join();
    }
    running.store(0, Ordering::SeqCst);
    result
}

/// One claimed batch on its own connection. Its slots come back as its
/// checks end; an error only when the pool itself cannot be reached.
fn work(
    url: &str,
    machine: &Machine,
    run: &RunRow,
    checks: &[String],
    batch: &str,
    free: &AtomicUsize,
) -> Result<()> {
    let mut released: Vec<String> = Vec::new();
    let outcome = (|| -> Result<()> {
        let mut client = retry_connect(url)?;
        let outcome = execute(
            &mut client,
            machine,
            run,
            checks,
            batch,
            free,
            &mut released,
        );
        let _ = crate::state::Store::drop_schema(url, &batch_schema(batch));
        let rest: Vec<String> = checks
            .iter()
            .filter(|check| !released.contains(check))
            .cloned()
            .collect();
        if let Err(error) = &outcome
            && error.downcast_ref::<Stopped>().is_some()
        {
            // Another agent takes them; nothing ran to completion here.
            client.execute(
                "update citrus.jobs set state = 'queued', agent = null, claimed = null
                 where run = $1 and agent = $2 and state = 'claimed' and check_name = any($3)",
                &[&run.id, &machine.name, &rest],
            )?;
            if !rest.is_empty() {
                emit(
                    &mut client,
                    &run.id,
                    &machine.name,
                    &[format!(
                        "CITRUS_STAGE {} stopped: {} back in the queue",
                        machine.name,
                        rest.join(", ")
                    )],
                )?;
            }
            notify(&mut client, "citrus_jobs", &run.id)?;
            return Ok(());
        }
        if let Err(error) = outcome {
            // The checks fail with the reason; the agent stays.
            let line = format!("citrus agent {}: {error:#}", machine.name);
            eprintln!("{line}");
            emit(&mut client, &run.id, &machine.name, &[line])?;
            for check in &rest {
                emit(
                    &mut client,
                    &run.id,
                    &machine.name,
                    &[format!("CITRUS_TARGET target={check} status=FAIL exit=125")],
                )?;
            }
            settle(&mut client, &run.id, &rest, &BTreeMap::new())?;
        }
        Ok(())
    })();
    free.fetch_add(checks.len() - released.len(), Ordering::SeqCst);
    outcome
}

fn retry_connect(url: &str) -> Result<Client> {
    let mut attempt = 0;
    loop {
        match connect(url) {
            Ok(client) => return Ok(client),
            Err(error) if attempt >= 5 => return Err(error),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

/// Whether another process holds a `flock` on `path` (a missing file is free).
fn held(path: &Path) -> bool {
    use std::os::fd::AsRawFd;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    // SAFETY: flock on a descriptor this function owns.
    let free = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if free {
        // SAFETY: as above; releases the probe lock at once.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }
    !free
}

#[derive(Debug, Clone)]
struct RunRow {
    id: String,
    repo: String,
    commit: String,
    refname: String,
    version: String,
    profile: String,
    image: Option<crate::model::Image>,
    prepare: Vec<String>,
}

/// Take up to `limit` checks of the oldest run this machine can run.
fn claim(
    client: &mut Client,
    machine: &Machine,
    limit: usize,
    min_priority: i32,
) -> Result<Option<(RunRow, Vec<String>)>> {
    let fits = "case when r.image is null then j.requires <@ $1::text[]
                     else ($3 and j.requires <@ $2::text[]) or ($4 and j.requires <@ $1::text[]) end";
    let query = format!(
        "with oldest as (
             select j.run from citrus.jobs j join citrus.runs r on r.id = j.run
             where j.state = 'queued' and r.state = 'open' and r.priority >= $7 and ({fits})
             order by r.priority desc, r.created limit 1),
         picked as (
             select j.run, j.check_name from citrus.jobs j join citrus.runs r on r.id = j.run
             left join citrus.durations d on d.repo = r.repo and d.check_name = j.check_name
             where j.run = (select run from oldest) and j.state = 'queued' and ({fits})
             order by coalesce(d.seconds, 90) desc, j.check_name limit $5 for update of j skip locked)
         update citrus.jobs j set state = 'claimed', agent = $6, claimed = now()
         from picked where j.run = picked.run and j.check_name = picked.check_name
         returning j.run, j.check_name"
    );
    let native = machine.native || !machine.docker;
    let slots = limit as i64;
    let rows = retry(|| {
        client.query(
            &query,
            &[
                &machine.labels,
                &machine.container,
                &machine.docker,
                &native,
                &slots,
                &machine.name,
                &min_priority,
            ],
        )
    })?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let id: String = first.get(0);
    let mut checks: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
    checks.sort();
    let row = client.query_one(
        "select id, repo, commit_sha, ref_name, version, profile, image, prepare from citrus.runs where id = $1",
        &[&id],
    )?;
    let image: Option<serde_json::Value> = row.get(6);
    let run = RunRow {
        id: row.get(0),
        repo: row.get(1),
        commit: row.get(2),
        refname: row.get(3),
        version: row.get(4),
        profile: row.get(5),
        image: image.map(serde_json::from_value).transpose()?,
        prepare: row.get(7),
    };
    Ok(Some((run, checks)))
}

fn emit(client: &mut Client, run: &str, agent: &str, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    retry(|| {
        let mut tx = client.transaction()?;
        let statement =
            tx.prepare("insert into citrus.events (run, agent, line) values ($1, $2, $3)")?;
        for line in lines {
            tx.execute(&statement, &[&run, &agent, line])?;
        }
        tx.execute("select pg_notify('citrus_events', $1)", &[&run])?;
        tx.commit()
    })
}

/// Results of a batch: reported ones as reported, the rest failed.
fn settle(
    client: &mut Client,
    run: &str,
    checks: &[String],
    reported: &BTreeMap<String, (String, Option<f32>)>,
) -> Result<()> {
    for check in checks {
        let (result, seconds) = match reported.get(check) {
            Some((status, seconds)) if status == "PASS" => ("passed", *seconds),
            Some((_, seconds)) => ("failed", *seconds),
            None => ("failed", None),
        };
        retry(|| {
            client.execute(
                "update citrus.jobs set state = 'done', result = $3, seconds = $4, finished = now()
                 where run = $1 and check_name = $2",
                &[&run, check, &result, &seconds],
            )
        })?;
        // A passed check's time moves its smoothed duration (a failure often
        // stops early and says nothing about how long the check takes).
        if let ("passed", Some(seconds)) = (result, seconds) {
            let _ = client.execute(
                "insert into citrus.durations (repo, check_name, seconds)
                 select r.repo, $2, $3 from citrus.runs r where r.id = $1
                 on conflict (repo, check_name)
                 do update set seconds = citrus.durations.seconds * 0.7 + excluded.seconds * 0.3",
                &[&run, check, &seconds],
            );
        }
    }
    client.execute("select pg_notify('citrus_events', $1)", &[&run])?;
    Ok(())
}

fn git_at(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("git {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The run's commit in the tree of a slot: a worktree of a cached mirror of the
/// repository, at a path that never changes (see `slots`).
fn checkout(
    machine: &Machine,
    run: &RunRow,
    slot: &crate::slots::Slot,
) -> Result<(PathBuf, PathBuf)> {
    // Batches of one agent share the mirror: one fetch or worktree at a time.
    static MIRRORS: Mutex<()> = Mutex::new(());
    let _guard = MIRRORS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mirror = machine
        .cache
        .join("repos")
        .join(format!("{}.git", short_hash(&run.repo)));
    if !mirror.join("HEAD").exists() {
        std::fs::create_dir_all(&mirror)?;
        git_at(&mirror, &["init", "--bare", "-q"])?;
    }
    let commit = format!("{}^{{commit}}", run.commit);
    if git_at(&mirror, &["cat-file", "-e", &commit]).is_err() {
        let spec = format!("+{}:{}", run.refname, run.refname);
        git_at(&mirror, &["fetch", "-q", "--no-tags", &run.repo, &spec])?;
    }
    let tree = slot.tree();
    if tree.join(".git").exists() {
        // Unchanged files keep their times, so Cargo rebuilds what changed.
        git_at(
            &tree,
            &["checkout", "-q", "--force", "--detach", &run.commit],
        )?;
        // Whatever the previous run left behind, ignored files included.
        git_at(&tree, &["clean", "-ffdxq"])?;
    } else {
        let _ = std::fs::remove_dir_all(&tree);
        std::fs::create_dir_all(tree.parent().context("slot dir")?)?;
        let _ = git_at(&mirror, &["worktree", "prune"]);
        let target = tree.to_string_lossy().into_owned();
        git_at(
            &mirror,
            &[
                "worktree",
                "add",
                "-q",
                "--force",
                "--detach",
                &target,
                &run.commit,
            ],
        )?;
    }
    overlay(&machine_files(&run.repo), &tree)?;
    Ok((mirror, tree))
}

/// Files this machine adds to every tree of a repository (`#![private]`
/// paths such as local test credentials): `CITRUS_AGENT_FILES/<repo>/…`,
/// by default `~/.config/citrus/files/<repo>/…`, where <repo> is the last
/// segment of the remote URL without `.git`.
fn machine_files(repo: &str) -> PathBuf {
    let name = repo
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(repo)
        .trim_end_matches(".git")
        .to_owned();
    let root = std::env::var_os("CITRUS_AGENT_FILES")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/citrus/files"));
    root.join(name)
}

fn overlay(from: &Path, to: &Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            std::fs::create_dir_all(&target)?;
            overlay(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)
                .with_context(|| format!("copy {} into the tree", source.display()))?;
        }
    }
    Ok(())
}

fn shell_quote(word: &str) -> String {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:,@%+".contains(c))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The executor's argv, after the run's `#![prepare]` command when it has one.
fn with_prepare(prepare: &[String], exe: &str, args: &[String]) -> Vec<String> {
    let mut argv = Vec::new();
    if !prepare.is_empty() {
        // `citrus …` in the command is this run's own executor.
        let script = prepare
            .iter()
            .enumerate()
            .map(|(at, word)| {
                if at == 0 && word == "citrus" {
                    shell_quote(exe)
                } else {
                    shell_quote(word)
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        argv.extend([
            "sh".to_owned(),
            "-c".to_owned(),
            format!("{script} && exec \"$@\""),
            "citrus-prepare".to_owned(),
        ]);
    }
    argv.push(exe.to_owned());
    argv.extend(args.iter().cloned());
    argv
}

/// A Citrus binary of `version` for this machine, or (`linux`) for its containers.
fn executor(
    machine: &Machine,
    version: &str,
    container: Option<&str>,
    announce: &mut dyn FnMut(String) -> Result<()>,
) -> Result<PathBuf> {
    let own = std::env::current_exe()?;
    let same_platform =
        container.is_none_or(|platform| platform == format!("{}/{}", os_label(), arch()));
    if std::env::var("CITRUS_POOL_EXECUTOR").as_deref() == Ok("self") {
        if !same_platform {
            bail!(
                "CITRUS_POOL_EXECUTOR=self cannot run in a {} container",
                container.unwrap_or("")
            );
        }
        return Ok(own);
    }
    if version == VERSION && same_platform {
        return Ok(own);
    }
    if version == "unknown" || version.ends_with("-dirty") || version.len() < 7 {
        bail!("the run's Citrus build {version} is not a commit");
    }
    let platform = container
        .map(|platform| platform.replace('/', "-"))
        .unwrap_or_else(|| format!("{}-{}", os_label(), arch()));
    let dir = machine.cache.join("bin").join(version).join(&platform);
    let binary = dir.join("bin/citrus");
    if binary.exists() {
        return Ok(binary);
    }
    if let Some(url) = url()
        && let Ok(mut client) = connect(&url)
        && fetch_binary(&mut client, version, &platform, &binary)?
    {
        announce(format!(
            "takes Citrus {} for {platform} from the pool",
            &version[..12.min(version.len())]
        ))?;
        return Ok(binary);
    }
    let staging = machine
        .cache
        .join("bin")
        .join(format!(".{version}-{platform}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    announce(format!(
        "builds Citrus {} for {platform} (once per version)",
        &version[..12.min(version.len())]
    ))?;
    // Incremental: one target directory per machine (or Docker volume) kept
    // between versions, profile `fast`; the next commit rebuilds in seconds.
    let status = match container {
        None => Command::new("cargo")
            .args(["install", "--quiet", "--locked", "--git", REPOSITORY])
            .args(["--rev", version, "--profile", "fast", "--root"])
            .arg(&staging)
            .env("CITRUS_BUILD_COMMIT", version)
            .env("CARGO_TARGET_DIR", machine.cache.join("citrus-target"))
            .stdin(Stdio::null())
            .status()
            .context("cargo builds Citrus for this machine")?,
        Some(platform) => Command::new("docker")
            .args(["run", "--rm", "--platform", platform])
            .arg("-v")
            .arg(format!("{}:/out", staging.display()))
            .args([
                "-v",
                "citrus-cargo-registry:/usr/local/cargo/registry",
                "-v",
                "citrus-cargo-git:/usr/local/cargo/git",
                "-v",
                "citrus-build-target:/citrus-target",
                "-e",
                "CARGO_TARGET_DIR=/citrus-target",
                "-e",
                &format!("CITRUS_BUILD_COMMIT={version}"),
                "rust:1-bookworm",
                "cargo",
                "install",
                "--quiet",
                "--locked",
                "--git",
                REPOSITORY,
                "--rev",
                version,
                "--profile",
                "fast",
                "--root",
                "/out",
            ])
            .stdin(Stdio::null())
            .status()
            .context("docker builds Citrus for the containers")?,
    };
    if !status.success() || !staging.join("bin/citrus").exists() {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("could not build Citrus {version} for {platform}");
    }
    std::fs::create_dir_all(dir.parent().context("bin dir")?)?;
    if std::fs::rename(&staging, &dir).is_err() {
        // Another agent on this machine built it first.
        let _ = std::fs::remove_dir_all(&staging);
    }
    // The other agents take this build instead of compiling it again.
    if let Some(url) = url()
        && let Ok(mut client) = connect(&url)
    {
        let _ = store_binary(&mut client, version, &platform, &binary);
    }
    Ok(binary)
}

/// Keep `file` as the build of `commit` for `platform` unless one is held.
fn store_binary(client: &mut Client, commit: &str, platform: &str, file: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(file)?;
    let sha = hex::encode(Sha256::digest(&data));
    client.execute(
        "insert into citrus.binaries (commit_sha, platform, sha256, data) values ($1, $2, $3, $4)
         on conflict (commit_sha, platform) do nothing",
        &[&commit, &platform, &sha, &data],
    )?;
    Ok(sha)
}

/// A published build of `commit` for `platform`, written to `to` (checked
/// against its sha256); false when the pool holds none.
fn fetch_binary(client: &mut Client, commit: &str, platform: &str, to: &Path) -> Result<bool> {
    use sha2::{Digest, Sha256};
    let Some(row) = client.query_opt(
        "select sha256, data from citrus.binaries where commit_sha = $1 and platform = $2",
        &[&commit, &platform],
    )?
    else {
        return Ok(false);
    };
    let (expected, data): (String, Vec<u8>) = (row.get(0), row.get(1));
    if hex::encode(Sha256::digest(&data)) != expected {
        bail!("the pool's Citrus {commit} for {platform} fails its checksum");
    }
    let dir = to.parent().context("binary dir")?;
    std::fs::create_dir_all(dir)?;
    let staging = dir.join(format!(".citrus-{}", std::process::id()));
    std::fs::write(&staging, &data)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&staging, to)?;
    Ok(true)
}

/// A published build of `commit` for `platform`, written to `to`; false when
/// there is no pool or it holds none.
pub fn fetch_published(commit: &str, platform: &str, to: &Path) -> Result<bool> {
    let Some(url) = url() else { return Ok(false) };
    let mut client = connect(&url)?;
    fetch_binary(&mut client, commit, platform, to)
}

/// Publish a Citrus build to the pool under the commit it reports
/// (`citrus --version`) and `platform` (default: this machine's). The first
/// build of a commit stays: whoever took it keeps the same bytes.
pub fn publish_binary(file: &Path, platform: Option<&str>) -> Result<(String, String, String)> {
    let own = format!("{}-{}", os_label(), arch());
    let platform = platform.map(str::to_owned).unwrap_or_else(|| own.clone());
    // A cross-compiled build cannot run here: its `--version` text is read
    // from its bytes instead.
    let text = if platform == own {
        let output = Command::new(file)
            .arg("--version")
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("run {} --version", file.display()))?;
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        embedded_version(&std::fs::read(file)?).unwrap_or_default()
    };
    let commit = text
        .split(['(', ')'])
        .nth(1)
        .filter(|commit| commit.len() == 40 && commit.chars().all(|c| c.is_ascii_hexdigit()))
        .with_context(|| {
            format!(
                "{} reports no clean commit: {}",
                file.display(),
                text.trim()
            )
        })?
        .to_owned();
    let url = url()
        .or_else(|| {
            std::env::var("CITRUS_AGENT_POOL")
                .ok()
                .filter(|value| !value.is_empty())
        })
        .context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    let mut client = connect(&url)?;
    let sha = store_binary(&mut client, &commit, &platform, file)?;
    Ok((commit, platform, sha))
}

/// The `X.Y.Z (<commit>)` version text a Citrus binary carries (`--version`).
fn embedded_version(bytes: &[u8]) -> Option<String> {
    let hex = |c: &u8| c.is_ascii_hexdigit() && !c.is_ascii_uppercase();
    bytes.windows(42).enumerate().find_map(|(at, window)| {
        if window[0] != b'(' || window[41] != b')' || !window[1..41].iter().all(hex) {
            return None;
        }
        // Preceded by "<digits>.<digits>.<digits> ".
        let head = &bytes[at.saturating_sub(16)..at];
        let head = std::str::from_utf8(head).ok()?;
        let version = head
            .rsplit(|c: char| !(c.is_ascii_digit() || c == '.' || c == ' '))
            .next()?;
        let version = version.trim();
        (version.split('.').count() == 3 && head.ends_with(' '))
            .then(|| format!("{version} ({})", String::from_utf8_lossy(&window[1..41])))
    })
}

/// Published Citrus builds, newest first: commit, platform, sha256, bytes.
pub fn binaries() -> Result<Vec<(String, String, String, i64)>> {
    let url = url().context("no pool: set CITRUS_POOL or ~/.config/citrus/pool")?;
    let mut client = connect(&url)?;
    Ok(client
        .query(
            "select commit_sha, platform, sha256, octet_length(data)::bigint from citrus.binaries
             order by created desc limit 50",
            &[],
        )?
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect())
}

/// The image of a run, built on this machine (Docker's cache keeps it cheap).
fn build_image(tree: &Path, image: &crate::model::Image) -> Result<String> {
    let mut command = Command::new("docker");
    command
        .args(["build", "-q", "-f"])
        .arg(tree.join(&image.dockerfile));
    if let Some(target) = &image.target {
        command.args(["--target", target]);
    }
    let output = command
        .arg(tree.join(&image.context))
        .stdin(Stdio::null())
        .output()
        .context("docker build")?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = text.lines().rev().take(20).collect();
        bail!(
            "docker build of {} failed:\n{}",
            image.dockerfile,
            tail.into_iter().rev().collect::<Vec<_>>().join("\n")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The Cargo target directories an image's environment names (see `slots`).
fn slot_targets(image: &str) -> Vec<String> {
    let output = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{range .Config.Env}}{{println .}}{{end}}",
            image,
        ])
        .stdin(Stdio::null())
        .output();
    let mut names = match output {
        Ok(output) if output.status.success() => {
            crate::slots::target_dirs(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Vec::new(),
    };
    if !names.iter().any(|name| name == "cargo-target") {
        names.insert(0, "cargo-target".to_owned());
    }
    names
}

fn execute(
    client: &mut Client,
    machine: &Machine,
    run: &RunRow,
    checks: &[String],
    batch: &str,
    free: &AtomicUsize,
    released: &mut Vec<String>,
) -> Result<()> {
    emit(
        client,
        &run.id,
        &machine.name,
        &[
            "CITRUS_RUNNING".to_owned(),
            format!("CITRUS_STAGE {} runs {}", machine.name, checks.join(", ")),
        ],
    )?;
    // A tree of its own for the batch, at a path that stays the same between runs.
    let slot = crate::slots::claim(
        &machine.cache.join("slots").join(short_hash(&run.repo)),
        machine.slots,
        &checks.join(","),
    )?;
    emit(
        client,
        &run.id,
        &machine.name,
        &[format!(
            "CITRUS_STAGE {} fetches the snapshot",
            machine.name
        )],
    )?;
    let (mirror, tree) = checkout(machine, run, &slot)?;
    let in_container = run.image.is_some() && machine.docker;
    let platform = if in_container {
        docker_platform()
    } else {
        None
    };
    let exe = executor(machine, &run.version, platform.as_deref(), &mut |what| {
        emit(
            client,
            &run.id,
            &machine.name,
            &[format!("CITRUS_STAGE {} {what}", machine.name)],
        )
    })?;
    // What each check brings back when it passes (#[outputs]).
    let outputs: BTreeMap<String, Vec<String>> = client
        .query(
            "select check_name, outputs from citrus.jobs where run = $1 and check_name = any($2)",
            &[&run.id, &checks],
        )?
        .iter()
        .map(|row| (row.get(0), row.get(1)))
        .collect();
    let jobs = checks.len().to_string();
    let mut args: Vec<String> = vec![
        "run".into(),
        "--local".into(),
        "--force".into(),
        "--text".into(),
        "--jobs".into(),
        jobs,
    ];
    if !run.profile.is_empty() {
        args.extend(["--profile".into(), run.profile.clone()]);
    }
    args.extend(checks.iter().cloned());
    let shared = machine.cache.join("shared").join(short_hash(&run.repo));
    std::fs::create_dir_all(&shared)?;
    let env: Vec<(String, String)> = vec![
        ("CITRUS_PROTOCOL".into(), "1".into()),
        ("CITRUS_AGENT".into(), format!("pool:{}", machine.name)),
        ("CITRUS_POOL".into(), String::new()),
        // Only `citrus pool publish` reads it: a check that builds Citrus
        // publishes that build; nested runs stay off the pool.
        ("CITRUS_AGENT_POOL".into(), url().unwrap_or_default()),
        ("CITRUS_POOL_SHARE".into(), machine.share.to_string()),
        // Names per-run resources (Compose projects, ports) on a shared machine.
        ("CITRUS_POOL_RUN".into(), batch.to_owned()),
        // The executor's own state: a throwaway schema of the pool's database,
        // dropped when the batch ends (the requester keeps the results).
        ("CITRUS_STATE".into(), url().unwrap_or_default()),
        ("CITRUS_STATE_SCHEMA".into(), batch_schema(batch)),
        // The repository's own launcher (bin/citrus) runs this build too.
        (
            "CITRUS_BIN".into(),
            if in_container {
                "/usr/local/bin/citrus-pool".to_owned()
            } else {
                exe.to_string_lossy().into_owned()
            },
        ),
        (
            "CITRUS_POOL_CACHE".into(),
            shared.to_string_lossy().into_owned(),
        ),
    ];
    let name = format!("citrus-{}-{}", run.id, short_hash(&checks.join(",")));
    // Docker Desktop shares host folders through a file system on which mmap
    // fails with SIGBUS (pnpm, linkers): there the tree is copied into a
    // volume for the batch, mounted at the same path.
    let desktop = in_container && std::env::consts::OS != "linux";
    let tree_volume = desktop.then(|| format!("{name}-tree"));
    let mut command;
    if in_container {
        emit(
            client,
            &run.id,
            &machine.name,
            &[format!("CITRUS_STAGE {} prepares the image", machine.name)],
        )?;
        let image = build_image(&tree, run.image.as_ref().context("image")?)?;
        command = Command::new("docker");
        command.args([
            "run",
            "--rm",
            "--init",
            "--name",
            &name,
            "--network",
            "host",
        ]);
        command.arg("--cpus").arg(machine.share.to_string());
        // All batches together stay within the share: a cgroup (systemd
        // slice) the agent's installer caps at it.
        if let Some(parent) =
            std::env::var_os("CITRUS_AGENT_CGROUP_PARENT").filter(|value| !value.is_empty())
        {
            command.arg("--cgroup-parent").arg(parent);
        }
        if let Some(volume) = &tree_volume {
            let copied = Command::new("docker")
                .args(["run", "--rm", "--entrypoint", "sh", "-v"])
                .arg(format!("{}:/citrus-source:ro", tree.display()))
                .arg("-v")
                .arg(format!("{volume}:{}", tree.display()))
                .arg(&image)
                .arg("-c")
                .arg(format!(
                    "cp -a /citrus-source/. {}/",
                    shell_quote(&tree.to_string_lossy())
                ))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .status()
                .context("copy the tree into a volume")?;
            if !copied.success() {
                bail!("could not copy the tree into the volume {volume}");
            }
            command
                .arg("-v")
                .arg(format!("{volume}:{}", tree.display()));
        } else {
            command.arg("-v").arg(format!("{0}:{0}", tree.display()));
        }
        command.arg("-v").arg(format!("{0}:{0}", mirror.display()));
        // Caches kept between runs, at a path images can name in ENV. Docker
        // Desktop shares host folders through a file system on which mmap
        // (linkers, databases) fails with SIGBUS: there the cache is a volume.
        let cache = if std::env::consts::OS == "linux" {
            shared.to_string_lossy().into_owned()
        } else {
            format!("citrus-cache-{}", short_hash(&run.repo))
        };
        command.arg("-v").arg(format!("{cache}:/citrus-cache"));
        // Cargo's target directories belong to the slot: what Cargo built in
        // this tree's path is only ever used in this tree's path.
        for name in slot_targets(&image) {
            let source = if std::env::consts::OS == "linux" {
                let dir = slot.cache(&name);
                std::fs::create_dir_all(&dir)?;
                dir.to_string_lossy().into_owned()
            } else {
                format!(
                    "citrus-slot-{}-{}-{name}",
                    short_hash(&run.repo),
                    slot.index
                )
            };
            command
                .arg("-v")
                .arg(format!("{source}:/citrus-cache/{name}"));
        }
        command
            .arg("-v")
            .arg(format!("{}:/usr/local/bin/citrus-pool:ro", exe.display()))
            // Checks call `citrus` like anywhere else.
            .arg("-v")
            .arg(format!("{}:/usr/local/bin/citrus:ro", exe.display()))
            .args(["-v", "/var/run/docker.sock:/var/run/docker.sock"])
            .arg("-w")
            .arg(&tree);
        for (key, value) in &env {
            let value = if key == "CITRUS_POOL_CACHE" {
                "/citrus-cache"
            } else {
                value
            };
            command.arg("-e").arg(format!("{key}={value}"));
        }
        // The tree belongs to the agent's user, not the container's.
        command.args([
            "-e",
            "GIT_CONFIG_COUNT=1",
            "-e",
            "GIT_CONFIG_KEY_0=safe.directory",
            "-e",
            "GIT_CONFIG_VALUE_0=*",
        ]);
        command.arg(image).args(with_prepare(
            &run.prepare,
            "/usr/local/bin/citrus-pool",
            &args,
        ));
    } else {
        let argv = with_prepare(&run.prepare, &exe.to_string_lossy(), &args);
        command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(&tree);
        let target = slot.cache("cargo-target");
        std::fs::create_dir_all(&target)?;
        command.env("CARGO_TARGET_DIR", target);
        for (key, value) in &env {
            command.env(key, value);
        }
        // Checks call `citrus` like anywhere else: this run's build first.
        if let Some(dir) = exe.parent() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![dir.to_path_buf()];
            paths.extend(std::env::split_paths(&path));
            command.env("PATH", std::env::join_paths(paths)?);
        }
        command.env_remove("CITRUS_POOL");
    }
    // One pipe for stdout and stderr keeps the lines in order.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("exec \"$@\" 2>&1")
        .arg("citrus-agent")
        .arg(command.get_program())
        .args(command.get_args())
        .envs(
            command
                .get_envs()
                .filter_map(|(key, value)| value.map(|value| (key.to_owned(), value.to_owned()))),
        )
        .env_remove("CITRUS_POOL")
        .current_dir(command.get_current_dir().unwrap_or(Path::new(".")))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("start the executor")?;
    let (sender, receiver) = mpsc::channel::<String>();
    let stdout = child.stdout.take().context("no stdout")?;
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let mut reported: BTreeMap<String, (String, Option<f32>)> = BTreeMap::new();
    let mut pending: Vec<String> = Vec::new();
    let mut flushed = Instant::now();
    let mut looked = Instant::now();
    let mut cancelled = false;
    let mut stopped = false;
    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(line) => {
                if let Some(rest) = line.strip_prefix("CITRUS_TARGET ") {
                    let field = |name: &str| {
                        rest.split_whitespace()
                            .find_map(|part| part.strip_prefix(name))
                            .map(str::to_owned)
                    };
                    if let (Some(target), Some(status)) = (field("target="), field("status="))
                        && status != "START"
                    {
                        let seconds = field("seconds=").and_then(|value| value.parse().ok());
                        let passed = status == "PASS";
                        reported.insert(target.clone(), (status, seconds));
                        // The result is final: report it and free its slot now.
                        if checks.contains(&target) && !released.contains(&target) {
                            if passed
                                && let Some(globs) =
                                    outputs.get(&target).filter(|globs| !globs.is_empty())
                                && let Some(data) = pack_outputs(&tree, globs)?
                            {
                                client.execute(
                                    "update citrus.jobs set output = $3 where run = $1 and check_name = $2",
                                    &[&run.id, &target, &data],
                                )?;
                            }
                            pending.push(line);
                            emit(client, &run.id, &machine.name, &pending)?;
                            pending.clear();
                            flushed = Instant::now();
                            settle(client, &run.id, std::slice::from_ref(&target), &reported)?;
                            released.push(target);
                            free.fetch_add(1, Ordering::SeqCst);
                            continue;
                        }
                    }
                }
                pending.push(line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if !pending.is_empty()
            && (flushed.elapsed() >= Duration::from_millis(300) || pending.len() >= 200)
        {
            emit(client, &run.id, &machine.name, &pending)?;
            pending.clear();
            flushed = Instant::now();
        }
        if !cancelled && STOP.load(Ordering::SeqCst) {
            cancelled = true;
            stopped = true;
            if in_container {
                let _ = Command::new("docker")
                    .args(["kill", &name])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
        }
        if !cancelled && looked.elapsed() >= Duration::from_secs(2) {
            looked = Instant::now();
            let state: String = client
                .query_one("select state from citrus.runs where id = $1", &[&run.id])?
                .get(0);
            if state == "cancelled" {
                cancelled = true;
                if in_container {
                    let _ = Command::new("docker")
                        .args(["kill", &name])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                let _ = child.kill();
            }
        }
    }
    let _ = reader.join();
    let status = child.wait()?;
    if let Some(volume) = &tree_volume {
        let _ = Command::new("docker")
            .args(["volume", "rm", "-f", volume])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if stopped {
        emit(client, &run.id, &machine.name, &pending)?;
        return Err(Stopped.into());
    }
    let code = status.code().unwrap_or(-1);
    let rest: Vec<String> = checks
        .iter()
        .filter(|check| !released.contains(check))
        .cloned()
        .collect();
    for check in &rest {
        if !reported.contains_key(check) {
            pending.push(format!(
                "CITRUS_TARGET target={check} status=FAIL exit={}",
                if code == 0 { 1 } else { code }
            ));
        }
    }
    emit(client, &run.id, &machine.name, &pending)?;
    // Their slots come back in `work` once the batch is over.
    settle(client, &run.id, &rest, &reported)
}

/// Housekeeping any agent does: requeue stale checks, close abandoned runs,
/// drop old records and this machine's finished worktrees.
fn maintain(client: &mut Client, machine: &Machine) -> Result<()> {
    requeue_stale(client)?;
    client.execute(
        "update citrus.runs set state = 'closed', closed = now()
         where state = 'open' and created < now() - interval '12 hours'",
        &[],
    )?;
    client.execute(
        "delete from citrus.runs where closed < now() - interval '7 days'",
        &[],
    )?;
    // Slots nobody used for two weeks go with their build directories.
    if let Ok(repos) = std::fs::read_dir(machine.cache.join("slots")) {
        for repo in repos.flatten() {
            crate::slots::remove_idle(&repo.path(), crate::slots::IDLE_DAYS);
        }
    }
    let work = machine.cache.join("work");
    let Ok(entries) = std::fs::read_dir(&work) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().into_owned();
        let rows = client.query(
            "select repo from citrus.runs where id = $1 and state = 'open'",
            &[&id],
        )?;
        if !rows.is_empty() {
            continue;
        }
        // The run is over: remove its worktree from whichever mirror holds it.
        let repos = machine.cache.join("repos");
        if let Ok(mirrors) = std::fs::read_dir(&repos) {
            for mirror in mirrors.flatten() {
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(mirror.path())
                    .args(["worktree", "remove", "--force"])
                    .arg(entry.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                let _ = Command::new("git")
                    .arg("-C")
                    .arg(mirror.path())
                    .args(["worktree", "prune"])
                    .status();
            }
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_runs_before_the_executor_in_one_shell() {
        let argv = with_prepare(
            &["python3".into(), "scripts/prep it.py".into()],
            "/bin/citrus",
            &["run".into(), "a".into()],
        );
        assert_eq!(
            argv,
            [
                "sh",
                "-c",
                "python3 'scripts/prep it.py' && exec \"$@\"",
                "citrus-prepare",
                "/bin/citrus",
                "run",
                "a"
            ]
        );
        assert_eq!(
            with_prepare(&[], "/bin/citrus", &["run".into()]),
            ["/bin/citrus", "run"]
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_prepare_command_named_citrus_runs_the_runs_own_executor() {
        let argv = with_prepare(
            &["citrus".into(), "ports".into(), "a.env".into()],
            "/opt/citrus-1",
            &["run".into()],
        );
        assert_eq!(argv[2], "/opt/citrus-1 ports a.env && exec \"$@\"");
    }

    #[test]
    fn machine_files_are_found_by_the_repository_name() {
        assert!(machine_files("https://github.com/o/garvis-app.git").ends_with("garvis-app"));
        assert!(machine_files("git@github.com:o/garvis-app").ends_with("garvis-app"));
    }

    #[test]
    fn a_held_lock_is_seen_and_a_free_one_is_not() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("release.lock");
        assert!(!held(&path), "a missing file is free");
        let holder = std::fs::File::create(&path).unwrap();
        assert!(!held(&path));
        // SAFETY: flock on a descriptor the test owns.
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) };
        assert!(held(&path));
        // SAFETY: as above.
        unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) };
        assert!(!held(&path));
    }

    #[test]
    fn reads_the_version_a_binary_carries() {
        let commit = "48a229ceaefd4985c50990b14116b6d856af0985";
        let bytes = format!("\0\0junk (0123)\0tool 0.3.0 ({commit})\0more").into_bytes();
        assert_eq!(embedded_version(&bytes), Some(format!("0.3.0 ({commit})")));
        assert_eq!(embedded_version(b"no version here"), None);
    }

    #[test]
    fn redacts_the_password_only() {
        assert_eq!(
            redact("postgres://citrus:s3cret@pool.example:5432/citrus?sslmode=require"),
            "postgres://citrus:***@pool.example:5432/citrus?sslmode=require"
        );
        assert_eq!(
            redact("postgres://pool.example/citrus"),
            "postgres://pool.example/citrus"
        );
    }
}
