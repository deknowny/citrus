//! Project configuration: `citrus.toml` at the repository root.
//!
//! Every project-specific choice lives here. Without the file Citrus works with
//! generic defaults: the built-in planner selects declared targets owning the
//! changed paths, targets run as `make <target>`, state is a local SQLite file
//! shared by all worktrees of the clone.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Declared targets, repository-relative.
    pub manifest: String,
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
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct StatusConfig {
    /// Slow command describing shared execution resources (builders, runners).
    /// `status` shows its last snapshot and refreshes it in the background.
    pub resources_command: Vec<String>,
    /// Lines of that command's output that describe one resource each.
    pub resource_prefix: String,
    /// Age after which the snapshot is refreshed.
    pub refresh_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlanConfig {
    /// Default base for "what changed" when `--base` is not given.
    pub base: String,
    /// External planner printing `TARGET\tmake:<name>` lines (plus optional
    /// PLAN/MAPPED/UNMAPPED). Empty: built-in planner over the manifest.
    pub command: Vec<String>,
    /// Extra argument for an explicit base; `{base}` is substituted.
    pub base_arg: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ReceiptsConfig {
    /// PASS receipts of declared targets; relative to the Git common dir.
    pub dir: String,
    /// Days a receipt of unchanged inputs stays valid.
    pub max_age_days: u64,
    /// Hours a whole-snapshot PASS stays valid (covers sources, not the host).
    pub snapshot_max_age_hours: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StateConfig {
    /// `sqlite`: one file shared by the worktrees of this clone.
    pub backend: String,
    /// SQLite directory, relative to the Git common dir.
    pub path: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            manifest: "ci/targets.toml".into(),
            toolchain_files: Vec::new(),
            log_dir: ".citrus/logs".into(),
            target_definitions: vec!["Makefile".into(), "*.mk".into(), "make/*.mk".into()],
            plan: PlanConfig::default(),
            run: RunConfig::default(),
            receipts: ReceiptsConfig::default(),
            state: StateConfig::default(),
            status: StatusConfig::default(),
        }
    }
}

impl Default for PlanConfig {
    fn default() -> Self {
        PlanConfig {
            base: "origin/main".into(),
            command: Vec::new(),
            base_arg: String::new(),
        }
    }
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            local: vec!["make".into(), "{target}".into()],
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
            backend: "sqlite".into(),
            path: "citrus".into(),
        }
    }
}

impl Config {
    pub fn load(root: &Path) -> Result<Config> {
        let path = root.join("citrus.toml");
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.state.backend != "sqlite" {
            bail!(
                "state.backend = {:?} is not supported yet; available: sqlite",
                self.state.backend
            );
        }
        if self.run.local.is_empty() {
            bail!("run.local must name a command");
        }
        for relative in [&self.manifest, &self.log_dir] {
            if Path::new(relative).is_absolute() || relative.split('/').any(|part| part == "..") {
                bail!("{relative}: must stay inside the repository");
            }
        }
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_generic_and_files_override_them() {
        let config: Config =
            toml::from_str("[run]\nremote = [\"make\", \"remote-check\"]\n").unwrap();
        assert_eq!(config.run.local, vec!["make", "{target}"]);
        assert_eq!(config.run.remote, vec!["make", "remote-check"]);
        assert_eq!(config.state.backend, "sqlite");
        assert!(toml::from_str::<Config>("bogus = 1\n").is_err());
    }

    #[test]
    fn unsupported_backend_is_explicit() {
        let config: Config = toml::from_str("[state]\nbackend = \"postgres\"\n").unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("not supported yet")
        );
    }
}
