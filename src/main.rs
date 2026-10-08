//! Citrus: one command surface for checks, for people and AI agents alike.
//!
//! `citrus status` says what the current changes need and what is already
//! proven; `citrus run` runs only what is not; `citrus log <run>` shows the
//! first error instead of the whole log. Runs execute in a detached worker,
//! so closing the terminal does not lose them.
#![allow(clippy::print_stdout, clippy::print_stderr)] // A CLI: stdout is the interface.

mod apply;
mod config;
mod deploy;
mod dockerfile;
mod doctor;
mod exec;
mod integrate;
mod lang;
mod manifest;
mod model;
mod plan;
mod release;
mod repo;
mod report;
mod resources;
mod state;
mod tasks;
mod versions;

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
        /// Fast-forward the base branch to the result once checks pass.
        #[arg(long)]
        push: bool,
        /// Only merge and carry results; do not run checks.
        #[arg(long)]
        no_run: bool,
    },
    /// Every worktree as a task: branch, unmerged commits, runs, notes.
    Tasks {
        /// Include idle and merged worktrees.
        #[arg(long)]
        all: bool,
        #[arg(long)]
        base: Option<String>,
    },
    /// Say what this worktree's task is doing and what blocks it (shown in status and tasks).
    Task {
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
    Targets,
    /// Artifacts and their input keys (at HEAD, or `--at` another revision).
    Artifacts {
        #[arg(long)]
        at: Option<String>,
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
    /// Run a task declared in citrus.ci (no name: list the tasks).
    Do { task: Option<String> },
    /// Check that this repository is set up so Citrus can be trusted.
    Doctor,
    /// How Citrus has been used: runs, reuse, time saved.
    Stats {
        #[arg(long, default_value_t = 7)]
        days: u64,
    },
    #[command(hide = true)]
    Worker { run: String },
    #[command(hide = true)]
    ReleaseWorker { release: String },
    #[command(hide = true)]
    ApplyWorker { release: String },
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
    /// Past releases of a unit.
    History {
        unit: String,
        #[arg(long, default_value_t = 10)]
        limit: i64,
    },
}

fn main() {
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
        _ => {}
    }
    Some(command)
}

fn execute(command: Option<Command>, json: bool, profile: Option<String>) -> Result<i32> {
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
        } => {
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
        Command::Do { task } => do_command(&context, task, json),
        Command::Integrate { base, push, no_run } => {
            integrate_command(&mut context, base, push, !no_run, json)
        }
        Command::Tasks { all, base } => tasks_command(&context, all, base, json),
        Command::Task {
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
        Command::Targets => targets_command(&context, json),
        Command::Artifacts { at } => {
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
            approve,
            plan,
            unchecked,
            detach,
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
        Command::ReleaseWorker { release } => {
            release::work(&context, &release)?;
            Ok(0)
        }
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
    loop {
        let run = context.reconcile(context.store.run(id)?.context("unknown run")?)?;
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
    let mut warnings = Vec::new();
    let mut next = Vec::new();
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
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({"schema": SCHEMA, "days": days, "stats": stats}))?
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
        "running" | "queued" | "waiting" => "…",
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

/// The push lost a race: the base has commits this branch lacks.
fn base_moved(error: &str) -> bool {
    error.contains("non-fast-forward")
        || error.contains("fetch first")
        || error.contains("[rejected]")
}

/// A failure of the remote itself (GitHub 5xx, a dropped connection), worth retrying.
fn transient_push_error(error: &str) -> bool {
    !base_moved(error)
        && [
            "Internal Server Error",
            "502",
            "503",
            "504",
            "Connection reset",
            "timed out",
            "RPC failed",
            "unexpected disconnect",
        ]
        .iter()
        .any(|marker| error.contains(marker))
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
        let output = std::process::Command::new("git")
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
        if output.status.success() || !transient_push_error(&error) || delay > 16 {
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
    json: bool,
) -> Result<i32> {
    let base = base.unwrap_or_else(|| context.repo.config.plan.base.clone());
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
        let mut run_value = Value::Null;
        if run_checks {
            let run = context.start(&Request {
                targets: Vec::new(),
                base: None,
                mode: Mode::Auto,
                key: None,
                force: false,
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
        if !base_moved(&error) {
            anyhow::bail!("git push failed: {}", error.trim());
        }
        if !json {
            println!("{base} moved while checking (attempt {attempt}); integrating again");
        }
    }
    anyhow::bail!("{base} kept moving; run citrus integrate --push again")
}

fn tasks_command(context: &Context, all: bool, base: Option<String>, json: bool) -> Result<i32> {
    let base = base.unwrap_or_else(|| context.repo.config.plan.base.clone());
    let tasks = tasks::list(context, &base)?;
    let shown: Vec<&tasks::Task> = tasks
        .iter()
        .filter(|task| all || tasks::active(task))
        .collect();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"schema": SCHEMA, "base": base, "total": tasks.len(), "tasks": shown, "agreements": context.store.agreements()?})
            )?
        );
        return Ok(0);
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

fn targets_command(context: &Context, json: bool) -> Result<i32> {
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
        } => {
            if dry_run {
                let plan = release::dry_run(&mut *context, &unit, version.as_deref())?;
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
        ReleaseAction::Rollback { unit, approve } => {
            let started = release::start(
                context,
                &release::Start {
                    unit,
                    approve,
                    unchecked: true,
                    rollback: true,
                    version: None,
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
        assert!(!base_moved(server));
        assert!(transient_push_error(server));
        let raced = " ! [rejected]        HEAD -> main (fetch first)";
        assert!(base_moved(raced));
        assert!(!transient_push_error(raced));
    }
}
