//! Cache hygiene: an agent shares its disk with production, so its caches stay small.
//!
//! The agent's Cargo target directories (one per slot) grow without bound:
//! incremental sessions of branches nobody builds any more, artifacts of old
//! dependency versions. Once an hour the agent
//!
//! * removes incremental sessions idle for 6 hours and dependency artifacts idle
//!   for three days (a check that needs one rebuilds it),
//! * drops all incremental data of a target above its own cap,
//! * empties the least recently used slots' build directories while the sum of
//!   all of them is above the total cap,
//! * keeps Docker's build cache and unused images within bounds.
//!
//! Only caches go; sources, results and production data are never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

const INCREMENTAL_IDLE: Duration = Duration::from_secs(6 * 3600);
const ARTIFACT_IDLE: Duration = Duration::from_secs(3 * 24 * 3600);
const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// One target directory above this loses its incremental data.
    pub per_target: u64,
    /// All target directories together above this lose their least recently used slots.
    pub total: u64,
}

impl Limits {
    pub fn gib(per_target: u64, total: u64) -> Limits {
        Limits {
            per_target: per_target * GIB,
            total: total * GIB,
        }
    }
}

/// Neither written nor read for `limit` (Cargo does not touch the artifacts it
/// reuses, but the linker reading one counts as use).
fn idle(path: &Path, limit: Duration) -> bool {
    let Ok(meta) = path.symlink_metadata() else {
        return false;
    };
    let used = [meta.modified().ok(), meta.accessed().ok()]
        .into_iter()
        .flatten()
        .max();
    used.and_then(|time| SystemTime::now().duration_since(time).ok())
        .is_some_and(|age| age > limit)
}

/// Bytes under `path` (allocated blocks, symlinks not followed).
pub fn size(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    let mut total = meta.blocks() * 512;
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.flatten() {
            total += size(&entry.path());
        }
    }
    total
}

/// Incremental sessions (`<profile>/incremental/<session>`) idle for `INCREMENTAL_IDLE`
/// and `deps` files idle for `ARTIFACT_IDLE`.
fn trim_target(target: &Path) -> u64 {
    let mut freed = 0;
    let Ok(profiles) = std::fs::read_dir(target) else {
        return 0;
    };
    // <target>/<profile> and <target>/<triple>/<profile>
    let mut roots = Vec::new();
    for profile in profiles.flatten() {
        let path = profile.path();
        if path.join("incremental").is_dir() || path.join("deps").is_dir() {
            roots.push(path);
        } else if path.is_dir()
            && let Ok(inner) = std::fs::read_dir(&path)
        {
            roots.extend(inner.flatten().map(|entry| entry.path()));
        }
    }
    for root in roots {
        if let Ok(sessions) = std::fs::read_dir(root.join("incremental")) {
            for session in sessions.flatten() {
                // The crate directory holds the sessions: judge each session.
                if let Ok(inner) = std::fs::read_dir(session.path()) {
                    for one in inner.flatten() {
                        if idle(&one.path(), INCREMENTAL_IDLE) {
                            freed += size(&one.path());
                            let _ = std::fs::remove_dir_all(one.path());
                        }
                    }
                }
            }
        }
        if let Ok(files) = std::fs::read_dir(root.join("deps")) {
            for file in files.flatten() {
                let path = file.path();
                if path.is_file() && idle(&path, ARTIFACT_IDLE) {
                    freed += size(&path);
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
    }
    freed
}

fn drop_incremental(target: &Path) -> u64 {
    let mut freed = 0;
    let Ok(profiles) = std::fs::read_dir(target) else {
        return 0;
    };
    for profile in profiles.flatten() {
        let incremental = profile.path().join("incremental");
        if incremental.is_dir() {
            freed += size(&incremental);
            let _ = std::fs::remove_dir_all(&incremental);
        }
    }
    freed
}

/// Every Cargo target directory under the agent's cache: slots first, then the
/// directories earlier agents kept per repository.
pub fn targets(cache: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let named = |dir: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                entry.path().is_dir() && (name.starts_with("cargo-target") || name == "target")
            })
            .map(|entry| entry.path())
            .collect()
    };
    for repo in std::fs::read_dir(cache.join("slots"))
        .into_iter()
        .flatten()
        .flatten()
    {
        for slot in std::fs::read_dir(repo.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            found.extend(named(&slot.path().join("cache")));
        }
    }
    for repo in std::fs::read_dir(cache.join("shared"))
        .into_iter()
        .flatten()
        .flatten()
    {
        found.extend(named(&repo.path()));
    }
    found
}

/// Applies the limits; returns the bytes freed.
pub fn prune(cache: &Path, limits: Limits) -> u64 {
    let mut freed = 0;
    let mut sizes: Vec<(PathBuf, u64)> = Vec::new();
    for target in targets(cache) {
        freed += trim_target(&target);
        let mut bytes = size(&target);
        if bytes > limits.per_target {
            freed += drop_incremental(&target);
            bytes = size(&target);
        }
        sizes.push((target, bytes));
    }
    // Over the total: the least recently used targets go first (a slot's
    // directory changes whenever a check builds in it).
    let mut total: u64 = sizes.iter().map(|(_, bytes)| bytes).sum();
    sizes.sort_by_key(|(path, _)| {
        path.metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    for (path, bytes) in sizes {
        if total <= limits.total {
            break;
        }
        // A target a build holds open is not empty after: it is simply rebuilt.
        if std::fs::remove_dir_all(&path).is_ok() {
            let _ = std::fs::create_dir_all(&path);
            freed += bytes;
            total = total.saturating_sub(bytes);
        }
    }
    freed
}

/// Docker: the build cache and images nothing has used for three days.
pub fn docker() {
    for args in [
        &["builder", "prune", "-f", "--keep-storage", "30gb"][..],
        &["image", "prune", "-f", "--filter", "until=72h"][..],
    ] {
        let _ = Command::new("docker")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn age(path: &Path, seconds: i64) {
        let when = SystemTime::now() - Duration::from_secs(seconds as u64);
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let stamp = libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        let times = [stamp, stamp];
        let path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: a NUL-terminated path and two timespecs.
        unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) };
    }

    fn file(path: &Path, bytes: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, vec![7u8; bytes]).unwrap();
    }

    #[test]
    fn old_incremental_sessions_and_artifacts_go_fresh_ones_stay() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("slots/repo/0/cache/cargo-target");
        file(
            &target.join("debug/incremental/backend-abc/s-old/data"),
            4096,
        );
        file(
            &target.join("debug/incremental/backend-abc/s-new/data"),
            4096,
        );
        file(&target.join("debug/deps/libold.rlib"), 4096);
        file(&target.join("debug/deps/libnew.rlib"), 4096);
        age(
            &target.join("debug/incremental/backend-abc/s-old"),
            7 * 3600,
        );
        age(&target.join("debug/deps/libold.rlib"), 4 * 24 * 3600);
        prune(dir.path(), Limits::gib(100, 100));
        assert!(!target.join("debug/incremental/backend-abc/s-old").exists());
        assert!(
            target
                .join("debug/incremental/backend-abc/s-new/data")
                .exists()
        );
        assert!(!target.join("debug/deps/libold.rlib").exists());
        assert!(target.join("debug/deps/libnew.rlib").exists());
    }

    #[test]
    fn a_target_over_its_cap_loses_incremental_data_and_the_total_cap_empties_the_oldest() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("slots/repo/0/cache/cargo-target");
        let b = dir.path().join("slots/repo/1/cache/cargo-target");
        file(&a.join("debug/incremental/c/s/data"), 200_000);
        file(&a.join("debug/deps/liba.rlib"), 200_000);
        file(&b.join("debug/deps/libb.rlib"), 200_000);
        age(&a, 3600);
        let limits = Limits {
            per_target: 300_000,
            total: 250_000,
        };
        prune(dir.path(), limits);
        assert!(!a.join("debug/incremental").exists(), "over its own cap");
        // Slot 0 is the older one: its build directory is emptied, slot 1's stays.
        assert!(!a.join("debug/deps/liba.rlib").exists());
        assert!(a.exists(), "the directory itself stays for the next build");
        assert!(b.join("debug/deps/libb.rlib").exists());
    }

    #[test]
    fn earlier_shared_targets_are_found_too() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("shared/r/cargo-target")).unwrap();
        std::fs::create_dir_all(dir.path().join("shared/r/cargo-target-clyerbot")).unwrap();
        std::fs::create_dir_all(dir.path().join("shared/r/pnpm-store")).unwrap();
        assert_eq!(targets(dir.path()).len(), 2);
    }
}
