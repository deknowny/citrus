//! What the Rust compiler actually read, from the dep-info files Cargo leaves
//! in the target directory, against the inputs Citrus inferred for a check.
//! A file a crate read that is not among the inputs of a check building that
//! crate would let the check be reused after a change it depends on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::manifest::{GlobList, Manifest};

/// A file a check's crates read that its inputs do not cover.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Missing {
    pub check: String,
    pub file: String,
    /// The dep-info file that lists it.
    pub crate_root: String,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct Report {
    /// Dep-info files read (the newest of each compiled crate).
    pub crates: usize,
    /// Checks whose commands Citrus understood as Cargo.
    pub checks: Vec<String>,
    pub missing: Vec<Missing>,
}

/// Compiled crates in `target`: for each, its root source and the repository
/// files it read, relative to `root`. Only the newest dep-info of a crate
/// name counts; files that no longer exist are skipped.
fn compiled(root: &Path, target: &Path) -> Result<Vec<(String, Vec<String>)>> {
    let mut newest: BTreeMap<String, (std::time::SystemTime, PathBuf)> = BTreeMap::new();
    for profile in std::fs::read_dir(target)?.flatten() {
        let deps = profile.path().join("deps");
        let Ok(entries) = std::fs::read_dir(&deps) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "d") {
                continue;
            }
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            let name = stem
                .rsplit_once('-')
                .map_or(stem.as_ref(), |(name, _)| name);
            let key = format!("{}:{name}", profile.file_name().to_string_lossy());
            let modified = entry.metadata()?.modified()?;
            if newest.get(&key).is_none_or(|(time, _)| modified > *time) {
                newest.insert(key, (modified, path));
            }
        }
    }
    let root = root.canonicalize()?;
    let mut crates = Vec::new();
    for (_, path) in newest.values() {
        let text = std::fs::read_to_string(path)?;
        // The first rule lists the crate's sources: `<output>: <src> <src> …`.
        let Some(line) = text.lines().find(|line| line.contains(": ")) else {
            continue;
        };
        let (_, sources) = line.split_once(": ").unwrap_or_default();
        let mut files = Vec::new();
        for source in split_escaped(sources) {
            let absolute = normalize(&if Path::new(&source).is_absolute() {
                PathBuf::from(&source)
            } else {
                root.join(&source)
            });
            let Ok(relative) = absolute.strip_prefix(&root) else {
                continue;
            };
            if relative.starts_with("target") || !absolute.exists() {
                continue;
            }
            files.push(relative.to_string_lossy().into_owned());
        }
        if let Some(first) = files.first().cloned() {
            crates.push((first, files));
        }
    }
    crates.extend(build_scripts(&root, target)?);
    Ok(crates)
}

/// What build scripts said they read (`cargo:rerun-if-changed=`), as crates
/// rooted at the package's `Cargo.toml`. Paths are relative to the package.
fn build_scripts(root: &Path, target: &Path) -> Result<Vec<(String, Vec<String>)>> {
    let listed = std::process::Command::new("git")
        .args(["ls-files", "-co", "--exclude-standard", "*Cargo.toml"])
        .current_dir(root)
        .output()?;
    let mut dirs: BTreeMap<String, String> = BTreeMap::new();
    for manifest in String::from_utf8_lossy(&listed.stdout).lines() {
        let Ok(text) = std::fs::read_to_string(root.join(manifest)) else {
            continue;
        };
        let name = text
            .parse::<toml::Table>()
            .ok()
            .and_then(|table| Some(table.get("package")?.get("name")?.as_str()?.to_owned()));
        if let Some(name) = name {
            let dir = manifest.trim_end_matches("Cargo.toml").trim_end_matches('/');
            dirs.insert(name.replace('-', "_"), dir.to_owned());
        }
    }
    let mut newest: BTreeMap<String, (std::time::SystemTime, PathBuf)> = BTreeMap::new();
    for profile in std::fs::read_dir(target)?.flatten() {
        let Ok(entries) = std::fs::read_dir(profile.path().join("build")) else {
            continue;
        };
        for entry in entries.flatten() {
            let output = entry.path().join("output");
            let Ok(meta) = std::fs::metadata(&output) else {
                continue;
            };
            let dir_name = entry.file_name().to_string_lossy().into_owned();
            let name = dir_name.rsplit_once('-').map_or(dir_name.as_str(), |(name, _)| name);
            let Some(package) = dirs.get(&name.replace('-', "_")) else {
                continue;
            };
            let modified = meta.modified()?;
            if newest.get(package).is_none_or(|(time, _)| modified > *time) {
                newest.insert(package.clone(), (modified, output));
            }
        }
    }
    let mut crates = Vec::new();
    for (dir, (_, output)) in newest {
        let text = std::fs::read_to_string(&output)?;
        let package_root = if dir.is_empty() { root.to_path_buf() } else { root.join(&dir) };
        let mut files = vec![if dir.is_empty() {
            "Cargo.toml".to_owned()
        } else {
            format!("{dir}/Cargo.toml")
        }];
        for line in text.lines() {
            let Some(path) = line
                .strip_prefix("cargo:rerun-if-changed=")
                .or_else(|| line.strip_prefix("cargo::rerun-if-changed="))
            else {
                continue;
            };
            let absolute = normalize(&package_root.join(path));
            let Ok(relative) = absolute.strip_prefix(root) else {
                continue;
            };
            if relative.starts_with("target") || !absolute.exists() {
                continue;
            }
            files.push(relative.to_string_lossy().into_owned());
        }
        crates.push((files[0].clone(), files));
    }
    Ok(crates)
}

/// `a/b/../c` as `a/c`, without touching the file system: dep-info names
/// files relative to the crate's sources.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Words of a Makefile rule, where `\ ` is a space inside a path.
fn split_escaped(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&' ') => {
                word.push(' ');
                chars.next();
            }
            ' ' => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// Each check Citrus understood as Cargo, against the crates in `target`
/// whose root source its inputs contain.
pub fn verify(root: &Path, target: &Path, manifest: &Manifest) -> Result<Report> {
    let crates = compiled(root, target)?;
    let mut report = Report {
        crates: crates.len(),
        ..Report::default()
    };
    for check in manifest.targets.values() {
        let understood = check
            .extensions
            .get("understood")
            .and_then(|value| value.as_array())
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str().is_some_and(|text| text.contains("cargo ")))
            });
        if !understood {
            continue;
        }
        report.checks.push(check.name.clone());
        let globs: Vec<String> = check
            .inputs
            .iter()
            .chain(&check.extra_inputs)
            .cloned()
            .collect();
        let inputs = GlobList::new(&globs)?;
        for (crate_root, files) in &crates {
            if !inputs.matches(crate_root) {
                continue;
            }
            for file in files {
                if !inputs.matches(file)
                    && !report
                        .missing
                        .iter()
                        .any(|missing| missing.check == check.name && &missing.file == file)
                {
                    report.missing.push(Missing {
                        check: check.name.clone(),
                        file: file.clone(),
                        crate_root: crate_root.clone(),
                    });
                }
            }
        }
    }
    report.checks.sort();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::split_escaped;

    #[test]
    fn dep_info_paths_keep_escaped_spaces() {
        assert_eq!(
            split_escaped("src/lib.rs  data/a\\ b.txt"),
            vec!["src/lib.rs", "data/a b.txt"]
        );
    }
}
