//! The Citrus build a repository pins runs its commands: `.citrus/pin` (one
//! commit; a file of its own, so moving it changes no configuration a plan
//! depends on), or `#![pin("<commit>")]` in a single-file `citrus.ci`. Any `citrus` finds that build — in the Git common
//! directory's cache, then among the pool's published builds, else it
//! compiles it once — and hands the command over to it. `citrus self pin`
//! moves the pin.

use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The configuration file holding project attributes, under `root`.
fn config_file(root: &Path) -> Option<PathBuf> {
    [".citrus/project.ci", "citrus.ci"]
        .iter()
        .map(|name| root.join(name))
        .find(|path| path.is_file())
}

/// The pinned commit of the configuration text, if any.
fn pinned(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("#![pin(\"")?;
        let commit = rest.strip_suffix("\")]")?;
        is_commit(commit).then(|| commit.to_owned())
    })
}

fn is_commit(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The commit `root` pins: `.citrus/pin`, else a `#![pin]` attribute.
fn pinned_in(root: &Path) -> Option<String> {
    if let Ok(text) = std::fs::read_to_string(root.join(".citrus/pin")) {
        let commit = text.trim();
        return is_commit(commit).then(|| commit.to_owned());
    }
    pinned(&std::fs::read_to_string(config_file(root)?).ok()?)
}

/// The repository around the working directory.
fn root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// Hand the command over to the pinned build unless this is it. Returns
/// only when this process should run the command itself.
pub fn handover() -> Result<()> {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("CITRUS_NO_PIN").is_some_and(|value| !value.is_empty()) {
        return Ok(());
    }
    let own = std::env::current_exe()?;
    // A pool executor or a caller names the build to use.
    if let Some(binary) = std::env::var_os("CITRUS_BIN").filter(|value| !value.is_empty()) {
        let binary = PathBuf::from(binary);
        if binary.canonicalize().ok() == own.canonicalize().ok() {
            return Ok(());
        }
        let error = Command::new(&binary)
            .args(std::env::args_os().skip(1))
            .exec();
        bail!("run {}: {error}", binary.display());
    }
    if std::env::var_os("CITRUS_PINNED").is_some() {
        return Ok(());
    }
    let Some(root) = root() else { return Ok(()) };
    let Some(commit) = pinned_in(&root) else {
        return Ok(());
    };
    if crate::pool::VERSION == commit {
        return Ok(());
    }
    let binary = prepare(&root, &commit)?;
    let error = Command::new(&binary)
        .args(std::env::args_os().skip(1))
        .env("CITRUS_PINNED", &commit)
        .exec();
    bail!("run {}: {error}", binary.display())
}

/// The pinned build on this machine: cached, fetched from the pool, or
/// compiled once.
fn prepare(root: &Path, commit: &str) -> Result<PathBuf> {
    let common = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
        .unwrap_or_else(|| root.join(".git"));
    let cache = std::env::var_os("CITRUS_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| common.join("citrus/build"));
    let binary = cache.join(format!("citrus-{commit}"));
    if binary.exists() {
        return Ok(binary);
    }
    std::fs::create_dir_all(&cache)?;
    if crate::pool::fetch_published(commit, &platform(), &binary).unwrap_or(false) {
        eprintln!("citrus: took {} from the pool", &commit[..12]);
        return Ok(binary);
    }
    eprintln!("citrus: building {} (once)…", &commit[..12]);
    let staging = cache.join(format!(".build-{commit}-{}", std::process::id()));
    let status = Command::new("cargo")
        .args(["install", "--quiet", "--locked", "--git", REPOSITORY])
        .args(["--rev", commit, "--profile", "fast", "--root"])
        .arg(&staging)
        .env("CITRUS_BUILD_COMMIT", commit)
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::null())
        .status()
        .context("cargo builds the pinned Citrus")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        bail!("could not build Citrus {commit}");
    }
    std::fs::rename(staging.join("bin/citrus"), &binary)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(binary)
}

/// Move the pin to `commit` in the repository's configuration; the pool
/// should hold its builds for this machine and Linux agents.
pub fn pin(commit: &str) -> Result<String> {
    if !is_commit(commit) {
        bail!("pin a full 40-character commit, not {commit:?}");
    }
    let root = root().context("not inside a Git repository")?;
    let file = if root.join(".citrus").is_dir() {
        let file = root.join(".citrus/pin");
        std::fs::write(&file, format!("{commit}\n"))?;
        // A `#![pin]` attribute would contradict the file.
        if let Some(project) = config_file(&root) {
            let text = std::fs::read_to_string(&project)?;
            if pinned(&text).is_some() {
                let kept: Vec<&str> = text.lines().filter(|line| pinned(line).is_none()).collect();
                std::fs::write(&project, kept.join("\n") + "\n")?;
            }
        }
        file
    } else {
        let file = config_file(&root).context("no citrus.ci or .citrus/")?;
        let text = std::fs::read_to_string(&file)?;
        let line = format!("#![pin(\"{commit}\")]");
        let updated = if pinned(&text).is_some() {
            text.lines()
                .map(|existing| {
                    if pinned(existing).is_some() {
                        line.clone()
                    } else {
                        existing.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
                + "\n"
        } else {
            let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
            let at = lines
                .iter()
                .position(|existing| existing.trim().starts_with("#![citrus("))
                .map_or(0, |index| index + 1);
            lines.insert(at, line);
            lines.join("\n") + "\n"
        };
        std::fs::write(&file, updated)?;
        file
    };
    let held: Vec<String> = crate::pool::binaries()
        .unwrap_or_default()
        .into_iter()
        .filter(|(held, ..)| held == commit)
        .map(|(_, platform, ..)| platform)
        .collect();
    let mut missing = Vec::new();
    for wanted in [platform(), "linux-x86_64".to_owned()] {
        if !held.contains(&wanted) && !missing.contains(&wanted) {
            missing.push(wanted);
        }
    }
    let relative = file
        .strip_prefix(&root)
        .unwrap_or(&file)
        .display()
        .to_string();
    Ok(if missing.is_empty() {
        format!("{relative} pins {}", &commit[..12])
    } else {
        format!(
            "{relative} pins {}; the pool holds no build for {} (compiled on first use)",
            &commit[..12],
            missing.join(", ")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_file_wins_over_an_attribute() {
        let dir = tempfile::tempdir().unwrap();
        let (file, attr) = ("a".repeat(40), "b".repeat(40));
        std::fs::create_dir_all(dir.path().join(".citrus")).unwrap();
        std::fs::write(
            dir.path().join(".citrus/project.ci"),
            format!("#![citrus(2)]\n#![pin(\"{attr}\")]\n"),
        )
        .unwrap();
        assert_eq!(pinned_in(dir.path()).as_deref(), Some(attr.as_str()));
        std::fs::write(dir.path().join(".citrus/pin"), format!("{file}\n")).unwrap();
        assert_eq!(pinned_in(dir.path()).as_deref(), Some(file.as_str()));
    }

    #[test]
    fn reads_only_a_full_commit_pin() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            pinned(&format!("#![citrus(2)]\n#![pin(\"{commit}\")]\n")).as_deref(),
            Some(commit)
        );
        assert_eq!(pinned("#![pin(\"abc\")]"), None);
        assert_eq!(pinned("// #![pin(…)]"), None);
    }
}
