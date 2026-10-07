//! `citrus add`: declare a check instead of writing another wrapper script.

use std::fs;

use anyhow::{Context as _, Result, bail};

use crate::exec::Context;
use crate::manifest::{Manifest, pattern_matches_any};

#[derive(Debug)]
pub struct Declaration {
    pub target: String,
    pub description: Option<String>,
    pub inputs: Vec<String>,
    pub extra_inputs: Vec<String>,
    pub cache: bool,
    pub resources: Vec<String>,
}

/// Validates the declaration against the checkout and appends it to the manifest.
/// Returns the TOML block that was written.
pub fn add(context: &Context, declaration: &Declaration) -> Result<String> {
    let name = &declaration.target;
    if context.manifest.targets.contains_key(name) {
        bail!(
            "{name} is already declared in {}; edit its entry instead",
            context.repo.config.manifest
        );
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
    if context.project.is_some() {
        if !declaration.inputs.iter().all(|pattern| !pattern.is_empty()) {
            bail!("--inputs: empty pattern");
        }
        let block = render_ci(declaration);
        let path = context.repo.root.join("citrus.ci");
        let mut text = fs::read_to_string(&path)?;
        text = format!("{}\n\n{block}", text.trim_end());
        fs::write(&path, &text)?;
        if let Err(rendered) = crate::lang::compile::load(&context.repo.root) {
            let original = text
                .trim_end()
                .strip_suffix(block.trim_end())
                .unwrap_or(&text)
                .trim_end()
                .to_owned()
                + "\n";
            fs::write(&path, original)?;
            bail!("the new check would make citrus.ci invalid:\n{rendered}");
        }
        return Ok(block);
    }
    let block = render(declaration);
    let path = context.repo.manifest_path();
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut text = existing.trim_end().to_owned();
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(&block);
    // The result must still be a valid manifest before anything is written.
    Manifest::parse(&text).context("the new entry would make the manifest invalid")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, text)?;
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

fn render(declaration: &Declaration) -> String {
    let list = |items: &[String]| {
        if items.len() <= 2 {
            format!(
                "[{}]",
                items
                    .iter()
                    .map(|item| quote(item))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "[\n{}\n]",
                items
                    .iter()
                    .map(|item| format!("  {},", quote(item)))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        }
    };
    let mut block = format!("[targets.{}]\n", declaration.target);
    if let Some(description) = &declaration.description {
        block.push_str(&format!("description = {}\n", quote(description)));
    }
    if declaration.cache {
        block.push_str("cache = true\n");
    }
    if !declaration.resources.is_empty() {
        block.push_str(&format!("resources = {}\n", list(&declaration.resources)));
    }
    block.push_str(&format!("inputs = {}\n", list(&declaration.inputs)));
    if !declaration.extra_inputs.is_empty() {
        block.push_str(&format!(
            "extra_inputs = {}\n",
            list(&declaration.extra_inputs)
        ));
    }
    block
}

fn quote(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

/// A `check` block for citrus.ci; the run step is `make <target>`.
fn render_ci(declaration: &Declaration) -> String {
    let list = |items: &[String]| {
        format!(
            "[{}]",
            items
                .iter()
                .map(|item| format!("{item:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let mut block = format!("check {:?} {{\n", declaration.target);
    if let Some(description) = &declaration.description {
        block.push_str(&format!("  about = {description:?}\n"));
    }
    block.push_str(&format!("  owns  = {}\n", list(&declaration.inputs)));
    if !declaration.extra_inputs.is_empty() {
        block.push_str(&format!("  reads = {}\n", list(&declaration.extra_inputs)));
    }
    block.push_str(&format!("  run   = make({:?})\n", declaration.target));
    if declaration.cache {
        block.push_str("  cache = true\n");
    }
    if !declaration.resources.is_empty() {
        block.push_str(&format!("  resources = {}\n", list(&declaration.resources)));
    }
    block.push_str("}\n");
    block
}
