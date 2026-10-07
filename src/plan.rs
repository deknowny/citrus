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
}

pub fn compute(repo: &Repo, manifest: &Manifest, base: Option<&str>) -> Result<Plan> {
    let config = &repo.config.plan;
    if config.command.is_empty() {
        return builtin(repo, manifest, base.unwrap_or(&config.base));
    }
    let extra = base
        .filter(|_| !config.base_arg.is_empty())
        .map(|base| config.base_arg.replace("{base}", base));
    external(repo, manifest, extra)
}

/// The checks that changes to exactly `paths` would select; `before` is the
/// revision those changes start from (for manifest edits).
pub fn for_paths(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    let config = &repo.config.plan;
    if config.command.is_empty() {
        return Ok(select(repo, manifest, paths.to_vec(), before));
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
    result
}

fn external(repo: &Repo, manifest: &Manifest, extra: Option<String>) -> Result<Plan> {
    let config = &repo.config.plan;
    let mut command = Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .current_dir(&repo.root)
        .env("CITRUS_CHECKS", manifest.export_file(repo)?);
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
    let fork = repo
        .git(&["merge-base", base, "HEAD"])
        .or_else(|_| repo.git(&["rev-parse", "HEAD"]))
        .unwrap_or_default();
    let mut paths: Vec<String> = Vec::new();
    if !fork.is_empty() {
        paths.extend(
            repo.git(&["diff", "--name-only", &fork])?
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
    Ok(select(repo, manifest, paths, &fork))
}

/// Declared targets owning `paths`; new or edited declarations since `before` too.
fn select(repo: &Repo, manifest: &Manifest, paths: Vec<String>, fork: &str) -> Plan {
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
        let before = (!fork.is_empty())
            .then(|| {
                crate::lang::compile::load_at(&repo.root, Some(fork))
                    .ok()
                    .flatten()
            })
            .flatten()
            .and_then(|(project, sources)| Manifest::from_project(&project, &sources).ok())
            .unwrap_or_default();
        let changed: Vec<String> = manifest
            .targets
            .values()
            .filter(|target| {
                before
                    .targets
                    .get(&target.name)
                    .is_none_or(|old| !old.same_declaration(target))
            })
            .map(|target| target.name.clone())
            .collect();
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
    for path in paths {
        if edited.contains(&path) {
            continue;
        }
        let owners = manifest.owners(&path);
        if owners.is_empty() {
            plan.unmapped.push(path);
            continue;
        }
        for owner in &owners {
            if !plan.targets.iter().any(|known| known == owner) {
                plan.targets.push((*owner).to_owned());
            }
        }
        plan.mapped.push((
            path,
            owners
                .iter()
                .map(|owner| format!("target:{owner}"))
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    plan.status = if plan.unmapped.is_empty() {
        "complete".into()
    } else {
        "partial".into()
    };
    plan
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
