//! `citrus disk`: free space and the Cargo `target` directories of every worktree.
//!
//! Many worktrees each keep a `target` of tens of gigabytes; together they fill the disk and
//! take Docker (and the Citrus state in it) down with them. This lists them, trims what
//! no build has touched for hours or days, and removes whole targets idle for a chosen time.
//! Only build caches go: sources, results and other people's uncommitted work stay.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::hygiene;
use crate::repo::Repo;

const GIB: u64 = 1 << 30;
/// Below this much free space `citrus run` and `citrus doctor` warn.
pub const LOW_FREE: u64 = 20 * GIB;

/// Bytes available to this user on the filesystem holding `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid NUL-terminated string and stat a valid out-pointer.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / GIB as f64)
}

/// A one-line warning when the disk of `path` is nearly full.
pub fn low_space_warning(path: &Path) -> Option<String> {
    let free = free_bytes(path)?;
    (free < LOW_FREE).then(|| {
        format!(
            "only {} free on the disk of {}: Docker and the Citrus state stop when it fills up (`citrus disk` lists what can go)",
            gib(free),
            path.display()
        )
    })
}

struct Target {
    worktree: PathBuf,
    target: PathBuf,
    bytes: u64,
    idle: Option<Duration>,
    current: bool,
}

/// When something under the target was last written: its newest entry two levels deep.
fn idle_for(target: &Path) -> Option<Duration> {
    let mut newest = target.symlink_metadata().ok()?.modified().ok()?;
    for profile in std::fs::read_dir(target).ok()?.flatten() {
        if let Ok(time) = profile.metadata().and_then(|meta| meta.modified()) {
            newest = newest.max(time);
        }
        if let Ok(inner) = std::fs::read_dir(profile.path()) {
            for entry in inner.flatten() {
                if let Ok(time) = entry.metadata().and_then(|meta| meta.modified()) {
                    newest = newest.max(time);
                }
            }
        }
    }
    std::time::SystemTime::now().duration_since(newest).ok()
}

fn worktrees(repo: &Repo) -> Result<Vec<PathBuf>> {
    let listing = repo.git(&["worktree", "list", "--porcelain"])?;
    Ok(listing
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

fn targets(repo: &Repo) -> Result<Vec<Target>> {
    let here = repo
        .root
        .canonicalize()
        .unwrap_or_else(|_| repo.root.clone());
    let mut found = Vec::new();
    for worktree in worktrees(repo)? {
        let target = worktree.join("target");
        if !target.is_dir() {
            continue;
        }
        let current = worktree.canonicalize().is_ok_and(|path| path == here);
        found.push(Target {
            bytes: hygiene::size(&target),
            idle: idle_for(&target),
            current,
            worktree,
            target,
        });
    }
    found.sort_by_key(|target| std::cmp::Reverse(target.bytes));
    Ok(found)
}

/// List the targets; `trim` drops idle incremental data and old artifacts everywhere;
/// `remove_idle_days` removes the whole target of other worktrees idle that long.
pub fn run(repo: &Repo, trim: bool, remove_idle_days: Option<u64>) -> Result<i32> {
    let free = free_bytes(&repo.root);
    if let Some(free) = free {
        println!("free: {}", gib(free));
    }
    let mut freed = 0;
    for target in targets(repo)? {
        let mut note = String::new();
        let age = target.idle.map_or(u64::MAX, |idle| idle.as_secs() / 3600);
        if trim {
            let gone = hygiene::trim_target(&target.target);
            freed += gone;
            if gone > 0 {
                note = format!("  trimmed {}", gib(gone));
            }
        }
        if let Some(days) = remove_idle_days
            && !target.current
            && age >= days * 24
            && target.idle.is_some()
        {
            let gone = hygiene::size(&target.target);
            if std::fs::remove_dir_all(&target.target).is_ok() {
                freed += gone;
                note = format!("  removed (idle {days} days or more, rebuilt on demand)");
            }
        }
        let idle = match target.idle {
            Some(idle) if idle.as_secs() >= 86400 => format!("{} d", idle.as_secs() / 86400),
            Some(idle) => format!("{} h", idle.as_secs() / 3600),
            None => "?".into(),
        };
        println!(
            "{:>10}  idle {:>5}  {}{}{}",
            gib(target.bytes),
            idle,
            target.worktree.display(),
            if target.current { "  (this one)" } else { "" },
            note
        );
    }
    if trim || remove_idle_days.is_some() {
        println!("freed: {}", gib(freed));
        if let Some(after) = free_bytes(&repo.root) {
            println!("free now: {}", gib(after));
        }
    } else if free.is_some_and(|free| free < LOW_FREE) {
        println!(
            "low on space: `citrus disk --trim` drops idle caches, `--remove-idle 7` also removes targets idle for 7 days"
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_space_of_a_real_directory_is_known_and_a_missing_one_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(free_bytes(dir.path()).is_some_and(|bytes| bytes > 0));
        assert_eq!(free_bytes(&dir.path().join("missing")), None);
    }

    #[test]
    fn idle_time_follows_the_newest_entry_of_a_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("target");
        std::fs::create_dir_all(target.join("debug/deps")).expect("dirs");
        let idle = idle_for(&target).expect("idle");
        assert!(idle < Duration::from_secs(60), "{idle:?}");
        assert_eq!(idle_for(&dir.path().join("none")), None);
    }
}
