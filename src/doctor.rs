//! `citrus doctor`: is this repository set up so Citrus can be trusted?

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

pub fn diagnose(context: &mut Context) -> Vec<Finding> {
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
    match context.repo.paths() {
        Ok(files) => {
            for target in context.manifest.targets.values() {
                // A reused pass must cover what the check reads; selection
                // paths may name removed or future files.
                let unmatched: Vec<&String> = target
                    .inputs
                    .iter()
                    .chain(&target.extra_inputs)
                    .filter(|_| target.cache)
                    .filter(|pattern| !pattern.starts_with('!'))
                    .filter(|pattern| !pattern_matches_any(pattern, &files).unwrap_or(false))
                    .collect();
                if !unmatched.is_empty() {
                    let list = unmatched
                        .iter()
                        .map(|pattern| pattern.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    note(
                        &format!("check {}", target.name),
                        "fail",
                        format!("globs match no file: {list}"),
                    );
                }
                // `make("x")` steps need an `x:` rule.
                let rules = target.steps.iter().filter_map(|step| match &step.work {
                    crate::model::Work::Process { argv, .. }
                        if argv.first().map(String::as_str) == Some("make") =>
                    {
                        argv.iter()
                            .skip(1)
                            .find(|part| !part.starts_with('-') && !part.contains('='))
                            .cloned()
                    }
                    _ => None,
                });
                for rule in rules {
                    match defined(context, &rule) {
                        Ok(false) => note(
                            &format!("check {}", target.name),
                            "fail",
                            format!(
                                "no `{rule}:` rule in {}",
                                config.target_definitions.join(", ")
                            ),
                        ),
                        Err(error) => note(
                            &format!("check {}", target.name),
                            "warn",
                            format!("{error:#}"),
                        ),
                        Ok(true) => {}
                    }
                }
            }
        }
        Err(error) => note("files", "fail", format!("{error:#}")),
    }

    // Logs inside the tree change the source snapshot unless Git ignores them.
    if let Some(dir) = &config.log_dir {
        let probe = format!("{}/probe.log", dir.trim_end_matches('/'));
        let ignored = crate::repo::git()
            .arg("-C")
            .arg(&context.repo.root)
            .args(["check-ignore", "-q", "--no-index", &probe])
            .status()
            .is_ok_and(|status| status.success());
        if ignored {
            note("logs", "ok", format!("{dir} is ignored by Git"));
        } else {
            note(
                "logs",
                "fail",
                format!(
                    "{dir} is not ignored by Git: every run would change the snapshot; add it to .gitignore"
                ),
            );
        }
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

/// Whether a rule for `name` exists in the configured target definition files.
fn defined(context: &Context, name: &str) -> anyhow::Result<bool> {
    let patterns = &context.repo.config.target_definitions;
    if patterns.is_empty() {
        return Ok(true);
    }
    let files = context.repo.files()?;
    for path in &files {
        if !patterns.iter().any(|pattern| {
            pattern_matches_any(pattern, std::slice::from_ref(path)).unwrap_or(false)
        }) {
            continue;
        }
        let text = std::fs::read_to_string(context.repo.root.join(path)).unwrap_or_default();
        let found = text.lines().any(|line| {
            line.strip_prefix(name)
                .map(str::trim_start)
                .is_some_and(|rest| rest.starts_with(':') && !rest.starts_with(":="))
        });
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}
