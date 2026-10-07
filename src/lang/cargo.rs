//! `cargo.closure("package")`: the files a Cargo package is built from inside
//! the repository — its crate and every workspace crate it reaches through
//! path dependencies (normal, dev and build, any target), files outside those
//! crates pulled in by `include_str!`, `include_bytes!` and `sqlx::migrate!`,
//! the workspace `Cargo.toml` and `Cargo.lock`, and workspace settings
//! (`.cargo/`, toolchain, rustfmt and clippy files). Read from files only (no
//! `cargo` run), so it is cheap enough to evaluate on every command.

use std::collections::{BTreeMap, BTreeSet};

/// Reads a repository file (working tree or a committed revision).
pub trait Files {
    fn read(&self, path: &str) -> Option<String>;
    fn list(&self) -> &[String];
}

/// Repository-relative globs; an error names what could not be resolved.
pub fn closure(files: &dyn Files, package: &str) -> Result<Vec<String>, String> {
    let root: toml::Table = files
        .read("Cargo.toml")
        .ok_or("no Cargo.toml at the repository root")?
        .parse()
        .map_err(|error| format!("Cargo.toml: {error}"))?;
    let workspace = root.get("workspace").and_then(|value| value.as_table());
    // Workspace dependencies declared with a path: name → crate directory.
    let mut shared: BTreeMap<String, String> = BTreeMap::new();
    if let Some(table) = workspace
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|value| value.as_table())
    {
        for (name, spec) in table {
            if let Some(path) = spec.get("path").and_then(|value| value.as_str()) {
                shared.insert(name.clone(), normalize("", path));
            }
        }
    }
    // Every crate directory of the repository: package name → directory.
    let mut crates: BTreeMap<String, String> = BTreeMap::new();
    for path in files.list() {
        let Some(dir) = path.strip_suffix("Cargo.toml") else {
            continue;
        };
        if !(dir.is_empty() || dir.ends_with('/')) {
            continue;
        }
        let dir = dir.trim_end_matches('/');
        if let Some(name) = files
            .read(path)
            .and_then(|text| text.parse::<toml::Table>().ok())
            .and_then(|manifest| {
                manifest
                    .get("package")?
                    .get("name")?
                    .as_str()
                    .map(str::to_owned)
            })
        {
            crates.entry(name).or_insert_with(|| dir.to_owned());
        }
    }
    let start = crates
        .get(package)
        .ok_or_else(|| format!("no Cargo package `{package}` in this repository"))?
        .clone();
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut pending = vec![start];
    while let Some(dir) = pending.pop() {
        if !dirs.insert(dir.clone()) {
            continue;
        }
        let manifest_path = join(&dir, "Cargo.toml");
        let Some(manifest) = files
            .read(&manifest_path)
            .and_then(|text| text.parse::<toml::Table>().ok())
        else {
            return Err(format!("cannot read {manifest_path}"));
        };
        for (name, spec) in dependencies(&manifest) {
            let target = match spec.get("path").and_then(|value| value.as_str()) {
                Some(path) => Some(normalize(&dir, path)),
                None if spec.get("workspace").and_then(|value| value.as_bool()) == Some(true) => {
                    shared.get(&name).cloned()
                }
                None => None,
            };
            if let Some(target) = target
                && !dirs.contains(&target)
            {
                pending.push(target);
            }
        }
    }
    let mut globs: BTreeSet<String> = ["Cargo.toml".to_owned(), "Cargo.lock".to_owned()].into();
    // Workspace settings that change how every crate builds or is linted.
    for path in files.list() {
        let config = path.starts_with(".cargo/")
            || [
                "rust-toolchain",
                "rust-toolchain.toml",
                "rustfmt.toml",
                ".rustfmt.toml",
                "clippy.toml",
                ".clippy.toml",
            ]
            .contains(&path.as_str());
        if config {
            globs.insert(path.clone());
        }
    }
    for dir in &dirs {
        globs.insert(if dir.is_empty() {
            "**".into()
        } else {
            format!("{dir}/**")
        });
    }
    // Sources may pull in files from outside their crate.
    for dir in &dirs {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        for path in files
            .list()
            .iter()
            .filter(|path| path.starts_with(&prefix) && path.ends_with(".rs"))
        {
            let Some(text) = files.read(path) else {
                continue;
            };
            let source_dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
            for (kind, reference) in references(&text) {
                let base = if kind == "migrate" {
                    dir.as_str()
                } else {
                    source_dir
                };
                let resolved = normalize(base, &reference);
                if resolved.starts_with("../") || dirs.iter().any(|dir| within(&resolved, dir)) {
                    continue;
                }
                if files.list().iter().any(|file| file == &resolved) {
                    globs.insert(resolved);
                } else {
                    globs.insert(format!("{resolved}/**"));
                }
            }
        }
    }
    Ok(globs.into_iter().collect())
}

fn within(path: &str, dir: &str) -> bool {
    dir.is_empty() || path == dir || path.starts_with(&format!("{dir}/"))
}

fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_owned()
    } else {
        format!("{dir}/{path}")
    }
}

/// `base/path` with `.` and `..` resolved; stays relative to the repository.
fn normalize(base: &str, path: &str) -> String {
    let mut parts: Vec<&str> = base.split('/').filter(|part| !part.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return format!("../{path}");
                }
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}

/// Every dependency table of a manifest: (name, spec).
fn dependencies(manifest: &toml::Table) -> Vec<(String, toml::Value)> {
    let mut tables: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key).and_then(|value| value.as_table()) {
            tables.push(table);
        }
    }
    if let Some(targets) = manifest.get("target").and_then(|value| value.as_table()) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key).and_then(|value| value.as_table()) {
                    tables.push(table);
                }
            }
        }
    }
    tables
        .into_iter()
        .flat_map(|table| {
            table
                .iter()
                .map(|(name, spec)| (name.clone(), spec.clone()))
        })
        .collect()
}

/// `("include", path)` for `include_str!`/`include_bytes!` (relative to the
/// source file) and `("migrate", path)` for `migrate!` (relative to the crate).
fn references(text: &str) -> Vec<(&'static str, String)> {
    let mut found = Vec::new();
    for (marker, kind) in [
        ("include_str!(", "include"),
        ("include_bytes!(", "include"),
        ("migrate!(", "migrate"),
    ] {
        let mut rest = text;
        while let Some(index) = rest.find(marker) {
            rest = &rest[index + marker.len()..];
            let trimmed = rest.trim_start();
            if let Some(literal) = trimmed.strip_prefix('"')
                && let Some(end) = literal.find('"')
            {
                found.push((kind, literal[..end].to_owned()));
            } else if kind == "migrate" && trimmed.starts_with(')') {
                found.push((kind, "migrations".to_owned()));
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(BTreeMap<String, String>, Vec<String>);

    impl Files for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.0.get(path).cloned()
        }
        fn list(&self) -> &[String] {
            &self.1
        }
    }

    fn fake(files: &[(&str, &str)]) -> Fake {
        Fake(
            files
                .iter()
                .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
                .collect(),
            files.iter().map(|(path, _)| (*path).to_owned()).collect(),
        )
    }

    #[test]
    fn follows_path_dependencies_and_included_files() {
        let files = fake(&[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/*\"]\n[workspace.dependencies]\nshared = { path = \"crates/shared\" }\n",
            ),
            (
                "crates/app/Cargo.toml",
                "[package]\nname = \"app\"\n[dependencies]\nlib = { path = \"../lib\" }\nserde = \"1\"\n[target.'cfg(unix)'.dev-dependencies]\nshared = { workspace = true }\n",
            ),
            (
                "crates/app/src/main.rs",
                "const A: &str = include_str!(\n  \"../../../assets/a.txt\");\nfn main() { sqlx::migrate!(\"../../migrations/app\"); }\n",
            ),
            ("crates/lib/Cargo.toml", "[package]\nname = \"lib\"\n"),
            ("crates/lib/src/lib.rs", "include_bytes!(\"data.bin\");\n"),
            ("crates/lib/src/data.bin", ""),
            ("crates/shared/Cargo.toml", "[package]\nname = \"shared\"\n"),
            ("crates/other/Cargo.toml", "[package]\nname = \"other\"\n"),
            ("assets/a.txt", "a"),
            ("migrations/app/1.sql", ""),
            ("rustfmt.toml", ""),
        ]);
        assert_eq!(
            closure(&files, "app").unwrap(),
            vec![
                "Cargo.lock",
                "Cargo.toml",
                "assets/a.txt",
                "crates/app/**",
                "crates/lib/**",
                "crates/shared/**",
                "migrations/app/**",
                "rustfmt.toml",
            ]
        );
        assert!(
            closure(&files, "missing")
                .unwrap_err()
                .contains("no Cargo package")
        );
    }
}
