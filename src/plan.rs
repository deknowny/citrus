//! Which checks the current changes need. A project with its own `planner`
//! keeps owning path → check routing (it reads the declared checks from
//! `CITRUS_CHECKS`); otherwise the declared checks owning the changed paths
//! are selected.

use std::process::Command;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::manifest::Manifest;
use crate::repo::Repo;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub status: String,
    pub files: usize,
    /// Make targets to run, in planner order.
    pub targets: Vec<String>,
    /// Non-runnable plan entries such as `no-heavy:docs`.
    pub notes: Vec<String>,
    /// (path, surfaces or `target:<name>`).
    pub mapped: Vec<(String, String)>,
    pub unmapped: Vec<String>,
    /// Groups the changed paths touch (built-in planner).
    pub groups: Vec<String>,
    /// Signals the project's signal command printed.
    pub signals: Vec<String>,
}

pub fn compute(repo: &Repo, manifest: &Manifest, base: Option<&str>) -> Result<Plan> {
    let mut plan = compute_all(repo, manifest, base)?;
    narrow(&mut plan, repo, manifest);
    Ok(plan)
}

/// Drops declared checks outside the current profile, and checks covered by
/// another check in the plan (docs/design/planner.md).
fn narrow(plan: &mut Plan, repo: &Repo, manifest: &Manifest) {
    let profile = repo.config.plan.profile.as_deref();
    let selected = plan.targets.clone();
    plan.targets.retain(|name| {
        let Some(target) = manifest.targets.get(name) else {
            return true;
        };
        let in_profile = target.profiles.is_empty()
            || profile.is_some_and(|profile| target.profiles.iter().any(|item| item == profile));
        let covered = target
            .covered_by
            .iter()
            .any(|cover| cover != name && selected.contains(cover));
        if !in_profile {
            plan.notes.push(format!("profile:{name}"));
        } else if covered {
            plan.notes.push(format!("covered:{name}"));
        }
        in_profile && !covered
    });
}

fn compute_all(repo: &Repo, manifest: &Manifest, base: Option<&str>) -> Result<Plan> {
    let config = &repo.config.plan;
    if config.command.is_empty() {
        return builtin(repo, manifest, base.unwrap_or(&config.base));
    }
    let extra = base
        .filter(|_| !config.base_arg.is_empty())
        .map(|base| config.base_arg.replace("{base}", base));
    let mut plan = external(repo, manifest, extra)?;
    let fork = fork_point(repo, base.unwrap_or(&config.base));
    add_edited(
        &mut plan,
        repo,
        manifest,
        &changed_paths(repo, &fork)?,
        &fork,
    );
    Ok(plan)
}

/// Checks whose declaration in `citrus.ci` changed join any plan, also one
/// made by the project's own planner, which does not read declarations.
fn add_edited(plan: &mut Plan, repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) {
    if !paths.iter().any(|path| manifest.files.contains(path)) {
        return;
    }
    for name in edited_checks(repo, manifest, before) {
        if !plan.targets.contains(&name) {
            plan.targets.push(name);
        }
    }
}

fn fork_point(repo: &Repo, base: &str) -> String {
    repo.git(&["merge-base", base, "HEAD"])
        .or_else(|_| repo.git(&["rev-parse", "HEAD"]))
        .unwrap_or_default()
}

/// Paths changed since `fork`, including untracked files.
fn changed_paths(repo: &Repo, fork: &str) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    if !fork.is_empty() {
        paths.extend(
            repo.git(&["diff", "--name-only", fork])?
                .lines()
                .map(str::to_owned),
        );
    }
    paths.extend(
        repo.git(&["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_owned),
    );
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Declared checks that are new or declared differently than at `before`.
fn edited_checks(repo: &Repo, manifest: &Manifest, before: &str) -> Vec<String> {
    let old = (!before.is_empty())
        .then(|| {
            crate::lang::compile::load_at(&repo.root, Some(before))
                .ok()
                .flatten()
        })
        .flatten()
        .and_then(|(project, sources)| Manifest::from_project(&project, &sources).ok())
        .unwrap_or_default();
    manifest
        .targets
        .values()
        .filter(|target| {
            old.targets
                .get(&target.name)
                .is_none_or(|previous| !previous.same_declaration(target))
        })
        .map(|target| target.name.clone())
        .collect()
}

/// The checks that changes to exactly `paths` would select; `before` is the
/// revision those changes start from (for manifest edits).
pub fn for_paths(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    let mut plan = for_paths_all(repo, manifest, paths, before)?;
    narrow(&mut plan, repo, manifest);
    Ok(plan)
}

fn for_paths_all(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    let config = &repo.config.plan;
    if config.command.is_empty() {
        return select(repo, manifest, paths.to_vec(), before);
    }
    if config.paths_arg.is_empty() {
        bail!("the planner takes no path list (`paths_var` of `planner` in citrus.ci)");
    }
    let dir = repo.state_dir().join("tmp");
    crate::repo::private_dir(&dir)?;
    let file = dir.join(format!(
        "paths-{}-{}",
        std::process::id(),
        crate::manifest::now()
    ));
    std::fs::write(
        &file,
        paths
            .iter()
            .map(|path| format!("{path}\n"))
            .collect::<String>(),
    )?;
    let result = external(
        repo,
        manifest,
        Some(
            config
                .paths_arg
                .replace("{file}", &file.display().to_string()),
        ),
    );
    let _ = std::fs::remove_file(&file);
    let mut plan = result?;
    add_edited(&mut plan, repo, manifest, paths, before);
    Ok(plan)
}

fn external(repo: &Repo, manifest: &Manifest, extra: Option<String>) -> Result<Plan> {
    let config = &repo.config.plan;
    let mut command = Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .current_dir(&repo.root)
        .env("CITRUS_CHECKS", manifest.export_file(repo)?)
        .env("CITRUS_PROFILE", config.profile.clone().unwrap_or_default());
    if let Some(profile) = config
        .profile
        .as_deref()
        .filter(|_| !config.profile_arg.is_empty())
    {
        command.arg(config.profile_arg.replace("{profile}", profile));
    }
    if let Some(extra) = extra {
        command.arg(extra);
    }
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() && !stdout.contains("TARGET\t") {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: Vec<&str> = stderr.lines().rev().take(5).collect();
        bail!(
            "planner {} failed: {}",
            config.command.join(" "),
            tail.into_iter().rev().collect::<Vec<_>>().join(" | ")
        );
    }
    Ok(parse(&stdout))
}

/// Declared targets that own a path changed since the fork point with `base`.
fn builtin(repo: &Repo, manifest: &Manifest, base: &str) -> Result<Plan> {
    let fork = fork_point(repo, base);
    let paths = changed_paths(repo, &fork)?;
    select(repo, manifest, paths, &fork)
}

/// Declared targets owning `paths`; new or edited declarations since `before` too.
fn select(repo: &Repo, manifest: &Manifest, paths: Vec<String>, fork: &str) -> Result<Plan> {
    let mut plan = Plan {
        files: paths.len(),
        ..Plan::default()
    };
    let edited: Vec<String> = paths
        .iter()
        .filter(|path| manifest.files.contains(path))
        .cloned()
        .collect();
    if !edited.is_empty() {
        // New or edited declarations are checked right away.
        let changed = edited_checks(repo, manifest, fork);
        let surfaces = if changed.is_empty() {
            "config".to_owned()
        } else {
            changed
                .iter()
                .map(|name| format!("target:{name}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        for path in &edited {
            plan.mapped.push((path.clone(), surfaces.clone()));
        }
        plan.targets.extend(changed);
    }
    // Which groups and checks the changed paths touch.
    let mut touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for path in &paths {
        if edited.contains(path) {
            continue;
        }
        let owners = manifest.owners(path);
        let groups: Vec<&str> = manifest
            .groups
            .iter()
            .filter(|group| group.owns(path))
            .map(|group| group.name.as_str())
            .collect();
        if owners.is_empty() && groups.is_empty() {
            plan.unmapped.push(path.clone());
            continue;
        }
        touched.extend(owners.iter().map(|name| (*name).to_owned()));
        touched.extend(groups.iter().map(|name| (*name).to_owned()));
        plan.mapped.push((
            path.clone(),
            owners
                .iter()
                .map(|owner| format!("target:{owner}"))
                .chain(groups.iter().map(|group| format!("group:{group}")))
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    let signals = signals(repo, manifest, &paths)?;
    // Owners whose condition holds, and checks selected by condition alone;
    // repeated until stable because conditions may name selected checks.
    let mut selected: Vec<String> = plan.targets.clone();
    for _ in 0..manifest.targets.len() + 1 {
        let mut added = false;
        let mut ordered: Vec<&crate::manifest::Target> = manifest.targets.values().collect();
        ordered.sort_by_key(|target| target.position);
        for target in ordered {
            if selected.contains(&target.name) {
                continue;
            }
            if !target.inputs.is_empty() && !touched.contains(&target.name) {
                continue;
            }
            let holds = target.when.as_ref().is_none_or(|when| {
                when.eval(&|fact| match fact {
                    crate::lang::compile::Cond::Touched(name) => touched.contains(name),
                    crate::lang::compile::Cond::Selected(name) => selected.contains(name),
                    crate::lang::compile::Cond::Signal(name) => signals.contains(name),
                    _ => false,
                })
            });
            if holds {
                selected.push(target.name.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    plan.targets = selected;
    plan.groups = manifest
        .groups
        .iter()
        .filter(|group| touched.contains(&group.name))
        .map(|group| group.name.clone())
        .collect();
    plan.signals = signals.clone();
    for group in &manifest.groups {
        if let Some(note) = group
            .note
            .as_ref()
            .filter(|_| touched.contains(&group.name))
            && !plan.notes.contains(note)
        {
            plan.notes.push(note.clone());
        }
    }
    plan.status = if plan.unmapped.is_empty() {
        "complete".into()
    } else {
        "partial".into()
    };
    Ok(plan)
}

/// Signals of the project's signal command for these paths (`SIGNAL <name>` lines).
fn signals(repo: &Repo, manifest: &Manifest, paths: &[String]) -> Result<Vec<String>> {
    let Some((program, args)) = manifest.signals.split_first() else {
        return Ok(Vec::new());
    };
    let dir = repo.state_dir().join("tmp");
    crate::repo::private_dir(&dir)?;
    let file = dir.join(format!(
        "signal-paths-{}-{}",
        std::process::id(),
        crate::manifest::now()
    ));
    std::fs::write(
        &file,
        paths
            .iter()
            .map(|path| format!("{path}\n"))
            .collect::<String>(),
    )?;
    let output = Command::new(program)
        .args(args)
        .current_dir(&repo.root)
        .env("CITRUS_PATHS", &file)
        .output();
    let _ = std::fs::remove_file(&file);
    let output = output?;
    if !output.status.success() {
        bail!(
            "signal command {} failed: {}",
            manifest.signals.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("SIGNAL "))
        .map(|name| name.trim().to_owned())
        .collect())
}

pub fn parse(output: &str) -> Plan {
    let mut plan = Plan::default();
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        match fields.as_slice() {
            ["PLAN", rest @ ..] => {
                for field in rest {
                    if let Some(value) = field.strip_prefix("status=") {
                        plan.status = value.to_owned();
                    } else if let Some(value) = field.strip_prefix("files=") {
                        plan.files = value.parse().unwrap_or_default();
                    }
                }
            }
            ["TARGET", entry] => match entry.split_once(':') {
                Some(("make", name)) if !plan.targets.iter().any(|known| known == name) => {
                    plan.targets.push(name.to_owned())
                }
                Some(("make", _)) => {}
                _ => plan.notes.push((*entry).to_owned()),
            },
            ["MAPPED", path, surfaces] => plan
                .mapped
                .push(((*path).to_owned(), (*surfaces).to_owned())),
            ["UNMAPPED", path] => plan.unmapped.push((*path).to_owned()),
            _ => {}
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_planner_output() {
        let plan = parse(
            "PLAN\tversion=40\tmode=fast\tstatus=complete\tfiles=2\n\
             MAPPED\tscripts/a.py\tpipeline\nMAPPED\tci/x\ttarget:test-x\nUNMAPPED\tweird\n\
             TARGET\tmake:test-pipeline-contract\nTARGET\tno-heavy:docs\nTARGET\tmake:test-pipeline-contract\n",
        );
        assert_eq!(plan.status, "complete");
        assert_eq!(plan.files, 2);
        assert_eq!(plan.targets, vec!["test-pipeline-contract"]);
        assert_eq!(plan.notes, vec!["no-heavy:docs"]);
        assert_eq!(plan.mapped.len(), 2);
        assert_eq!(plan.unmapped, vec!["weird"]);
    }
}
