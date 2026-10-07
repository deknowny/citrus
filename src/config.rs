//! Settings Citrus runs with: generic defaults, overridden by the `project`,
//! `planner` and `pool` blocks of `citrus.ci` (see `exec::Context::open`).
//! Without them the built-in planner selects declared checks owning the
//! changed paths, and state is a local SQLite file shared by all worktrees.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    /// Files every declared target's fingerprint depends on (toolchain pins).
    pub toolchain_files: Vec<String>,
    /// Where run logs go, repository-relative; should be ignored by Git.
    pub log_dir: String,
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
    /// External planner printing `TARGET\tmake:<name>` lines (plus optional
    /// PLAN/MAPPED/UNMAPPED). Empty: built-in planner over the manifest.
    pub command: Vec<String>,
    /// Extra argument for an explicit base; `{base}` is substituted.
    pub base_arg: String,
    /// Extra argument handing the external planner a file of changed paths
    /// (one per line); `{file}` is substituted. Lets `citrus integrate` ask
    /// which checks the incoming changes select. Empty: not supported.
    pub paths_arg: String,
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Command for one target on this machine; `{target}` is substituted.
    pub local: Vec<String>,
    /// Command that runs the whole planned set elsewhere (builder, CI). Empty: no remote mode.
    pub remote: Vec<String>,
    /// Extra environment per target.
    pub env: BTreeMap<String, BTreeMap<String, String>>,
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
            log_dir: ".citrus/logs".into(),
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
            command: Vec::new(),
            base_arg: String::new(),
            paths_arg: String::new(),
        }
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            local: vec![
                "make".into(),
                "--no-print-directory".into(),
                "{target}".into(),
            ],
            remote: Vec::new(),
            env: BTreeMap::new(),
            progress_prefixes: Vec::new(),
            waiting_prefix: String::new(),
            acquired_prefixes: Vec::new(),
            stage_prefix: String::new(),
            linked_log_markers: Vec::new(),
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
