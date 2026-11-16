//! Lanes: one queue for each scarce resource of a machine (Xcode and the
//! simulator on the Mac), with a capacity the project sets and priorities.
//!
//! `citrus lane run <lane> [--priority P] -- <command>` waits for its turn,
//! runs the command, and leaves. The queue is a file per lane under
//! `~/.local/state/citrus/lanes` guarded by `flock`: it needs no network,
//! survives any process, and every worktree on the machine sees the same one.
//!
//! - Order: a higher priority goes first (owner-build > release > integrate >
//!   agent-check), equal ones in arrival order. A running command is never
//!   interrupted.
//! - A waiter or holder whose process is gone, or that stopped beating for
//!   [`STALE`], leaves the queue on the next look of anyone: no lane stays
//!   taken by a dead process.
//! - Inside a lane (`CITRUS_LANE` set) the same lane is already held: nested
//!   `citrus lane run` and the shims run the command directly.
//! - The legacy lock file a lane names (`legacy_lock`) is taken too, so scripts
//!   that still use `lockf` on it keep excluding and being excluded; a
//!   descendant of the lane's command that waits for it gets it (the lane lets
//!   go of it), so old scripts run inside a lane do not deadlock.
//! - Every finished entry is a line of `history.jsonl`: wait and run time by
//!   lane and priority for `citrus status` and `citrus stats`.

use anyhow::{Context as _, Result, bail};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

/// Priorities, highest first.
pub const PRIORITIES: &[(&str, i32)] = &[
    ("owner-build", 30),
    ("release", 20),
    ("integrate", 10),
    ("agent-check", 0),
];

/// An entry that stopped beating this long is gone (its process hangs or
/// lives on another machine that stopped).
pub const STALE: f64 = 120.0;
const BEAT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(500);

pub fn priority_value(name: &str) -> Result<i32> {
    PRIORITIES
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, value)| *value)
        .with_context(|| {
            format!(
                "unknown priority `{name}`; one of {}",
                PRIORITIES
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

pub fn priority_name(value: i32) -> &'static str {
    PRIORITIES
        .iter()
        .find(|(_, known)| value >= *known)
        .map_or("agent-check", |(name, _)| name)
}

/// A lane as the project declares it: `#![lane("mac-xcode", capacity = 1)]`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Policy {
    pub name: String,
    pub capacity: usize,
    /// A lock file older scripts take with `lockf` for the same resource.
    #[serde(default)]
    pub legacy_lock: Option<String>,
    /// A file older scripts read as "an owner build waits": kept fresh while
    /// an owner-build waits in this lane.
    #[serde(default)]
    pub legacy_priority: Option<String>,
    /// What the lane is for (shown by `citrus lane status`).
    #[serde(default)]
    pub about: String,
    /// The longest a command may hold the lane, seconds (0: no limit): then its
    /// whole process group is stopped and the lane is free.
    #[serde(default)]
    pub ttl: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub id: String,
    pub priority: i32,
    pub label: String,
    pub host: String,
    pub pid: i64,
    #[serde(default)]
    pub child: i64,
    pub cwd: String,
    pub enqueued: f64,
    #[serde(default)]
    pub started: Option<f64>,
    pub beat: f64,
    /// Why a started entry is not running its command yet (legacy lock).
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Finished {
    pub lane: String,
    pub priority: i32,
    pub label: String,
    pub wait: f64,
    pub run: f64,
    pub exit: Option<i32>,
    /// passed, failed, cancelled (left the queue), lost (process gone)
    pub outcome: String,
    pub ended: f64,
}

pub fn root() -> PathBuf {
    if let Some(dir) = std::env::var_os("CITRUS_LANES_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    home().join(".local/state/citrus/lanes")
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn host() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: gethostname writes at most the buffer's length.
    let ok = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let name = if ok {
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        String::from_utf8_lossy(&buffer[..end]).into_owned()
    } else {
        String::new()
    };
    name.split('.').next().unwrap_or_default().to_owned()
}

/// SIGTERM to a process group, SIGKILL if it is still there after 5 s.
fn stop_group(leader: i64) {
    if leader <= 1 {
        return;
    }
    // SAFETY: plain signal delivery to a process group this lane started.
    unsafe { libc::kill(-(leader as libc::pid_t), libc::SIGTERM) };
    for _ in 0..50 {
        if !alive(leader) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: as above.
    unsafe { libc::kill(-(leader as libc::pid_t), libc::SIGKILL) };
}

fn alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 only checks for existence.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

// ---------------------------------------------------------------- policy

/// Remember the project's lanes where `citrus lane` finds them without the
/// project (a command in a checkout outside the repository uses them too).
pub fn sync_policy(lanes: &[Policy]) {
    if lanes.is_empty() {
        return;
    }
    let path = root().join("policy.json");
    let mut known = policies();
    for lane in lanes {
        known.insert(lane.name.clone(), lane.clone());
    }
    let Ok(text) = serde_json::to_string_pretty(&known) else {
        return;
    };
    if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
        return;
    }
    let _ = std::fs::create_dir_all(root());
    let temporary = path.with_extension(format!("json.{}", std::process::id()));
    if std::fs::write(&temporary, text).is_ok() {
        let _ = std::fs::rename(&temporary, &path);
    }
}

pub fn policies() -> BTreeMap<String, Policy> {
    std::fs::read_to_string(root().join("policy.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn policy(lane: &str) -> Policy {
    policies().remove(lane).unwrap_or_else(|| Policy {
        name: lane.to_owned(),
        capacity: 1,
        ..Policy::default()
    })
}

fn valid_name(lane: &str) -> Result<()> {
    if lane.is_empty()
        || !lane
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        bail!("lane names are letters, digits, - and _: `{lane}`");
    }
    Ok(())
}

// ---------------------------------------------------------------- queue

/// The lane's queue, locked while this value lives.
struct Queue {
    lane: String,
    ttl: u64,
    path: PathBuf,
    _lock: File,
    entries: Vec<Entry>,
}

impl Queue {
    fn open(lane: &str) -> Result<Queue> {
        valid_name(lane)?;
        let dir = root();
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{lane}.lock")))?;
        // SAFETY: flock on a descriptor this value owns; released when it drops.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("lock the lane {lane}: {}", std::io::Error::last_os_error());
        }
        let path = dir.join(format!("{lane}.json"));
        let entries = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let mut queue = Queue {
            lane: lane.to_owned(),
            ttl: policy(lane).ttl,
            path,
            _lock: lock,
            entries,
        };
        queue.prune();
        // An empty lane leaves no "owner build waits" marker behind (a holder
        // that died would otherwise keep old scripts yielding for 30 minutes).
        if queue.entries.is_empty()
            && let Some(marker) = policy(lane).legacy_priority
            && std::fs::read_to_string(&marker).is_ok_and(|text| text.starts_with("citrus lane"))
        {
            let _ = std::fs::remove_file(marker);
        }
        Ok(queue)
    }

    /// Entries whose process is gone leave the queue (and are recorded).
    fn prune(&mut self) {
        let here = host();
        let at = now();
        let (gone, kept): (Vec<Entry>, Vec<Entry>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|entry| {
                let stale = at - entry.beat > STALE
                    || (self.ttl > 0
                        && entry
                            .started
                            .is_some_and(|started| at - started > self.ttl as f64 + STALE));
                let dead = entry.host == here && !alive(entry.pid) && !alive(entry.child);
                stale || dead
            });
        self.entries = kept;
        for entry in &gone {
            // The holder is gone but its command runs on: the lease ended, so
            // its whole process group ends too. Nothing outlives its lease.
            if entry.host == here && entry.child > 0 && alive(entry.child) {
                stop_group(entry.child);
            }
        }
        for entry in gone {
            record(&Finished {
                lane: self.lane.clone(),
                priority: entry.priority,
                label: entry.label.clone(),
                wait: entry.started.unwrap_or(at) - entry.enqueued,
                run: entry.started.map_or(0.0, |started| at - started),
                exit: None,
                outcome: "lost".into(),
                ended: at,
            });
        }
    }

    fn save(&self) -> Result<()> {
        let temporary = self.path.with_extension("json.new");
        std::fs::write(&temporary, serde_json::to_string_pretty(&self.entries)?)?;
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }

    fn find(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }

    /// Whether `id` may start now: fewer running than the capacity and no
    /// waiter ahead of it beyond the free places.
    fn may_start(&self, id: &str, capacity: usize) -> bool {
        let running = self
            .entries
            .iter()
            .filter(|entry| entry.started.is_some())
            .count();
        let free = capacity.max(1).saturating_sub(running);
        waiting_order(&self.entries)
            .iter()
            .take(free)
            .any(|entry| entry.id == id)
    }
}

/// Waiters in the order they start.
pub fn waiting_order(entries: &[Entry]) -> Vec<&Entry> {
    let mut waiting: Vec<&Entry> = entries
        .iter()
        .filter(|entry| entry.started.is_none())
        .collect();
    waiting.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then(left.enqueued.total_cmp(&right.enqueued))
            .then(left.id.cmp(&right.id))
    });
    waiting
}

fn record(finished: &Finished) {
    let Ok(line) = serde_json::to_string(finished) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root().join("history.jsonl"))
    {
        let _ = writeln!(file, "{line}");
    }
}

/// The lane's entries now (dead ones removed).
pub fn entries(lane: &str) -> Result<Vec<Entry>> {
    let queue = Queue::open(lane)?;
    queue.save()?;
    Ok(queue.entries.clone())
}

/// Every lane with a queue file or a policy.
pub fn lanes() -> Vec<String> {
    let mut names: Vec<String> = policies().into_keys().collect();
    if let Ok(dir) = std::fs::read_dir(root()) {
        for entry in dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(lane) = name.strip_suffix(".json")
                && lane != "policy"
            {
                names.push(lane.to_owned());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

pub fn history(since: f64) -> Vec<Finished> {
    std::fs::read_to_string(root().join("history.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<Finished>(line).ok())
        .filter(|finished| finished.ended >= since)
        .collect()
}

// ---------------------------------------------------------------- legacy lock

/// The process table: pid → (parent, command).
fn processes() -> BTreeMap<i64, (i64, String)> {
    let Ok(output) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,command="])
        .stderr(Stdio::null())
        .output()
    else {
        return BTreeMap::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let parent = words.next()?.parse().ok()?;
            Some((pid, (parent, words.collect::<Vec<_>>().join(" "))))
        })
        .collect()
}

fn takes_lock(command: &str, path: &str) -> bool {
    command.contains("lockf") && command.contains(path)
}

/// Processes that have `path` open (a lock holder keeps it open).
fn holders(path: &str) -> Vec<i64> {
    Command::new("lsof")
        .args(["-t", "--", path])
        .stderr(Stdio::null())
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .filter_map(|pid| pid.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether an ancestor of this process holds `path`: an old script took the
/// lock (`lockf`, or any process that keeps the file open) and runs this
/// command inside it. Then the resource is already this command's.
fn ancestor_takes(table: &BTreeMap<i64, (i64, String)>, path: &str) -> bool {
    let open = holders(path);
    let mut pid = i64::from(std::process::id());
    for _ in 0..64 {
        let Some((parent, command)) = table.get(&pid) else {
            return false;
        };
        if pid != i64::from(std::process::id())
            && (takes_lock(command, path) || open.contains(&pid))
        {
            return true;
        }
        if *parent <= 1 {
            return false;
        }
        pid = *parent;
    }
    false
}

/// Whether a descendant of `root_pid` waits for or holds `path` with `lockf`.
fn descendant_takes(table: &BTreeMap<i64, (i64, String)>, root_pid: i64, path: &str) -> bool {
    table.iter().any(|(pid, (_, command))| {
        if !takes_lock(command, path) {
            return false;
        }
        let mut current = *pid;
        for _ in 0..64 {
            let Some((parent, _)) = table.get(&current) else {
                return false;
            };
            if *parent == root_pid {
                return true;
            }
            if *parent <= 1 {
                return false;
            }
            current = *parent;
        }
        false
    })
}

fn try_flock(path: &str) -> Option<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ok()?;
    // SAFETY: flock on a descriptor this function owns.
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(file)
}

// ---------------------------------------------------------------- run

static SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::SeqCst);
}

pub struct Request<'a> {
    pub lane: &'a str,
    pub priority: i32,
    pub label: Option<&'a str>,
    pub command: &'a [String],
    pub quiet: bool,
}

fn new_id() -> String {
    format!(
        "{}-{}-{:x}",
        host(),
        std::process::id(),
        (now() * 1000.0) as u64 % 0xffff_ffff
    )
}

/// The command's argv: one argument with a space is a shell command line.
fn argv(command: &[String]) -> Vec<String> {
    match command {
        [line] if line.contains(char::is_whitespace) => {
            vec!["/bin/sh".into(), "-c".into(), line.clone()]
        }
        _ => command.to_vec(),
    }
}

/// Wait for the lane, run the command, return its exit code.
pub fn run(request: &Request) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    valid_name(request.lane)?;
    if request.command.is_empty() {
        bail!("citrus lane run {} -- <command>", request.lane);
    }
    let argv = argv(request.command);
    let held: Vec<String> = std::env::var("CITRUS_LANE")
        .unwrap_or_default()
        .split(',')
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if held.iter().any(|name| name == request.lane) {
        // Already inside this lane: run in place.
        let error = Command::new(&argv[0]).args(&argv[1..]).exec();
        bail!("run {}: {error}", argv[0]);
    }
    let policy = policy(request.lane);
    // Inside an old script that holds the legacy lock the resource is already
    // held: run in place. Waiting for the lane here deadlocks (the lane's
    // holder waits for that same lock, 2026-11-16).
    if let Some(path) = &policy.legacy_lock
        && ancestor_takes(&processes(), path)
    {
        let mut lanes = held.clone();
        lanes.push(request.lane.to_owned());
        let error = Command::new(&argv[0])
            .args(&argv[1..])
            .env("CITRUS_LANE", lanes.join(","))
            .exec();
        bail!("run {}: {error}", argv[0]);
    }
    let label = request.label.map_or_else(
        || {
            argv.iter()
                .map(|word| word.rsplit('/').next().unwrap_or(word))
                .take(4)
                .collect::<Vec<_>>()
                .join(" ")
        },
        str::to_owned,
    );
    let label: String = label.chars().take(120).collect();
    let id = new_id();
    let enqueued = now();
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, on_signal as *const () as libc::sighandler_t);
    }
    {
        let mut queue = Queue::open(request.lane)?;
        queue.entries.push(Entry {
            id: id.clone(),
            priority: request.priority,
            label: label.clone(),
            host: host(),
            pid: i64::from(std::process::id()),
            child: 0,
            cwd: std::env::current_dir()
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default(),
            enqueued,
            started: None,
            beat: enqueued,
            note: String::new(),
        });
        queue.save()?;
    }
    let leave = |outcome: &str, started: Option<f64>, exit: Option<i32>| {
        if let Ok(mut queue) = Queue::open(request.lane) {
            queue.entries.retain(|entry| entry.id != id);
            let _ = queue.save();
            let at = now();
            record(&Finished {
                lane: request.lane.to_owned(),
                priority: request.priority,
                label: label.clone(),
                wait: started.unwrap_or(at) - enqueued,
                run: started.map_or(0.0, |started| at - started),
                exit,
                outcome: outcome.to_owned(),
                ended: at,
            });
            if let Some(marker) = &policy.legacy_priority
                && !queue.entries.iter().any(|entry| {
                    entry.started.is_none()
                        && entry.priority >= priority_value("owner-build").unwrap_or(30)
                })
            {
                let _ = std::fs::remove_file(marker);
            }
        }
    };
    // Wait for the turn.
    let mut said = false;
    let started = loop {
        if SIGNAL.load(Ordering::SeqCst) != 0 {
            leave("cancelled", None, None);
            return Ok(130);
        }
        let mut queue = Queue::open(request.lane)?;
        let capacity = policy.capacity;
        if queue.find(&id).is_none() {
            // Removed as stale (this process was suspended): queue again.
            queue.entries.push(Entry {
                id: id.clone(),
                priority: request.priority,
                label: label.clone(),
                host: host(),
                pid: i64::from(std::process::id()),
                child: 0,
                cwd: String::new(),
                enqueued,
                started: None,
                beat: now(),
                note: String::new(),
            });
        }
        let go = queue.may_start(&id, capacity);
        let at = now();
        let ahead: Vec<String> = queue
            .entries
            .iter()
            .filter(|entry| entry.id != id)
            .filter(|entry| {
                entry.started.is_some()
                    || entry.priority > request.priority
                    || (entry.priority == request.priority && entry.enqueued < enqueued)
            })
            .map(|entry| {
                format!(
                    "{} ({}{})",
                    entry.label,
                    priority_name(entry.priority),
                    if entry.started.is_some() {
                        ", running"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        if let Some(entry) = queue.find(&id) {
            entry.beat = at;
            if go {
                entry.started = Some(at);
            }
        }
        queue.save()?;
        drop(queue);
        if go {
            break at;
        }
        if request.priority >= priority_value("owner-build")?
            && let Some(marker) = &policy.legacy_priority
        {
            let _ = std::fs::write(marker, "citrus lane: an owner build waits\n");
        }
        if !said && !request.quiet {
            eprintln!(
                "citrus lane {}: waiting ({} {}) behind {}",
                request.lane,
                priority_name(request.priority),
                label,
                ahead.join("; ")
            );
            said = true;
        }
        std::thread::sleep(POLL);
    };
    // Older scripts' lock: held beside the lane until they all use lanes.
    let mut legacy: Option<File> = None;
    if let Some(path) = &policy.legacy_lock {
        {
            let mut said = false;
            loop {
                if SIGNAL.load(Ordering::SeqCst) != 0 {
                    leave("cancelled", None, None);
                    return Ok(130);
                }
                if let Some(file) = try_flock(path) {
                    legacy = Some(file);
                    break;
                }
                if !said && !request.quiet {
                    eprintln!(
                        "citrus lane {}: waiting for {path}, held by a script outside the lane",
                        request.lane
                    );
                    said = true;
                }
                // Old scripts yield the legacy lock while this marker is
                // fresh (they read it as "an owner build waits"): the lane's
                // holder is not starved by a stream of lockf waiters.
                if let Some(marker) = &policy.legacy_priority {
                    let _ = std::fs::write(marker, "citrus lane: the lane's holder waits\n");
                }
                if let Ok(mut queue) = Queue::open(request.lane) {
                    if let Some(entry) = queue.find(&id) {
                        entry.beat = now();
                        entry.note = format!("waits for the legacy lock {path}");
                    }
                    let _ = queue.save();
                }
                // Short polls: a blocked lockf wakes at once when the lock frees.
                for _ in 0..5 {
                    if let Some(file) = try_flock(path) {
                        legacy = Some(file);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                if legacy.is_some() {
                    break;
                }
            }
        }
    }
    if !request.quiet && said {
        eprintln!(
            "citrus lane {}: running after {:.0} s in the queue",
            request.lane,
            now() - enqueued
        );
    }
    let mut lanes = held.clone();
    lanes.push(request.lane.to_owned());
    // The command and everything it starts form one process group: when the
    // lease ends (done, cancelled, TTL, holder gone) the whole group ends.
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .env("CITRUS_LANE", lanes.join(","))
        .env("CITRUS_LANE_ID", &id)
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            leave("failed", Some(started), Some(127));
            bail!("run {}: {error}", argv[0]);
        }
    };
    let child_pid = i64::from(child.id());
    let mut last_beat = std::time::Instant::now() - BEAT;
    let mut last_look = std::time::Instant::now();
    let mut forwarded = 0;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        let signal = SIGNAL.load(Ordering::SeqCst);
        if signal != 0 && signal != forwarded {
            // SAFETY: plain signal delivery to our own child's group.
            unsafe { libc::kill(-(child_pid as libc::pid_t), signal) };
            forwarded = signal;
        }
        if policy.ttl > 0 && now() - started > policy.ttl as f64 {
            eprintln!(
                "citrus lane {}: {label} held the lane longer than its {} s; stopped",
                request.lane, policy.ttl
            );
            stop_group(child_pid);
            let _ = child.wait();
            leave("ttl", Some(started), Some(124));
            return Ok(124);
        }
        if last_beat.elapsed() >= BEAT {
            last_beat = std::time::Instant::now();
            if let Ok(mut queue) = Queue::open(request.lane) {
                if let Some(entry) = queue.find(&id) {
                    entry.beat = now();
                    entry.child = child_pid;
                    entry.note.clear();
                } else {
                    // Pruned while this process was stopped: take the place back.
                    queue.entries.push(Entry {
                        id: id.clone(),
                        priority: request.priority,
                        label: label.clone(),
                        host: host(),
                        pid: i64::from(std::process::id()),
                        child: child_pid,
                        cwd: String::new(),
                        enqueued,
                        started: Some(started),
                        beat: now(),
                        note: String::new(),
                    });
                }
                let _ = queue.save();
            }
        }
        // An old script inside the lane waits for the legacy lock: it is ours.
        if let (Some(path), Some(_)) = (&policy.legacy_lock, &legacy)
            && last_look.elapsed() >= Duration::from_secs(1)
        {
            last_look = std::time::Instant::now();
            if descendant_takes(&processes(), child_pid, path) {
                legacy = None;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    drop(legacy);
    // What the command left running in its group ends with the lease.
    // SAFETY: the group is the command's own.
    unsafe { libc::kill(-(child_pid as libc::pid_t), libc::SIGTERM) };
    let code = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(0)
    });
    leave(
        if code == 0 { "passed" } else { "failed" },
        Some(started),
        Some(code),
    );
    Ok(code)
}

// ---------------------------------------------------------------- report

#[derive(Debug, Clone, serde::Serialize)]
pub struct LaneState {
    pub lane: String,
    pub capacity: usize,
    pub about: String,
    pub running: Vec<Entry>,
    pub waiting: Vec<Entry>,
}

pub fn states() -> Vec<LaneState> {
    let policies = policies();
    lanes()
        .into_iter()
        .filter_map(|lane| {
            let entries = entries(&lane).ok()?;
            let policy = policies.get(&lane).cloned().unwrap_or_default();
            Some(LaneState {
                capacity: policy.capacity.max(1),
                about: policy.about,
                running: entries
                    .iter()
                    .filter(|entry| entry.started.is_some())
                    .cloned()
                    .collect(),
                waiting: waiting_order(&entries).into_iter().cloned().collect(),
                lane,
            })
        })
        .collect()
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Times {
    pub lane: String,
    pub priority: String,
    pub count: usize,
    pub lost: usize,
    pub wait_avg: f64,
    pub wait_max: f64,
    pub run_avg: f64,
    pub run_max: f64,
}

/// Wait and run time by lane and priority since `since`.
pub fn times(since: f64) -> Vec<Times> {
    let mut groups: BTreeMap<(String, i32), Vec<Finished>> = BTreeMap::new();
    for finished in history(since) {
        groups
            .entry((finished.lane.clone(), -finished.priority))
            .or_default()
            .push(finished);
    }
    groups
        .into_iter()
        .map(|((lane, priority), items)| {
            let ran: Vec<&Finished> = items
                .iter()
                .filter(|item| matches!(item.outcome.as_str(), "passed" | "failed"))
                .collect();
            let count = items.len().max(1) as f64;
            Times {
                lane,
                priority: priority_name(-priority).to_owned(),
                count: items.len(),
                lost: items.iter().filter(|item| item.outcome == "lost").count(),
                wait_avg: items.iter().map(|item| item.wait).sum::<f64>() / count,
                wait_max: items.iter().map(|item| item.wait).fold(0.0, f64::max),
                run_avg: ran.iter().map(|item| item.run).sum::<f64>() / ran.len().max(1) as f64,
                run_max: ran.iter().map(|item| item.run).fold(0.0, f64::max),
            }
        })
        .collect()
}

pub fn describe_entry(entry: &Entry) -> String {
    let at = now();
    let age = |seconds: f64| crate::report::age(seconds.max(0.0) as i64);
    let place = Path::new(&entry.cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    match entry.started {
        Some(started) => format!(
            "{} · {} · {} · running {}{}{}",
            entry.label,
            priority_name(entry.priority),
            place,
            age(at - started),
            if entry.note.is_empty() { "" } else { " · " },
            entry.note
        ),
        None => format!(
            "{} · {} · {} · waiting {}",
            entry.label,
            priority_name(entry.priority),
            place,
            age(at - entry.enqueued)
        ),
    }
}

// ---------------------------------------------------------------- guard

/// The Mac's heavy tools: what a direct call in an agent's shell must not
/// start outside `mac-xcode`.
const LIGHT_XCODEBUILD: &[&str] = &[
    "-version",
    "-list",
    "-showsdks",
    "-showBuildSettings",
    "-showdestinations",
    "-help",
    "-usage",
    "-find",
    "-checkFirstLaunchStatus",
    "-runFirstLaunch",
];
// xc-build.sh takes the lane itself.
const HEAVY_SCRIPTS: &[&str] = &[
    "ship.sh",
    "perf-sim.sh",
    "perf-device.sh",
    "test-smoke.sh",
    "test-fast.sh",
    "build-core.sh",
    "run-device.sh",
    "testflight.sh",
];

/// Whether `citrus lane install` put a shim for `name` in ~/.local/bin.
fn shimmed(name: &str) -> bool {
    std::fs::read_to_string(home().join(".local/bin").join(name))
        .is_ok_and(|text| text.contains("Citrus lane shim"))
}

/// What in a shell command line needs the Mac's Xcode lane, if anything.
/// Only words in command position count (after `;`, `|`, `&&`, wrappers such
/// as `timeout`, `env`, `lockf`, `bash`), so `grep xcodebuild` is not a build.
/// A bare `xcodebuild` or `xcrun` passes when its shim takes the lane itself.
pub fn heavy(command: &str) -> Option<String> {
    heavy_with(command, &shimmed)
}

/// The command line without here-document bodies: they are a program's
/// input (a Python script, a file), not commands of this shell.
fn without_heredocs(command: &str) -> String {
    let mut out = String::new();
    let mut terminator: Option<String> = None;
    for line in command.lines() {
        if let Some(end) = &terminator {
            if line.trim() == end {
                terminator = None;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if let Some(at) = line.find("<<") {
            let rest = line[at + 2..].trim_start_matches('-').trim_start();
            let word: String = rest
                .trim_start_matches(['\'', '"'])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !word.is_empty() {
                terminator = Some(word);
            }
        }
    }
    out
}

/// Quoted text is data (an `echo`, a commit message, JSON), except the
/// script of `sh -c '…'` / `bash -c "…"`, which is a command line itself.
fn unquote(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        if c != '\'' && c != '"' {
            out.push(c);
            continue;
        }
        let mut inner = String::new();
        let mut escaped = false;
        for next in chars.by_ref() {
            if c == '"' && next == '\\' && !escaped {
                escaped = true;
                continue;
            }
            if next == c && !escaped {
                break;
            }
            escaped = false;
            inner.push(next);
        }
        let before = out.trim_end();
        if before.ends_with(" -c") || before.ends_with("\t-c") || before == "-c" {
            out.push_str(" ;");
            out.push_str(&unquote(&inner));
            out.push_str(" ;");
        } else {
            out.push_str("QUOTED");
        }
    }
    out
}

fn heavy_with(command: &str, shimmed: &dyn Fn(&str) -> bool) -> Option<String> {
    if command.contains("citrus lane run") || command.contains("citrus-lane") {
        return None;
    }
    let command = unquote(&without_heredocs(command));
    let segments = command.split([';', '|', '&', '(', ')', '`', '\n', '{', '}']);
    for segment in segments {
        let words: Vec<String> = segment
            .split_whitespace()
            .map(|word| word.trim_matches(|c| c == '"' || c == '\'').to_owned())
            .filter(|word| !word.is_empty())
            .collect();
        let mut index = 0;
        let mut path_changed = false;
        // Assignments and wrappers before the command.
        while let Some(word) = words.get(index) {
            let base = word.rsplit('/').next().unwrap_or(word);
            if word.contains('=') && !word.starts_with('-') && !word.starts_with('/') {
                path_changed |= word.starts_with("PATH=");
                index += 1;
                continue;
            }
            match base {
                "env" | "exec" | "time" | "nice" | "nohup" | "caffeinate" | "command" | "sudo" => {
                    index += 1;
                    while words.get(index).is_some_and(|word| word.starts_with('-')) {
                        index += 1;
                        if base == "nice" {
                            index += 1;
                        }
                    }
                }
                "timeout" | "gtimeout" => {
                    index += 1;
                    while words.get(index).is_some_and(|word| word.starts_with('-')) {
                        index += 1;
                    }
                    index += 1; // the duration
                }
                "lockf" => {
                    index += 1;
                    while let Some(word) = words.get(index).filter(|word| word.starts_with('-')) {
                        index += if word == "-t" { 2 } else { 1 };
                    }
                    index += 1; // the lock file
                }
                "bash" | "sh" | "zsh" => {
                    index += 1;
                    while words.get(index).is_some_and(|word| word.starts_with('-')) {
                        index += 1;
                    }
                }
                _ => break,
            }
        }
        let Some(word) = words.get(index) else {
            continue;
        };
        let base = word.rsplit('/').next().unwrap_or(word);
        let next = words.get(index + 1).map_or("", String::as_str);
        let through_shim = !word.contains('/') && !path_changed;
        if base == "xcodebuild"
            && !next.is_empty()
            && !LIGHT_XCODEBUILD.contains(&next)
            && !(through_shim && shimmed("xcodebuild"))
        {
            return Some("xcodebuild".into());
        }
        if base == "xcrun" {
            let after = words.get(index + 2).map_or("", String::as_str);
            let heavy = next == "xcodebuild" || (next == "simctl" && after == "boot");
            if heavy && !(through_shim && shimmed("xcrun")) {
                return Some(format!("xcrun {next}"));
            }
        }
        if base == "simctl" && next == "boot" {
            return Some("simctl boot".into());
        }
        if HEAVY_SCRIPTS.contains(&base) {
            return Some(base.to_owned());
        }
    }
    None
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A Claude Code `PreToolUse` hook on Bash: a heavy Mac command outside the
/// lane is refused with the command that runs it in the lane.
pub fn hook(input: &str) -> (i32, String) {
    let value: serde_json::Value = serde_json::from_str(input).unwrap_or_default();
    if value["tool_name"].as_str().unwrap_or("Bash") != "Bash" {
        return (0, String::new());
    }
    let command = value["tool_input"]["command"].as_str().unwrap_or_default();
    let Some(what) = heavy(command) else {
        return (0, String::new());
    };
    record_violation(&what, command);
    let reason = format!(
        "{what} on the Mac runs only in the Citrus lane mac-xcode (one at a time, owner builds first). Run instead:\n  citrus-lane mac-xcode -- {}\n(owner build: citrus-lane mac-xcode --priority owner-build -- …; queue: citrus-lane status)",
        shell_quote(command)
    );
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    (0, output.to_string())
}

fn record_violation(what: &str, command: &str) {
    let line = serde_json::json!({
        "at": now(),
        "what": what,
        "command": command.chars().take(200).collect::<String>(),
        "cwd": std::env::current_dir().map(|dir| dir.to_string_lossy().into_owned()).unwrap_or_default(),
    });
    let _ = std::fs::create_dir_all(root());
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(root().join("violations.jsonl"))
    {
        let _ = writeln!(file, "{line}");
    }
}

pub fn violations(since: f64) -> Vec<serde_json::Value> {
    std::fs::read_to_string(root().join("violations.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|value| value["at"].as_f64().unwrap_or(0.0) >= since)
        .collect()
}

/// Heavy Xcode processes running outside any lane (and outside the legacy
/// lock): pid and command.
pub fn outside() -> Vec<(i64, String)> {
    let table = processes();
    let lanes_running: Vec<i64> = lanes()
        .iter()
        .filter_map(|lane| entries(lane).ok())
        .flatten()
        .filter(|entry| entry.started.is_some())
        .flat_map(|entry| [entry.pid, entry.child])
        .collect();
    let mut found = Vec::new();
    for (pid, (_, command)) in &table {
        let first = command.split_whitespace().next().unwrap_or("");
        let base = first.rsplit('/').next().unwrap_or(first);
        if base != "xcodebuild" {
            continue;
        }
        let mut current = *pid;
        let mut inside = false;
        for _ in 0..64 {
            let Some((parent, command)) = table.get(&current) else {
                break;
            };
            if lanes_running.contains(&current) || command.contains("lockf") {
                inside = true;
                break;
            }
            if *parent <= 1 {
                break;
            }
            current = *parent;
        }
        if !inside {
            found.push((*pid, command.chars().take(160).collect()));
        }
    }
    found
}

// ---------------------------------------------------------------- install

const SHIM_XCODEBUILD: &str = r#"#!/bin/sh
# Citrus lane shim (citrus lane install): Xcode builds and tests on this Mac
# take turns in the lane mac-xcode. Light queries run directly.
case "$1" in
  ""|-version|-list|-showsdks|-showBuildSettings|-showdestinations|-help|-usage|-find|-checkFirstLaunchStatus|-runFirstLaunch)
    exec /usr/bin/xcodebuild "$@" ;;
esac
case ",$CITRUS_LANE," in *,mac-xcode,*) exec /usr/bin/xcodebuild "$@" ;; esac
CITRUS_NO_PIN=1 exec "$HOME/.local/lib/citrus/lane/citrus" lane run mac-xcode --label "xcodebuild $*" -- /usr/bin/xcodebuild "$@"
"#;

const SHIM_XCRUN: &str = r#"#!/bin/sh
# Citrus lane shim (citrus lane install): `xcrun xcodebuild` and `xcrun simctl
# boot` take turns in the lane mac-xcode; everything else runs directly.
case ",$CITRUS_LANE," in *,mac-xcode,*) exec /usr/bin/xcrun "$@" ;; esac
if [ "$1" = xcodebuild ] || { [ "$1" = simctl ] && [ "$2" = boot ]; }; then
  CITRUS_NO_PIN=1 exec "$HOME/.local/lib/citrus/lane/citrus" lane run mac-xcode --label "xcrun $*" -- /usr/bin/xcrun "$@"
fi
exec /usr/bin/xcrun "$@"
"#;

const CITRUS_LANE: &str = r#"#!/bin/sh
# citrus-lane <lane> [--priority owner-build|release|integrate|agent-check] -- <command>
# citrus-lane status
if [ "$1" = status ]; then shift; CITRUS_NO_PIN=1 exec "$HOME/.local/lib/citrus/lane/citrus" lane status "$@"; fi
CITRUS_NO_PIN=1 exec "$HOME/.local/lib/citrus/lane/citrus" lane run "$@"
"#;

/// Put this build where the shims call it and write the shims into `bin`.
pub fn install(bin: &Path) -> Result<Vec<String>> {
    let own = std::env::current_exe()?;
    let dir = home().join(".local/lib/citrus/lane");
    std::fs::create_dir_all(&dir)?;
    let target = dir.join("citrus");
    let temporary = dir.join(format!("citrus.{}", std::process::id()));
    std::fs::copy(&own, &temporary)?;
    std::fs::rename(&temporary, &target)?;
    std::fs::create_dir_all(bin)?;
    let mut written = vec![target.to_string_lossy().into_owned()];
    for (name, text) in [
        ("xcodebuild", SHIM_XCODEBUILD),
        ("xcrun", SHIM_XCRUN),
        ("citrus-lane", CITRUS_LANE),
    ] {
        use std::os::unix::fs::PermissionsExt;
        let path = bin.join(name);
        let temporary = bin.join(format!(".{name}.{}", std::process::id()));
        std::fs::write(&temporary, text)?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&temporary, &path)?;
        written.push(path.to_string_lossy().into_owned());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test's lane files in one temporary directory, never the user's.
    fn isolate() -> &'static Path {
        static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let dir = DIR.get_or_init(|| tempfile::tempdir().unwrap());
        // SAFETY: every test sets the same value before reading it.
        unsafe { std::env::set_var("CITRUS_LANES_DIR", dir.path()) };
        dir.path()
    }

    fn entry(id: &str, priority: i32, enqueued: f64, started: bool) -> Entry {
        Entry {
            id: id.into(),
            priority,
            label: id.into(),
            host: host(),
            pid: i64::from(std::process::id()),
            child: 0,
            cwd: String::new(),
            enqueued,
            started: started.then_some(enqueued),
            beat: now(),
            note: String::new(),
        }
    }

    #[test]
    fn a_higher_priority_goes_first_and_equal_ones_by_arrival() {
        let entries = vec![
            entry("check-a", 0, 1.0, false),
            entry("owner", 30, 3.0, false),
            entry("check-b", 0, 2.0, false),
            entry("release", 20, 4.0, false),
        ];
        let order: Vec<&str> = waiting_order(&entries)
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(order, ["owner", "release", "check-a", "check-b"]);
    }

    #[test]
    fn a_running_entry_is_not_overtaken_and_capacity_holds() {
        let dir = tempfile::tempdir().unwrap();
        let lock = File::create(dir.path().join("x.lock")).unwrap();
        let queue = Queue {
            lane: "x".into(),
            ttl: 0,
            path: dir.path().join("x.json"),
            _lock: lock,
            entries: vec![entry("check", 0, 1.0, true), entry("owner", 30, 2.0, false)],
        };
        assert!(
            !queue.may_start("owner", 1),
            "a running check is never interrupted"
        );
        assert!(queue.may_start("owner", 2));
    }

    #[test]
    fn priorities_parse_and_name() {
        assert_eq!(priority_value("owner-build").unwrap(), 30);
        assert!(priority_value("urgent").is_err());
        assert_eq!(priority_name(20), "release");
        assert_eq!(priority_name(10), "integrate");
        assert_eq!(priority_name(0), "agent-check");
        assert_eq!(priority_name(-5), "agent-check");
    }

    #[test]
    fn heavy_commands_are_found_and_wrapped_ones_pass() {
        let none = |_: &str| false;
        let all = |_: &str| true;
        let heavy = |command: &str| heavy_with(command, &none);
        assert_eq!(
            heavy("cd x && xcodebuild build -scheme T").as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(
            heavy("/usr/bin/xcodebuild test-without-building").as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(
            heavy("xcrun simctl boot 655C").as_deref(),
            Some("xcrun simctl")
        );
        assert_eq!(
            heavy("telenok/scripts/ship.sh --release").as_deref(),
            Some("ship.sh")
        );
        assert_eq!(
            heavy("bash -x scripts/perf-sim.sh").as_deref(),
            Some("perf-sim.sh")
        );
        assert_eq!(
            heavy("timeout 590 /usr/bin/xcodebuild test").as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(
            heavy("/usr/bin/lockf -t 3600 /tmp/l xcodebuild build").as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(heavy("xcodebuild -version"), None);
        assert_eq!(heavy("xcrun simctl list devices"), None);
        assert_eq!(heavy("citrus-lane mac-xcode -- 'xcodebuild build'"), None);
        assert_eq!(heavy("grep -rn xcodebuild scripts/"), None);
        assert_eq!(heavy("cat telenok/scripts/ship.sh | head"), None);
        assert_eq!(heavy("git log --oneline"), None);
        assert_eq!(
            heavy("echo '{\"command\":\"PATH=/usr/bin xcodebuild test\"}' | jq ."),
            None
        );
        assert_eq!(
            heavy("git commit -m 'run xcodebuild test via ship.sh'"),
            None
        );
        assert_eq!(
            heavy("python3 - <<'EOF'\nrun('sh -c \"/usr/bin/xcodebuild build\"')\nEOF\nls"),
            None
        );
        assert_eq!(
            heavy("sh -c 'cd t && /usr/bin/xcodebuild build'").as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(
            heavy("bash -c \"scripts/ship.sh --release\"").as_deref(),
            Some("ship.sh")
        );
        // A bare call goes through the shim, which takes the lane.
        assert_eq!(heavy_with("timeout 590 xcodebuild test", &all), None);
        assert_eq!(heavy_with("xcrun simctl boot X", &all), None);
        assert_eq!(
            heavy_with("/usr/bin/xcodebuild build", &all).as_deref(),
            Some("xcodebuild")
        );
        assert_eq!(
            heavy_with("PATH=/usr/bin:/bin xcodebuild build", &all).as_deref(),
            Some("xcodebuild")
        );
    }

    #[test]
    fn the_hook_denies_with_the_lane_command() {
        isolate();
        let (code, output) = hook(
            r#"{"tool_name":"Bash","tool_input":{"command":"/usr/bin/xcodebuild build -scheme T"}}"#,
        );
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(
            value["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .contains("citrus-lane mac-xcode -- '/usr/bin/xcodebuild build -scheme T'")
        );
        let (_, output) = hook(r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#);
        assert!(output.is_empty());
        assert!(!violations(0.0).is_empty());
    }

    #[test]
    fn a_dead_process_leaves_the_queue_and_is_recorded() {
        isolate();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.json");
        let mut dead = entry("dead", 0, 1.0, true);
        dead.pid = 999_999;
        let fresh = entry("alive", 0, 2.0, false);
        let mut stale = entry("stale", 0, 3.0, false);
        stale.beat = now() - STALE - 1.0;
        let lock = File::create(dir.path().join("x.lock")).unwrap();
        let mut queue = Queue {
            lane: "x".into(),
            ttl: 0,
            path,
            _lock: lock,
            entries: vec![dead, fresh, stale],
        };
        queue.prune();
        let left: Vec<&str> = queue
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(left, ["alive"]);
        let lost = history(0.0)
            .into_iter()
            .filter(|item| item.lane == "x" && item.outcome == "lost")
            .count();
        assert_eq!(lost, 2);
    }

    #[test]
    fn a_lane_runs_the_command_and_records_its_times() {
        isolate();
        let code = run(&Request {
            lane: "test-run",
            priority: 0,
            label: Some("true"),
            command: &["true".to_owned()],
            quiet: true,
        })
        .unwrap();
        assert_eq!(code, 0);
        let code = run(&Request {
            lane: "test-run",
            priority: 30,
            label: Some("exit 3"),
            command: &["exit 3".to_owned()],
            quiet: true,
        })
        .unwrap();
        assert_eq!(code, 3);
        assert!(
            entries("test-run").unwrap().is_empty(),
            "nothing stays in the lane"
        );
        let times = times(0.0);
        assert!(
            times
                .iter()
                .any(|row| row.lane == "test-run" && row.priority == "owner-build")
        );
        assert!(
            times
                .iter()
                .any(|row| row.lane == "test-run" && row.priority == "agent-check")
        );
    }
}
