//! Slot trees: the checks of a pool agent run in a few trees whose paths never change.
//!
//! Cargo bakes absolute paths into what it builds (`env!("CARGO_MANIFEST_DIR")`
//! and friends) and trusts source modification times. A tree at a new path for
//! every run would recompile the workspace every time; one shared build
//! directory beside trees at changing paths would reuse artifacts that point
//! into trees long gone. A slot is a tree with a fixed path and its own build
//! directories: `git checkout` leaves unchanged files alone, so their times
//! stay and Cargo rebuilds exactly what changed, and what Cargo baked into the
//! artifacts is always the tree it is looking at.

use anyhow::{Context, Result, bail};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a batch waits for a free slot before giving up.
const WAIT: Duration = Duration::from_secs(30 * 60);
/// A slot idle this long is deleted with its build directories.
pub const IDLE_DAYS: u64 = 14;

/// A claimed slot; the claim ends when it is dropped.
#[derive(Debug)]
pub struct Slot {
    pub index: usize,
    dir: PathBuf,
    _lock: File,
}

impl Slot {
    /// The tree: the same path every time this slot is used.
    pub fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }

    /// A cache directory of this slot (Cargo's target directory, for one).
    pub fn cache(&self, name: &str) -> PathBuf {
        self.dir.join("cache").join(name)
    }
}

fn try_lock(path: &Path) -> Result<Option<File>> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    // SAFETY: flock on a descriptor this process owns.
    let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if status == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error).with_context(|| format!("flock {}", path.display()))
    }
}

/// The lowest free slot of `root` (`<agent cache>/slots/<repository>`), of `count`.
pub fn claim(root: &Path, count: usize) -> Result<Slot> {
    std::fs::create_dir_all(root)?;
    let started = Instant::now();
    loop {
        for index in 0..count.max(1) {
            let dir = root.join(index.to_string());
            std::fs::create_dir_all(&dir)?;
            if let Some(lock) = try_lock(&root.join(format!("{index}.lock")))? {
                return Ok(Slot {
                    index,
                    dir,
                    _lock: lock,
                });
            }
        }
        if started.elapsed() > WAIT {
            bail!("no free slot in {} after {:?}", root.display(), WAIT);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Deletes the slots of `root` nobody used for `IDLE_DAYS`; returns their indices.
pub fn remove_idle(root: &Path, days: u64) -> Vec<usize> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let limit = Duration::from_secs(days * 24 * 3600);
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = name
            .strip_suffix(".lock")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            continue;
        };
        let idle = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age > limit);
        if !idle {
            continue;
        }
        // Only a slot nobody holds right now.
        let Ok(Some(_lock)) = try_lock(&entry.path()) else {
            continue;
        };
        if std::fs::remove_dir_all(root.join(index.to_string())).is_ok() {
            let _ = std::fs::remove_file(entry.path());
            removed.push(index);
        }
    }
    removed
}

/// The directories under /citrus-cache an image's environment points Cargo at
/// (`CARGO_TARGET_DIR=/citrus-cache/cargo-target`, a second target, …): each
/// becomes a directory of the slot.
pub fn target_dirs(environment: &str) -> Vec<String> {
    let mut names: Vec<String> = environment
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter_map(|(_, value)| value.strip_prefix("/citrus-cache/"))
        .filter_map(|rest| rest.split('/').next())
        .filter(|name| name.starts_with("cargo-target"))
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_claimed_lowest_first_and_given_back_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let first = claim(dir.path(), 2).unwrap();
        let second = claim(dir.path(), 2).unwrap();
        assert_eq!((first.index, second.index), (0, 1));
        assert_ne!(first.tree(), second.tree());
        assert_eq!(first.tree(), dir.path().join("0/tree"));
        drop(first);
        let again = claim(dir.path(), 2).unwrap();
        assert_eq!(again.index, 0, "the same path comes back");
        assert_eq!(
            again.cache("cargo-target"),
            dir.path().join("0/cache/cargo-target")
        );
    }

    #[test]
    fn only_idle_unheld_slots_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let held = claim(dir.path(), 2).unwrap();
        drop(claim(dir.path(), 2).unwrap_or_else(|_| unreachable!()));
        let other = dir.path().join("1");
        std::fs::create_dir_all(other.join("tree")).unwrap();
        std::fs::write(other.join("tree/file"), "x").unwrap();
        // Nothing is old yet.
        assert!(remove_idle(dir.path(), 14).is_empty());
        // Everything is "idle" for a zero-day limit, except what is held.
        std::thread::sleep(Duration::from_millis(20));
        let removed = remove_idle(dir.path(), 0);
        assert_eq!(removed, vec![1]);
        assert!(!other.exists());
        assert!(dir.path().join("0").exists(), "a held slot stays");
        drop(held);
    }

    #[test]
    fn cargo_targets_are_found_in_an_images_environment() {
        let environment = "PATH=/usr/bin\nCARGO_TARGET_DIR=/citrus-cache/cargo-target\n\
            CLYER_TARGET=/citrus-cache/cargo-target-clyerbot\nCARGO_HOME=/citrus-cache/cargo-home\n\
            SCCACHE_DIR=/citrus-cache/sccache\nOTHER=/citrus-cache/cargo-target/debug\n";
        assert_eq!(
            target_dirs(environment),
            ["cargo-target", "cargo-target-clyerbot"]
        );
        assert!(target_dirs("A=b").is_empty());
    }
}
