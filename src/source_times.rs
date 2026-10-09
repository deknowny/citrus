//! Stable source times: a fresh checkout does not make Cargo rebuild.
//!
//! Cargo decides a crate is fresh by comparing the modification times of its
//! sources with its outputs. A new worktree stamps every file "now", so every
//! run of a pool agent would recompile the workspace. The ledger remembers, per
//! repository on one machine, each tracked file's git object and the time it had
//! when it first showed up with that content. A new tree gets those times back
//! for every file whose content is unchanged; a changed file keeps its new time,
//! which is later than any output built from the old one, so Cargo rebuilds
//! exactly what changed.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// A ledger entry unseen this long is dropped.
const KEEP_SECONDS: i64 = 30 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    oid: String,
    sec: i64,
    nsec: i64,
    seen: i64,
}

#[derive(Debug, Default, PartialEq)]
pub struct Stats {
    pub restored: usize,
    pub recorded: usize,
}

/// Tracked regular files of `tree`: (git object, path).
fn tracked(tree: &Path) -> Result<Vec<(String, String)>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(tree)
        .args(["ls-files", "-s", "-z"])
        .stdin(Stdio::null())
        .output()
        .context("git ls-files")?;
    anyhow::ensure!(output.status.success(), "git ls-files failed");
    let mut files = Vec::new();
    for record in output.stdout.split(|byte| *byte == 0) {
        let text = String::from_utf8_lossy(record);
        // "<mode> <oid> <stage>\t<path>"
        let Some((head, path)) = text.split_once('\t') else {
            continue;
        };
        let mut fields = head.split_whitespace();
        let (Some(mode), Some(oid)) = (fields.next(), fields.next()) else {
            continue;
        };
        // Symlinks and submodules have no source time of their own.
        if mode == "120000" || mode == "160000" {
            continue;
        }
        files.push((oid.to_owned(), path.to_owned()));
    }
    Ok(files)
}

fn set_times(path: &Path, sec: i64, nsec: i64) -> bool {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let stamp = libc::timespec {
        tv_sec: sec as libc::time_t,
        tv_nsec: nsec as _,
    };
    let times = [stamp, stamp];
    // SAFETY: `path` is a NUL-terminated string and `times` holds two timespecs.
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        ) == 0
    }
}

/// Holds an exclusive advisory lock on `lock` until dropped.
#[derive(Debug)]
struct Locked {
    /// Closing the descriptor releases the lock.
    _file: std::fs::File,
}

impl Locked {
    fn take(lock: &Path) -> Result<Locked> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock)
            .with_context(|| format!("open {}", lock.display()))?;
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this process owns.
        let status = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        anyhow::ensure!(status == 0, "flock {}", lock.display());
        Ok(Locked { _file: file })
    }
}

/// Gives the files of `tree` the times the ledger remembers for their content,
/// and remembers the times of the files it has not seen with this content.
pub fn restore(ledger: &Path, tree: &Path) -> Result<Stats> {
    if let Some(dir) = ledger.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let files = tracked(tree)?;
    let _lock = Locked::take(&ledger.with_extension("lock"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let mut known: BTreeMap<String, Entry> = std::fs::read(ledger)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    known.retain(|_, entry| now - entry.seen < KEEP_SECONDS);
    let mut stats = Stats::default();
    for (oid, path) in files {
        let file = tree.join(&path);
        match known.get_mut(&path) {
            Some(entry) if entry.oid == oid => {
                entry.seen = now;
                if set_times(&file, entry.sec, entry.nsec) {
                    stats.restored += 1;
                }
            }
            _ => {
                let Ok(meta) = std::fs::symlink_metadata(&file) else {
                    continue;
                };
                known.insert(
                    path,
                    Entry {
                        oid,
                        sec: meta.mtime(),
                        nsec: meta.mtime_nsec(),
                        seen: now,
                    },
                );
                stats.recorded += 1;
            }
        }
    }
    let temporary = ledger.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(&known)?)?;
    std::fs::rename(&temporary, ledger)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn mtime(path: &Path) -> (i64, i64) {
        let meta = std::fs::metadata(path).unwrap();
        (meta.mtime(), meta.mtime_nsec())
    }

    #[test]
    fn unchanged_files_get_their_old_times_and_changed_ones_stay_new() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("src/a.rs"), "a").unwrap();
        std::fs::write(repo.join("src/b.rs"), "b").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "one"]);
        let ledger = dir.path().join("ledger/times.json");

        // First tree: everything is new and recorded.
        let first = dir.path().join("first");
        git(
            &repo,
            &["worktree", "add", "-q", "--detach", first.to_str().unwrap()],
        );
        set_times(&first.join("src/a.rs"), 1_000_000, 5);
        set_times(&first.join("src/b.rs"), 2_000_000, 7);
        let stats = restore(&ledger, &first).unwrap();
        assert_eq!(
            stats,
            Stats {
                restored: 0,
                recorded: 2
            }
        );

        // b changes; the second tree is a fresh checkout stamped "now".
        std::fs::write(repo.join("src/b.rs"), "b2").unwrap();
        git(&repo, &["commit", "-qam", "two"]);
        let second = dir.path().join("second");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                second.to_str().unwrap(),
            ],
        );
        let fresh_b = mtime(&second.join("src/b.rs"));
        let stats = restore(&ledger, &second).unwrap();
        assert_eq!(
            stats,
            Stats {
                restored: 1,
                recorded: 1
            }
        );
        assert_eq!(mtime(&second.join("src/a.rs")), (1_000_000, 5));
        assert_eq!(
            mtime(&second.join("src/b.rs")),
            fresh_b,
            "a changed file is not backdated"
        );
        assert!(fresh_b.0 > 2_000_000);

        // Going back to the first content: b is not the ledger's content now, so it stays new.
        let third = dir.path().join("third");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                third.to_str().unwrap(),
                "HEAD~1",
            ],
        );
        let fresh_b = mtime(&third.join("src/b.rs"));
        restore(&ledger, &third).unwrap();
        assert_eq!(mtime(&third.join("src/b.rs")), fresh_b);
        assert_eq!(mtime(&third.join("src/a.rs")), (1_000_000, 5));
    }

    #[test]
    fn symlinks_and_a_missing_ledger_are_fine() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        std::fs::write(repo.join("x"), "x").unwrap();
        std::os::unix::fs::symlink("x", repo.join("link")).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "one"]);
        let stats = restore(&dir.path().join("nested/ledger.json"), &repo).unwrap();
        assert_eq!(
            stats,
            Stats {
                restored: 0,
                recorded: 1
            }
        );
    }
}
