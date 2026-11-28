//! Citrus: one command surface for checks, for people and AI agents alike.
//!
//! `citrus status` says what the current changes need and what is already
//! proven; `citrus run` runs only what is not; `citrus log <run>` shows the
//! first error instead of the whole log. Runs execute in a detached worker,
//! so closing the terminal does not lose them.
#![allow(clippy::print_stdout, clippy::print_stderr)] // A CLI: stdout is the interface.

mod apply;
mod config;
mod depinfo;
mod deploy;
mod disk;
mod dockerfile;
mod doctor;
mod exec;
mod governor;
mod hygiene;
mod integrate;
mod lanes;
mod lang;
mod lock;
mod manifest;
mod model;
mod observe;
mod pin;
mod plan;
mod pool;
mod ports;
mod queue;
mod release;
mod repo;
mod report;
mod resources;
mod slots;
mod state;
mod tasks;
mod versions;
mod work;

use std::io::IsTerminal;
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

use exec::{Context, Mode, Request};
use manifest::now;
use report::age;
use state::{Run, RunTarget};

const SCHEMA: &str = "citrus/v1";

#[derive(Parser, Debug)]
#[command(
    name = "citrus",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CITRUS_COMMIT"), ")"),
    about = "Plan, run and explain checks; reuse results that are already proven."
)]
struct Cli {
    /// JSON output (default when stdout is not a terminal).
    #[arg(long, global = true)]
    json: bool,
    /// Text output even when stdout is not a terminal.
    #[arg(long, global = true, conflicts_with = "json")]
    text: bool,
    /// Plan checks of this profile (citrus.ci `project { profiles = [...] }`).
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Without a command: what Citrus can do in this repository.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// What the current changes need, what is proven, what is running.
    Status {
        #[arg(long)]
        base: Option<String>,
    },
    /// The checks the current changes need, with reuse decisions.
    Plan {
        #[arg(long)]
        base: Option<String>,
        /// Plan exactly these changed paths (one per line; `-` for stdin)
        /// instead of the changes since the base.
        #[arg(long, value_name = "FILE")]
        paths_file: Option<String>,
    },
    /// Run the needed checks (or the named targets), reusing proven results.
    Run {
        targets: Vec<String>,
        #[arg(long)]
        base: Option<String>,
        /// Run on this machine.
        #[arg(long, conflicts_with = "remote")]
        local: bool,
        /// Run the planned set with the configured remote runner.
        #[arg(long)]
        remote: bool,
        /// Idempotency key: the same key returns the existing run.
        #[arg(long)]
        key: Option<String>,
        /// Return immediately; follow with `citrus wait <run>`.
        #[arg(long)]
        detach: bool,
        /// Run even if a proven result exists.
        #[arg(long)]
        force: bool,
        /// Checks run at once on this machine (default: CITRUS_JOBS, else half its CPUs).
        #[arg(long)]
        jobs: Option<usize>,
        /// Plan for exactly the changed paths in this file (one per line).
        #[arg(long)]
        paths_file: Option<std::path::PathBuf>,
        /// Place in the pool queue: owner-build, release, integrate or
        /// agent-check (default; CITRUS_RUN_PRIORITY overrides).
        #[arg(long)]
        priority: Option<String>,
    },
    /// Wait for a run (id, unique prefix or `last`) and print its result.
    Wait { run: String },
    /// Current state of a run.
    Show { run: String },
    /// First error of a failed run; `--target` for one target, `--full` for everything.
    Log {
        run: String,
        #[arg(long)]
        target: Option<String>,
        #[arg(long)]
        full: bool,
    },
    /// Why a target is needed or why its last result cannot be reused.
    Why {
        target: String,
        #[arg(long)]
        base: Option<String>,
    },
    /// Stop a run and everything it started.
    Cancel { run: String },
    /// Merge the base branch, keep checks the incoming changes do not touch,
    /// run what is needed again, and optionally push.
    Integrate {
        /// Base to merge (default: plan.base, e.g. origin/main).
        #[arg(long)]
        base: Option<String>,
        /// Land the result through the merge queue (`citrus queue add --wait`).
        #[arg(long)]
        push: bool,
        /// Only merge and carry results; do not run checks.
        #[arg(long)]
        no_run: bool,
        /// With --push: release this unit after landing (`unit[:minor|major]`).
        #[arg(long = "release", value_name = "UNIT")]
        release: Vec<String>,
    },
    /// The merge queue: committed branches land on the base in checked batches.
    Queue {
        #[command(subcommand)]
        action: Option<QueueAction>,
    },
    /// Tracked tasks (step, whose turn) and every worktree as a task: branch,
    /// unmerged commits, runs, notes.
    Tasks {
        /// Include idle and merged worktrees, and closed tracked tasks.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base: Option<String>,
        /// Only tracked tasks nobody moves: a silent claim, merged and not
        /// released, released and not verified, a failed release, no movement.
        #[arg(long)]
        stuck: bool,
        /// Only tracked tasks whose turn this is (owner, lead, lead2, agent, citrus).
        #[arg(long)]
        turn: Option<String>,
        /// --stuck: merged and not released (or released and not verified) this long.
        #[arg(long, default_value = "15m", value_name = "DURATION")]
        unreleased_after: String,
        /// --stuck: an open task without an event this long.
        #[arg(long, default_value = "24h", value_name = "DURATION")]
        idle_after: String,
    },
    /// Say what this worktree's task is doing and what blocks it (shown in
    /// status and tasks); `citrus task new|claim|report|link|verify|close|show`
    /// track a task from its card to a verified release.
    #[command(args_conflicts_with_subcommands = true)]
    Task {
        #[command(subcommand)]
        action: Option<TaskAction>,
        /// What it is doing, in a line.
        #[arg(value_name = "TITLE")]
        title: Vec<String>,
        /// What it may change: products, files, systems.
        #[arg(long)]
        scope: Option<String>,
        /// The action it cannot take yet (with --needs).
        #[arg(long)]
        blocked: Option<String>,
        /// The decision or data that action needs, and from whom.
        #[arg(long)]
        needs: Option<String>,
        /// Where the proof lives: logs, runs, links.
        #[arg(long)]
        evidence: Option<String>,
        #[arg(long)]
        clear_blocker: bool,
        /// Forget the task's description.
        #[arg(long)]
        clear: bool,
    },
    /// Record a decision between tasks: who does what, when it is reopened.
    Agree {
        /// A short lowercase slug naming it.
        key: String,
        #[arg(long)]
        terms: String,
        /// What makes it open again.
        #[arg(long)]
        reopen: String,
        /// Where the decision was made.
        #[arg(long)]
        evidence: String,
        /// The revision read before changing it (0 for a new one).
        #[arg(long, default_value_t = 0)]
        revision: i64,
    },
    /// Release versions held by committed sources: reserve one, check or list them.
    Version {
        #[command(subcommand)]
        action: VersionAction,
    },
    /// Declared checks with their inputs and last result.
    Targets {
        /// Also the current input fingerprint of each check that may be reused.
        #[arg(long)]
        fingerprints: bool,
    },
    /// Artifacts and their input keys (at HEAD, or `--at` another revision).
    Artifacts {
        #[arg(long)]
        at: Option<String>,
        /// Build and publish this artifact for HEAD (reused when one was
        /// built from the same inputs) and print its reference.
        #[arg(long)]
        build: Option<String>,
    },
    /// What an environment runs versus what HEAD would run (read-only).
    Diff { environment: String },
    /// Make an environment run what HEAD builds (build by input key, roll by digest, verify).
    Apply {
        environment: String,
        /// Allow changing a protected environment.
        #[arg(long)]
        approve: bool,
        /// Apply only if the plan still has this hash (from citrus diff).
        #[arg(long)]
        plan: Option<String>,
        /// Apply even if checks are not proven for this commit (recorded).
        #[arg(long)]
        unchecked: bool,
        #[arg(long)]
        detach: bool,
        /// Change nothing: prove the checks the shipped change needs and build
        /// every artifact for HEAD, so an apply or release of this commit only
        /// reuses them, rolls and verifies. One prepare at a time per machine.
        #[arg(long, conflicts_with_all = ["approve", "plan", "unchecked", "detach"])]
        prepare: bool,
    },
    /// Releases: declared units, running them with gates and recorded steps.
    Release {
        #[command(subcommand)]
        action: Option<ReleaseAction>,
    },
    /// Check citrus.ci before anything runs: syntax, names, fields, globs, portability.
    Check,
    /// Run the configuration's `#[test]` functions (plans of example changes).
    Test { filter: Option<String> },
    /// Lay out the .ci files: indentation by brackets, no trailing spaces.
    Fmt {
        /// Change nothing; exit 1 and name the files that need formatting.
        #[arg(long)]
        check: bool,
    },
    /// Compare what the compiler read (Cargo dep-info) with the inputs Citrus
    /// inferred for checks it understood as Cargo; run it after a build.
    Deps {
        /// Cargo's target directory (default: CARGO_TARGET_DIR, else target).
        #[arg(long)]
        target_dir: Option<std::path::PathBuf>,
    },
    /// Run a task declared in citrus.ci (no name: list the tasks).
    Do { task: Option<String> },
    /// Check that this repository is set up so Citrus can be trusted.
    Doctor,
    /// Free disk space and the Cargo target of every worktree; clean build caches.
    Disk {
        /// Drop incremental data idle for hours and artifacts idle for days, in every worktree.
        #[arg(long)]
        trim: bool,
        /// Also remove the whole target of other worktrees idle for this many days.
        #[arg(long, value_name = "DAYS")]
        remove_idle: Option<u64>,
    },
    /// How Citrus has been used: runs, reuse, time saved.
    Stats {
        #[arg(long, default_value_t = 7)]
        days: u64,
    },
    /// Join the pool: take checks from its queue until stopped
    /// (CITRUS_POOL or ~/.config/citrus/pool; docs/design/runners.md).
    Agent {
        /// CPUs this machine gives the pool (default: all but one).
        #[arg(long)]
        share: Option<usize>,
        /// Checks taken at once (default: a quarter of the share).
        #[arg(long)]
        slots: Option<usize>,
        /// Extra labels checks may require (`#[meta(requires = [...])]`).
        #[arg(long, value_delimiter = ',')]
        labels: Vec<String>,
        /// Name in the pool (default: the host name).
        #[arg(long)]
        name: Option<String>,
        /// Run checks of runs with an image natively (this machine has their tools).
        #[arg(long)]
        native: bool,
        /// Leave the pool after this many idle seconds.
        #[arg(long, value_name = "SECONDS")]
        idle_exit: Option<u64>,
        /// Take no new checks while another process holds this lock file
        /// (repeatable), e.g. a release build that owns the machine.
        #[arg(long, value_name = "FILE")]
        pause_while_locked: Vec<std::path::PathBuf>,
        /// Keep the share fixed instead of adapting to the machine's other load
        #[arg(long)]
        fixed: bool,
        /// The least CPUs the pool keeps when production needs the machine (default: a quarter of the share)
        #[arg(long, value_name = "CPUS")]
        min_cpus: Option<f64>,
        /// Share of the machine the pool and everything else may use together
        #[arg(long, value_name = "FRACTION", default_value_t = 0.8)]
        target_util: f64,
        /// Disk the slots' Cargo target directories may use together (GiB)
        #[arg(long, value_name = "GIB", default_value_t = 200)]
        cache_gib: u64,
        /// Write bandwidth the pool's checks may use on the cache disk (MiB/s);
        /// the agent halves it while the disk syncs slowly. 0 = no limit
        #[arg(long, value_name = "MIB", default_value_t = 200.0)]
        io_mib: f64,
        /// Never take checks whose name matches this SQL `like` pattern (repeatable, or
        /// comma-separated): a machine whose disk must stay quiet
        #[arg(long, value_name = "PATTERN", value_delimiter = ',')]
        decline: Vec<String>,
    },
    /// The pool: its agents, their load, and the checks queued and running.
    Pool {
        #[command(subcommand)]
        action: Option<PoolAction>,
    },
    /// Lanes: queues for a machine's scarce resources (Xcode on the Mac), by priority.
    Lane {
        #[command(subcommand)]
        action: LaneAction,
    },
    /// The Citrus build this repository runs (`#![pin(…)]`)
    #[command(name = "self")]
    Myself {
        #[command(subcommand)]
        action: SelfAction,
    },
    /// Moves a test stack's ports in FILE to a free block (for `#![prepare]`)
    Ports {
        file: std::path::PathBuf,
        /// Port variables: NAME gets block + its position; NAME+ is added when missing
        #[arg(required = true)]
        names: Vec<String>,
        /// A variable whose value mentions a port follows it: KEY=NAME
        #[arg(long = "url")]
        urls: Vec<String>,
    },
    #[command(hide = true)]
    Worker { run: String },
    /// Runs one check's steps (a JSON file) for a parallel worker.
    #[command(hide = true)]
    ExecSteps { file: std::path::PathBuf },
    #[command(hide = true)]
    ReleaseWorker { release: String },
    #[command(hide = true)]
    ApplyWorker { release: String },
    /// Runs one step of an apply (a roll beside the others).
    #[command(hide = true)]
    ApplyStep { release: String, step: String },
    #[command(hide = true)]
    RefreshResources,
}

#[derive(Subcommand, Debug)]
enum VersionAction {
    /// Hold the first free version at or after START for this commit and print it.
    Reserve {
        start: String,
        /// What it is for: the images or packages published under it.
        #[arg(long, required = true)]
        scope: Vec<String>,
    },
    /// Fail when VERSION belongs to another worktree or commit.
    Check {
        version: String,
        /// Only reservations for these names (default: what this commit holds).
        #[arg(long)]
        scope: Vec<String>,
    },
    /// Print the commit VERSION was reserved for.
    Source { version: String },
    /// Reserved versions, newest last.
    List {
        /// Only those reserved in the last N days.
        #[arg(long)]
        days: Option<f64>,
    },
}

#[derive(Subcommand, Debug)]
enum ReleaseAction {
    /// Release units and their last release (also `citrus release`).
    List,
    /// Release a unit from the current commit.
    Start {
        unit: String,
        /// Allow steps that change production.
        #[arg(long)]
        approve: bool,
        /// Release even if checks are not proven for this commit (recorded).
        #[arg(long)]
        unchecked: bool,
        #[arg(long)]
        detach: bool,
        /// Show the gates and exact commands without running anything.
        #[arg(long)]
        dry_run: bool,
        /// Release this version instead of reserving one (the unit's `version`
        /// step is skipped), e.g. for a project that names versions by hand.
        #[arg(long = "version", value_name = "VERSION")]
        version: Option<String>,
        /// What the reserved version raises: a fix (patch), a visible
        /// feature (minor) or a breaking change (major).
        #[arg(long, value_enum, default_value_t, conflicts_with = "version")]
        bump: release::Level,
    },
    /// Wait for a release (id, prefix or `last`) and print its result.
    Wait {
        release: String,
    },
    Show {
        release: String,
    },
    /// First error of the failed step; `--step` for one step, `--full` for everything.
    Log {
        release: String,
        #[arg(long)]
        step: Option<String>,
        #[arg(long)]
        full: bool,
    },
    /// Continue a failed or unknown release (an unknown step runs its recovery first).
    Resume {
        release: String,
        #[arg(long)]
        approve: bool,
    },
    /// Give up an unfinished release after checking the environment by hand.
    Abandon {
        release: String,
        #[arg(long)]
        reason: String,
    },
    /// Roll a unit back to the release before its last passed one.
    Rollback {
        unit: String,
        #[arg(long)]
        approve: bool,
    },
    /// Build and publish the images a release of HEAD would roll, by input key, and nothing else
    /// (the queue runs it beside a batch's checks; the release then reuses them).
    #[command(hide = true)]
    Prebuild {
        #[arg(required = true)]
        units: Vec<String>,
    },
    /// Past releases of a unit.
    History {
        unit: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
    },
}

#[derive(Subcommand, Debug)]
enum LaneAction {
    /// Wait for a place in LANE, run the command, free the place.
    /// One argument with spaces is a shell command line.
    Run {
        lane: String,
        /// owner-build, release, integrate or agent-check (the default).
        #[arg(long, default_value = "agent-check")]
        priority: String,
        /// What the queue shows for this command.
        #[arg(long)]
        label: Option<String>,
        /// No messages while waiting.
        #[arg(long)]
        quiet: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Who holds and waits for each lane, and wait/run times.
    Status {
        /// Times over the last N days.
        #[arg(long, default_value_t = 1.0)]
        days: f64,
    },
    /// Copy this build where the shims call it and write the xcodebuild,
    /// xcrun and citrus-lane shims into DIR (default ~/.local/bin).
    Install {
        #[arg(long)]
        bin: Option<std::path::PathBuf>,
    },
    /// Claude Code PreToolUse hook: refuse heavy Mac commands outside the lane.
    #[command(hide = true)]
    Hook,
}

#[derive(Subcommand, Debug)]
enum QueueAction {
    /// Queue this worktree's commit to land on the base; asking again returns the same entry.
    Add {
        /// Release this unit after landing (`unit[:minor|major]`), repeatable.
        #[arg(long = "release", value_name = "UNIT")]
        release: Vec<String>,
        #[arg(long)]
        base: Option<String>,
        /// Wait until it lands or fails (`--wait-release`: and its releases end).
        #[arg(long)]
        wait: bool,
        #[arg(long)]
        wait_release: bool,
        /// Queue a failed, conflicting or cancelled commit again.
        #[arg(long)]
        retry: bool,
        /// The tracked task this lands (`citrus task new`): it follows the entry.
        #[arg(long)]
        task: Option<String>,
    },
    /// What waits, what is checked, what landed (also `citrus queue`).
    List {
        /// Entries of the last week, not just the last day.
        #[arg(long)]
        all: bool,
    },
    /// Wait for an entry (default: this worktree's latest) to land or fail.
    Wait {
        entry: Option<String>,
        #[arg(long)]
        release: bool,
    },
    /// Take a waiting entry (default: this worktree's) out of the queue.
    Cancel { entry: Option<String> },
    /// The queue processor (started by `queue add`).
    #[command(hide = true)]
    Work,
}

#[derive(Subcommand, Debug)]
enum TaskAction {
    /// Track a task: what it is, its owner-hub card, whose turn it is.
    New {
        #[arg(value_name = "TITLE", required = true)]
        title: Vec<String>,
        /// Its id (`t-` is added); default `t-<utc>-<random>`.
        #[arg(long)]
        id: Option<String>,
        /// The owner-hub card it answers (`Q-123`).
        #[arg(long)]
        card: Option<String>,
        /// owner, lead, lead2, agent or citrus.
        #[arg(long, default_value = "lead")]
        turn: String,
        /// A release unit the change must reach (repeatable).
        #[arg(long = "unit", value_name = "UNIT")]
        units: Vec<String>,
        /// The assignment in full.
        #[arg(long)]
        about: Option<String>,
    },
    /// Take a task: the turn is the claimant's while it reports within the lease.
    Claim {
        task: String,
        /// Who takes it: lead, lead2 or an agent's name.
        #[arg(long = "as", value_name = "WHO")]
        who: String,
        /// Silence the claim survives (`2h`, `90m`).
        #[arg(long, default_value = "2h")]
        lease: String,
    },
    /// Say how it goes; the turn returns to the lead (`--turn` to another).
    Report {
        task: String,
        /// What was done, what is next.
        #[arg(required = true, value_name = "TEXT")]
        note: Vec<String>,
        /// The claimant's part is done: the claim is released.
        #[arg(long)]
        done: bool,
        #[arg(long, default_value = "lead")]
        turn: String,
    },
    /// Link what the task is made of: branches, queue entries, releases, units.
    Link {
        task: String,
        #[arg(long)]
        branch: Vec<String>,
        /// A queue entry (`q-…`).
        #[arg(long)]
        queue: Vec<String>,
        /// A release (`rel-…`, a unique prefix).
        #[arg(long)]
        release: Vec<String>,
        /// A release unit the change must reach.
        #[arg(long = "unit", value_name = "UNIT")]
        units: Vec<String>,
        #[arg(long)]
        card: Option<String>,
    },
    /// Record that the released result was checked (when no verify or
    /// postcheck step of its release proved it).
    Verify {
        task: String,
        /// What was checked and how.
        #[arg(required = true, value_name = "TEXT")]
        note: Vec<String>,
    },
    /// Close a task: nothing more happens to it.
    Close {
        task: String,
        /// Why, in a line.
        #[arg(value_name = "TEXT")]
        note: Vec<String>,
    },
    /// A task with its links and latest events.
    Show { task: String },
}

#[derive(Subcommand, Debug)]
enum SelfAction {
    /// Pin this repository to a Citrus commit; every `citrus` here runs it
    Pin { commit: String },
    /// The pinned commit and this build
    Version,
}

#[derive(Subcommand, Debug)]
enum PoolAction {
    /// Let an agent finish its checks and take no more (it stays connected).
    Drain { name: String },
    /// Let a drained agent take checks again (when it runs).
    Resume { name: String },
    /// Store a Citrus build for the commit it reports, so agents and
    /// launchers take it instead of compiling that version.
    Publish {
        file: std::path::PathBuf,
        /// `linux-x86_64`, `macos-aarch64`…; default: this machine's.
        #[arg(long)]
        platform: Option<String>,
    },
    /// The Citrus builds the pool holds, newest first.
    Binaries,
    /// Write the pool's build of a commit to a file (installers).
    Fetch {
        commit: String,
        to: std::path::PathBuf,
        /// `linux-x86_64`, `macos-aarch64`…; default: this machine's.
        #[arg(long)]
        platform: Option<String>,
    },
}

fn lane_command(action: &LaneAction, json: bool) -> Result<i32> {
    match action {
        LaneAction::Run {
            lane,
            priority,
            label,
            quiet,
            command,
        } => lanes::run(&lanes::Request {
            lane,
            priority: lanes::priority_value(priority)?,
            label: label.as_deref(),
            command,
            quiet: *quiet,
        }),
        LaneAction::Status { days } => {
            let states = lanes::states();
            let times = lanes::times(manifest::now() as f64 - days * 86400.0);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "lanes": states, "times": times})
                    )?
                );
                return Ok(0);
            }
            print_lanes(&states);
            print_lane_times(&times);
            Ok(0)
        }
        LaneAction::Install { bin } => {
            let bin = bin.clone().unwrap_or_else(|| {
                std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                    .join(".local/bin")
            });
            for path in lanes::install(&bin)? {
                println!("{path}");
            }
            Ok(0)
        }
        LaneAction::Hook => {
            let input = std::io::read_to_string(std::io::stdin()).unwrap_or_default();
            let (code, output) = lanes::hook(&input);
            if !output.is_empty() {
                println!("{output}");
            }
            Ok(code)
        }
    }
}

fn print_lanes(states: &[lanes::LaneState]) {
    for state in states {
        println!(
            "lane {} · {}/{} running · {} waiting{}",
            state.lane,
            state.running.len(),
            state.capacity,
            state.waiting.len(),
            if state.about.is_empty() {
                String::new()
            } else {
                format!(" · {}", state.about)
            }
        );
        for entry in state.running.iter().chain(&state.waiting) {
            println!("  {}", lanes::describe_entry(entry));
        }
    }
}

fn print_lane_times(times: &[lanes::Times]) {
    if times.is_empty() {
        return;
    }
    println!("lane times (wait avg/max · run avg/max):");
    for row in times {
        println!(
            "  {:<12} {:<12} {:>3}× · wait {}/{} · run {}/{}{}",
            row.lane,
            row.priority,
            row.count,
            age(row.wait_avg as i64),
            age(row.wait_max as i64),
            age(row.run_avg as i64),
            age(row.run_max as i64),
            if row.lost > 0 {
                format!(" · {} lost", row.lost)
            } else {
                String::new()
            }
        );
    }
}

fn pool_command(action: Option<&PoolAction>, json: bool) -> Result<i32> {
    match action {
        Some(PoolAction::Drain { name }) => {
            pool::set_agent_state(name, "draining")?;
            println!("agent {name} drains: it finishes its checks and takes no more");
            return Ok(0);
        }
        Some(PoolAction::Resume { name }) => {
            pool::set_agent_state(name, "ready")?;
            println!("agent {name} takes checks again");
            return Ok(0);
        }
        Some(PoolAction::Publish { file, platform }) => {
            let (commit, platform, sha) = pool::publish_binary(file, platform.as_deref())?;
            println!(
                "published Citrus {} for {platform} · sha256 {}",
                &commit[..12],
                &sha[..12]
            );
            return Ok(0);
        }
        Some(PoolAction::Fetch {
            commit,
            to,
            platform,
        }) => {
            let platform = platform
                .clone()
                .unwrap_or_else(|| format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH));
            if !pool::fetch_published(commit, &platform, to)? {
                anyhow::bail!("the pool holds no Citrus {commit} for {platform}");
            }
            println!(
                "fetched Citrus {} for {platform} to {}",
                &commit[..12.min(commit.len())],
                to.display()
            );
            return Ok(0);
        }
        Some(PoolAction::Binaries) => {
            let rows = pool::binaries()?;
            if json {
                let rows: Vec<_> = rows
                    .iter()
                    .map(|(commit, platform, sha, bytes)| {
                        json!({"commit": commit, "platform": platform, "sha256": sha, "bytes": bytes})
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schema": SCHEMA, "binaries": rows}))?
                );
            } else {
                for (commit, platform, sha, bytes) in rows {
                    println!(
                        "{}  {platform:<16} sha256 {} · {} KiB",
                        &commit[..12],
                        &sha[..12],
                        bytes / 1024
                    );
                }
            }
            return Ok(0);
        }
        None => {}
    }
    let overview = pool::overview()?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"schema": SCHEMA, "pool": overview}))?
        );
        return Ok(0);
    }
    println!("pool {}", overview.pool);
    for agent in &overview.agents {
        let state = if agent.seen_seconds > 60.0 {
            "gone".to_owned()
        } else if agent.running > 0 {
            format!("running {}", agent.running)
        } else {
            agent.state.clone()
        };
        println!(
            "  {:<20} {:<12} {:>2}/{:<2} CPUs · {} at once · load {:.1}{} · {}",
            agent.name,
            state,
            agent.share,
            agent.cpus,
            agent.slots,
            agent.load,
            if agent.budget > 0.0 && agent.budget + 0.05 < agent.share as f32 {
                format!(
                    " · limited to {:.1} CPUs{}",
                    agent.budget,
                    if agent.throttle.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", agent.throttle)
                    }
                )
            } else {
                String::new()
            },
            agent.labels.join(",")
        );
    }
    if overview.agents.is_empty() {
        println!("  no agents: `citrus agent` on a machine joins it");
    }
    println!(
        "queued {} · running {}",
        overview.queued,
        overview.running.len()
    );
    for (run, check, agent) in &overview.running {
        println!("  {run} {check} on {agent}");
    }
    Ok(0)
}

fn main() {
    if let Err(error) = pin::handover() {
        eprintln!("citrus: {error:#}");
        std::process::exit(2);
    }
    let cli = Cli::parse();
    let json = cli.json || (!cli.text && !std::io::stdout().is_terminal());
    match execute(cli.command, json, cli.profile) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            if json {
                println!(
                    "{}",
                    json!({"schema": SCHEMA, "error": format!("{error:#}")})
                );
            } else {
                eprintln!("citrus: {error:#}");
            }
            std::process::exit(2);
        }
    }
}

/// Checks run at once on this machine when nothing says otherwise: half its
/// CPUs (resource classes still bound heavy ones).
fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |cpus| (cpus.get() / 2).max(1))
}

/// Names on the command line may be written as in the language (`test_db`)
/// or as Citrus shows them (`test-db`).
fn dashed(command: Option<Command>) -> Option<Command> {
    let dash = |name: &mut String| *name = lang::compile::dash(name);
    let mut command = command?;
    match &mut command {
        Command::Run { targets, .. } => targets.iter_mut().for_each(dash),
        Command::Why { target, .. } => dash(target),
        Command::Log {
            target: Some(target),
            ..
        } => dash(target),
        Command::Do { task: Some(task) } => dash(task),
        Command::Diff { environment } | Command::Apply { environment, .. } => dash(environment),
        Command::Release {
            action:
                Some(
                    ReleaseAction::Start { unit, .. }
                    | ReleaseAction::Rollback { unit, .. }
                    | ReleaseAction::History { unit, .. },
                ),
        } => dash(unit),
        Command::Release {
            action: Some(ReleaseAction::Prebuild { units }),
        } => units.iter_mut().for_each(dash),
        _ => {}
    }
    Some(command)
}

fn execute(command: Option<Command>, json: bool, profile: Option<String>) -> Result<i32> {
    // A parallel worker's child needs no configuration: its steps are given.
    if let Some(Command::ExecSteps { file }) = &command {
        return exec::exec_steps(file);
    }
    if let Some(Command::Ports { file, names, urls }) = &command {
        return ports::run(&ports::Request { file, names, urls });
    }
    // The pool serves any repository: no configuration is read.
    match &command {
        Some(Command::Agent {
            share,
            slots,
            labels,
            name,
            native,
            idle_exit,
            pause_while_locked,
            fixed,
            min_cpus,
            target_util,
            cache_gib,
            io_mib,
            decline,
        }) => {
            return pool::agent(&pool::AgentOptions {
                name: name.clone(),
                share: *share,
                slots: *slots,
                labels: labels.clone(),
                native: *native,
                idle_exit: *idle_exit,
                pause_while_locked: pause_while_locked.clone(),
                fixed: *fixed,
                min_cpus: *min_cpus,
                target_util: Some(*target_util),
                cache_gib: Some(*cache_gib),
                io_mib: Some(*io_mib),
                decline: decline.clone(),
            });
        }
        Some(Command::Pool { action }) => return pool_command(action.as_ref(), json),
        Some(Command::Lane { action }) => return lane_command(action, json),
        Some(Command::Myself { action }) => {
            match action {
                SelfAction::Pin { commit } => println!("{}", pin::pin(commit)?),
                SelfAction::Version => println!("citrus {}", pool::VERSION),
            }
            return Ok(0);
        }
        _ => {}
    }
    let command = dashed(command);
    let mut context = Context::open(profile)?;
    let Some(command) = command else {
        return overview(&mut context, json);
    };
    match command {
        Command::Status { base } => status(&mut context, base.as_deref(), json),
        Command::Plan { base, paths_file } => {
            plan_command(&mut context, base.as_deref(), paths_file.as_deref(), json)
        }
        Command::Run {
            targets,
            base,
            local,
            remote,
            key,
            detach,
            force,
            jobs,
            paths_file,
            priority,
        } => {
            if let Some(priority) = &priority
                && std::env::var_os("CITRUS_RUN_PRIORITY").is_none()
            {
                let value = lanes::priority_value(priority)?;
                // SAFETY: set before any thread of this command starts.
                unsafe { std::env::set_var("CITRUS_RUN_PRIORITY", value.to_string()) };
            }
            if let Some(warning) = disk::low_space_warning(&context.repo.root) {
                eprintln!("! {warning}");
            }
            let paths = match &paths_file {
                Some(file) => {
                    let text = std::fs::read_to_string(file)
                        .with_context(|| format!("read {}", file.display()))?;
                    let mut paths: Vec<String> = text
                        .lines()
                        .map(str::trim)
                        .filter(|line| !line.is_empty())
                        .map(str::to_owned)
                        .collect();
                    paths.sort();
                    paths.dedup();
                    Some(paths)
                }
                None => None,
            };
            let jobs = jobs
                .or_else(|| {
                    std::env::var("CITRUS_JOBS")
                        .ok()
                        .and_then(|value| value.parse().ok())
                })
                .unwrap_or_else(default_jobs)
                .max(1);
            let mode = if local {
                Mode::Local
            } else if remote {
                Mode::Remote
            } else {
                Mode::Auto
            };
            let run = context.start(&Request {
                targets,
                base,
                mode,
                key,
                force,
                jobs,
                paths,
            })?;
            if detach || run.finished() {
                return emit_run(&context, &run, json);
            }
            eprintln!(
                "citrus: {} {} — Ctrl-C leaves it running; `citrus wait {}` to follow",
                run.id, run.mode, run.id
            );
            follow(&context, &run.id, json)
        }
        Command::Wait { run } => {
            let run = context.store.resolve(&run, &context.worktree())?;
            follow(&context, &run.id, json)
        }
        Command::Show { run } => {
            let run = context.reconcile(context.store.resolve(&run, &context.worktree())?)?;
            emit_run(&context, &run, json)
        }
        Command::Log { run, target, full } => log(&context, &run, target.as_deref(), full),
        Command::Why { target, base } => why(&mut context, &target, base.as_deref(), json),
        Command::Cancel { run } => {
            let run = context.reconcile(context.store.resolve(&run, &context.worktree())?)?;
            if !run.finished() {
                context.cancel(&run)?;
            }
            let run = context.store.run(&run.id)?.context("run disappeared")?;
            emit_run(&context, &run, json)
        }
        Command::Stats { days } => stats(&context, days, json),
        Command::Check => check_command(&context, json),
        Command::Test { filter } => test_command(&context, filter.as_deref(), json),
        Command::Fmt { check } => fmt_command(&context, check, json),
        Command::Deps { target_dir } => deps_command(&context, target_dir, json),
        Command::Do { task } => do_command(&context, task, json),
        Command::Integrate {
            base,
            push,
            no_run,
            release,
        } => integrate_command(&mut context, base, push, !no_run, &release, json),
        Command::Queue { action } => queue_command(&mut context, action, json),
        Command::Tasks {
            all,
            base,
            stuck,
            turn,
            unreleased_after,
            idle_after,
        } => {
            let limits = work::Limits {
                unreleased: work::parse_duration(&unreleased_after)?,
                idle: work::parse_duration(&idle_after)?,
            };
            if let Some(turn) = &turn {
                work::check_turn(turn)?;
            }
            if stuck {
                return stuck_command(&context, limits, turn.as_deref(), json);
            }
            tasks_command(&context, all, base, turn.as_deref(), json)
        }
        Command::Task {
            action: Some(action),
            ..
        } => task_action(&mut context, action, json),
        Command::Task {
            action: None,
            title,
            scope,
            blocked,
            needs,
            evidence,
            clear_blocker,
            clear,
        } => {
            let change = state::TaskChange {
                title: (!title.is_empty()).then(|| title.join(" ")),
                scope,
                blocked,
                needs,
                evidence,
                clear_blocker,
                clear,
            };
            let info = context
                .store
                .update_task(&context.worktree(), &change, &exec::agent())?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schema": SCHEMA, "task": info}))?
                );
            } else if clear {
                println!("task description cleared");
            } else {
                println!("{}", describe_task(&info));
            }
            Ok(0)
        }
        Command::Agree {
            key,
            terms,
            reopen,
            evidence,
            revision,
        } => {
            let agreement = context.store.agree(
                &key,
                [&terms, &reopen, &evidence],
                revision,
                &exec::agent(),
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "agreement": agreement})
                    )?
                );
            } else {
                println!(
                    "agreement {} · revision {}",
                    agreement.key, agreement.revision
                );
            }
            Ok(0)
        }
        Command::Version { action } => version_command(&context, action, json),
        Command::Targets { fingerprints } => targets_command(&context, fingerprints, json),
        Command::Artifacts {
            build: Some(name), ..
        } => {
            let name = name.replace('_', "-");
            let reference = apply::build_one(&context, &name)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "artifact": name, "reference": reference})
                    )?
                );
            } else {
                println!("{name} {reference}");
            }
            Ok(0)
        }
        Command::Artifacts { at, build: None } => {
            let revision = context
                .repo
                .git(&["rev-parse", at.as_deref().unwrap_or("HEAD")])?;
            let mut rows = Vec::new();
            for (name, artifact) in deploy::artifacts(&context)? {
                let paths = deploy::command_paths(&context, &artifact)?;
                let (key, files) =
                    deploy::key_at(&context, &name, &artifact, &revision, paths.as_deref())?;
                rows.push(json!({"artifact": name, "key": key, "files": files.len(), "description": artifact.description}));
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "revision": revision, "artifacts": rows})
                    )?
                );
            } else {
                println!("artifacts at {}", &revision[..12]);
                for row in rows {
                    println!(
                        "  {:<28} {} · {} input files",
                        row["artifact"].as_str().unwrap_or_default(),
                        &row["key"].as_str().unwrap_or_default()[..12],
                        row["files"]
                    );
                }
            }
            Ok(0)
        }
        Command::Diff { environment } => {
            let diff = deploy::diff(&context, &environment)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schema": SCHEMA, "diff": diff}))?
                );
                return Ok(0);
            }
            println!(
                "{} · running {} ({}) · HEAD {}",
                diff.environment,
                diff.running_release.as_deref().unwrap_or("?"),
                diff.running_commit
                    .as_deref()
                    .map(|commit| &commit[..commit.len().min(12)])
                    .unwrap_or("commit unknown"),
                &diff.head[..12]
            );
            println!("  running commit found by: {}", diff.found_by);
            for item in &diff.workloads {
                let mark = match item.change.as_str() {
                    "unchanged" => "≡",
                    "changed" => "○",
                    _ => "?",
                };
                println!(
                    "  {mark} {:<24} {} · {} · {}",
                    item.workload,
                    item.artifact,
                    item.detail,
                    item.running_digest
                        .as_deref()
                        .map(|digest| &digest[..digest.len().min(19)])
                        .unwrap_or("no digest")
                );
                if !item.changed_inputs.is_empty() {
                    println!(
                        "      {} inputs changed: {}",
                        item.changed_inputs.len(),
                        preview(&item.changed_inputs)
                    );
                }
            }
            if diff.actions.is_empty() {
                println!("nothing to apply: the environment runs what HEAD would build");
            } else {
                println!("apply would:");
                for action in &diff.actions {
                    println!("  · {action}");
                }
            }
            println!("plan {}", &diff.plan_hash[..12]);
            Ok(0)
        }
        Command::Release { action } => {
            release_command(&mut context, action.unwrap_or(ReleaseAction::List), json)
        }
        Command::Apply {
            environment,
            prepare: true,
            ..
        } => {
            let Some(prepared) = apply::prepare(&mut context, &environment, true)? else {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"schema": SCHEMA, "environment": environment, "state": "unchanged"})
                        )?
                    );
                } else {
                    println!("{environment} already runs what HEAD builds; nothing to prepare");
                }
                return Ok(0);
            };
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema": SCHEMA,
                        "state": "prepared",
                        "prepared": prepared,
                        "next": [format!("citrus apply {environment} --approve")],
                    }))?
                );
            } else {
                let short = &prepared.commit[..12.min(prepared.commit.len())];
                println!(
                    "{environment} · {short} prepared in {}s: {} checks proven, {} of {} artifacts built",
                    prepared.seconds,
                    prepared.checks.len(),
                    prepared.built.len(),
                    prepared.artifacts.len()
                );
                for (name, reference) in &prepared.artifacts {
                    println!("  {name}  {reference}");
                }
                println!("an apply or release of {short} now reuses them: migrate, roll, verify");
            }
            Ok(0)
        }
        Command::Apply {
            environment,
            approve,
            plan,
            unchecked,
            detach,
            prepare: false,
        } => {
            let started = apply::start(
                &mut context,
                &apply::Request {
                    environment: environment.clone(),
                    approve,
                    unchecked,
                    plan,
                },
            )?;
            let Some(started) = started else {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"schema": SCHEMA, "environment": environment, "state": "unchanged"})
                        )?
                    );
                } else {
                    println!("{environment} already runs what HEAD builds; nothing to apply");
                }
                return Ok(0);
            };
            if detach {
                return emit_release(&context, &started, json);
            }
            eprintln!(
                "citrus: {} — Ctrl-C leaves it running; `citrus release wait {}` to follow",
                started.id, started.id
            );
            let finished = wait_release(&context, &started.id, json)?;
            emit_release(&context, &finished, json)
        }
        Command::ApplyWorker { release } => {
            apply::work(&context, &release)?;
            Ok(0)
        }
        Command::ApplyStep { release, step } => {
            apply::one_step(&context, &release, &step)?;
            Ok(0)
        }
        Command::ReleaseWorker { release } => {
            release::work(&context, &release)?;
            Ok(0)
        }
        Command::Disk { trim, remove_idle } => disk::run(&context.repo, trim, remove_idle),
        Command::Doctor => {
            let findings = doctor::diagnose(&mut context);
            let failed = findings.iter().any(|finding| finding.status == "fail");
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "ok": !failed, "findings": findings})
                    )?
                );
            } else {
                for finding in &findings {
                    let mark = match finding.status {
                        "ok" => "✓",
                        "warn" => "!",
                        _ => "✗",
                    };
                    println!("{mark} {:<12} {}", finding.check, finding.detail);
                }
            }
            Ok(if failed { 1 } else { 0 })
        }
        Command::ExecSteps { .. } => unreachable!("handled before the configuration loads"),
        Command::Agent { .. }
        | Command::Pool { .. }
        | Command::Lane { .. }
        | Command::Myself { .. } => {
            unreachable!("handled before the configuration")
        }
        Command::Ports { .. } => unreachable!("handled before the configuration is read"),
        Command::Worker { run } => {
            context.work(&run)?;
            Ok(0)
        }
        Command::RefreshResources => {
            resources::refresh(&context)?;
            Ok(0)
        }
    }
}

/// Wait for the worker, reporting target results on stderr as they land.
fn wait_for(context: &Context, id: &str, json: bool) -> Result<Run> {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut shown_note = String::new();
    // CITRUS_PROTOCOL=1: the run log on stdout as it grows, so a Citrus that
    // runs this one as its runner sees CITRUS_TARGET lines and output live.
    let protocol = std::env::var("CITRUS_PROTOCOL").is_ok_and(|value| value == "1");
    let mut offset: u64 = 0;
    let mut stream = |run: &Run, finished: bool| {
        use std::io::{Read, Seek, SeekFrom, Write};
        let Ok(mut file) = std::fs::File::open(&run.log) else {
            return;
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return;
        }
        // Whole lines only, unless the run is over.
        let end = if finished {
            bytes.len()
        } else {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1)
        };
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&bytes[..end]);
        let _ = out.flush();
        offset += end as u64;
    };
    loop {
        let run = context.reconcile(context.store.run(id)?.context("unknown run")?)?;
        if protocol {
            stream(&run, run.finished());
        }
        for target in context.store.targets(id)? {
            let key = (target.target.clone(), target.result.clone());
            if !seen.contains(&key) && target.result != "pending" {
                if !json && std::io::stderr().is_terminal() || target.result != "running" {
                    eprintln!(
                        "  {} {}{}",
                        symbol(&target.result),
                        target.target,
                        seconds(&target)
                    );
                }
                seen.push(key);
            }
        }
        if !run.note.is_empty() && run.note != shown_note {
            eprintln!("  … {}", run.note);
            shown_note = run.note.clone();
        }
        if run.finished() {
            return Ok(run);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn follow(context: &Context, id: &str, json: bool) -> Result<i32> {
    let run = wait_for(context, id, json)?;
    emit_run(context, &run, json)
}

fn exit_code(run: &Run) -> i32 {
    match run.state.as_str() {
        "passed" => 0,
        "failed" => 1,
        "queued" | "waiting" | "running" => 0,
        _ => 3,
    }
}

fn next_steps(run: &Run, targets: &[RunTarget]) -> Vec<String> {
    match run.state.as_str() {
        "failed" => {
            let mut next = vec![format!("citrus log {}", run.id)];
            if let Some(target) = targets
                .iter()
                .find(|target| target.result == "failed" && target.reason != "suite_error")
            {
                next.push(format!("citrus why {}", target.target));
            }
            next.push("citrus run".into());
            next
        }
        "unknown" => vec![
            format!("citrus log {} --full", run.id),
            "citrus status".into(),
        ],
        "cancelled" => vec!["citrus run".into()],
        "passed" => Vec::new(),
        _ => vec![
            format!("citrus wait {}", run.id),
            format!("citrus cancel {}", run.id),
        ],
    }
}

fn emit_run(context: &Context, run: &Run, json: bool) -> Result<i32> {
    let targets = context.store.targets(&run.id)?;
    let next = next_steps(run, &targets);
    let elapsed = run.ended.unwrap_or(now() as i64) - run.started;
    if json {
        let value = json!({
            "schema": SCHEMA,
            "run": {
                "id": run.id, "state": run.state, "mode": run.mode, "note": run.note,
                "agent": run.agent, "worktree": run.worktree, "branch": run.branch,
                "snapshot": run.snapshot, "seconds": elapsed, "exit": run.exit, "pid": run.pid, "log": run.log, "linked_log": run.linked_log,
            },
            "targets": targets,
            "next": next,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "{} {} {} · {}",
            symbol(&run.state),
            run.id,
            run.state,
            age(elapsed)
        );
        if !run.note.is_empty() {
            println!("  {}", run.note);
        }
        for target in &targets {
            println!(
                "  {} {:<40} {}{}",
                symbol(&target.result),
                target.target,
                describe(target),
                seconds(target)
            );
            if let Some(error) = &target.first_error {
                for line in error.lines().take(12) {
                    println!("      {line}");
                }
            }
        }
        print_next(&next);
    }
    Ok(exit_code(run))
}

fn status(context: &mut Context, base: Option<&str>, json: bool) -> Result<i32> {
    let plan = plan::compute(&context.repo, &context.manifest, base)?;
    let files = context.repo.files()?;
    let snapshot = context.repo.snapshot()?;
    let decisions = plan
        .targets
        .iter()
        .map(|name| context.decide(&files, &snapshot, name, false))
        .collect::<Result<Vec<_>>>()?;
    let active = context
        .store
        .active()?
        .into_iter()
        .map(|run| context.reconcile(run))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|run| !run.finished())
        .collect::<Vec<_>>();
    let last = context
        .store
        .recent(&context.worktree(), 1)?
        .into_iter()
        .next();
    let needed = decisions
        .iter()
        .filter(|decision| decision.result != "reused")
        .count();
    let shared = resources::observe(context)?;
    // What other tasks said in the last day: their work and blockers.
    let others: Vec<state::TaskInfo> = context
        .store
        .task_infos()?
        .into_iter()
        .filter(|info| info.worktree != context.worktree() && now() as i64 - info.updated < 86400)
        .take(5)
        .collect();
    // The merge queue: what waits or is checked, and this worktree's latest entry.
    let queued: Vec<state::QueueEntry> =
        context
            .store
            .queue_in(&["queued", "checking", "releasing"])?;
    let own_entry = context.store.queue_of(&context.worktree())?;
    if queued.iter().any(|entry| entry.state != "merged") && !queue::processing(context) {
        // A processor that stopped is replaced by whoever looks.
        queue::ensure_processor(context)?;
    }
    let mut warnings = Vec::new();
    let mut next = Vec::new();
    // Lanes are this machine's, shown where the project declares them.
    let declares_lanes = context
        .project
        .as_ref()
        .is_some_and(|project| !project.lanes.is_empty());
    let lane_states: Vec<lanes::LaneState> = if declares_lanes {
        lanes::states()
            .into_iter()
            .filter(|state| !state.running.is_empty() || !state.waiting.is_empty())
            .collect()
    } else {
        Vec::new()
    };
    if declares_lanes {
        let refused = lanes::violations(manifest::now() as f64 - 3600.0).len();
        if refused > 0 {
            warnings.push(format!(
                "{refused} heavy Mac commands were refused outside the lane in the last hour: run them as `citrus-lane mac-xcode -- '<command>'`"
            ));
        }
        for (pid, command) in lanes::outside() {
            warnings.push(format!(
                "xcodebuild outside the lane mac-xcode (pid {pid}): {command} — Mac builds run only as `citrus-lane mac-xcode -- …`"
            ));
        }
    }
    let mine = active.iter().find(|run| run.worktree == context.worktree());
    if let Some(run) = mine.filter(|run| run.snapshot != snapshot) {
        warnings.push(format!(
            "sources changed after {} started: its result covers the earlier snapshot, and a remote runner may reject the edit — wait for it before editing, or cancel and rerun",
            run.id
        ));
    }
    if let Some(run) = mine {
        next.push(format!("citrus wait {}", run.id));
    } else if needed > 0 {
        next.push("citrus run".to_owned());
    }
    if let Some(run) = last.as_ref().filter(|run| run.state == "failed") {
        next.push(format!("citrus log {}", run.id));
    }
    if json {
        let value = json!({
            "schema": SCHEMA,
            "worktree": context.worktree(), "branch": context.repo.branch(), "head": context.repo.head(),
            "snapshot": snapshot,
            "plan": {"status": plan.status, "files": plan.files, "notes": plan.notes, "unmapped": plan.unmapped},
            "targets": decisions,
            "active_runs": active.iter().map(run_brief).collect::<Vec<_>>(),
            "last_run": last.as_ref().map(run_brief),
            "resources": shared,
            "tasks": others,
            "lanes": lane_states,
            "queue": {"entries": queued, "mine": own_entry, "processing": queue::processing(context)},
            "warnings": warnings,
            "next": next,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    for warning in &warnings {
        println!("! {warning}");
    }
    println!(
        "{} @ {} · {} changed files · plan {}",
        context.repo.branch(),
        context.repo.head(),
        plan.files,
        plan.status
    );
    if decisions.is_empty() {
        println!(
            "  nothing to check{}",
            if plan.notes.is_empty() {
                String::new()
            } else {
                format!(" ({})", plan.notes.join(", "))
            }
        );
    }
    for decision in &decisions {
        println!(
            "  {} {:<40} {}",
            symbol(&decision.result),
            decision.target,
            describe(decision)
        );
    }
    if !plan.unmapped.is_empty() {
        println!(
            "  ? {} paths no check claims: {}",
            plan.unmapped.len(),
            preview(&plan.unmapped)
        );
    }
    if let Some(run) = &last {
        let when = match run.ended {
            Some(ended) => format!("{} ago", age(now() as i64 - ended)),
            None => format!("running for {}", age(now() as i64 - run.started)),
        };
        println!("last run: {} {} · {when}", run.id, run.state);
    }
    if !active.is_empty() {
        println!("running now:");
        for run in &active {
            let worktree = run.worktree.rsplit('/').next().unwrap_or(&run.worktree);
            let note = if run.note.is_empty() {
                String::new()
            } else {
                format!(" · {}", run.note)
            };
            println!(
                "  {} {} {} · {} · {} · {}{}",
                run.id,
                run.state,
                run.mode,
                worktree,
                run.agent,
                age(now() as i64 - run.started),
                note
            );
        }
    }
    if !others.is_empty() {
        println!("other tasks:");
        for info in &others {
            let name = std::path::Path::new(&info.worktree)
                .file_name()
                .map_or(info.worktree.clone(), |name| {
                    name.to_string_lossy().into_owned()
                });
            println!(
                "  {name}: {} ({} ago)",
                describe_task(info),
                age(now() as i64 - info.updated)
            );
        }
    }
    print_lanes(&lane_states);
    if !queued.is_empty() || own_entry.is_some() {
        println!(
            "merge queue ({} waiting or checked, processor {}):",
            queued.len(),
            if queue::processing(context) {
                "working"
            } else {
                "idle"
            }
        );
        for entry in &queued {
            println!("  {}", queue::describe(entry));
        }
        if let Some(entry) = own_entry
            .as_ref()
            .filter(|entry| !queued.iter().any(|item| item.id == entry.id))
            .filter(|entry| now() as i64 - entry.updated < 86400)
        {
            println!("  ▸ yours: {}", queue::describe(entry));
        }
    }
    if let Some(shared) = &shared {
        let when = match shared.age {
            Some(age) if shared.refreshing => format!("{} ago, refreshing", report::age(age)),
            Some(age) => format!("{} ago", report::age(age)),
            None => "first snapshot is being taken".into(),
        };
        println!("resources ({when}):");
        for item in &shared.items {
            println!("  {}", resources::describe(item));
        }
    }
    print_next(&next);
    Ok(0)
}

fn stats(context: &Context, days: u64, json: bool) -> Result<i32> {
    let since = now() as i64 - (days * 86400) as i64;
    let stats = context.store.stats(since)?;
    let lane_times = lanes::times(since as f64);
    let pool_times = pool::queue_times(days).unwrap_or_default();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "days": days, "stats": stats, "lanes": lane_times, "pool": pool_times})
            )?
        );
        return Ok(0);
    }
    let total: i64 = stats.runs_by_state.iter().map(|(_, count)| count).sum();
    let join = |items: &[(String, i64)]| {
        items
            .iter()
            .map(|(key, count)| format!("{key} {count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    println!(
        "last {days} days: {total} runs ({})",
        join(&stats.runs_by_state)
    );
    println!("  by agent: {}", join(&stats.runs_by_agent));
    println!("  by mode: {}", join(&stats.runs_by_mode));
    let targets: i64 = stats.outcomes.iter().map(|(_, count, _)| count).sum();
    let reused = stats
        .outcomes
        .iter()
        .find(|(result, _, _)| result == "reused")
        .map_or(0, |(_, count, _)| *count);
    println!(
        "  targets: {targets}, reused {reused} ({}%)",
        if targets > 0 {
            reused * 100 / targets
        } else {
            0
        }
    );
    for (result, count, seconds) in &stats.outcomes {
        println!(
            "    {} {result}: {count}{}",
            symbol(result),
            if *seconds > 0 {
                format!(" · {}", age(*seconds))
            } else {
                String::new()
            }
        );
    }
    println!(
        "  time not spent thanks to reuse: {}",
        age(stats.saved_seconds)
    );
    println!(
        "  duplicate runs avoided (joined a running one): {}",
        stats.joined_runs
    );
    print_lane_times(&lane_times);
    if !pool_times.is_empty() {
        println!("pool queue (wait for an agent avg/max · run avg/max):");
        for row in &pool_times {
            println!(
                "  {:<12} {:>5}× · wait {}/{} · run {}/{}",
                row.priority,
                row.count,
                age(row.wait_avg as i64),
                age(row.wait_max as i64),
                age(row.run_avg as i64),
                age(row.run_max as i64)
            );
        }
    }
    Ok(0)
}

fn plan_command(
    context: &mut Context,
    base: Option<&str>,
    paths_file: Option<&str>,
    json: bool,
) -> Result<i32> {
    let plan = match paths_file {
        None => plan::compute(&context.repo, &context.manifest, base)?,
        Some(file) => {
            let text = if file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(file).with_context(|| format!("read {file}"))?
            };
            let mut paths: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            paths.sort();
            paths.dedup();
            let base = base.unwrap_or(&context.repo.config.plan.base).to_owned();
            let before = context
                .repo
                .git(&["merge-base", &base, "HEAD"])
                .unwrap_or_default();
            plan::for_paths(&context.repo, &context.manifest, &paths, &before)?
        }
    };
    let files = context.repo.files()?;
    let snapshot = context.repo.snapshot()?;
    let decisions = plan
        .targets
        .iter()
        .map(|name| context.decide(&files, &snapshot, name, false))
        .collect::<Result<Vec<_>>>()?;
    if json {
        // What each planned check runs.
        let runs: serde_json::Map<String, serde_json::Value> = plan
            .targets
            .iter()
            .filter_map(|name| {
                let target = context.manifest.targets.get(name)?;
                Some((name.clone(), target.declaration()["run"].clone()))
            })
            .collect();
        let value = json!({"schema": SCHEMA, "plan": plan, "targets": decisions, "run": runs});
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    println!("plan {} · {} changed files", plan.status, plan.files);
    for (path, surfaces) in plan.mapped.iter().take(20) {
        println!("  {path} → {surfaces}");
    }
    if plan.mapped.len() > 20 {
        println!("  … {} more paths", plan.mapped.len() - 20);
    }
    for decision in &decisions {
        println!(
            "  {} {:<40} {}",
            symbol(&decision.result),
            decision.target,
            describe(decision)
        );
    }
    for note in &plan.notes {
        println!("  · {note}");
    }
    Ok(0)
}

fn log(context: &Context, reference: &str, target: Option<&str>, full: bool) -> Result<i32> {
    let run = context.reconcile(context.store.resolve(reference, &context.worktree())?)?;
    let mut lines = exec::read_log(&run.log);
    if let Some(linked) = &run.linked_log {
        lines.push(format!("── linked log: {linked}"));
        lines.extend(exec::read_log(linked));
    }
    if full {
        for line in &lines {
            println!("{line}");
        }
        return Ok(exit_code(&run));
    }
    let prefixes = &context.repo.config.run.progress_prefixes;
    if let Some(target) = target {
        let part = exec::segment(&lines, target, prefixes);
        if part.is_empty() {
            println!(
                "no output recorded for {target} in {} (log: {})",
                run.id, run.log
            );
        }
        for line in part {
            println!("{line}");
        }
        return Ok(exit_code(&run));
    }
    let targets = context.store.targets(&run.id)?;
    let failed: Vec<&RunTarget> = targets
        .iter()
        .filter(|target| target.result == "failed")
        .collect();
    if failed.is_empty() {
        println!("{} {}: no failed targets", run.id, run.state);
        if run.state != "passed" {
            println!("{}", report::first_error(&lines).unwrap_or_default());
        }
    }
    for target in failed {
        println!(
            "── {} (exit {})",
            target.target,
            target.exit.map_or("?".into(), |code| code.to_string())
        );
        let excerpt = target.first_error.clone().or_else(|| {
            report::first_error(&exec::failure_segment(&lines, &target.target, prefixes))
        });
        println!(
            "{}",
            excerpt.unwrap_or_else(|| "(no output captured)".into())
        );
    }
    match &run.linked_log {
        Some(linked) => println!(
            "── full log: {} and {linked} ({} lines)",
            run.log,
            lines.len()
        ),
        None => println!("── full log: {} ({} lines)", run.log, lines.len()),
    }
    Ok(exit_code(&run))
}

fn why(context: &mut Context, target: &str, base: Option<&str>, json: bool) -> Result<i32> {
    let plan = plan::compute(&context.repo, &context.manifest, base)?;
    let files = context.repo.files()?;
    let snapshot = context.repo.snapshot()?;
    let decision = context.decide(&files, &snapshot, target, false)?;
    let declared = context.manifest.targets.get(target);
    let mut changed: Vec<String> = Vec::new();
    if decision.reason == "input_changed" {
        if let Some(entry) = declared.filter(|entry| entry.cache) {
            if let Some(previous) = context.store.latest_evidence(target, "inputs")? {
                let before: Vec<(String, String)> =
                    serde_json::from_str(&previous.detail).unwrap_or_default();
                let now = manifest::fingerprint(
                    &context.repo.root,
                    &files,
                    entry,
                    &context.repo.config.toolchain_files,
                )?
                .files;
                changed = now
                    .iter()
                    .filter(|item| !before.contains(item))
                    .map(|(path, _)| path.clone())
                    .collect();
                changed.extend(
                    before
                        .iter()
                        .filter(|(path, _)| !now.iter().any(|(other, _)| other == path))
                        .map(|(path, _)| format!("{path} (removed)")),
                );
            }
        } else if let Some(previous) = context.store.latest_evidence(target, "snapshot")? {
            changed = context.repo.changed_between(&previous.key, &snapshot);
        }
    }
    let selected = plan.targets.iter().any(|name| name == target);
    let owned: Vec<&String> = plan
        .mapped
        .iter()
        .filter(|(_, surfaces)| surfaces.contains(&format!("target:{target}")))
        .map(|(path, _)| path)
        .collect();
    if json {
        let value = json!({
            "schema": SCHEMA, "target": target, "selected": selected, "declared": declared.is_some(),
            "cache": declared.is_some_and(|entry| entry.cache),
            "resources": declared.map(|entry| entry.resources.clone()).unwrap_or_default(), "owned_changed_paths": owned,
            "decision": decision, "changed_since_last_pass": changed,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    println!(
        "{target}: {}",
        if selected {
            "needed by the current changes"
        } else {
            "not selected by the plan"
        }
    );
    if let Some(description) = declared.and_then(|entry| entry.description.as_deref()) {
        println!("  {description}");
    }
    if let Some(entry) = declared.filter(|entry| !entry.resources.is_empty()) {
        println!("  resources: {}", entry.resources.join(", "));
    }
    for (key, value) in declared
        .map(|entry| &entry.extensions)
        .into_iter()
        .flatten()
    {
        println!("  {key}: {value}");
    }
    if !owned.is_empty() {
        println!(
            "  owns changed paths: {}",
            preview(&owned.iter().map(|path| (*path).clone()).collect::<Vec<_>>())
        );
    }
    match declared {
        Some(entry) if entry.cache => println!(
            "  declared with cache: reused while its {} input globs are unchanged",
            entry.inputs.len() + entry.extra_inputs.len()
        ),
        Some(_) => {
            println!("  declared without cache: reused only for the identical source snapshot")
        }
        None => {
            println!("  not declared in citrus.ci: reused only for the identical source snapshot")
        }
    }
    println!("  {} {}", symbol(&decision.result), describe(&decision));
    if !changed.is_empty() {
        println!(
            "  changed since the last PASS ({}): {}",
            changed.len(),
            preview(&changed)
        );
    }
    Ok(0)
}

fn run_brief(run: &Run) -> Value {
    json!({
        "id": run.id, "state": run.state, "mode": run.mode, "note": run.note, "agent": run.agent,
        "worktree": run.worktree, "seconds": run.ended.unwrap_or(now() as i64) - run.started,
    })
}

fn symbol(result: &str) -> &'static str {
    match result {
        "passed" | "recovered" => "✓",
        "reused" => "≡",
        "failed" => "✗",
        "running" | "queued" | "waiting" | "joined" => "…",
        "pending" => "○",
        "cancelled" | "not_run" => "–",
        _ => "?",
    }
}

fn describe(target: &RunTarget) -> String {
    let reason = match target.reason.as_str() {
        "inputs_unchanged" => "proven: declared inputs unchanged",
        "same_snapshot" => "proven: identical sources passed",
        "carried_over" => "proven: passed before integration, incoming changes do not touch it",
        "no_evidence" => "needed: never passed",
        "input_changed" => "needed: inputs changed since the last pass",
        "evidence_expired" => "needed: last pass is too old",
        "forced" => "needed: --force",
        "ran" => "",
        "suite_passed" => "passed with the suite",
        "suite_failed" => "not run: the suite stopped earlier",
        "suite_error" => "the suite failed outside its targets",
        "planned_by_suite" => "",
        "process_lost" => "unknown: worker disappeared",
        "cancelled" => "cancelled",
        other => other,
    };
    match &target.evidence_run {
        Some(run) if target.result == "reused" => format!("{reason} ({run})"),
        _ if target.result == "running" => "running".into(),
        _ => reason.to_owned(),
    }
}

fn seconds(target: &RunTarget) -> String {
    target
        .seconds
        .map(|seconds| format!(" {}", age(seconds)))
        .unwrap_or_default()
}

fn preview(items: &[String]) -> String {
    let shown: Vec<&str> = items.iter().take(5).map(String::as_str).collect();
    if items.len() > 5 {
        format!("{} … +{}", shown.join(", "), items.len() - 5)
    } else {
        shown.join(", ")
    }
}

fn print_next(next: &[String]) {
    if !next.is_empty() {
        println!("next: {}", next.join(" · "));
    }
}

/// `citrus` alone: where things stand and what can be done here.
fn overview(context: &mut Context, json: bool) -> Result<i32> {
    let plan = plan::compute(&context.repo, &context.manifest, None).ok();
    let (mut needed, mut proven) = (0, 0);
    if let Some(plan) = &plan {
        let files = context.repo.files()?;
        let snapshot = context.repo.snapshot()?;
        for name in &plan.targets {
            match context
                .decide(&files, &snapshot, name, false)?
                .result
                .as_str()
            {
                "reused" => proven += 1,
                _ => needed += 1,
            }
        }
    }
    let running = context.store.active()?.len();
    let catalog = &context.repo.config.catalog;
    let commands = [
        (
            "citrus status",
            "what the current changes need, what is proven, who runs what",
        ),
        (
            "citrus run",
            "run what is not proven yet (citrus wait last after a lost session)",
        ),
        ("citrus log last", "first error of the last failed run"),
        ("citrus why <check>", "why a check is needed or not reused"),
        (
            "citrus integrate [--push]",
            "merge the base, keep what is still proven, recheck the rest",
        ),
        (
            "citrus queue add [--release <unit>]",
            "land the committed branch through the merge queue, then release",
        ),
        (
            "citrus tasks",
            "every worktree: branch, unmerged commits, runs, what it does; agreements",
        ),
        (
            "citrus task <title> [--blocked … --needs …]",
            "say what this worktree does and what blocks it",
        ),
        (
            "citrus agree <key> --terms … --reopen … --evidence …",
            "record who does what between tasks",
        ),
        (
            "citrus version reserve <start> --scope …",
            "hold a release version for this commit",
        ),
        ("citrus targets", "declared checks and their last result"),
        ("citrus doctor", "is this repository set up correctly"),
        (
            "citrus disk [--trim] [--remove-idle DAYS]",
            "free space and the target of every worktree; clean idle build caches",
        ),
    ];
    if json {
        let value = json!({
            "schema": SCHEMA,
            "branch": context.repo.branch(), "head": context.repo.head(),
            "needed": needed, "proven": proven, "running": running,
            "declared_checks": context.manifest.targets.keys().collect::<Vec<_>>(),
            "commands": commands.iter().map(|(command, purpose)| json!({"command": command, "purpose": purpose})).collect::<Vec<_>>(),
            "catalog": catalog,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    println!(
        "{} @ {} · {needed} checks needed, {proven} proven · {running} runs in progress",
        context.repo.branch(),
        context.repo.head()
    );
    println!("\nchecks:");
    for (command, purpose) in commands {
        println!("  {command:<32} {purpose}");
    }
    if !catalog.is_empty() {
        let mut groups: Vec<&str> = catalog.iter().map(|entry| entry.group.as_str()).collect();
        groups.dedup();
        let mut seen = Vec::new();
        for group in groups {
            if seen.contains(&group) {
                continue;
            }
            seen.push(group);
            println!(
                "\n{}:",
                if group.is_empty() {
                    "this repository"
                } else {
                    group
                }
            );
            for entry in catalog.iter().filter(|entry| entry.group == group) {
                println!("  {:<32} {}", entry.command, entry.description);
            }
        }
    }
    println!(
        "\nnext: {}",
        if needed > 0 {
            "citrus run"
        } else {
            "citrus status"
        }
    );
    Ok(0)
}

/// `git push HEAD:<branch>` without force; remote failures are retried after
/// 2, 4, 8 and 16 seconds.
fn push_with_retries(
    context: &Context,
    remote: &str,
    branch: &str,
) -> Result<std::process::Output> {
    let mut delay = 2;
    loop {
        let output = crate::repo::git()
            .arg("-C")
            .arg(&context.repo.root)
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
        if output.status.success() || !integrate::transient_push_error(&error) || delay > 16 {
            return Ok(output);
        }
        eprintln!(
            "citrus: the remote failed ({}); retrying the push in {delay}s",
            error
                .lines()
                .find(|line| line.contains("remote"))
                .unwrap_or("error")
                .trim()
        );
        std::thread::sleep(std::time::Duration::from_secs(delay));
        delay *= 2;
    }
}

fn integrate_command(
    context: &mut Context,
    base: Option<String>,
    push: bool,
    run_checks: bool,
    releases: &[String],
    json: bool,
) -> Result<i32> {
    let base = base.unwrap_or_else(|| context.repo.config.plan.base.clone());
    if push {
        return integrate_through_queue(context, &base, releases, json);
    }
    for attempt in 1..=3 {
        let result = integrate::integrate(context, &base)?;
        if !json {
            match result.outcome.as_str() {
                "up_to_date" => println!("{base}: nothing new to merge"),
                "conflict" => {
                    println!(
                        "✗ merging {base} conflicts in {} files:",
                        result.conflicts.len()
                    );
                    for path in &result.conflicts {
                        println!("    {path}");
                    }
                    print_next(&[
                        "resolve the files, git add, git commit".into(),
                        "citrus integrate".into(),
                    ]);
                }
                _ => {
                    println!(
                        "merged {base}: {} commits, {} changed paths",
                        result.incoming_commits,
                        result.incoming_paths.len()
                    );
                    if !result.carried.is_empty() {
                        println!(
                            "  ≡ still proven (incoming changes do not touch them): {}",
                            preview(&result.carried)
                        );
                    }
                    if !result.reselected.is_empty() {
                        println!(
                            "  ○ selected again by incoming changes: {}",
                            preview(&result.reselected)
                        );
                    }
                    if !result.carry_note.is_empty() {
                        println!("  ! {}", result.carry_note);
                    }
                }
            }
        }
        if result.outcome == "conflict" {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "integration": result, "next": ["resolve the files, git add, git commit", "citrus integrate"]})
                    )?
                );
            }
            return Ok(1);
        }
        // The merge may have moved the pin: the checks run with that Citrus.
        if result.outcome != "up_to_date" {
            pin::follow()?;
        }
        let mut run_value = Value::Null;
        if run_checks {
            // An integration's checks go before agents' checks in the pool.
            if std::env::var_os("CITRUS_RUN_PRIORITY").is_none() {
                // SAFETY: set before any thread of this command starts.
                unsafe { std::env::set_var("CITRUS_RUN_PRIORITY", "10") };
            }
            let run = context.start(&Request {
                targets: Vec::new(),
                base: None,
                mode: Mode::Auto,
                key: None,
                force: false,
                jobs: std::env::var("CITRUS_JOBS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or_else(default_jobs)
                    .max(1),
                paths: None,
            })?;
            let run = if run.finished() {
                run
            } else {
                wait_for(context, &run.id, json)?
            };
            if json {
                run_value = json!({"id": run.id, "state": run.state, "targets": context.store.targets(&run.id)?});
            } else {
                emit_run(context, &run, false)?;
            }
            if run.state != "passed" {
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &json!({"schema": SCHEMA, "integration": result, "run": run_value, "pushed": false})
                        )?
                    );
                }
                return Ok(exit_code(&run).max(1));
            }
        }
        if !push {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "integration": result, "run": run_value, "pushed": false})
                    )?
                );
            } else if result.outcome != "up_to_date" || run_checks {
                print_next(&["citrus integrate --push".into()]);
            }
            return Ok(0);
        }
        let (remote, branch) = integrate::split_base(context, &base);
        let Some(remote) = remote else {
            anyhow::bail!("--push needs a remote base such as origin/main, not {base}");
        };
        let pushed = push_with_retries(context, &remote, &branch)?;
        if pushed.status.success() {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "integration": result, "run": run_value, "pushed": true})
                    )?
                );
            } else {
                println!("✓ pushed {} to {base}", context.repo.head());
            }
            return Ok(0);
        }
        let error = String::from_utf8_lossy(&pushed.stderr);
        if !integrate::base_moved(&error) {
            anyhow::bail!("git push failed: {}", error.trim());
        }
        if !json {
            println!("{base} moved while checking (attempt {attempt}); integrating again");
        }
    }
    anyhow::bail!("{base} kept moving; run citrus integrate --push again")
}

/// `integrate --push`: merge the base here (conflicts are resolved where the
/// work is, and proven checks are carried), then land through the queue.
fn integrate_through_queue(
    context: &mut Context,
    base: &str,
    releases: &[String],
    json: bool,
) -> Result<i32> {
    let releases = queue::parse_releases(context, releases)?;
    let result = integrate::integrate(context, base)?;
    if result.outcome == "conflict" {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"schema": SCHEMA, "integration": result, "pushed": false, "next": ["resolve the files, git add, git commit", "citrus integrate --push"]})
                )?
            );
        } else {
            println!(
                "✗ merging {base} conflicts in {} files:",
                result.conflicts.len()
            );
            for path in &result.conflicts {
                println!("    {path}");
            }
            print_next(&[
                "resolve the files, git add, git commit".into(),
                "citrus integrate --push".into(),
            ]);
        }
        return Ok(1);
    }
    if result.outcome != "up_to_date" {
        // The merge may have moved the pin: the queue is driven by that Citrus.
        pin::follow()?;
        if !json {
            println!(
                "merged {base}: {} commits; carried: {}",
                result.incoming_commits,
                preview(&result.carried)
            );
        }
    }
    let (entry, _) = queue::add(context, base, &releases, false)?;
    if !json {
        println!("queued {}", queue::describe(&entry));
    }
    let entry = queue::wait(context, &entry.id, false)?;
    let landed = entry.landed();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA, "integration": result, "queue": entry, "pushed": landed,
                "next": if landed { Vec::<String>::new() } else { vec![format!("citrus log {}", entry.run), "citrus queue add --retry".into()] },
            }))?
        );
    } else if landed {
        println!(
            "✓ {} landed on {base} as {} ({}){}",
            entry.id,
            &entry.merged[..12.min(entry.merged.len())],
            report::compact_utc(entry.ended.unwrap_or(now() as i64) as u64),
            if entry.releases.is_empty() {
                String::new()
            } else {
                format!(
                    "; releasing {} (citrus queue wait --release)",
                    entry.releases
                )
            }
        );
    } else {
        println!("✗ {}", queue::describe(&entry));
    }
    Ok(if landed { 0 } else { 1 })
}

fn queue_command(context: &mut Context, action: Option<QueueAction>, json: bool) -> Result<i32> {
    let base = context.repo.config.plan.base.clone();
    let here = context.worktree();
    let entry_of = |context: &Context, reference: Option<String>| -> Result<state::QueueEntry> {
        match reference {
            Some(id) => context
                .store
                .queue_entry(&id)?
                .with_context(|| format!("no queue entry {id}")),
            None => context
                .store
                .queue_of(&here)?
                .context("this worktree queued nothing yet: citrus queue add"),
        }
    };
    let emit = |entry: &state::QueueEntry, next: Vec<String>| -> Result<()> {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"schema": SCHEMA, "queue": entry, "next": next})
                )?
            );
        } else {
            println!("{}", queue::describe(entry));
            print_next(&next);
        }
        Ok(())
    };
    let code = |entry: &state::QueueEntry| match entry.state.as_str() {
        "failed" | "conflict" | "release_failed" => 1,
        "cancelled" => 3,
        _ => 0,
    };
    match action.unwrap_or(QueueAction::List { all: false }) {
        QueueAction::Add {
            release,
            base: given,
            wait,
            wait_release,
            retry,
            task,
        } => {
            let releases = queue::parse_releases(context, &release)?;
            let base = given.unwrap_or(base);
            let tracked = task
                .map(|task| context.store.work_resolve(&task))
                .transpose()?;
            let (entry, inserted) = queue::add(context, &base, &releases, retry)?;
            if let Some(tracked) = tracked {
                link_queue_entry(context, tracked, &entry)?;
            }
            if !inserted && !json {
                println!("already queued (the same commit):");
            }
            if wait || wait_release {
                let entry = queue::wait(context, &entry.id, wait_release)?;
                emit(&entry, Vec::new())?;
                return Ok(code(&entry));
            }
            emit(
                &entry,
                vec![
                    format!("citrus queue wait {}", entry.id),
                    "citrus status".into(),
                ],
            )?;
            Ok(code(&entry))
        }
        QueueAction::List { all } => {
            let since = now() as i64 - if all { 7 * 86400 } else { 86400 };
            let entries = context.store.queue_recent(since)?;
            let processing = queue::processing(context);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "processing": processing, "queue": entries})
                    )?
                );
                return Ok(0);
            }
            println!(
                "queue · {} · processor {}",
                base,
                if processing { "working" } else { "idle" }
            );
            for entry in &entries {
                println!(
                    "  {}{}",
                    if entry.worktree == here { "▸ " } else { "" },
                    queue::describe(entry)
                );
            }
            if entries.is_empty() {
                println!(
                    "  nothing queued in the last {}",
                    if all { "week" } else { "day" }
                );
            }
            Ok(0)
        }
        QueueAction::Wait { entry, release } => {
            let entry = entry_of(context, entry)?;
            let entry = queue::wait(context, &entry.id, release)?;
            emit(&entry, Vec::new())?;
            Ok(code(&entry))
        }
        QueueAction::Cancel { entry } => {
            let entry = entry_of(context, entry)?;
            if !context.store.queue_move(
                &entry.id,
                &["queued"],
                "cancelled",
                "cancelled by its owner",
            )? {
                anyhow::bail!(
                    "{} is {}: only a waiting entry leaves the queue",
                    entry.id,
                    entry.state
                );
            }
            let entry = entry_of(context, Some(entry.id))?;
            emit(&entry, Vec::new())?;
            Ok(0)
        }
        QueueAction::Work => {
            queue::work(context)?;
            Ok(0)
        }
    }
}

fn tasks_command(
    context: &Context,
    all: bool,
    base: Option<String>,
    turn: Option<&str>,
    json: bool,
) -> Result<i32> {
    let base = base.unwrap_or_else(|| context.repo.config.plan.base.clone());
    let mut tracked = Vec::new();
    for task in context.store.work_tasks(all)? {
        let (task, _) = work::refresh(context, task)?;
        if turn.is_none_or(|turn| task.turn == turn && task.closed.is_none()) {
            tracked.push(task);
        }
    }
    if turn.is_some() {
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"schema": SCHEMA, "tracked": tracked}))?
            );
        } else {
            for task in &tracked {
                println!("{}", work::describe(task));
            }
        }
        return Ok(0);
    }
    let tasks = tasks::list(context, &base)?;
    let shown: Vec<&tasks::Task> = tasks
        .iter()
        .filter(|task| all || tasks::active(task))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "base": base, "tracked": tracked, "total": tasks.len(), "tasks": shown, "agreements": context.store.agreements()?})
            )?
        );
        return Ok(0);
    }
    if !tracked.is_empty() {
        println!("tracked tasks:");
        for task in &tracked {
            println!("  {}", work::describe(task));
        }
    }
    println!(
        "{} of {} worktrees{} · base {base}",
        shown.len(),
        tasks.len(),
        if all {
            ""
        } else {
            " with activity (--all for every one)"
        }
    );
    for task in shown {
        let ahead = match (task.ahead, task.behind) {
            (Some(ahead), Some(behind)) => format!("+{ahead}/-{behind}"),
            _ => "?".into(),
        };
        let idle = task.idle.map(age).unwrap_or_default();
        let run = match (&task.last_run_state, task.running) {
            (_, true) => " · running".to_owned(),
            (Some(state), _) => format!(" · last run {state}"),
            _ => String::new(),
        };
        let marker = if task.current {
            "▸"
        } else if task.missing {
            "✗"
        } else {
            " "
        };
        println!(
            "{marker} {:<36} {:<34} {ahead:>9} · {idle}{run}",
            task.name, task.branch
        );
        if let Some(info) = &task.info {
            println!("      {} — {}", describe_task(info), info.agent);
        }
        if let Some(entry) = &task.queue {
            println!("      queue: {entry}");
        }
    }
    let agreements = context.store.agreements()?;
    if !agreements.is_empty() {
        println!("agreements:");
        for agreement in &agreements {
            println!(
                "  {} (revision {}, {}): {}",
                agreement.key, agreement.revision, agreement.owner, agreement.terms
            );
            println!("      reopen: {}", agreement.reopen);
        }
    }
    Ok(0)
}

/// `citrus tasks --stuck`: the tracked tasks nobody moves, one line each.
fn stuck_command(
    context: &Context,
    limits: work::Limits,
    turn: Option<&str>,
    json: bool,
) -> Result<i32> {
    let mut found = Vec::new();
    for task in context.store.work_tasks(false)? {
        let (task, progress) = work::refresh(context, task)?;
        found.extend(
            work::stuck(context, &task, &progress, limits)?
                .into_iter()
                .filter(|stuck| turn.is_none_or(|turn| stuck.to == turn)),
        );
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"schema": SCHEMA, "stuck": found}))?
        );
    } else if found.is_empty() {
        println!("no stuck tasks");
    } else {
        for stuck in &found {
            println!(
                "{} {}{} → {}: {}",
                stuck.task,
                stuck.kind,
                if stuck.card.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", stuck.card)
                },
                stuck.to,
                stuck.text
            );
        }
    }
    Ok(0)
}

/// Follow `entry` from `task`: the entry, its branch and worktree, its units.
fn link_queue_entry(
    context: &Context,
    mut task: state::WorkTask,
    entry: &state::QueueEntry,
) -> Result<()> {
    let store = &context.store;
    store.work_link(&task.id, "queue", &entry.id)?;
    store.work_link(&task.id, "worktree", &entry.worktree)?;
    if !entry.branch.is_empty() && entry.branch != "(detached)" {
        store.work_link(&task.id, "branch", &entry.branch)?;
    }
    work::add_units(
        &mut task,
        entry.release_units().into_iter().map(|(unit, _)| unit),
    );
    if work::step_rank(&task.step) < work::step_rank("in-queue") && task.closed.is_none() {
        task.step = "in-queue".into();
        task.turn = "citrus".into();
    }
    task.updated = now() as i64;
    store.work_update(&task, &exec::agent(), &format!("queued {}", entry.id))?;
    Ok(())
}

/// Units named on the command line, checked against the declared release units.
fn release_units(context: &mut Context, units: &[String]) -> Result<Vec<String>> {
    Ok(queue::parse_releases(context, units)?
        .split(',')
        .filter(|item| !item.is_empty())
        .map(|item| item.split(':').next().unwrap_or(item).to_owned())
        .collect())
}

fn task_action(context: &mut Context, action: TaskAction, json: bool) -> Result<i32> {
    let agent = exec::agent();
    let at = now() as i64;
    let (task, text) = match action {
        TaskAction::New {
            title,
            id,
            card,
            turn,
            units,
            about: text,
        } => {
            let title = title.join(" ");
            work::check_text(&title)?;
            anyhow::ensure!(!title.trim().is_empty(), "a task has a title");
            work::check_turn(&turn)?;
            if let Some(card) = &card {
                work::check_name("a card", card)?;
            }
            if let Some(text) = &text {
                work::check_text(text)?;
            }
            let units = release_units(context, &units)?;
            let task = state::WorkTask {
                id: work::new_id(id.as_deref())?,
                title: title.trim().to_owned(),
                card: card.unwrap_or_default(),
                step: "open".into(),
                turn,
                claimed_by: String::new(),
                lease: work::DEFAULT_LEASE,
                units: units.join(","),
                agent: agent.clone(),
                created: at,
                updated: at,
                closed: None,
            };
            let event = text.unwrap_or_else(|| format!("opened: {}", task.title));
            if !context.store.work_insert(&task, &event)? {
                let existing = context.store.work_resolve(&task.id)?;
                anyhow::ensure!(
                    existing.title == task.title,
                    "task {} exists with another title: {}",
                    task.id,
                    existing.title
                );
                // Asking again (a lost terminal) returns the same task.
                return show_task(context, existing, json);
            }
            return show_task(context, task, json);
        }
        TaskAction::Claim { task, who, lease } => {
            work::check_name("a claimant", &who)?;
            let lease = work::parse_duration(&lease)?;
            let mut task = context.store.work_resolve(&task)?;
            anyhow::ensure!(task.closed.is_none(), "{} is closed", task.id);
            if !task.claimed_by.is_empty() && task.claimed_by != who {
                eprintln!(
                    "citrus: {} was claimed by {}; now {who}",
                    task.id, task.claimed_by
                );
            }
            task.claimed_by = who.clone();
            task.lease = lease;
            task.turn = work::turn_of(&who).to_owned();
            if task.step == "open" {
                task.step = "claimed".into();
            }
            // The worktree and branch it is claimed from are where its work shows.
            let branch = context.repo.branch();
            let base_branch = context
                .repo
                .config
                .plan
                .base
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            if branch != "(detached)"
                && !branch.is_empty()
                && branch != base_branch
                && branch != "HEAD"
            {
                context.store.work_link(&task.id, "branch", &branch)?;
                context
                    .store
                    .work_link(&task.id, "worktree", &context.worktree())?;
            }
            (
                task,
                format!("claimed by {who} (lease {})", report::age(lease)),
            )
        }
        TaskAction::Report {
            task,
            note,
            done,
            turn,
        } => {
            let text = note.join(" ");
            work::check_text(&text)?;
            work::check_turn(&turn)?;
            let mut task = context.store.work_resolve(&task)?;
            task.turn = turn;
            if done {
                task.claimed_by.clear();
            }
            (task, if done { format!("done: {text}") } else { text })
        }
        TaskAction::Link {
            task,
            branch,
            queue,
            release,
            units,
            card,
        } => {
            let mut task = context.store.work_resolve(&task)?;
            let units = release_units(context, &units)?;
            let mut linked = Vec::new();
            for name in &branch {
                work::check_name("a branch", name)?;
                context.store.work_link(&task.id, "branch", name)?;
                linked.push(format!("branch {name}"));
            }
            for id in &queue {
                let entry = context
                    .store
                    .queue_entry(id)?
                    .with_context(|| format!("no queue entry {id}"))?;
                link_queue_entry(context, task.clone(), &entry)?;
                task = context.store.work_resolve(&task.id)?;
                linked.push(format!("queue {id}"));
            }
            for reference in &release {
                let found = context
                    .store
                    .resolve_release(reference, &context.worktree())?;
                context.store.work_link(&task.id, "release", &found.id)?;
                linked.push(format!("release {}", found.id));
            }
            if !units.is_empty() {
                work::add_units(&mut task, units.iter().cloned());
                linked.push(format!("units {}", units.join(",")));
            }
            if let Some(card) = card {
                work::check_name("a card", &card)?;
                linked.push(format!("card {card}"));
                task.card = card;
            }
            anyhow::ensure!(
                !linked.is_empty(),
                "link what: --branch, --queue, --release, --unit or --card"
            );
            (task, format!("linked {}", linked.join(", ")))
        }
        TaskAction::Verify { task, note } => {
            let text = note.join(" ");
            work::check_text(&text)?;
            anyhow::ensure!(!text.trim().is_empty(), "say what was checked");
            let mut task = context.store.work_resolve(&task)?;
            anyhow::ensure!(task.closed.is_none(), "{} is closed", task.id);
            context.store.work_link(&task.id, "verified", &agent)?;
            task.step = "verified".into();
            task.turn = "lead".into();
            (task, format!("verified by {agent}: {text}"))
        }
        TaskAction::Close { task, note } => {
            let text = (!note.is_empty()).then(|| note.join(" "));
            let mut task = context.store.work_resolve(&task)?;
            if let Some(text) = &text {
                work::check_text(text)?;
            }
            task.step = "closed".into();
            task.claimed_by.clear();
            task.closed = Some(at);
            (
                task,
                text.map_or_else(|| "closed".to_owned(), |text| format!("closed: {text}")),
            )
        }
        TaskAction::Show { task } => {
            let task = context.store.work_resolve(&task)?;
            return show_task(context, task, json);
        }
    };
    let mut task = task;
    task.updated = at;
    context.store.work_update(&task, &agent, &text)?;
    show_task(context, task, json)
}

fn show_task(context: &Context, task: state::WorkTask, json: bool) -> Result<i32> {
    let (task, progress) = work::refresh(context, task)?;
    let links = context.store.work_links(&task.id)?;
    let events = context.store.work_events(&task.id, 10)?;
    let stuck = work::stuck(context, &task, &progress, work::Limits::default())?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA, "task": task, "links": links, "progress": progress,
                "events": events, "stuck": stuck,
            }))?
        );
        return Ok(0);
    }
    println!("{}", work::describe(&task));
    if !task.units.is_empty() {
        println!("  units: {}", task.units);
    }
    for link in &links {
        println!("  {}: {}", link.kind, link.value);
    }
    for event in &events {
        println!(
            "  {} {} [{} · {}] {}",
            report::compact_utc(event.at as u64),
            event.agent,
            event.step,
            event.turn,
            event.text
        );
    }
    for stuck in &stuck {
        println!("! {}: {}", stuck.kind, stuck.text);
    }
    Ok(0)
}

/// A task's description in a line: what it does, what blocks it.
fn version_command(context: &Context, action: VersionAction, json: bool) -> Result<i32> {
    let committed = || -> Result<(String, String)> {
        let repo = &context.repo;
        anyhow::ensure!(
            repo.git(&["status", "--porcelain", "--untracked-files=normal"])?
                .is_empty(),
            "commit the source first: a version belongs to a committed source"
        );
        Ok((
            repo.root.display().to_string(),
            repo.git(&["rev-parse", "HEAD"])?,
        ))
    };
    match action {
        VersionAction::Reserve { start, scope } => {
            let scope = versions::scope(&scope)?;
            let (owner, source) = committed()?;
            let free_version = context
                .project
                .as_ref()
                .map(|project| project.free_version.clone())
                .unwrap_or_default();
            let agent = exec::agent();
            let version = versions::reserve(
                &context.store,
                &context.repo.root,
                &free_version,
                &start,
                &scope,
                &versions::Holder {
                    owner: &owner,
                    source: &source,
                    agent: &agent,
                },
                &|_| None,
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "version": version, "scope": scope, "source": source})
                    )?
                );
            } else {
                println!("{version}");
            }
        }
        VersionAction::Check { version, scope } => {
            let scope = if scope.is_empty() {
                None
            } else {
                Some(versions::scope(&scope)?)
            };
            let (owner, source) = committed()?;
            versions::check(&context.store, &version, scope.as_deref(), &owner, &source)?;
        }
        VersionAction::Source { version } => {
            println!("{}", versions::source(&context.store, &version)?);
        }
        VersionAction::List { days } => {
            let since = days.map_or(0, |days| now() as i64 - (days * 86400.0) as i64);
            let rows: Vec<_> = context
                .store
                .reservations(None)?
                .into_iter()
                .filter(|r| r.created >= since)
                .collect();
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schema": SCHEMA, "reservations": rows}))?
                );
            } else {
                for r in rows {
                    println!(
                        "{:<22} {} · {} · {} · {} ago",
                        r.version,
                        r.scope.join(","),
                        &r.source[..r.source.len().min(8)],
                        std::path::Path::new(&r.owner)
                            .file_name()
                            .map_or(r.owner.clone(), |name| name.to_string_lossy().into_owned()),
                        age(now() as i64 - r.created),
                    );
                }
            }
        }
    }
    Ok(0)
}

fn describe_task(info: &state::TaskInfo) -> String {
    let mut parts = Vec::new();
    if !info.title.is_empty() {
        parts.push(format!("“{}”", info.title));
    }
    if !info.scope.is_empty() {
        parts.push(format!("scope: {}", info.scope));
    }
    if !info.blocked.is_empty() {
        parts.push(format!("blocked: {} — needs: {}", info.blocked, info.needs));
    }
    if !info.evidence.is_empty() {
        parts.push(format!("evidence: {}", info.evidence));
    }
    if parts.is_empty() {
        "(no description)".to_owned()
    } else {
        parts.join(" · ")
    }
}

fn targets_command(context: &Context, fingerprints: bool, json: bool) -> Result<i32> {
    let files = if fingerprints {
        Some(context.repo.files()?)
    } else {
        None
    };
    let mut rows = Vec::new();
    for target in context.manifest.targets.values() {
        let last = context
            .store
            .latest_evidence(&target.name, "inputs")?
            .or(context.store.latest_evidence(&target.name, "snapshot")?);
        rows.push((target, last));
    }
    if json {
        let export = context
            .manifest
            .export(&context.repo.config.toolchain_files);
        let value: Vec<Value> = export["checks"]
            .as_array()
            .into_iter()
            .flatten()
            .zip(&rows)
            .map(|(check, (_, last))| {
                let mut check = check.clone();
                check["last_pass"] = json!(last.as_ref().map(|evidence| json!({"run": evidence.run, "seconds_ago": now() as i64 - evidence.created})));
                if let (Some(files), Some(target)) = (&files, check["target"].as_str().and_then(|name| context.manifest.targets.get(name)))
                    && target.cache
                    && let Ok(found) = manifest::fingerprint(&context.repo.root, files, target, &context.repo.config.toolchain_files)
                {
                    check["fingerprint"] = json!(found.value);
                }
                check
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA, "files": export["files"], "toolchain": export["toolchain"], "targets": value
            }))?
        );
        return Ok(0);
    }
    println!("{} declared checks in citrus.ci", rows.len());
    for (target, last) in rows {
        let when = last
            .map(|evidence| format!("passed {} ago", age(now() as i64 - evidence.created)))
            .unwrap_or_else(|| "never passed here".into());
        println!(
            "  {:<36} {}{} · {when}",
            target.name,
            if target.cache { "cache · " } else { "" },
            target.description.as_deref().unwrap_or("")
        );
    }
    Ok(0)
}

fn release_command(context: &mut Context, action: ReleaseAction, json: bool) -> Result<i32> {
    let here = context.worktree();
    match action {
        ReleaseAction::List => {
            let units = release::Releases::load(context)?;
            let mut rows = Vec::new();
            for (name, unit) in &units.releases {
                let last = context.store.releases_of(name, 1)?.into_iter().next();
                let last = last
                    .map(|item| release::reconcile(context, item))
                    .transpose()?;
                let holder = context.store.environment_holder(&unit.environment)?;
                rows.push((name, unit, last, holder));
            }
            if json {
                let value: Vec<Value> = rows
                    .iter()
                    .map(|(name, unit, last, holder)| {
                        json!({
                            "unit": name, "description": unit.description, "environment": unit.environment,
                            "steps": unit.steps.iter().map(|step| json!({"name": step.name, "production": step.production})).collect::<Vec<_>>(),
                            "last": last, "environment_holder": holder,
                        })
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"schema": SCHEMA, "releases": value}))?
                );
                return Ok(0);
            }
            if rows.is_empty() {
                println!("no release blocks in citrus.ci");
            }
            for (name, unit, last, holder) in rows {
                let steps: Vec<String> = unit
                    .steps
                    .iter()
                    .map(|step| {
                        if step.production {
                            format!("{}*", step.name)
                        } else {
                            step.name.clone()
                        }
                    })
                    .collect();
                println!("{name:<16} {}", unit.description);
                println!("  steps: {}   (* changes production)", steps.join(" → "));
                match last {
                    Some(last) => println!(
                        "  last: {} {} {} · {} ago{}",
                        last.id,
                        last.version,
                        last.state,
                        age(now() as i64 - last.ended.unwrap_or(last.started)),
                        holder
                            .map(|holder| format!(" · {} held by {holder}", unit.environment))
                            .unwrap_or_default()
                    ),
                    None => println!("  no releases recorded yet"),
                }
            }
            print_next(&["citrus release start <unit> --approve".into()]);
            Ok(0)
        }
        ReleaseAction::Start {
            unit,
            approve,
            unchecked,
            detach,
            dry_run,
            version,
            bump,
        } => {
            if dry_run {
                let plan = release::dry_run(&mut *context, &unit, version.as_deref(), bump)?;
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({"schema": SCHEMA, "dry_run": plan}))?
                    );
                    return Ok(0);
                }
                println!(
                    "release {unit} → {} (dry run, nothing runs)",
                    plan["environment"].as_str().unwrap_or_default()
                );
                println!(
                    "  source: {}",
                    if plan["clean"] == true {
                        "committed"
                    } else {
                        "has uncommitted changes — commit first"
                    }
                );
                if let Some(holder) = plan["environment_holder"].as_str() {
                    println!("  environment held by {holder}");
                }
                let needed = plan["checks_needed"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if plan["checks_gate"] == "proven" && !needed.is_empty() {
                    println!(
                        "  checks not proven yet: {}",
                        needed
                            .iter()
                            .filter_map(|item| item.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
                let reserved = plan["steps"]
                    .as_array()
                    .is_some_and(|steps| steps.iter().any(|step| step["step"] == "version"));
                println!(
                    "  previous: {} · next: {}{}",
                    plan["previous"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .unwrap_or("none recorded"),
                    plan["next_version"].as_str().unwrap_or_default(),
                    if reserved {
                        " (the reservation may pick a later free version)"
                    } else {
                        ""
                    }
                );
                for step in plan["steps"].as_array().cloned().unwrap_or_default() {
                    let mark = if step["production"] == true { "*" } else { " " };
                    println!(
                        "  {mark} {:<12} {}",
                        step["step"].as_str().unwrap_or_default(),
                        step["run"].as_str().unwrap_or_default()
                    );
                    if let Some(recover) = step["recover"].as_str() {
                        println!("    {:<12} recover: {recover}", "");
                    }
                }
                return Ok(0);
            }
            let started = release::start(
                context,
                &release::Start {
                    unit,
                    approve,
                    unchecked,
                    rollback: false,
                    version,
                    bump,
                },
            )?;
            if detach {
                return emit_release(context, &started, json);
            }
            eprintln!(
                "citrus: {} — Ctrl-C leaves it running; `citrus release wait {}` to follow",
                started.id, started.id
            );
            let finished = wait_release(context, &started.id, json)?;
            emit_release(context, &finished, json)
        }
        ReleaseAction::Prebuild { units } => {
            if !queue::prebuild_enabled() {
                println!(
                    "prebuild is off (CITRUS_QUEUE_PREBUILD=1 turns it on): the release builds {}",
                    units.join(", ")
                );
                return Ok(0);
            }
            let mut rows = Vec::new();
            for unit in &units {
                let prepared = release::prebuild(context, unit)?;
                if !json {
                    match &prepared {
                        Some(prepared) => println!(
                            "{unit} · {} prebuilt in {}s: {} of {} artifacts built",
                            prepared.environment,
                            prepared.seconds,
                            prepared.built.len(),
                            prepared.artifacts.len()
                        ),
                        None => println!("{unit}: nothing to build"),
                    }
                }
                rows.push(json!({"unit": unit, "prepared": prepared}));
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "state": "prebuilt", "units": rows})
                    )?
                );
            }
            Ok(0)
        }
        ReleaseAction::Rollback { unit, approve } => {
            let started = release::start(
                context,
                &release::Start {
                    unit,
                    approve,
                    unchecked: true,
                    rollback: true,
                    version: None,
                    bump: release::Level::Patch,
                },
            )?;
            let finished = wait_release(context, &started.id, json)?;
            emit_release(context, &finished, json)
        }
        ReleaseAction::Wait { release } => {
            let found = context.store.resolve_release(&release, &here)?;
            let finished = wait_release(context, &found.id, json)?;
            emit_release(context, &finished, json)
        }
        ReleaseAction::Show { release } => {
            let found =
                release::reconcile(context, context.store.resolve_release(&release, &here)?)?;
            emit_release(context, &found, json)
        }
        ReleaseAction::Resume { release, approve } => {
            let found = context.store.resolve_release(&release, &here)?;
            let resumed = release::resume(context, &found, approve)?;
            let finished = wait_release(context, &resumed.id, json)?;
            emit_release(context, &finished, json)
        }
        ReleaseAction::Abandon { release, reason } => {
            let found =
                release::reconcile(context, context.store.resolve_release(&release, &here)?)?;
            release::abandon(context, &found, &reason)?;
            let found = context
                .store
                .release(&found.id)?
                .context("release disappeared")?;
            emit_release(context, &found, json)
        }
        ReleaseAction::Log {
            release,
            step,
            full,
        } => {
            let found = context.store.resolve_release(&release, &here)?;
            let lines = exec::read_log(&found.log);
            if full {
                for line in &lines {
                    println!("{line}");
                }
                return Ok(0);
            }
            let prefixes = vec!["CITRUS_STEP".to_owned()];
            if let Some(step) = step {
                for line in exec::segment(&lines, &step, &prefixes) {
                    println!("{line}");
                }
                return Ok(0);
            }
            let steps = release::steps_of(context, &found.id)?;
            let failed: Vec<_> = steps
                .iter()
                .filter(|step| matches!(step.state.as_str(), "failed" | "unknown"))
                .collect();
            if failed.is_empty() {
                println!("{} {}: no failed steps", found.id, found.state);
            }
            for step in failed {
                println!("── {} ({})", step.name, step.state);
                let excerpt = step
                    .first_error
                    .clone()
                    .or_else(|| report::first_error(&exec::segment(&lines, &step.name, &prefixes)));
                println!(
                    "{}",
                    excerpt.unwrap_or_else(|| "(no output captured)".into())
                );
            }
            println!("── full log: {} ({} lines)", found.log, lines.len());
            Ok(0)
        }
        ReleaseAction::History { unit, limit } => {
            let history = context.store.releases_of(&unit, limit)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "unit": unit, "releases": history})
                    )?
                );
                return Ok(0);
            }
            for item in history {
                println!(
                    "{} {:<10} {:<9} {:<18} {} · {} · {} ago{}",
                    symbol(match item.state.as_str() {
                        "abandoned" => "cancelled",
                        other => other,
                    }),
                    item.kind,
                    item.state,
                    item.version,
                    &item.commit[..item.commit.len().min(10)],
                    item.agent,
                    age(now() as i64 - item.started),
                    if item.note.is_empty() {
                        String::new()
                    } else {
                        format!(" · {}", item.note)
                    }
                );
            }
            Ok(0)
        }
    }
}

fn wait_release(context: &Context, id: &str, json: bool) -> Result<crate::state::Release> {
    let mut seen: Vec<(String, String)> = Vec::new();
    loop {
        let current = release::reconcile(
            context,
            context.store.release(id)?.context("unknown release")?,
        )?;
        for step in context.store.release_steps(id)? {
            let key = (step.name.clone(), step.state.clone());
            if step.state != "pending" && !seen.contains(&key) {
                if !json || step.state != "running" {
                    let extra = if step.name == "version" && !current.version.is_empty() {
                        format!(" {}", current.version)
                    } else {
                        String::new()
                    };
                    eprintln!(
                        "  {} {}{}{}",
                        symbol(&step.state),
                        step.name,
                        extra,
                        step.seconds
                            .map(|seconds| format!(" {}", age(seconds)))
                            .unwrap_or_default()
                    );
                }
                seen.push(key);
            }
        }
        if current.finished() {
            return Ok(current);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn emit_release(context: &Context, item: &crate::state::Release, json: bool) -> Result<i32> {
    let steps = context.store.release_steps(&item.id)?;
    let next: Vec<String> = match item.state.as_str() {
        "failed" => vec![
            format!("citrus release log {}", item.id),
            format!("citrus release resume {} --approve", item.id),
        ],
        "unknown" => vec![
            format!("citrus release resume {} --approve", item.id),
            format!("citrus release abandon {} --reason \"…\"", item.id),
        ],
        "passed" => vec![format!("citrus release history {}", item.unit)],
        "queued" | "running" => vec![format!("citrus release wait {}", item.id)],
        _ => Vec::new(),
    };
    let code = match item.state.as_str() {
        "passed" | "queued" | "running" => 0,
        "failed" => 1,
        _ => 3,
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "release": item, "steps": steps, "next": next})
            )?
        );
        return Ok(code);
    }
    let elapsed = item.ended.unwrap_or(now() as i64) - item.started;
    println!(
        "{} {} {} {} {} · {} · commit {}",
        symbol(&item.state),
        item.kind,
        item.unit,
        if item.version.is_empty() {
            "(no version yet)"
        } else {
            &item.version
        },
        item.state,
        age(elapsed),
        &item.commit[..item.commit.len().min(10)]
    );
    if !item.note.is_empty() {
        println!("  {}", item.note);
    }
    for step in &steps {
        println!(
            "  {} {:<14} {}{}",
            symbol(&step.state),
            step.name,
            step.state,
            step.seconds
                .map(|seconds| format!(" · {}", age(seconds)))
                .unwrap_or_default()
        );
        if let Some(error) = &step.first_error {
            for line in error.lines().take(12) {
                println!("      {line}");
            }
        }
    }
    print_next(&next);
    Ok(code)
}

fn test_command(context: &Context, filter: Option<&str>, json: bool) -> Result<i32> {
    let started = std::time::Instant::now();
    let outcomes = citrus_lang_tests(&context.repo.root, filter)?;
    let failed = outcomes
        .iter()
        .filter(|outcome| outcome.failure.is_some())
        .count();
    if json {
        let tests: Vec<Value> = outcomes
            .iter()
            .map(|outcome| json!({"name": outcome.name, "ok": outcome.failure.is_none(), "error": outcome.failure}))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "tests": tests, "passed": outcomes.len() - failed, "failed": failed})
            )?
        );
    } else {
        for outcome in &outcomes {
            match &outcome.failure {
                None => println!("test {} ... ok", outcome.name),
                Some(failure) => {
                    println!("test {} ... FAILED", outcome.name);
                    print!("{failure}");
                }
            }
        }
        println!(
            "{} passed, {failed} failed in {:.1}s",
            outcomes.len() - failed,
            started.elapsed().as_secs_f64()
        );
    }
    if outcomes.is_empty() {
        if !json {
            eprintln!(
                "no #[test] functions{}",
                filter
                    .map(|f| format!(" matching `{f}`"))
                    .unwrap_or_default()
            );
        }
        return Ok(2);
    }
    Ok(i32::from(failed > 0))
}

fn fmt_command(context: &Context, check: bool, json: bool) -> Result<i32> {
    let root = &context.repo.root;
    let mut changed = Vec::new();
    for file in lang::config_files(root) {
        let path = root.join(&file);
        let text = std::fs::read_to_string(&path).with_context(|| format!("read {file}"))?;
        let formatted = lang::layout::format(&text);
        if formatted != text {
            if !check {
                std::fs::write(&path, &formatted).with_context(|| format!("write {file}"))?;
            }
            changed.push(file);
        }
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "check": check, "files": changed})
            )?
        );
    } else if changed.is_empty() {
        println!("✓ formatted");
    } else {
        for file in &changed {
            println!(
                "{} {file}",
                if check {
                    "✗ needs formatting:"
                } else {
                    "formatted"
                }
            );
        }
        if check {
            println!("next: citrus fmt");
        }
    }
    Ok(i32::from(check && !changed.is_empty()))
}

fn deps_command(
    context: &Context,
    target_dir: Option<std::path::PathBuf>,
    json: bool,
) -> Result<i32> {
    let root = &context.repo.root;
    let target = target_dir
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(Into::into))
        .map(|dir| {
            if dir.is_absolute() {
                dir
            } else {
                root.join(dir)
            }
        })
        .unwrap_or_else(|| root.join("target"));
    if !target.is_dir() {
        anyhow::bail!(
            "no Cargo target directory at {}: build first, or pass --target-dir",
            target.display()
        );
    }
    let report = depinfo::verify(root, &target, &context.manifest)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"schema": SCHEMA, "deps": report}))?
        );
    } else {
        for missing in &report.missing {
            println!(
                "✗ {} reads {} (crate {}), not among its inputs",
                missing.check, missing.file, missing.crate_root
            );
        }
        println!(
            "{} checks understood as Cargo, {} compiled crates, {} files outside their inputs",
            report.checks.len(),
            report.crates,
            report.missing.len()
        );
        if !report.missing.is_empty() {
            println!(
                "next: add them with #[reads(…)], or keep the check from reuse with #[cache(false)]"
            );
        }
    }
    Ok(i32::from(!report.missing.is_empty()))
}

fn citrus_lang_tests(
    root: &std::path::Path,
    filter: Option<&str>,
) -> Result<Vec<lang::TestOutcome>> {
    lang::run_tests(root, filter).map_err(|rendered| anyhow::anyhow!("{rendered}"))
}

fn check_command(context: &Context, json: bool) -> Result<i32> {
    let Some(project) = &context.project else {
        if json {
            println!("{}", json!({"schema": SCHEMA, "ok": true, "file": null}));
        } else {
            println!("no Citrus configuration here (citrus.ci or .citrus/*.ci); nothing to check");
        }
        return Ok(0);
    };
    // Context::open already failed on errors; here only warnings remain.
    let (_, sources) = model::load(&context.repo.root)
        .map_err(|rendered| anyhow::anyhow!("{rendered}"))?
        .context("the configuration disappeared")?;
    let mut project = project.clone();
    let dead = model::dead_globs(&project, &context.repo.root);
    project.warnings.extend(dead);
    let project = &project;
    if json {
        let warnings: Vec<Value> = project
            .warnings
            .iter()
            .map(|warning| {
                let (file, line, column) = sources.locate(warning.span);
                json!({"message": warning.message, "help": warning.help, "file": file, "line": line, "column": column})
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema": SCHEMA, "ok": true, "checks": project.checks.len(), "tasks": project.tasks.len(),
                "not_executed_yet": project.pending.iter().map(|(kind, name, _)| format!("{kind} {name}")).collect::<Vec<_>>(),
                "warnings": warnings,
            }))?
        );
        return Ok(0);
    }
    for warning in &project.warnings {
        print!(
            "{}",
            sources.render(warning).replacen("error:", "warning:", 1)
        );
    }
    println!(
        "✓ configuration: {} checks, {} tasks{}",
        project.checks.len(),
        project.tasks.len(),
        if project.warnings.is_empty() {
            String::new()
        } else {
            format!(", {} warnings", project.warnings.len())
        }
    );
    if !project.pending.is_empty() {
        let names: Vec<String> = project
            .pending
            .iter()
            .map(|(kind, name, _)| format!("{kind} {name}"))
            .collect();
        println!(
            "  declared, not executed from .ci yet: {}",
            names.join(", ")
        );
    }
    Ok(0)
}

fn do_command(context: &Context, task: Option<String>, json: bool) -> Result<i32> {
    let project = context
        .project
        .as_ref()
        .context("tasks live in citrus.ci; this repository has none")?;
    let Some(name) = task else {
        if json {
            let tasks: Vec<Value> = project.tasks.iter().map(|task| json!({"task": task.name, "about": task.about, "steps": task.steps.iter().map(|step| &step.label).collect::<Vec<_>>()})).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({"schema": SCHEMA, "tasks": tasks}))?
            );
        } else if project.tasks.is_empty() {
            println!("no tasks in citrus.ci");
        } else {
            for task in &project.tasks {
                println!("  {:<24} {}", task.name, task.about);
            }
            print_next(&["citrus do <task>".into()]);
        }
        return Ok(0);
    };
    let task = project
        .tasks
        .iter()
        .find(|task| task.name == name)
        .with_context(|| {
            let known: Vec<&str> = project
                .tasks
                .iter()
                .map(|task| task.name.as_str())
                .collect();
            match lang::suggest(&name, known.iter().copied()) {
                Some(close) => format!("no task {name}; did you mean {close}?"),
                None => format!("no task {name}; tasks: {}", known.join(", ")),
            }
        })?;
    let (_, sources) = model::load(&context.repo.root)
        .map_err(|rendered| anyhow::anyhow!("{rendered}"))?
        .context("citrus.ci disappeared")?;
    let started = std::time::Instant::now();
    for (index, step) in task.steps.iter().enumerate() {
        let (file, line, _) = sources.locate(step.span);
        eprintln!(
            "[{}/{}] {}  ({file}:{line})",
            index + 1,
            task.steps.len(),
            step.label
        );
        let code = model::execute(step, &context.repo.root, json)?;
        if code != 0 {
            if json {
                println!(
                    "{}",
                    json!({"schema": SCHEMA, "task": name, "state": "failed", "step": step.label, "source": format!("{file}:{line}"), "exit": code})
                );
            } else {
                println!(
                    "✗ {name} failed at step {} ({file}:{line}), exit {code}",
                    index + 1
                );
            }
            return Ok(1);
        }
    }
    if json {
        println!(
            "{}",
            json!({"schema": SCHEMA, "task": name, "state": "passed", "seconds": started.elapsed().as_secs()})
        );
    } else {
        println!("✓ {name} · {}", age(started.elapsed().as_secs() as i64));
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_error_is_not_a_moved_base() {
        let server = "remote: Internal Server Error\n ! [remote rejected]   HEAD -> main (Internal Server Error)";
        assert!(!integrate::base_moved(server));
        assert!(integrate::transient_push_error(server));
        let raced = " ! [rejected]        HEAD -> main (fetch first)";
        assert!(integrate::base_moved(raced));
        assert!(!integrate::transient_push_error(raced));
    }
}
