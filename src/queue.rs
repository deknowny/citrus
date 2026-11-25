//! `citrus queue`: one queue lands every branch on the base.
//!
//! An owner queues a committed branch (`citrus queue add`) and is done: the
//! queue merges the waiting branches together onto the tip of the base in its
//! own checkout, checks the batch once (proven checks are reused as anywhere)
//! and fast-forwards the base. A red batch is halved until the branch that
//! breaks it is found; the others land. A branch marked for release
//! (`--release <unit>`) starts that unit's release from the merged commit,
//! one release per unit at a time, with the release's own gates and verify.
//!
//! The state lives in the store, one row per branch commit, and moves only
//! through `Store::queue_move`: asking again after a lost terminal returns the
//! same row, and a processor that stopped is replaced by the next caller.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

use crate::exec::Context;
use crate::manifest::now;
use crate::state::QueueEntry;

/// Branches merged into one batch at most.
const BATCH: usize = 8;
/// A base that keeps moving under a green batch: integrate this often, then fail it.
const PUSH_ATTEMPTS: usize = 6;
/// Release levels, lowest first.
const BUMPS: [&str; 3] = ["patch", "minor", "major"];
/// Pool priority of a batch's checks (`citrus integrate`'s): a prebuild waits while any wait.
const INTEGRATE_PRIORITY: i32 = 10;
/// Seconds a prebuild waits for its batch's run to show up before it starts anyway.
const PREBUILD_PATIENCE: i64 = 120;
/// A prebuild running longer than this is stopped: a release no longer waits for it.
const PREBUILD_LIMIT: Duration = Duration::from_secs(30 * 60);

/// Where the queue keeps its checkouts: CITRUS_QUEUE_DIR, else beside the
/// main checkout (`<checkout>.citrus-queue`).
pub fn dir(context: &Context) -> PathBuf {
    if let Some(dir) = std::env::var_os("CITRUS_QUEUE_DIR").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir);
    }
    let common = &context.repo.common;
    let main = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent().unwrap_or(common).to_path_buf()
    } else {
        common.clone()
    };
    let name = main
        .file_name()
        .map_or("repo".into(), |name| name.to_string_lossy().into_owned());
    main.with_file_name(format!("{name}.citrus-queue"))
}

fn lock_path(context: &Context) -> PathBuf {
    context.repo.state_dir().join("queue.lock")
}

/// The processor's lock, if no other process holds it.
fn try_lock(path: &Path) -> Result<Option<fs::File>> {
    use std::os::fd::AsRawFd;
    if let Some(parent) = path.parent() {
        crate::repo::private_dir(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock on a descriptor this function owns.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
    }
    Ok(Some(file))
}

/// Whether a processor works the queue now.
pub fn processing(context: &Context) -> bool {
    matches!(try_lock(&lock_path(context)), Ok(None))
}

/// Start a processor unless one runs; it leaves when nothing is left to do.
pub fn ensure_processor(context: &Context) -> Result<()> {
    if processing(context) {
        return Ok(());
    }
    let log = dir(context).join("queue.log");
    fs::create_dir_all(dir(context))?;
    let output = OpenOptions::new().create(true).append(true).open(&log)?;
    context
        .spawn_detached(&["queue", "work"], output)
        .context("start the queue processor")?;
    Ok(())
}

/// `unit[:bump]` items, checked against the declared release units.
pub fn parse_releases(context: &mut Context, items: &[String]) -> Result<String> {
    if items.is_empty() {
        return Ok(String::new());
    }
    let units = crate::release::Releases::load(context)?;
    let mut parsed = Vec::new();
    for item in items.iter().flat_map(|item| item.split(',')) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        let (unit, bump) = item.split_once(':').unwrap_or((item, "patch"));
        let unit = crate::lang::compile::dash(unit);
        if !units.releases.contains_key(&unit) {
            bail!(
                "no release unit {unit}; units: {}",
                units
                    .releases
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !BUMPS.contains(&bump) {
            bail!("a release raises patch, minor or major, not {bump}");
        }
        parsed.push(format!("{unit}:{bump}"));
    }
    Ok(parsed.join(","))
}

/// Queue the current commit for `base`. The tree must be committed: the
/// queue lands commits, not working trees.
pub fn add(
    context: &Context,
    base: &str,
    releases: &str,
    retry: bool,
) -> Result<(QueueEntry, bool)> {
    let repo = &context.repo;
    if !repo
        .git(&["status", "--porcelain", "--untracked-files=no"])?
        .is_empty()
    {
        bail!("commit the changes first: the queue lands commits");
    }
    // A local base name lands on that branch of origin.
    let base = match crate::integrate::split_base(context, base) {
        (Some(_), _) => base.to_owned(),
        (None, branch) if repo.git(&["remote"])?.lines().any(|name| name == "origin") => {
            format!("origin/{branch}")
        }
        _ => bail!("the queue lands on a remote base such as origin/main, not {base}"),
    };
    let base = base.as_str();
    let commit = repo.git(&["rev-parse", "HEAD"])?;
    let entry = QueueEntry {
        id: format!(
            "q-{}-{:04x}",
            crate::report::compact_utc(now()),
            crate::exec::random16()
        ),
        worktree: context.worktree(),
        branch: repo.branch(),
        agent: crate::exec::agent(),
        commit,
        base: base.to_owned(),
        releases: releases.to_owned(),
        state: "queued".into(),
        note: String::new(),
        batch: String::new(),
        run: String::new(),
        merged: String::new(),
        started_releases: String::new(),
        added: now() as i64,
        updated: now() as i64,
        ended: None,
    };
    // An older commit of this worktree still waiting is replaced by this one.
    for waiting in context.store.queue_in(&["queued"])? {
        if waiting.worktree == entry.worktree
            && waiting.base == entry.base
            && waiting.commit != entry.commit
        {
            context.store.queue_move(
                &waiting.id,
                &["queued"],
                "cancelled",
                &format!(
                    "replaced by {}",
                    &entry.commit[..12.min(entry.commit.len())]
                ),
            )?;
        }
    }
    let (stored, inserted) = context.store.queue_add(&entry, retry)?;
    if !stored.settled() {
        ensure_processor(context)?;
    }
    Ok((stored, inserted))
}

/// Wait until `id` lands or fails (its releases are followed when `releases`).
pub fn wait(context: &Context, id: &str, releases: bool) -> Result<QueueEntry> {
    let mut last_note = String::new();
    loop {
        let entry = context
            .store
            .queue_entry(id)?
            .with_context(|| format!("no queue entry {id}"))?;
        let done = if releases {
            entry.settled()
        } else {
            entry.settled() || entry.landed()
        };
        if done {
            return Ok(entry);
        }
        let line = format!("{} {}", entry.state, entry.note);
        if line != last_note {
            eprintln!("citrus: {id} {line}");
            last_note = line;
        }
        if matches!(entry.state.as_str(), "queued" | "checking" | "releasing") {
            ensure_processor(context)?;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

// ------------------------------------------------------------- processor

fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = crate::repo::git().arg("-C").arg(dir).args(args).output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn log(line: &str) {
    println!("{} {line}", crate::report::compact_utc(now()));
}

/// A checkout of the queue named `name`, detached at `commit` with nothing
/// else in it (ignored files such as build caches stay).
fn tree_at(context: &Context, name: &str, commit: &str) -> Result<PathBuf> {
    let path = dir(context).join(name);
    if !path.join(".git").exists() {
        fs::create_dir_all(dir(context))?;
        let _ = git_in(&context.repo.root, &["worktree", "prune"]);
        git_in(
            &context.repo.root,
            &[
                "worktree",
                "add",
                "--force",
                "--detach",
                &path.display().to_string(),
                commit,
            ],
        )?;
    }
    let _ = git_in(&path, &["merge", "--abort"]);
    git_in(
        &path,
        &["checkout", "--quiet", "--force", "--detach", commit],
    )?;
    git_in(&path, &["reset", "--quiet", "--hard", commit])?;
    git_in(&path, &["clean", "-q", "-fd"])?;
    Ok(path)
}

/// This binary in `dir`, with the Citrus that checkout pins.
fn citrus_in(dir: &Path) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .current_dir(dir)
        .env_remove("CITRUS_BIN")
        .stdin(Stdio::null());
    Ok(command)
}

/// The processor: lands batches while entries wait, follows releases, leaves
/// when nothing is left. One at a time per clone (a lock in the state dir).
pub fn work(context: &Context) -> Result<()> {
    let Some(_lock) = try_lock(&lock_path(context))? else {
        return Ok(());
    };
    log("queue processor started");
    // Let more branches join the first batch (tests, a burst of agents).
    if let Some(seconds) = std::env::var("CITRUS_QUEUE_GATHER_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_secs(seconds));
    }
    // A processor that stopped mid-batch left its entries checking.
    for entry in context.store.queue_in(&["checking"])? {
        context.store.queue_move(
            &entry.id,
            &["checking"],
            "queued",
            "the queue processor restarted; checking again",
        )?;
    }
    loop {
        let releasing = advance_releases(context)?;
        let queued = context.store.queue_in(&["queued"])?;
        let Some(first) = queued.first() else {
            if releasing {
                std::thread::sleep(Duration::from_secs(15));
                continue;
            }
            break;
        };
        let base = first.base.clone();
        let batch: Vec<QueueEntry> = queued
            .into_iter()
            .filter(|entry| entry.base == base)
            .take(BATCH)
            .collect();
        if let Err(error) = land(context, &base, batch.clone()) {
            log(&format!("batch failed outside its checks: {error:#}"));
            for entry in &batch {
                context.store.queue_move(
                    &entry.id,
                    &["queued", "checking"],
                    "failed",
                    &format!("the queue could not land it: {error:#}"),
                )?;
            }
        }
    }
    prebuild_cancel(None);
    log("queue processor done");
    drop(_lock);
    // An entry queued while this processor was leaving gets the next one.
    if !context.store.queue_in(&["queued"])?.is_empty() {
        ensure_processor(context)?;
    }
    Ok(())
}

struct Outcome {
    state: String,
    run: String,
    error: String,
}

/// Merge `entries` onto the tip of `base`, check, push; halve a red batch.
fn land(context: &Context, base: &str, mut entries: Vec<QueueEntry>) -> Result<()> {
    let store = &context.store;
    let (remote, branch) = crate::integrate::split_base(context, base);
    let remote = remote.context("the queue lands on a remote base")?;
    let mut retried = false;
    for attempt in 1..=PUSH_ATTEMPTS {
        git_in(&context.repo.root, &["fetch", "--quiet", &remote, &branch])?;
        let tip = git_in(&context.repo.root, &["rev-parse", base])?;
        let tree = tree_at(context, "queue", &tip)?;
        let mut merged = Vec::new();
        for entry in entries {
            let Some(entry) = store.queue_entry(&entry.id)? else {
                continue;
            };
            if !matches!(entry.state.as_str(), "queued" | "checking") {
                continue;
            }
            if git_in(&tree, &["merge-base", "--is-ancestor", &entry.commit, &tip]).is_ok() {
                finish_landed(context, &entry, &tip, "already on the base")?;
                continue;
            }
            let message = format!(
                "Merge {} into {branch} (citrus queue {})",
                entry.branch, entry.id
            );
            let merge = crate::repo::git()
                .arg("-C")
                .arg(&tree)
                .args([
                    "merge",
                    "--no-edit",
                    "--quiet",
                    "-m",
                    &message,
                    &entry.commit,
                ])
                .output()?;
            if !merge.status.success() {
                let files =
                    git_in(&tree, &["diff", "--name-only", "--diff-filter=U"]).unwrap_or_default();
                let _ = git_in(&tree, &["merge", "--abort"]);
                let ahead = if merged.is_empty() {
                    String::new()
                } else {
                    " (or a branch ahead of it in the queue)".to_owned()
                };
                store.queue_move(
                    &entry.id,
                    &["queued", "checking"],
                    "conflict",
                    &format!(
                        "conflicts with {base}{ahead}: {}; run citrus integrate, resolve, queue again",
                        files.lines().collect::<Vec<_>>().join(", ")
                    ),
                )?;
                log(&format!("{} conflicts", entry.id));
                continue;
            }
            merged.push(entry);
        }
        if merged.is_empty() {
            return Ok(());
        }
        let batch = format!(
            "b-{}-{:04x}",
            crate::report::compact_utc(now()),
            crate::exec::random16()
        );
        let names: Vec<&str> = merged.iter().map(|entry| entry.branch.as_str()).collect();
        log(&format!(
            "{batch}: {} on {} ({})",
            merged.len(),
            &tip[..12],
            names.join(", ")
        ));
        for entry in &merged {
            store.queue_move(
                &entry.id,
                &["queued", "checking"],
                "checking",
                &format!(
                    "checking with {} on {}",
                    if merged.len() > 1 {
                        format!("{} branches", merged.len())
                    } else {
                        "nothing else".into()
                    },
                    &tip[..12]
                ),
            )?;
            store.queue_set(&entry.id, "batch", &batch)?;
        }
        // The release units the batch asks for: their images build while it is checked.
        let mut units: Vec<String> = Vec::new();
        for (unit, _) in merged.iter().flat_map(QueueEntry::release_units) {
            if !units.contains(&unit) {
                units.push(unit);
            }
        }
        let head = git_in(&tree, &["rev-parse", "HEAD"])?;
        let outcome = check(context, &tree, &tip, &head, &units)?;
        for entry in &merged {
            store.queue_set(&entry.id, "run", &outcome.run)?;
        }
        log(&format!("{batch}: run {} {}", outcome.run, outcome.state));
        if outcome.state == "passed" {
            let pushed = push(&tree, &remote, &branch)?;
            if pushed.status.success() {
                log(&format!("{batch}: pushed {} to {base}", &head[..12]));
                for entry in &merged {
                    finish_landed(context, entry, &head, &format!("merged as {}", &head[..12]))?;
                }
                // Its prebuild goes on: the release of `head` waits for it.
                return Ok(());
            }
            prebuild_cancel(Some(&head));
            let error = String::from_utf8_lossy(&pushed.stderr).into_owned();
            if !crate::integrate::base_moved(&error) {
                bail!("git push failed: {}", error.trim());
            }
            log(&format!(
                "{batch}: {base} moved (attempt {attempt}); merging again"
            ));
            entries = merged;
            continue;
        }
        // Not landed: what its prebuild builds is never used (a later batch of the same
        // inputs finds it by key), so the builder is free for the next one.
        prebuild_cancel(Some(&head));
        if outcome.state != "failed" && !retried {
            // The run itself broke (lost worker, pool error): once more.
            retried = true;
            entries = merged;
            continue;
        }
        if merged.len() == 1 {
            let entry = &merged[0];
            store.queue_move(
                &entry.id,
                &["checking"],
                "failed",
                &format!(
                    "run {} {} on {}: {}",
                    outcome.run,
                    outcome.state,
                    &tip[..12],
                    outcome.error
                ),
            )?;
            return Ok(());
        }
        // Halve: the first half is checked now, the second waits its turn.
        let half = merged.len() / 2;
        let rest = merged.split_off(half);
        for entry in &rest {
            store.queue_move(
                &entry.id,
                &["checking"],
                "queued",
                &format!("batch {batch} failed; checked again in a smaller batch"),
            )?;
        }
        log(&format!(
            "{batch}: failed; checking {} and {} apart",
            merged.len(),
            rest.len()
        ));
        return land(context, base, merged);
    }
    for entry in &entries {
        store.queue_move(
            &entry.id,
            &["queued", "checking"],
            "queued",
            &format!("{base} kept moving under the batch; trying again"),
        )?;
    }
    Ok(())
}

/// Landed: merged, or releasing when it asks for releases.
fn finish_landed(context: &Context, entry: &QueueEntry, commit: &str, note: &str) -> Result<()> {
    let store = &context.store;
    store.queue_set(&entry.id, "merged", commit)?;
    let state = if entry.releases.is_empty() {
        "merged"
    } else {
        "releasing"
    };
    store.queue_move(&entry.id, &["queued", "checking"], state, note)?;
    Ok(())
}

/// Check the merged batch in `tree` against `tip`, reusing what is proven; the images of
/// `units` at `head` build beside the checks (`prebuild_poll`).
fn check(
    context: &Context,
    tree: &Path,
    tip: &str,
    head: &str,
    units: &[String],
) -> Result<Outcome> {
    let mut command = citrus_in(tree)?;
    command.args(["run", "--base", tip, "--json"]);
    // A batch's checks go before agents' checks in the pool, as an integration's do: at
    // agent-check priority a batch waited behind every agent run (2026-11-24: 10 min before its
    // first check, 36 min in all, while 24 agent checks were queued).
    if std::env::var_os("CITRUS_RUN_PRIORITY").is_none() {
        command.env("CITRUS_RUN_PRIORITY", "10");
    }
    // The queue's checkout has no dependencies installed: checks go to the pool.
    if crate::pool::has_live_agent() {
        command.arg("--remote");
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start the batch's checks")?;
    let stderr = child.stderr.take().context("stderr")?;
    let errors = std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            println!("  {line}");
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    let stdout = child.stdout.take().context("stdout")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            text.push_str(&line);
            text.push('\n');
        }
        text
    });
    // Releases move on while the batch is checked, and its images build.
    let mut prebuild_started = units.is_empty() || !prebuild_enabled();
    let checking_since = now() as i64;
    loop {
        if !prebuild_started {
            match prebuild_poll(context, tree, checking_since, head, units) {
                Ok(started) => prebuild_started = started,
                Err(error) => {
                    log(&format!("prebuild of {}: {error:#}", units.join(", ")));
                    prebuild_started = true;
                }
            }
        }
        if child.try_wait()?.is_some() {
            break;
        }
        if let Err(error) = advance_releases(context) {
            log(&format!("releases: {error:#}"));
        }
        std::thread::sleep(Duration::from_secs(if prebuild_started { 10 } else { 3 }));
    }
    let code = child.wait()?.code().unwrap_or(-1);
    let text = reader.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let run = value["run"]["id"].as_str().unwrap_or_default().to_owned();
    let state = match (value["run"]["state"].as_str(), code) {
        (Some(state), _) => state.to_owned(),
        (None, _) => "error".to_owned(),
    };
    let error = value["targets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|target| target["result"] == "failed")
        .map(|target| {
            format!(
                "{} failed{}",
                target["target"].as_str().unwrap_or("?"),
                target["first_error"]
                    .as_str()
                    .and_then(|error| error.lines().next())
                    .map(|line| format!(": {line}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let error = if !error.is_empty() {
        error
    } else if let Some(message) = value["error"].as_str() {
        message.to_owned()
    } else {
        stderr.lines().last().unwrap_or("no result").to_owned()
    };
    Ok(Outcome { state, run, error })
}

/// `git push HEAD:<branch>` without force; remote failures are retried.
fn push(tree: &Path, remote: &str, branch: &str) -> Result<std::process::Output> {
    let mut delay = 2;
    loop {
        let output = crate::repo::git()
            .arg("-C")
            .arg(tree)
            .args([
                "push",
                "--quiet",
                "--no-force",
                "--no-follow-tags",
                "--recurse-submodules=no",
                remote,
                &format!("HEAD:{branch}"),
            ])
            .output()?;
        let error = String::from_utf8_lossy(&output.stderr);
        if output.status.success() || !crate::integrate::transient_push_error(&error) || delay > 16
        {
            return Ok(output);
        }
        std::thread::sleep(Duration::from_secs(delay));
        delay *= 2;
    }
}

// --------------------------------------------------------------- releases

/// Move releasing entries on: settle the ones whose releases ended, start a
/// unit's release from the newest merged commit that asks for it once no
/// release of that unit runs. True while any entry is still releasing.
fn advance_releases(context: &Context) -> Result<bool> {
    let store = &context.store;
    let mut pending = false;
    let releasing = store.queue_in(&["releasing"])?;
    // Units by the entries waiting for them, newest merge last.
    let mut wanted: Vec<(String, Vec<QueueEntry>)> = Vec::new();
    for entry in &releasing {
        let started = entry.started();
        let mut done = true;
        let mut failed = Vec::new();
        let mut passed = Vec::new();
        for (unit, _) in entry.release_units() {
            match started.iter().find(|(name, _)| *name == unit) {
                Some((_, id)) if id.starts_with("error:") => failed.push(format!("{unit} {id}")),
                Some((_, id)) => match store.release(id)? {
                    Some(release) => {
                        let release = crate::release::reconcile(context, release)?;
                        if !release.finished() {
                            done = false;
                        } else if release.state == "passed" {
                            passed.push(format!("{unit} {} {id}", release.version));
                        } else {
                            failed.push(format!("{unit} {id} {}", release.state));
                        }
                    }
                    None => failed.push(format!("{unit} {id} missing")),
                },
                None => {
                    done = false;
                    match wanted.iter_mut().find(|(name, _)| *name == unit) {
                        Some((_, list)) => list.push(entry.clone()),
                        None => wanted.push((unit, vec![entry.clone()])),
                    }
                }
            }
        }
        if done {
            let (state, note) = if failed.is_empty() {
                ("released", format!("released {}", passed.join(", ")))
            } else {
                (
                    "release_failed",
                    format!(
                        "merged as {}; release failed: {}",
                        &entry.merged[..12.min(entry.merged.len())],
                        failed.join(", ")
                    ),
                )
            };
            store.queue_move(&entry.id, &["releasing"], state, &note)?;
        } else {
            pending = true;
        }
    }
    for (unit, entries) in wanted {
        let newest_merged = entries
            .iter()
            .max_by_key(|entry| (entry.ended.unwrap_or(0), entry.updated))
            .map(|entry| entry.merged.clone())
            .unwrap_or_default();
        // The images this release rolls are being built for it: the release waits rather than
        // build them a second time.
        if prebuilding(&newest_merged, &unit) {
            for entry in &entries {
                let note = format!("waiting for the images of {unit}, built while it was checked");
                if entry.note != note {
                    store.queue_move(&entry.id, &["releasing"], "releasing", &note)?;
                }
            }
            continue;
        }
        if let Some(last) = store.releases_of(&unit, 1)?.into_iter().next() {
            let last = crate::release::reconcile(context, last)?;
            if !last.finished() {
                for entry in &entries {
                    let note = format!("waiting for release {} of {unit} to end", last.id);
                    if entry.note != note {
                        store.queue_move(&entry.id, &["releasing"], "releasing", &note)?;
                    }
                }
                continue;
            }
        }
        // The newest merge carries the older ones.
        let newest = entries
            .iter()
            .max_by_key(|entry| (entry.ended.unwrap_or(0), entry.updated))
            .context("no entry")?;
        let bump = entries
            .iter()
            .flat_map(|entry| entry.release_units())
            .filter(|(name, _)| *name == unit)
            .map(|(_, bump)| BUMPS.iter().position(|level| *level == bump).unwrap_or(0))
            .max()
            .unwrap_or(0);
        let id = match start_release(context, &unit, &newest.merged, BUMPS[bump]) {
            Ok(id) => {
                log(&format!(
                    "release {id} of {unit} from {}",
                    &newest.merged[..12.min(newest.merged.len())]
                ));
                id
            }
            Err(error) => {
                log(&format!("release of {unit} did not start: {error:#}"));
                format!("error:{}", error.to_string().replace([',', '='], ";"))
            }
        };
        for entry in &entries {
            let mut started = entry.started_releases.clone();
            if !started.is_empty() {
                started.push(',');
            }
            started.push_str(&format!("{unit}={id}"));
            store.queue_set(&entry.id, "started_releases", &started)?;
            store.queue_move(
                &entry.id,
                &["releasing"],
                "releasing",
                &format!("release of {unit}: {id}"),
            )?;
        }
    }
    Ok(pending)
}

/// `citrus release start <unit> --approve --detach` from a checkout of the
/// queue at `commit`, prepared as any new checkout of the project.
fn start_release(context: &Context, unit: &str, commit: &str, bump: &str) -> Result<String> {
    let tree = prepared_tree(context, &format!("release-{unit}"), commit)?;
    let output = citrus_in(&tree)?
        .args([
            "release",
            "start",
            unit,
            "--approve",
            "--detach",
            "--bump",
            bump,
            "--json",
        ])
        .stderr(Stdio::inherit())
        .output()?;
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    match value["release"]["id"].as_str() {
        Some(id) => Ok(id.to_owned()),
        None => bail!(
            "{}",
            value["error"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        ),
    }
}

/// A checkout of the queue at `commit` (`tree_at`), prepared once as any new checkout of the
/// project (`citrus do setup-worktree`, when the project has one).
fn prepared_tree(context: &Context, name: &str, commit: &str) -> Result<PathBuf> {
    let tree = tree_at(context, name, commit)?;
    let ready = dir(context).join(format!("{name}.ready"));
    let setup = !ready.exists()
        && context.project.as_ref().is_some_and(|project| {
            project
                .tasks
                .iter()
                .any(|task| matches!(task.name.as_str(), "setup-worktree" | "setup_worktree"))
        });
    if setup {
        let status = citrus_in(&tree)?
            .args(["do", "setup-worktree", "--text"])
            .status()?;
        if !status.success() {
            bail!("citrus do setup-worktree failed in {}", tree.display());
        }
        fs::write(&ready, commit)?;
    }
    Ok(tree)
}

// -------------------------------------------------------------- prebuild

/// The prebuild running now: `citrus release prebuild <units>` in the queue's `prebuild`
/// checkout of a batch's merge commit. One at a time: the images of a batch that landed finish
/// before the next batch's start, so a release never waits behind a newer batch's build.
struct Prebuild {
    commit: String,
    units: Vec<String>,
    child: Child,
    started: std::time::Instant,
}

static PREBUILD: Mutex<Option<Prebuild>> = Mutex::new(None);

/// Building beside the checks is off unless CITRUS_QUEUE_PREBUILD=1. A prebuild the queue
/// stops mid-build (its batch failed) leaves the builder's shared cache mounts half-written
/// with the dropped branch's sources, and the next build of another commit reused them: garvis
/// 0.249.28 (2026-11-25) rolled a backend image of d1f3bb8 holding code of a branch that had
/// left its batch. `citrus release prebuild` checks this too, so a processor started by an
/// older Citrus builds nothing either once a batch's pin carries this one.
pub fn prebuild_enabled() -> bool {
    std::env::var("CITRUS_QUEUE_PREBUILD").is_ok_and(|value| value == "1")
}

/// The running prebuild, after forgetting one that ended.
fn prebuild_reap(slot: &mut Option<Prebuild>) {
    let ended = match slot.as_mut() {
        Some(prebuild) => match prebuild.child.try_wait() {
            Ok(Some(status)) => Some(status.success()),
            Ok(None) => None,
            Err(_) => Some(false),
        },
        None => None,
    };
    if let Some(ok) = ended
        && let Some(prebuild) = slot.take()
    {
        log(&format!(
            "prebuild of {} at {} {}",
            prebuild.units.join(", "),
            &prebuild.commit[..12.min(prebuild.commit.len())],
            if ok {
                "done"
            } else {
                "failed; the release builds what is missing"
            }
        ));
    }
}

/// Start the prebuild of `units` at `head` (the batch checked in `tree` since `since`) when the
/// builder is free, the batch's run has queued its checks and no check of integrate priority
/// waits for a pool slot: the builds share the machine the pool checks on, and a batch's checks
/// take their slots first. True once started.
fn prebuild_poll(
    context: &Context,
    tree: &Path,
    since: i64,
    head: &str,
    units: &[String],
) -> Result<bool> {
    let mut slot = PREBUILD.lock().unwrap_or_else(|poison| poison.into_inner());
    prebuild_reap(&mut slot);
    if slot.is_some() {
        return Ok(false);
    }
    // A run that never shows up (it broke before recording) holds nothing back for long.
    if now() as i64 - since < PREBUILD_PATIENCE && !batch_run_queued(context, tree, since) {
        return Ok(false);
    }
    match crate::pool::waiting(INTEGRATE_PRIORITY) {
        Ok(0) => {}
        Ok(_) => return Ok(false),
        // An unreachable pool runs no checks to give way to.
        Err(_) => {}
    }
    let tree = prepared_tree(context, "prebuild", head)?;
    let mut command = citrus_in(&tree)?;
    command
        .args(["release", "prebuild"])
        .args(units)
        .arg("--text")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // Its own process group: cancelling stops the builds it started too.
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn().context("start the prebuild")?;
    log(&format!(
        "prebuild of {} at {} beside the checks (pid {})",
        units.join(", "),
        &head[..12.min(head.len())],
        child.id()
    ));
    *slot = Some(Prebuild {
        commit: head.to_owned(),
        units: units.to_vec(),
        child,
        started: std::time::Instant::now(),
    });
    Ok(true)
}

/// Whether the batch's run in `tree` (started since `since`) put its checks in the pool, or
/// ended, or runs without a pool.
fn batch_run_queued(context: &Context, tree: &Path, since: i64) -> bool {
    let worktree = fs::canonicalize(tree)
        .unwrap_or_else(|_| tree.to_path_buf())
        .display()
        .to_string();
    let Ok(runs) = context.store.recent(&worktree, 1) else {
        return false;
    };
    let Some(run) = runs.into_iter().next().filter(|run| run.started >= since) else {
        return false;
    };
    run.finished() || crate::pool::run_known(&run.id).unwrap_or(true)
}

/// Whether the images of `unit` at `commit` are being built now (a prebuild past
/// `PREBUILD_LIMIT` is stopped instead: the release builds what is missing).
fn prebuilding(commit: &str, unit: &str) -> bool {
    let overdue = {
        let mut slot = PREBUILD.lock().unwrap_or_else(|poison| poison.into_inner());
        prebuild_reap(&mut slot);
        let Some(prebuild) = slot.as_ref().filter(|prebuild| {
            prebuild.commit == commit && prebuild.units.iter().any(|name| name == unit)
        }) else {
            return false;
        };
        prebuild.started.elapsed() >= PREBUILD_LIMIT
    };
    if overdue {
        prebuild_cancel(Some(commit));
    }
    !overdue
}

/// Stop the prebuild of `commit` (any, for None): its batch did not land. What it published
/// stays under its input key and is simply not used.
fn prebuild_cancel(commit: Option<&str>) {
    let mut slot = PREBUILD.lock().unwrap_or_else(|poison| poison.into_inner());
    prebuild_reap(&mut slot);
    if slot
        .as_ref()
        .is_some_and(|prebuild| commit.is_none_or(|commit| prebuild.commit == commit))
        && let Some(mut prebuild) = slot.take()
    {
        // SAFETY: signals the process group this processor started for the prebuild.
        unsafe {
            libc::kill(-(prebuild.child.id() as i32), libc::SIGTERM);
        }
        let _ = prebuild.child.wait();
        log(&format!(
            "prebuild of {} at {} stopped",
            prebuild.units.join(", "),
            &prebuild.commit[..12.min(prebuild.commit.len())]
        ));
    }
}

/// One line about an entry for people.
pub fn describe(entry: &QueueEntry) -> String {
    let releases = if entry.releases.is_empty() {
        String::new()
    } else {
        format!(" → release {}", entry.releases)
    };
    let took = entry.ended.unwrap_or(now() as i64) - entry.added;
    format!(
        "{} {:<14} {} {}{} · {}{}",
        entry.id,
        entry.state,
        entry.branch,
        &entry.commit[..10.min(entry.commit.len())],
        releases,
        crate::report::age(took),
        if entry.note.is_empty() {
            String::new()
        } else {
            format!(" · {}", entry.note)
        }
    )
}
