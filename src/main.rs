//! Citrus: one command surface for checks, for people and AI agents alike.
//!
//! `citrus status` says what the current changes need and what is already
//! proven; `citrus run` runs only what is not; `citrus log <run>` shows the
//! first error instead of the whole log. Runs execute in a detached worker,
//! so closing the terminal does not lose them.
#![allow(clippy::print_stdout, clippy::print_stderr)] // A CLI: stdout is the interface.

mod add;
mod config;
mod doctor;
mod exec;
mod manifest;
mod plan;
mod repo;
mod report;
mod resources;
mod state;

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
    version,
    about = "Plan, run and explain checks; reuse results that are already proven."
)]
struct Cli {
    /// JSON output (default when stdout is not a terminal).
    #[arg(long, global = true)]
    json: bool,
    /// Text output even when stdout is not a terminal.
    #[arg(long, global = true, conflicts_with = "json")]
    text: bool,
    #[command(subcommand)]
    command: Command,
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
    /// Declare a check in the manifest instead of writing a wrapper script.
    Add {
        /// The command that runs the check (a Make target by default).
        target: String,
        /// Files the check owns: changing them selects it (globs, repeatable).
        #[arg(long = "inputs", num_args = 1.., required = true)]
        inputs: Vec<String>,
        /// Files it only reads; they invalidate reuse but do not select it.
        #[arg(long = "extra", num_args = 1..)]
        extra: Vec<String>,
        /// Reuse a PASS while these inputs are unchanged (list everything it reads).
        #[arg(long)]
        cache: bool,
        /// Resource classes for the project's scheduler.
        #[arg(long, num_args = 1..)]
        resources: Vec<String>,
        #[arg(long)]
        description: Option<String>,
    },
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
    RefreshResources,
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json || (!cli.text && !std::io::stdout().is_terminal());
    match execute(cli.command, json) {
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

fn execute(command: Command, json: bool) -> Result<i32> {
    let context = Context::open()?;
    match command {
        Command::Status { base } => status(&context, base.as_deref(), json),
        Command::Plan { base } => plan_command(&context, base.as_deref(), json),
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
        Command::Why { target, base } => why(&context, &target, base.as_deref(), json),
        Command::Cancel { run } => {
            let run = context.reconcile(context.store.resolve(&run, &context.worktree())?)?;
            if !run.finished() {
                context.cancel(&run)?;
            }
            let run = context.store.run(&run.id)?.context("run disappeared")?;
            emit_run(&context, &run, json)
        }
        Command::Add {
            target,
            inputs,
            extra,
            cache,
            resources,
            description,
        } => {
            let declaration = add::Declaration {
                target,
                description,
                inputs,
                extra_inputs: extra,
                cache,
                resources,
            };
            let block = add::add(&context, &declaration)?;
            let next = vec![
                format!("citrus run {}", declaration.target),
                "citrus plan".to_owned(),
            ];
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &json!({"schema": SCHEMA, "manifest": context.repo.config.manifest, "added": block, "next": next})
                    )?
                );
            } else {
                println!("added to {}:\n\n{block}", context.repo.config.manifest);
                print_next(&next);
            }
            Ok(0)
        }
        Command::Stats { days } => stats(&context, days, json),
        Command::Doctor => {
            let findings = doctor::diagnose(&context);
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
fn follow(context: &Context, id: &str, json: bool) -> Result<i32> {
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut note = String::new();
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
        if run.note != note && !run.note.is_empty() {
            eprintln!("  … {}", run.note);
        }
        note = run.note.clone();
        if run.finished() {
            return emit_run(context, &run, json);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
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

fn status(context: &Context, base: Option<&str>, json: bool) -> Result<i32> {
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

fn plan_command(context: &Context, base: Option<&str>, json: bool) -> Result<i32> {
    let plan = plan::compute(&context.repo, &context.manifest, base)?;
    let files = context.repo.files()?;
    let snapshot = context.repo.snapshot()?;
    let decisions = plan
        .targets
        .iter()
        .map(|name| context.decide(&files, &snapshot, name, false))
        .collect::<Result<Vec<_>>>()?;
    if json {
        let value = json!({"schema": SCHEMA, "plan": plan, "targets": decisions});
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
        let excerpt = target
            .first_error
            .clone()
            .or_else(|| report::first_error(&exec::segment(&lines, &target.target, prefixes)));
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

fn why(context: &Context, target: &str, base: Option<&str>, json: bool) -> Result<i32> {
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
        None => println!(
            "  not declared in {}: reused only for the identical source snapshot",
            context.repo.config.manifest
        ),
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
        "passed" => "✓",
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
