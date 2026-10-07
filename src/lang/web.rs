//! `next("@acme/shop")` and `package("@acme/ui")`: the files a pnpm workspace
//! package is built from — its directory (minus the workspace packages nested
//! in it), the workspace packages it depends on (`workspace:` versions,
//! followed recursively), and the workspace's manifest, lockfile and root
//! package.json. `next` also checks the package is a Next.js app.

use std::collections::{BTreeMap, BTreeSet};

use super::cargo::Files;

/// The workspace: its directory, and package name → directory.
struct Workspace {
    root: String,
    packages: BTreeMap<String, String>,
    dirs: Vec<String>,
}

/// What a name must be for the function that asked for it.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// Any workspace package.
    Package,
    /// A package that depends on `next`.
    Next,
}

fn workspace(files: &dyn Files) -> Result<Workspace, String> {
    let manifest = files
        .list()
        .iter()
        .filter(|path| path.ends_with("pnpm-workspace.yaml"))
        .min_by_key(|path| path.len())
        .ok_or("no pnpm-workspace.yaml in this repository")?
        .clone();
    let root = manifest
        .strip_suffix("pnpm-workspace.yaml")
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_owned();
    let text = files.read(&manifest).unwrap_or_default();
    // `packages:` followed by `- "glob"` lines.
    let mut globs = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.starts_with('-') {
            inside = line.trim_end() == "packages:";
            continue;
        }
        if inside && let Some(item) = line.trim().strip_prefix('-') {
            globs.push(item.trim().trim_matches(['"', '\'']).to_owned());
        }
    }
    let join = |path: &str| {
        if root.is_empty() {
            path.to_owned()
        } else if path == "." {
            root.clone()
        } else {
            format!("{root}/{path}")
        }
    };
    let mut packages = BTreeMap::new();
    let mut dirs = Vec::new();
    for path in files.list() {
        let Some(dir) = path
            .strip_suffix("/package.json")
            .or_else(|| (path == "package.json").then_some(""))
        else {
            continue;
        };
        let relative = if root.is_empty() {
            dir.to_owned()
        } else if dir == root {
            ".".to_owned()
        } else if let Some(rest) = dir.strip_prefix(&format!("{root}/")) {
            rest.to_owned()
        } else {
            continue;
        };
        let listed = globs.iter().any(|glob| {
            glob == &relative
                || glob.strip_suffix("/*").is_some_and(|parent| {
                    relative
                        .rsplit_once('/')
                        .is_some_and(|(up, _)| up == parent)
                })
        });
        if !listed {
            continue;
        }
        let dir = join(&relative);
        let manifest: serde_json::Value = files
            .read(path)
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if let Some(name) = manifest.get("name").and_then(|name| name.as_str()) {
            packages.insert(name.to_owned(), dir.clone());
        }
        dirs.push(dir);
    }
    Ok(Workspace {
        root,
        packages,
        dirs,
    })
}

fn manifest(files: &dyn Files, dir: &str) -> serde_json::Value {
    files
        .read(&format!("{dir}/package.json"))
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn depends_on(manifest: &serde_json::Value, name: &str) -> bool {
    ["dependencies", "devDependencies"].iter().any(|section| {
        manifest
            .get(section)
            .and_then(|deps| deps.get(name))
            .is_some()
    })
}

/// Package names as in their package.json; `*` in a name matches several.
pub fn closure(files: &dyn Files, names: &[String], kind: Kind) -> Result<Vec<String>, String> {
    let workspace = workspace(files)?;
    let file = |name: &str| {
        if workspace.root.is_empty() {
            name.to_owned()
        } else {
            format!("{}/{name}", workspace.root)
        }
    };
    let mut globs: BTreeSet<String> = ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"]
        .iter()
        .map(|name| file(name))
        .collect();
    let mut pending: Vec<String> = Vec::new();
    for name in names {
        let matched: Vec<(&String, &String)> = if name.contains('*') {
            let pattern = crate::manifest::GlobList::new(std::slice::from_ref(name))
                .map_err(|error| error.to_string())?;
            workspace
                .packages
                .iter()
                .filter(|(package, _)| pattern.matches(package))
                .collect()
        } else {
            workspace.packages.get_key_value(name).into_iter().collect()
        };
        if matched.is_empty() {
            let mut message = format!("no workspace package `{name}`");
            // `vpn` for `@acme/vpn`: the scope is part of the name.
            let scoped = workspace
                .packages
                .keys()
                .find(|package| package.ends_with(&format!("/{name}")))
                .cloned();
            if let Some(close) = scoped
                .or_else(|| super::suggest(name, workspace.packages.keys().map(String::as_str)))
            {
                message.push_str(&format!("; did you mean `{close}`?"));
            }
            return Err(message);
        }
        for (package, dir) in matched {
            if kind == Kind::Next && !depends_on(&manifest(files, dir), "next") {
                return Err(format!(
                    "`{package}` is not a Next.js app (it does not depend on next); use package(\"{package}\")"
                ));
            }
            pending.push(dir.clone());
        }
    }
    let mut seen = BTreeSet::new();
    while let Some(dir) = pending.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let mut entry = vec![format!("{dir}/**")];
        // Packages nested in this one are theirs, not its.
        for other in &workspace.dirs {
            if other != &dir && other.starts_with(&format!("{dir}/")) {
                entry.push(format!("!{other}/**"));
            }
        }
        globs.insert(entry.join("\u{0}"));
        let manifest = manifest(files, &dir);
        for section in ["dependencies", "devDependencies", "peerDependencies"] {
            for (name, version) in manifest
                .get(section)
                .and_then(|value| value.as_object())
                .into_iter()
                .flatten()
            {
                if version
                    .as_str()
                    .is_some_and(|version| version.starts_with("workspace:"))
                    && let Some(dep) = workspace.packages.get(name)
                {
                    pending.push(dep.clone());
                }
            }
        }
    }
    // A package with nested packages is a glob with exclusions right after it;
    // the joined entries keep them together through the set.
    // Entries with exclusions first: a later plain glob may re-include a
    // nested package that is itself part of the closure.
    let (mut nested, plain): (Vec<String>, Vec<String>) =
        globs.into_iter().partition(|entry| entry.contains('\u{0}'));
    nested.extend(plain);
    Ok(nested
        .into_iter()
        .flat_map(|entry| entry.split('\u{0}').map(str::to_owned).collect::<Vec<_>>())
        .collect())
}

/// The workspace directory, for running pnpm (`pnpm --dir <it>`).
pub fn root(files: &dyn Files) -> Result<String, String> {
    Ok(workspace(files)?.root)
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

    #[test]
    fn follows_workspace_dependencies_and_leaves_nested_packages_out() {
        let entries = [
            (
                "web/pnpm-workspace.yaml",
                "packages:\n  - \".\"\n  - \"apps/*\"\n  - \"packages/*\"\n\nother:\n  - x\n",
            ),
            (
                "web/package.json",
                r#"{"name": "garvis-web", "dependencies": {"next": "16", "@x/ui": "workspace:*"}}"#,
            ),
            (
                "web/apps/vpn/package.json",
                r#"{"name": "@x/vpn", "dependencies": {"next": "16", "@x/tma": "workspace:^"}}"#,
            ),
            ("web/packages/ui/package.json", r#"{"name": "@x/ui"}"#),
            (
                "web/packages/tma/package.json",
                r#"{"name": "@x/tma", "dependencies": {"react": "19"}}"#,
            ),
            ("web/pnpm-lock.yaml", ""),
        ];
        let files = Fake(
            entries
                .iter()
                .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
                .collect(),
            entries.iter().map(|(path, _)| (*path).to_owned()).collect(),
        );
        let vpn = closure(&files, &["@x/vpn".into()], Kind::Next).unwrap();
        assert!(
            vpn.contains(&"web/apps/vpn/**".into()) && vpn.contains(&"web/packages/tma/**".into()),
            "{vpn:?}"
        );
        assert!(!vpn.contains(&"web/packages/ui/**".into()), "{vpn:?}");
        let root = closure(&files, &["garvis-web".into()], Kind::Next).unwrap();
        let at = root.iter().position(|glob| glob == "web/**").unwrap();
        assert!(
            root[at + 1..]
                .iter()
                .take_while(|glob| glob.starts_with('!'))
                .any(|glob| glob == "!web/apps/vpn/**"),
            "{root:?}"
        );
        assert!(
            root.contains(&"web/packages/ui/**".into())
                && root.contains(&"web/pnpm-lock.yaml".into())
        );
        assert!(
            closure(&files, &["vpn".into()], Kind::Next)
                .unwrap_err()
                .contains("did you mean `@x/vpn`")
        );
        assert!(
            closure(&files, &["@x/ui".into()], Kind::Next)
                .unwrap_err()
                .contains("not a Next.js app")
        );
        assert!(
            closure(&files, &["@x/*".into()], Kind::Package)
                .unwrap()
                .contains(&"web/packages/ui/**".into())
        );
    }
}
