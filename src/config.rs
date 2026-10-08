//! Settings Citrus runs with: generic defaults, overridden by the `project`,
//! `planner` and `pool` blocks of `citrus.ci` (see `exec::Context::open`).
//! Without them the built-in planner selects declared checks owning the
//! changed paths, and state is a local SQLite file shared by all worktrees.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    /// Files every declared target's fingerprint depends on (toolchain pins).
    pub toolchain_files: Vec<String>,
    /// Where run logs go: repository-relative when the project says so
    /// (`project { logs = … }`), otherwise `citrus/logs` in the Git directory.
    pub log_dir: Option<String>,
    /// Files that define the targets `citrus add` may declare (globs). Empty: no check.
    pub target_definitions: Vec<String>,
    pub plan: PlanConfig,
    pub run: RunConfig,
    pub receipts: ReceiptsConfig,
    pub state: StateConfig,
    pub status: StatusConfig,
    pub integrate: IntegrateConfig,
    /// Commands people and agents use in this repository, shown by `citrus`.
    pub catalog: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct IntegrateConfig {
    /// Runs after a successful merge, before checks; `{before}` is the commit
    /// before the merge. For project housekeeping such as retiring removed
    /// submodule checkouts. A failure stops the integration.
    pub after_merge: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CatalogEntry {
    /// What to type, e.g. `make release-prepare-web`.
    pub command: String,
    pub description: String,
    /// Optional heading the entry is listed under.
    pub group: String,
}

#[derive(Debug, Clone, Default)]
pub struct StatusConfig {
    /// Slow command describing shared execution resources (builders, runners).
    /// `status` shows its last snapshot and refreshes it in the background.
    pub resources_command: Vec<String>,
    /// Lines of that command's output that describe one resource each.
    pub resource_prefix: String,
    /// Age after which the snapshot is refreshed.
    pub refresh_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct PlanConfig {
    /// Default base for "what changed" when `--base` is not given.
    pub base: String,
    /// The profile checks are planned for (`--profile`, `CITRUS_PROFILE`, or
    /// the project's first); None when the project declares no profiles.
    pub profile: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Command that runs the whole planned set elsewhere (builder, CI). Empty: no remote mode.
    pub remote: Vec<String>,
    /// Line prefixes, besides `CITRUS_TARGET `, that report
    /// `target=<name> status=START|PASS|FAIL [exit=] [seconds=]`.
    pub progress_prefixes: Vec<String>,
    /// Line prefix meaning "queued for a resource", followed by its name.
    pub waiting_prefix: String,
    /// Line prefixes meaning the resource was granted.
    pub acquired_prefixes: Vec<String>,
    /// Line prefix whose remainder is a human-readable stage, shown in status.
    pub stage_prefix: String,
    /// Markers followed by the path of a fuller log the remote runner keeps;
    /// Citrus reads target results and errors from that file too.
    pub linked_log_markers: Vec<String>,
    /// The image pool agents run the checks in (`#![image]`).
    pub image: Option<crate::model::Image>,
    /// Paths a pool snapshot never carries (`#![private]`).
    pub private: Vec<String>,
    /// Run by a pool agent in the tree before the checks (`#![prepare]`).
    pub prepare: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReceiptsConfig {
    /// PASS receipts of declared targets; relative to the Git common dir.
    pub dir: String,
    /// Days a receipt of unchanged inputs stays valid.
    pub max_age_days: u64,
    /// Hours a whole-snapshot PASS stays valid (covers sources, not the host).
    pub snapshot_max_age_hours: u64,
}

#[derive(Debug, Clone)]
pub struct StateConfig {
    /// SQLite directory, relative to the Git common dir.
    pub path: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            toolchain_files: Vec::new(),
            log_dir: None,
            target_definitions: vec!["Makefile".into(), "*.mk".into(), "make/*.mk".into()],
            plan: PlanConfig::default(),
            run: RunConfig::default(),
            receipts: ReceiptsConfig::default(),
            state: StateConfig::default(),
            status: StatusConfig::default(),
            integrate: IntegrateConfig::default(),
            catalog: Vec::new(),
        }
    }
}

impl Default for PlanConfig {
    fn default() -> Self {
        PlanConfig {
            base: "origin/main".into(),
            profile: None,
        }
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            remote: Vec::new(),
            // The runner protocol (docs/protocol.md); CITRUS_TARGET is always read.
            progress_prefixes: Vec::new(),
            waiting_prefix: "CITRUS_WAIT ".into(),
            acquired_prefixes: vec!["CITRUS_RUNNING".into()],
            stage_prefix: "CITRUS_STAGE ".into(),
            linked_log_markers: vec!["CITRUS_LOG ".into()],
            image: None,
            private: Vec::new(),
            prepare: Vec::new(),
        }
    }
}

impl Default for ReceiptsConfig {
    fn default() -> Self {
        ReceiptsConfig {
            dir: "citrus/receipts".into(),
            max_age_days: 7,
            snapshot_max_age_hours: 24,
        }
    }
}

impl Default for StateConfig {
    fn default() -> Self {
        StateConfig {
            path: "citrus".into(),
        }
    }
}

impl Config {
    pub fn in_common(common: &Path, relative: &str) -> PathBuf {
        if Path::new(relative).is_absolute() {
            PathBuf::from(relative)
        } else {
            common.join(relative)
        }
    }
}

pub fn substitute(template: &[String], key: &str, value: &str) -> Vec<String> {
    template
        .iter()
        .map(|part| part.replace(key, value))
        .collect()
}
