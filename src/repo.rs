//! The Git checkout Citrus works in and the exact snapshot of its sources.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::manifest::Receipts;

#[derive(Debug, Clone)]
pub struct Repo {
    pub root: PathBuf,
    pub common: PathBuf,
    pub config: Config,
}

impl Repo {
    pub fn discover() -> Result<Repo> {
        let root = PathBuf::from(
            git_in(Path::new("."), &["rev-parse", "--show-toplevel"])
                .context("not inside a Git checkout")?,
        );
        let common = PathBuf::from(git_in(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?);
        let config = Config::load(&root)?;
        Ok(Repo {
            root,
            common,
            config,
        })
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        git_in(&self.root, args)
    }

    /// Shared by every worktree of this clone.
    pub fn state_dir(&self) -> PathBuf {
        Config::in_common(&self.common, &self.config.state.path)
    }

    pub fn receipts(&self) -> Receipts {
        Receipts {
            dir: Config::in_common(&self.common, &self.config.receipts.dir),
            max_age: self.config.receipts.max_age_days * 24 * 60 * 60,
        }
    }

    pub fn log_dir(&self) -> PathBuf {
        self.root.join(&self.config.log_dir)
    }

    pub fn releases_path(&self) -> PathBuf {
        self.root.join(&self.config.releases)
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join(&self.config.manifest)
    }

    pub fn branch(&self) -> String {
        self.git(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_else(|_| "?".into())
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "--short", "HEAD"])
            .unwrap_or_else(|_| "?".into())
    }

    /// Tracked and untracked, non-ignored files that exist on disk, sorted.
    pub fn files(&self) -> Result<Vec<String>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ])
            .output()?;
        if !output.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut files: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .split('\0')
            .filter(|path| !path.is_empty() && self.root.join(path).is_file())
            .map(str::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// Git tree id of the working tree as it is now: committed, modified and
    /// untracked non-ignored files. Equal ids mean byte-identical sources.
    pub fn snapshot(&self) -> Result<String> {
        let dir = self.state_dir().join("tmp");
        private_dir(&dir)?;
        let index = dir.join(format!(
            "index-{}-{}",
            std::process::id(),
            crate::manifest::now()
        ));
        let current = PathBuf::from(self.git(&[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index",
        ])?);
        if current.exists() {
            fs::copy(&current, &index)?;
        }
        let run = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .arg("-C")
                .arg(&self.root)
                .args(args)
                .env("GIT_INDEX_FILE", &index)
                .output()?;
            if !output.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        };
        let result = run(&["add", "-A", "--", "."]).and_then(|_| run(&["write-tree"]));
        let _ = fs::remove_file(&index);
        result
    }

    /// Paths that differ between two snapshots.
    pub fn changed_between(&self, old: &str, new: &str) -> Vec<String> {
        self.git(&["diff", "--name-only", old, new])
            .map(|out| out.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

/// Create `path` and missing parents readable by the owner only: logs and
/// state may hold command output, and tools often refuse group-readable state.
pub fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| anyhow::anyhow!("create {}: {error}", path.display()))
}

fn git_in(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output()?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
