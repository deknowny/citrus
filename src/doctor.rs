//! `citrus doctor`: is this repository set up so Citrus can be trusted?

use std::process::Command;

use serde::Serialize;

use crate::exec::Context;
use crate::manifest::pattern_matches_any;
use crate::plan;

#[derive(Debug, Serialize)]
pub struct Finding {
    pub check: String,
    /// `ok`, `warn` or `fail`.
    pub status: &'static str,
    pub detail: String,
}

pub fn diagnose(context: &Context) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut note = |check: &str, status: &'static str, detail: String| {
        findings.push(Finding {
            check: check.to_owned(),
            status,
            detail,
        })
    };
    note(
        "citrus",
        if env!("CITRUS_COMMIT").ends_with("-dirty") || env!("CITRUS_COMMIT") == "unknown" {
            "warn"
        } else {
            "ok"
        },
        format!("{} ({})", env!("CARGO_PKG_VERSION"), env!("CITRUS_COMMIT")),
    );
    let config = &context.repo.config;
    match &context.project {
        Some(project) => note(
            "citrus.ci",
            "ok",
            format!(
                "{} checks, {} tasks, {} releases",
                context.manifest.targets.len(),
                project.tasks.len(),
                project.releases.len()
            ),
        ),
        None => note(
            "citrus.ci",
            "warn",
            "no citrus.ci: nothing is declared, only identical snapshots are reused (`citrus add` starts one)".into(),
        ),
    }
    if context.repo.root.join("citrus.toml").exists() {
        note(
            "citrus.toml",
            "warn",
            "Citrus no longer reads citrus.toml; describe the project in citrus.ci".into(),
        );
    }
    match context.repo.files() {
        Ok(files) => {
            for target in context.manifest.targets.values() {
                let unmatched: Vec<&String> = target
                    .inputs
                    .iter()
                    .chain(&target.extra_inputs)
                    .filter(|pattern| !pattern_matches_any(pattern, &files).unwrap_or(false))
                    .collect();
                if !unmatched.is_empty() {
                    let list = unmatched
                        .iter()
                        .map(|pattern| pattern.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    note(
                        &format!("target {}", target.name),
                        "fail",
                        format!("globs match no file: {list}"),
                    );
                }
                let defined = if target.steps.is_empty() {
                    crate::add::defined(context, &target.name)
                } else {
                    Ok(true)
                };
                match defined {
                    Ok(false) => note(
                        &format!("target {}", target.name),
                        "fail",
                        format!(
                            "no `{}:` rule in {}",
                            target.name,
                            config.target_definitions.join(", ")
                        ),
                    ),
                    Err(error) => note(
                        &format!("target {}", target.name),
                        "warn",
                        format!("{error:#}"),
                    ),
                    Ok(true) => {}
                }
            }
        }
        Err(error) => note("files", "fail", format!("{error:#}")),
    }

    // Logs inside the tree change the source snapshot unless Git ignores them.
    let probe = format!("{}/probe.log", config.log_dir.trim_end_matches('/'));
    let ignored = Command::new("git")
        .arg("-C")
        .arg(&context.repo.root)
        .args(["check-ignore", "-q", "--no-index", &probe])
        .status()
        .is_ok_and(|status| status.success());
    if ignored {
        note(
            "logs",
            "ok",
            format!("{} is ignored by Git", config.log_dir),
        );
    } else {
        note(
            "logs",
            "fail",
            format!(
                "{} is not ignored by Git: every run would change the snapshot; add it to .gitignore",
                config.log_dir
            ),
        );
    }

    match plan::compute(&context.repo, &context.manifest, None) {
        Ok(plan) => note(
            "planner",
            if plan.unmapped.is_empty() {
                "ok"
            } else {
                "warn"
            },
            format!(
                "{} changed files, {} checks selected{}",
                plan.files,
                plan.targets.len(),
                if plan.unmapped.is_empty() {
                    String::new()
                } else {
                    format!(", {} paths no check claims", plan.unmapped.len())
                }
            ),
        ),
        Err(error) => note("planner", "fail", format!("{error:#}")),
    }
    note(
        "remote",
        "ok",
        if config.run.remote.is_empty() {
            "no remote runner: everything runs locally".into()
        } else {
            format!("remote runner: {}", config.run.remote.join(" "))
        },
    );
    if !config.status.resources_command.is_empty() {
        note(
            "resources",
            "ok",
            format!(
                "snapshot from {} every {}s",
                config.status.resources_command.join(" "),
                config.status.refresh_seconds.max(10)
            ),
        );
    }
    findings
}
