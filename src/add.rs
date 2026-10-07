//! `citrus add`: declare a check instead of writing another wrapper script.

use std::fs;

use anyhow::{Result, bail};

use crate::exec::Context;
use crate::manifest::pattern_matches_any;

#[derive(Debug)]
pub struct Declaration {
    pub target: String,
    pub description: Option<String>,
    pub inputs: Vec<String>,
    pub extra_inputs: Vec<String>,
    pub cache: bool,
    pub resources: Vec<String>,
}

/// Validates the declaration against the checkout and appends a `check` block
/// to `citrus.ci` (created when missing). Returns the block that was written.
pub fn add(context: &Context, declaration: &Declaration) -> Result<String> {
    let name = &declaration.target;
    if context.manifest.targets.contains_key(name) {
        bail!("{name} is already declared in citrus.ci; edit its block instead");
    }
    if declaration.inputs.is_empty() {
        bail!("--inputs: name the files this check owns (globs)");
    }
    let files = context.repo.files()?;
    for pattern in declaration.inputs.iter().chain(&declaration.extra_inputs) {
        if !pattern_matches_any(pattern, &files)? {
            bail!("{pattern} matches no file in this checkout");
        }
    }
    let requires_resources = !context.manifest.targets.is_empty()
        && context
            .manifest
            .targets
            .values()
            .all(|target| !target.resources.is_empty());
    if declaration.resources.is_empty() && requires_resources {
        let known: std::collections::BTreeSet<&str> = context
            .manifest
            .targets
            .values()
            .flat_map(|target| target.resources.iter().map(String::as_str))
            .collect();
        bail!(
            "every target here names its resources; pass --resources (used so far: {})",
            known.into_iter().collect::<Vec<_>>().join(", ")
        );
    }
    if !defined(context, name)? {
        bail!(
            "no `{name}:` rule in {}; add the command first",
            context.repo.config.target_definitions.join(", ")
        );
    }
    let block = render_ci(declaration);
    let path = context.repo.root.join("citrus.ci");
    let created = !path.exists();
    let existing = fs::read_to_string(&path).unwrap_or_else(|_| "citrus 1\n".into());
    fs::write(&path, format!("{}\n\n{block}", existing.trim_end()))?;
    // The result must still be a valid project; otherwise nothing changes.
    if let Err(rendered) = crate::lang::compile::load(&context.repo.root) {
        if created {
            fs::remove_file(&path)?;
        } else {
            fs::write(&path, existing)?;
        }
        bail!("the new check would make citrus.ci invalid:\n{rendered}");
    }
    Ok(block)
}

/// Whether a rule for `name` exists in the configured target definition files.
pub fn defined(context: &Context, name: &str) -> Result<bool> {
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
        let text = fs::read_to_string(context.repo.root.join(path)).unwrap_or_default();
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

/// A `.ci` string literal: braces are literal, not interpolation.
pub fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// A `check` block for citrus.ci; the run step is `make <target>`.
fn render_ci(declaration: &Declaration) -> String {
    let list = |items: &[String]| {
        format!(
            "[{}]",
            items
                .iter()
                .map(|item| quote(item))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut block = format!("check {} {{\n", quote(&declaration.target));
    if let Some(description) = &declaration.description {
        block.push_str(&format!("  about = {}\n", quote(description)));
    }
    block.push_str(&format!("  owns = {}\n", list(&declaration.inputs)));
    if !declaration.extra_inputs.is_empty() {
        block.push_str(&format!("  reads = {}\n", list(&declaration.extra_inputs)));
    }
    block.push_str(&format!("  run = make({})\n", quote(&declaration.target)));
    if declaration.cache {
        block.push_str("  cache = true\n");
    }
    if !declaration.resources.is_empty() {
        block.push_str(&format!("  resources = {}\n", list(&declaration.resources)));
    }
    block.push_str("}\n");
    block
}
