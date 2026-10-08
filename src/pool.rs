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
use std::sync::{Arc, mpsc};
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
    let home = std::env::var_os("HOME")?;
    let text = std::fs::read_to_string(Path::new(&home).join(".config/citrus/pool")).ok()?;
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
fn migrate(client: &mut Client) -> Result<()> {
    client.batch_execute(
        "begin;
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
        alter table citrus.events add column if not exists tx xid8 not null default pg_current_xact_id();
        create index if not exists events_by_run on citrus.events (run, id);
        create index if not exists jobs_queued on citrus.jobs (state) where state = 'queued';
        commit;",
    )?;
    Ok(())
}

/// Checks of agents that stopped answering go back to the queue.
fn requeue_stale(client: &mut Client) -> Result<Vec<(String, String, String)>> {
    let rows = client.query(
        "update citrus.jobs j set state = 'queued', agent = null, claimed = null
         where j.state = 'claimed' and not exists (
             select 1 from citrus.agents a
             where a.name = j.agent and a.seen > now() - make_interval(secs => $1))
         returning j.run, j.check_name, coalesce(j.agent, '')",
        &[&(STALE_SECONDS as f64)],
    )?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
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
    {
        let mut tx = client.transaction()?;
        tx.execute(
            "insert into citrus.runs (id, repo, commit_sha, ref_name, version, profile, image, requester)
             values ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[&id, &repo, &commit, &refname, &VERSION, &profile, &image, &crate::exec::agent()],
        )?;
        for check in checks {
            let needs = context
                .manifest
                .targets
                .get(check)
                .map(requirements)
                .unwrap_or_default();
            tx.execute(
                "insert into citrus.jobs (run, check_name, requires) values ($1, $2, $3)",
                &[&id, check, &needs],
            )?;
        }
        tx.execute("select pg_notify('citrus_jobs', $1)", &[&id])?;
        tx.commit()?;
    }
    context.store.set_fact(&format!("pool:{id}"), &refname)?;
    on_line(format!("CITRUS_WAIT pool · {} checks queued", checks.len()))?;
    let outcome = follow(&mut client, id, on_line);
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
                    extract(epoch from now() - seen)::float8
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
}

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

fn cache_root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CITRUS_AGENT_CACHE").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("citrus/agent");
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into());
    PathBuf::from(home).join(".cache/citrus/agent")
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
             slots = $6, version = $7, state = 'ready', running = 0, seen = now(), started = now()",
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
    let draining = Arc::new(AtomicBool::new(false));
    let running = Arc::new(AtomicUsize::new(0));
    let heartbeat = {
        let (url, name) = (url.clone(), machine.name.clone());
        let (stop, draining, running) = (stop.clone(), draining.clone(), running.clone());
        std::thread::spawn(move || -> Result<()> {
            let mut client = connect(&url)?;
            while !stop.load(Ordering::SeqCst) {
                let rows = client.query(
                    "update citrus.agents set seen = now(), load = $2, running = $3
                     where name = $1 returning state",
                    &[
                        &name,
                        &load_average(),
                        &(running.load(Ordering::SeqCst) as i32),
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
    let result = serve(&mut client, &machine, options, &draining, &running);
    stop.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();
    let _ = client.execute(
        "update citrus.agents set seen = now() - interval '1 hour', running = 0 where name = $1",
        &[&machine.name],
    );
    result
}

fn serve(
    client: &mut Client,
    machine: &Machine,
    options: &AgentOptions,
    draining: &AtomicBool,
    running: &AtomicUsize,
) -> Result<i32> {
    client.batch_execute("listen citrus_jobs")?;
    let mut idle_since = Instant::now();
    let mut maintained = Instant::now() - Duration::from_secs(3600);
    loop {
        if maintained.elapsed() >= Duration::from_secs(60) {
            maintained = Instant::now();
            maintain(client, machine)?;
        }
        if draining.load(Ordering::SeqCst) {
            eprintln!("citrus agent {}: drained", machine.name);
            return Ok(0);
        }
        if STOP.load(Ordering::SeqCst) {
            eprintln!("citrus agent {}: stopped", machine.name);
            return Ok(0);
        }
        let claimed = claim(client, machine)?;
        if let Some((run, checks)) = claimed {
            running.store(checks.len(), Ordering::SeqCst);
            let outcome = execute(client, machine, &run, &checks);
            running.store(0, Ordering::SeqCst);
            if let Err(error) = &outcome
                && error.downcast_ref::<Stopped>().is_some()
            {
                // Another agent takes them; nothing ran to completion here.
                client.execute(
                    "update citrus.jobs set state = 'queued', agent = null, claimed = null
                     where run = $1 and agent = $2 and state = 'claimed'",
                    &[&run.id, &machine.name],
                )?;
                emit(
                    client,
                    &run.id,
                    &machine.name,
                    &[format!(
                        "CITRUS_STAGE {} stopped: {} back in the queue",
                        machine.name,
                        checks.join(", ")
                    )],
                )?;
                notify(client, "citrus_jobs", &run.id)?;
                continue;
            }
            if let Err(error) = outcome {
                // The checks fail with the reason; the agent stays.
                let line = format!("citrus agent {}: {error:#}", machine.name);
                eprintln!("{line}");
                emit(client, &run.id, &machine.name, &[line])?;
                for check in &checks {
                    emit(
                        client,
                        &run.id,
                        &machine.name,
                        &[format!("CITRUS_TARGET target={check} status=FAIL exit=125")],
                    )?;
                }
                settle(client, &run.id, &checks, &BTreeMap::new())?;
            }
            idle_since = Instant::now();
            continue;
        }
        if let Some(limit) = options.idle_exit
            && idle_since.elapsed() >= Duration::from_secs(limit)
        {
            return Ok(0);
        }
        let _ = client
            .notifications()
            .timeout_iter(Duration::from_secs(5))
            .next()?;
        while client.notifications().iter().next()?.is_some() {}
    }
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
}

/// Take up to `slots` checks of the oldest run this machine can run.
fn claim(client: &mut Client, machine: &Machine) -> Result<Option<(RunRow, Vec<String>)>> {
    let fits = "case when r.image is null then j.requires <@ $1::text[]
                     else ($3 and j.requires <@ $2::text[]) or ($4 and j.requires <@ $1::text[]) end";
    let query = format!(
        "with oldest as (
             select j.run from citrus.jobs j join citrus.runs r on r.id = j.run
             where j.state = 'queued' and r.state = 'open' and ({fits})
             order by r.created limit 1),
         picked as (
             select j.run, j.check_name from citrus.jobs j join citrus.runs r on r.id = j.run
             where j.run = (select run from oldest) and j.state = 'queued' and ({fits})
             order by j.check_name limit $5 for update of j skip locked)
         update citrus.jobs j set state = 'claimed', agent = $6, claimed = now()
         from picked where j.run = picked.run and j.check_name = picked.check_name
         returning j.run, j.check_name"
    );
    let rows = client.query(
        &query,
        &[
            &machine.labels,
            &machine.container,
            &machine.docker,
            &(machine.native || !machine.docker),
            &(machine.slots as i64),
            &machine.name,
        ],
    )?;
    let Some(first) = rows.first() else {
        return Ok(None);
    };
    let id: String = first.get(0);
    let mut checks: Vec<String> = rows.iter().map(|row| row.get(1)).collect();
    checks.sort();
    let row = client.query_one(
        "select id, repo, commit_sha, ref_name, version, profile, image from citrus.runs where id = $1",
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
    };
    Ok(Some((run, checks)))
}

fn emit(client: &mut Client, run: &str, agent: &str, lines: &[String]) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let mut tx = client.transaction()?;
    let statement =
        tx.prepare("insert into citrus.events (run, agent, line) values ($1, $2, $3)")?;
    for line in lines {
        tx.execute(&statement, &[&run, &agent, line])?;
    }
    tx.execute("select pg_notify('citrus_events', $1)", &[&run])?;
    tx.commit()?;
    Ok(())
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
        client.execute(
            "update citrus.jobs set state = 'done', result = $3, seconds = $4, finished = now()
             where run = $1 and check_name = $2",
            &[&run, check, &result, &seconds],
        )?;
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

/// The run's tree in a worktree of a cached mirror of its repository.
fn checkout(machine: &Machine, run: &RunRow) -> Result<(PathBuf, PathBuf)> {
    let mirror = machine
        .cache
        .join("repos")
        .join(format!("{}.git", short_hash(&run.repo)));
    if !mirror.join("HEAD").exists() {
        std::fs::create_dir_all(&mirror)?;
        git_at(&mirror, &["init", "--bare", "-q"])?;
    }
    let tree = machine.cache.join("work").join(&run.id);
    if !tree.join(".git").exists() {
        let spec = format!("+{}:{}", run.refname, run.refname);
        git_at(&mirror, &["fetch", "-q", "--no-tags", &run.repo, &spec])?;
        std::fs::create_dir_all(tree.parent().context("work dir")?)?;
        let target = tree.to_string_lossy().into_owned();
        git_at(
            &mirror,
            &["worktree", "add", "-q", "--detach", &target, &run.commit],
        )?;
    }
    Ok((mirror, tree))
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
    let status = match container {
        None => Command::new("cargo")
            .args([
                "install", "--quiet", "--locked", "--git", REPOSITORY, "--rev", version,
            ])
            .arg("--root")
            .arg(&staging)
            .env("CITRUS_BUILD_COMMIT", version)
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
    Ok(binary)
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

fn execute(client: &mut Client, machine: &Machine, run: &RunRow, checks: &[String]) -> Result<()> {
    emit(
        client,
        &run.id,
        &machine.name,
        &[
            "CITRUS_RUNNING".to_owned(),
            format!("CITRUS_STAGE {} runs {}", machine.name, checks.join(", ")),
        ],
    )?;
    if !machine
        .cache
        .join("work")
        .join(&run.id)
        .join(".git")
        .exists()
    {
        emit(
            client,
            &run.id,
            &machine.name,
            &[format!(
                "CITRUS_STAGE {} fetches the snapshot",
                machine.name
            )],
        )?;
    }
    let (mirror, tree) = checkout(machine, run)?;
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
        ("CITRUS_POOL_SHARE".into(), machine.share.to_string()),
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
        for dir in [&tree, &mirror] {
            command.arg("-v").arg(format!("{0}:{0}", dir.display()));
        }
        // Caches kept between runs, at a path images can name in ENV.
        command
            .arg("-v")
            .arg(format!("{}:/citrus-cache", shared.display()));
        command
            .arg("-v")
            .arg(format!("{}:/usr/local/bin/citrus-pool:ro", exe.display()))
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
        command
            .arg(image)
            .arg("/usr/local/bin/citrus-pool")
            .args(&args);
    } else {
        command = Command::new(&exe);
        command.args(&args).current_dir(&tree);
        for (key, value) in &env {
            command.env(key, value);
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
                        reported.insert(target, (status, seconds));
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
    if stopped {
        emit(client, &run.id, &machine.name, &pending)?;
        return Err(Stopped.into());
    }
    let code = status.code().unwrap_or(-1);
    for check in checks {
        if !reported.contains_key(check) {
            pending.push(format!(
                "CITRUS_TARGET target={check} status=FAIL exit={}",
                if code == 0 { 1 } else { code }
            ));
        }
    }
    emit(client, &run.id, &machine.name, &pending)?;
    settle(client, &run.id, checks, &reported)
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
