//! Shared execution resources (builders, runners) as the project reports them.
//!
//! The project's own status command is often slow (it asks remote machines), so
//! `status` shows the last snapshot immediately and refreshes it in the background.

use std::collections::BTreeMap;
use std::fs;
use std::process::Command;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::exec::Context;
use crate::manifest::now;

const SNAPSHOT: &str = "resources";
const REFRESHING: &str = "resources_refresh";
/// A refresh that has not finished after this long is considered dead.
const REFRESH_TIMEOUT: i64 = 120;

#[derive(Debug, Clone, Serialize)]
pub struct Resources {
    pub items: Vec<BTreeMap<String, String>>,
    /// Seconds since the snapshot was taken; None when there is none yet.
    pub age: Option<i64>,
    pub refreshing: bool,
}

/// Last snapshot; starts a background refresh when it is missing or stale.
pub fn observe(context: &Context) -> Result<Option<Resources>> {
    let config = &context.repo.config.status;
    if config.resources_command.is_empty() {
        return Ok(None);
    }
    let current = now() as i64;
    let snapshot = context.store.fact(SNAPSHOT)?;
    let age = snapshot.as_ref().map(|(_, updated)| current - updated);
    let refreshing = context.store.fact(REFRESHING)?.is_some_and(|(_, started)| {
        current - started < REFRESH_TIMEOUT
            && snapshot
                .as_ref()
                .is_none_or(|(_, updated)| *updated < started)
    });
    let stale = age.is_none_or(|age| age > config.refresh_seconds.max(10) as i64);
    if stale && !refreshing {
        context.store.set_fact(REFRESHING, "")?;
        context.spawn_detached(&["refresh-resources"], fs::File::create("/dev/null")?)?;
    }
    let items = snapshot
        .map(|(value, _)| serde_json::from_str(&value).unwrap_or_default())
        .unwrap_or_default();
    Ok(Some(Resources {
        items,
        age,
        refreshing: refreshing || stale,
    }))
}

/// Runs the project's status command and stores the resource lines.
pub fn refresh(context: &Context) -> Result<()> {
    let config = &context.repo.config.status;
    let Some((program, args)) = config.resources_command.split_first() else {
        return Ok(());
    };
    let output = Command::new(program)
        .args(args)
        .current_dir(&context.repo.root)
        .output()?;
    if !output.status.success() && output.stdout.is_empty() {
        bail!("{} failed", config.resources_command.join(" "));
    }
    let items: Vec<BTreeMap<String, String>> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix(config.resource_prefix.as_str()))
        .map(parse_fields)
        .collect();
    context
        .store
        .set_fact(SNAPSHOT, &serde_json::to_string(&items)?)
}

/// `key=value` pairs; anything else is kept under `text`.
pub fn parse_fields(line: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut text = Vec::new();
    for part in line.split_whitespace() {
        match part.split_once('=') {
            Some((key, value)) if !key.is_empty() => {
                fields.insert(key.to_owned(), value.to_owned());
            }
            _ => text.push(part),
        }
    }
    if !text.is_empty() {
        fields.insert("text".into(), text.join(" "));
    }
    fields
}

/// One line for people: the usual builder fields when present, the raw text otherwise.
pub fn describe(item: &BTreeMap<String, String>) -> String {
    let field = |key: &str| {
        item.get(key)
            .filter(|value| !matches!(value.as_str(), "none" | "unknown" | ""))
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(host) = field("host").or(field("name")) {
        parts.push(host.rsplit('@').next().unwrap_or(host).to_owned());
    }
    for key in ["state", "operation", "owner"] {
        if let Some(value) = field(key) {
            parts.push(value.clone());
        }
    }
    if let Some(seconds) = field("elapsed_seconds").and_then(|value| value.parse::<i64>().ok()) {
        parts.push(crate::report::age(seconds));
    }
    if parts.is_empty() {
        return item
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_lines_become_short_descriptions() {
        let item = parse_fields(
            "host=ci@builder-1.example state=busy operation=remote-test:changed owner=agent-x pid=1 elapsed_seconds=116 reason=none",
        );
        assert_eq!(
            describe(&item),
            "builder-1.example · busy · remote-test:changed · agent-x · 1m"
        );
        let free =
            parse_fields("host=local state=free operation=none owner=none elapsed_seconds=unknown");
        assert_eq!(describe(&free), "local · free");
        assert_eq!(describe(&parse_fields("just words")), "text=just words");
    }
}
