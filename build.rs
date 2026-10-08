//! Embeds the source commit, so `citrus --version` tells which build runs.
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CITRUS_BUILD_COMMIT");
    // An edit not yet added changes no Git file but makes the build dirty.
    for path in ["src", "build.rs", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    // The files that move with the checked-out commit, wherever Git keeps
    // them: in a linked worktree `.git` is a file and HEAD lives elsewhere,
    // and a build that missed that published a new tree as an old commit.
    for (args, file) in [
        (&["rev-parse", "--absolute-git-dir"][..], "HEAD"),
        (&["rev-parse", "--absolute-git-dir"][..], "index"),
        (
            &["rev-parse", "--path-format=absolute", "--git-common-dir"][..],
            "packed-refs",
        ),
    ] {
        if let Some(dir) = git(args) {
            println!("cargo:rerun-if-changed={dir}/{file}");
        }
    }
    if let (Some(common), Some(head)) = (
        git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]),
        git(&["symbolic-ref", "-q", "HEAD"]),
    ) {
        println!("cargo:rerun-if-changed={common}/{head}");
    }
    let commit = std::env::var("CITRUS_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            let head = git(&["rev-parse", "HEAD"])?;
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|out| !out.is_empty());
            Some(if dirty { format!("{head}-dirty") } else { head })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=CITRUS_COMMIT={commit}");
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
