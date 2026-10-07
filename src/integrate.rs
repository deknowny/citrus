//! `citrus integrate`: bring the base branch in and keep what is still proven.
//!
//! After merging, a check that passed on the previous sources stays proven when
//! the incoming changes do not select it (asked from the planner with exactly
//! the incoming paths). Declared checks need nothing special: their input
//! fingerprint already tells whether incoming changes touched them.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::exec::Context;
use crate::plan;

#[derive(Debug, Serialize)]
pub struct Integration {
    pub base: String,
    /// `up_to_date`, `merged` or `conflict`.
    pub outcome: String,
    pub incoming_commits: usize,
    pub incoming_paths: Vec<String>,
    pub conflicts: Vec<String>,
    /// Checks whose earlier PASS still holds after the merge.
    pub carried: Vec<String>,
    /// Checks the incoming changes select again.
    pub reselected: Vec<String>,
    /// Why nothing could be carried, when that is the case.
    pub carry_note: String,
}

/// `remote/branch` → (Some(remote), branch) when `remote` is a configured remote.
pub fn split_base(context: &Context, base: &str) -> (Option<String>, String) {
    let remotes = context.repo.git(&["remote"]).unwrap_or_default();
    if let Some((remote, branch)) = base.split_once('/')
        && remotes.lines().any(|known| known == remote)
    {
        return (Some(remote.to_owned()), branch.to_owned());
    }
    (None, base.to_owned())
}

pub fn integrate(context: &Context, base: &str) -> Result<Integration> {
    let repo = &context.repo;
    if !repo
        .git(&["status", "--porcelain", "--untracked-files=no"])?
        .is_empty()
    {
        bail!("commit or stash tracked changes first: integration merges into HEAD");
    }
    let (remote, branch) = split_base(context, base);
    if let Some(remote) = &remote {
        repo.git(&["fetch", "--quiet", remote, &branch])?;
    }
    let before = repo.git(&["rev-parse", "HEAD"])?;
    let mut result = Integration {
        base: base.to_owned(),
        outcome: "up_to_date".into(),
        incoming_commits: 0,
        incoming_paths: Vec::new(),
        conflicts: Vec::new(),
        carried: Vec::new(),
        reselected: Vec::new(),
        carry_note: String::new(),
    };
    result.incoming_commits = repo
        .git(&["rev-list", "--count", &format!("HEAD..{base}")])?
        .parse()
        .unwrap_or(0);
    if result.incoming_commits == 0 {
        return Ok(result);
    }
    let snapshot_before = repo.snapshot()?;
    let merge = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo.root)
        .args(["merge", "--no-edit", "--quiet", base])
        .output()?;
    if !merge.status.success() {
        result.outcome = "conflict".into();
        result.conflicts = repo
            .git(&["diff", "--name-only", "--diff-filter=U"])?
            .lines()
            .map(str::to_owned)
            .collect();
        if result.conflicts.is_empty() {
            bail!(
                "git merge {base} failed: {}",
                String::from_utf8_lossy(&merge.stderr).trim()
            );
        }
        return Ok(result);
    }
    result.outcome = "merged".into();
    result.incoming_paths = repo
        .git(&["diff", "--name-only", &before, "HEAD"])?
        .lines()
        .map(str::to_owned)
        .collect();
    carry(context, &mut result, &snapshot_before, &before)?;
    Ok(result)
}

/// Carry earlier passes of checks the incoming changes do not select.
pub fn carry(
    context: &Context,
    result: &mut Integration,
    snapshot_before: &str,
    before: &str,
) -> Result<()> {
    let selected = match plan::for_paths(
        &context.repo,
        &context.manifest,
        &result.incoming_paths,
        before,
    ) {
        Ok(plan) if plan.unmapped.is_empty() => plan.targets,
        Ok(plan) => {
            result.carry_note = format!(
                "{} incoming paths are claimed by no check, so nothing is carried over: {}",
                plan.unmapped.len(),
                plan.unmapped
                    .iter()
                    .take(3)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Ok(());
        }
        Err(error) => {
            result.carry_note = format!("nothing carried over: {error:#}");
            return Ok(());
        }
    };
    let snapshot_after = context.repo.snapshot()?;
    let current = plan::compute(&context.repo, &context.manifest, None)?;
    for target in &current.targets {
        // Declared cached checks follow their input fingerprint; nothing to carry.
        if context
            .manifest
            .targets
            .get(target)
            .is_some_and(|entry| entry.cache)
        {
            continue;
        }
        if selected.contains(target) {
            result.reselected.push(target.clone());
            continue;
        }
        let Some(evidence) = context
            .store
            .evidence(target, "snapshot", snapshot_before)?
        else {
            continue;
        };
        if evidence.result != "passed" {
            continue;
        }
        let detail = format!(
            "carried from {snapshot_before} over {} incoming paths",
            result.incoming_paths.len()
        );
        context.store.put_evidence(
            target,
            "snapshot",
            &snapshot_after,
            "passed",
            &evidence.run,
            &detail,
        )?;
        result.carried.push(target.clone());
    }
    Ok(())
}
