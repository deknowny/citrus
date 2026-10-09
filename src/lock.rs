//! `Cargo.lock` by package: a change to the lock file matters to a check only
//! when it touches a package the check's crates depend on.
//!
//! A check that builds crate `userbot` is not affected by a new dependency of
//! crate `clyer`. The closure of a Cargo command (`lang::cargo`) therefore
//! carries the names of every lock package its crates reach (a `cargo-lock:`
//! marker among its globs); the planner selects the check for `Cargo.lock`
//! only if one of those packages changed between the commit the change is
//! measured from and the working tree, and the check's fingerprint hashes just
//! their entries. Artifacts keep the whole lock file.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// A glob that is not one: the lock packages a closure reaches, comma-separated.
pub const MARKER: &str = "cargo-lock:";

/// package name → its entries (every version), as normalized text.
type Entries = BTreeMap<String, Vec<String>>;

fn parse(text: &str) -> Option<Entries> {
    let table: toml::Table = text.parse().ok()?;
    let packages = table.get("package")?.as_array()?;
    let mut entries: Entries = BTreeMap::new();
    for package in packages {
        let table = package.as_table()?;
        let name = table.get("name")?.as_str()?.to_owned();
        entries.entry(name).or_default().push(format!("{table:?}"));
    }
    for versions in entries.values_mut() {
        versions.sort();
    }
    Some(entries)
}

/// name → the names it depends on (every version merged).
type Graph = BTreeMap<String, BTreeSet<String>>;

/// The dependency graph of a lock file, once per distinct content: every Cargo
/// closure of a configuration asks for it.
fn graph(lock: &str) -> Option<std::sync::Arc<Graph>> {
    use std::hash::{Hash, Hasher};
    static CACHE: Mutex<BTreeMap<u64, Option<std::sync::Arc<Graph>>>> = Mutex::new(BTreeMap::new());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    lock.hash(&mut hasher);
    let key = hasher.finish();
    if let Some(found) = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&key)
    {
        return found.clone();
    }
    let built = lock.parse::<toml::Table>().ok().and_then(|table| {
        let mut graph: Graph = BTreeMap::new();
        for package in table.get("package")?.as_array()? {
            let package = package.as_table()?;
            let name = package.get("name")?.as_str()?.to_owned();
            let next = graph.entry(name).or_default();
            for item in package
                .get("dependencies")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten()
                .filter_map(|item| item.as_str())
            {
                if let Some(first) = item.split_whitespace().next() {
                    next.insert(first.to_owned());
                }
            }
        }
        Some(std::sync::Arc::new(graph))
    });
    CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, built.clone());
    built
}

/// Every lock package reachable from `members` (their names), members included.
pub fn reach(lock: &str, members: &[String]) -> Option<BTreeSet<String>> {
    let graph = graph(lock)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut pending: Vec<&str> = members.iter().map(String::as_str).collect();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.to_owned()) {
            continue;
        }
        if let Some(next) = graph.get(name) {
            pending.extend(next.iter().map(String::as_str));
        }
    }
    Some(seen)
}

/// The packages whose entries differ between two lock files; None when either
/// cannot be read (everything counts as changed then).
pub fn changed(old: &str, new: &str) -> Option<BTreeSet<String>> {
    let old = parse(old)?;
    let new = parse(new)?;
    let names: BTreeSet<&String> = old.keys().chain(new.keys()).collect();
    Some(
        names
            .into_iter()
            .filter(|name| old.get(*name) != new.get(*name))
            .cloned()
            .collect(),
    )
}

/// A digest of the entries of `reach` in `lock`.
pub fn slice_digest(lock: &str, reach: &BTreeSet<String>) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    match parse(lock) {
        Some(entries) => {
            for name in reach {
                if let Some(versions) = entries.get(name) {
                    for entry in versions {
                        digest.update(entry.as_bytes());
                        digest.update([0]);
                    }
                }
            }
        }
        None => digest.update(lock.as_bytes()),
    }
    hex::encode(digest.finalize())
}

/// The packages the plan being made sees changed; None: unknown, all of them.
static CHANGED: Mutex<Option<BTreeSet<String>>> = Mutex::new(None);

/// A `#[test]` plan asks about paths alone, with no lock file to compare:
/// there `Cargo.lock` counts as changed for everyone.
static UNKNOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn assume_unknown(on: bool) {
    UNKNOWN.store(on, std::sync::atomic::Ordering::SeqCst);
}

pub fn unknown() -> bool {
    UNKNOWN.load(std::sync::atomic::Ordering::SeqCst)
}

pub fn set_changed(changed: Option<BTreeSet<String>>) {
    *CHANGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = changed;
}

/// Whether a change to `Cargo.lock` reaches a closure with these packages.
pub fn relevant(reach: &BTreeSet<String>) -> bool {
    match &*CHANGED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        None => true,
        Some(changed) => changed.iter().any(|name| reach.contains(name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
version = 4

[[package]]
name = "clyer"
version = "0.1.0"
dependencies = ["serde", "ton_core 0.2.0"]

[[package]]
name = "userbot"
version = "0.1.0"
dependencies = ["serde", "tokio"]

[[package]]
name = "serde"
version = "1.0.0"

[[package]]
name = "tokio"
version = "1.0.0"
dependencies = ["mio"]

[[package]]
name = "mio"
version = "1.0.0"

[[package]]
name = "ton_core"
version = "0.2.0"
"#;

    #[test]
    fn a_closure_reaches_its_dependencies_through_the_lock_graph() {
        let userbot = reach(LOCK, &["userbot".to_owned()]).unwrap();
        assert_eq!(
            userbot.iter().map(String::as_str).collect::<Vec<_>>(),
            ["mio", "serde", "tokio", "userbot"]
        );
        let clyer = reach(LOCK, &["clyer".to_owned()]).unwrap();
        assert!(clyer.contains("ton_core") && !clyer.contains("tokio"));
    }

    #[test]
    fn only_packages_with_a_different_entry_changed() {
        // Clyer gains a dependency; userbot's closure is untouched.
        let edited = LOCK.replace(
            "dependencies = [\"serde\", \"ton_core 0.2.0\"]",
            "dependencies = [\"serde\", \"ton_core 0.2.0\", \"mio\"]",
        );
        let changed = changed(LOCK, &edited).unwrap();
        assert_eq!(changed.iter().cloned().collect::<Vec<_>>(), ["clyer"]);
        let userbot = reach(LOCK, &["userbot".to_owned()]).unwrap();
        assert!(!changed.iter().any(|name| userbot.contains(name)));
        // A new version of tokio reaches userbot but not clyer.
        let bumped = LOCK.replace(
            "name = \"tokio\"\nversion = \"1.0.0\"",
            "name = \"tokio\"\nversion = \"1.1.0\"",
        );
        let changed = changed_of(&bumped);
        assert_eq!(changed, ["tokio"]);
    }

    fn changed_of(text: &str) -> Vec<String> {
        changed(LOCK, text).unwrap().into_iter().collect()
    }

    #[test]
    fn the_slice_digest_ignores_other_packages() {
        let userbot = reach(LOCK, &["userbot".to_owned()]).unwrap();
        let edited = LOCK.replace("ton_core 0.2.0\"]", "ton_core 0.2.0\", \"mio\"]");
        assert_eq!(
            slice_digest(LOCK, &userbot),
            slice_digest(&edited, &userbot)
        );
        let clyer = reach(LOCK, &["clyer".to_owned()]).unwrap();
        assert_ne!(slice_digest(LOCK, &clyer), slice_digest(&edited, &clyer));
    }

    #[test]
    fn relevance_follows_the_changed_set() {
        let userbot: BTreeSet<String> = ["userbot", "tokio"].map(str::to_owned).into();
        set_changed(Some(["clyer".to_owned()].into()));
        assert!(!relevant(&userbot));
        set_changed(Some(["tokio".to_owned()].into()));
        assert!(relevant(&userbot));
        set_changed(None);
        assert!(relevant(&userbot), "unknown counts as changed");
    }
}
