//! Release versions held by committed sources (`citrus version`).
//!
//! A task reserves a version for the names it publishes (its scope) before it
//! builds them. The version then belongs to that worktree and commit: another
//! source never publishes under it, and a retry gets the same version back.
//! The project's `free_version` hook says which versions are already taken
//! outside Citrus (in a registry, say).

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::manifest::now;
use crate::release::bump;
use crate::state::{Reservation, Store};

/// `1.2.3` or `1.2.3-suffix` → (major, minor, patch, suffix).
pub fn parse(version: &str) -> Option<(u64, u64, u64, &str)> {
    let (core, suffix) = version.split_once('-').unwrap_or((version, ""));
    if !suffix.is_empty()
        && !suffix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
    {
        return None;
    }
    let mut parts = core.split('.').map(|part| {
        (!part.is_empty() && part.len() <= 6 && part.chars().all(|c| c.is_ascii_digit()))
            .then(|| part.parse::<u64>().ok())
            .flatten()
    });
    let version = (parts.next()??, parts.next()??, parts.next()??, suffix);
    parts.next().is_none().then_some(version)
}

/// Sorted, unique names; each a lowercase registry-style name.
pub fn scope(names: &[String]) -> Result<Vec<String>> {
    let mut scope: Vec<String> = names
        .iter()
        .flat_map(|name| name.split(','))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    scope.sort();
    scope.dedup();
    ensure!(
        !scope.is_empty(),
        "name what the version is for: --scope image-a,image-b"
    );
    for name in &scope {
        ensure!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._/-".contains(c)),
            "scope names are lowercase registry names: {name}"
        );
    }
    Ok(scope)
}

pub struct Holder<'a> {
    pub owner: &'a str,
    pub source: &'a str,
    pub agent: &'a str,
}

/// The first version at or after `start` that nobody holds for any name of
/// `scope` and that `free_version` (when declared) reports free.
pub fn reserve(
    store: &Store,
    root: &Path,
    free_version: &[String],
    start: &str,
    scope: &[String],
    holder: &Holder,
) -> Result<String> {
    let (major, minor, _, suffix) = parse(start)
        .with_context(|| format!("not a version: {start} (major.minor.patch[-suffix])"))?;
    let held = |store: &Store| -> Result<Option<String>> {
        Ok(store
            .reservations(None)?
            .into_iter()
            .find(|r| {
                r.owner == holder.owner
                    && r.source == holder.source
                    && r.scope == scope
                    && r.start == start
            })
            .map(|r| r.version))
    };
    if let Some(version) = held(store)? {
        return Ok(version);
    }
    let mut candidate = start.to_owned();
    for _ in 0..1000 {
        if !free_version.is_empty() {
            let free = probe(root, free_version, &candidate, scope)?;
            match parse(&free) {
                Some((a, b, patch, s))
                    if (a, b, s) == (major, minor, suffix)
                        && patch >= parse(&candidate).map_or(0, |v| v.2) =>
                {
                    candidate = free;
                }
                _ => bail!(
                    "free_version answered {free:?} for {candidate}: expected {major}.{minor}.N{} at or after it",
                    if suffix.is_empty() {
                        String::new()
                    } else {
                        format!("-{suffix}")
                    }
                ),
            }
        }
        let reservation = Reservation {
            version: candidate.clone(),
            scope: scope.to_vec(),
            start: start.to_owned(),
            owner: holder.owner.to_owned(),
            source: holder.source.to_owned(),
            agent: holder.agent.to_owned(),
            created: now() as i64,
        };
        if store.try_reserve(&reservation)? {
            return Ok(candidate);
        }
        // A concurrent retry of this same request may have won the race.
        if let Some(version) = held(store)? {
            return Ok(version);
        }
        candidate = bump(&candidate).context("cannot bump the version")?;
    }
    bail!("no free version after {start} for {}", scope.join(","))
}

fn probe(root: &Path, command: &[String], version: &str, scope: &[String]) -> Result<String> {
    let (program, args) = command
        .split_first()
        .context("empty free_version command")?;
    let output = Command::new(program)
        .args(args)
        .current_dir(root)
        .env("CITRUS_VERSION", version)
        .env("CITRUS_SCOPE", scope.join(","))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("free_version: cannot run {program}"))?;
    ensure!(
        output.status.success(),
        "free_version failed ({}) for {version}",
        output.status
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .context("free_version printed nothing")?;
    Ok(line.rsplit('=').next().unwrap_or(line).trim().to_owned())
}

/// Fails when `version` is held by another worktree or commit for `scope`
/// (any name when it is not given). An unreserved version passes.
pub fn check(
    store: &Store,
    version: &str,
    scope: Option<&[String]>,
    owner: &str,
    source: &str,
) -> Result<()> {
    let overlaps = |a: &[String], b: &[String]| a.iter().any(|name| b.contains(name));
    let mut records: Vec<Reservation> = store
        .reservations(Some(version))?
        .into_iter()
        .filter(|r| scope.is_none_or(|scope| overlaps(scope, &r.scope)))
        .collect();
    if scope.is_none() {
        // Without a scope, what this source holds says which names matter.
        let own: Vec<String> = records
            .iter()
            .filter(|r| r.owner == owner && r.source == source)
            .flat_map(|r| r.scope.clone())
            .collect();
        if !own.is_empty() {
            records.retain(|r| overlaps(&own, &r.scope));
        }
    }
    if let Some(other) = records
        .iter()
        .find(|r| r.owner != owner || r.source != source)
    {
        bail!(
            "version {version} belongs to {} at {} ({}); reserve a new version for this source",
            other.owner,
            &other.source[..other.source.len().min(12)],
            other.scope.join(",")
        );
    }
    Ok(())
}

/// The commit `version` was last reserved for.
pub fn source(store: &Store, version: &str) -> Result<String> {
    store
        .reservations(Some(version))?
        .pop()
        .map(|r| r.source)
        .with_context(|| format!("no reservation for {version}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_a_product_suffix() {
        assert_eq!(parse("0.49.268-clyer"), Some((0, 49, 268, "clyer")));
        assert_eq!(parse("1.2.3"), Some((1, 2, 3, "")));
        for bad in ["1.2", "1.2.3.4", "v1.2.3", "1.2.3-Clyer", "1.2.1234567"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
