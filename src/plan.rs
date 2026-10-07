//! Which checks the current changes need: the declared checks owning the
//! changed paths, with their conditions (docs/design/planner.md).

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Result, bail};
use serde::Serialize;

use crate::manifest::Manifest;
use crate::repo::Repo;

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub status: String,
    pub files: usize,
    /// Checks to run, in declaration order.
    pub targets: Vec<String>,
    /// Non-runnable plan entries such as `no-heavy:docs`.
    pub notes: Vec<String>,
    /// (path, surfaces or `target:<name>`).
    pub mapped: Vec<(String, String)>,
    pub unmapped: Vec<String>,
    /// Groups the changed paths touch (built-in planner).
    pub groups: Vec<String>,
    /// Signals the project's signal command printed.
    pub signals: Vec<String>,
    /// Labels whose condition holds.
    pub labels: Vec<String>,
    /// `match changed` arm chosen for a check (its index); absent: `_`.
    pub arms: BTreeMap<String, usize>,
}

/// The plan for the changes since `base`; `manifest` takes the `match
/// changed` arms it chose, so fingerprints and runs follow them.
pub fn compute(repo: &Repo, manifest: &mut Manifest, base: Option<&str>) -> Result<Plan> {
    let mut plan = compute_all(repo, manifest, base)?;
    narrow(&mut plan, repo, manifest);
    manifest.choose_arms(&plan.arms);
    Ok(plan)
}

/// Drops declared checks outside the current profile, and checks covered by
/// another check in the plan (docs/design/planner.md).
fn narrow(plan: &mut Plan, repo: &Repo, manifest: &Manifest) {
    let profile = repo.config.plan.profile.as_deref();
    let selected = plan.targets.clone();
    plan.targets.retain(|name| {
        let Some(target) = manifest.targets.get(name) else {
            return true;
        };
        let in_profile = target.profiles.is_empty()
            || profile.is_some_and(|profile| target.profiles.iter().any(|item| item == profile));
        let covered = target
            .covered_by
            .iter()
            .any(|cover| cover != name && selected.contains(cover));
        if !in_profile {
            plan.notes.push(format!("profile:{name}"));
        } else if covered {
            plan.notes.push(format!("covered:{name}"));
        }
        in_profile && !covered
    });
}

fn compute_all(repo: &Repo, manifest: &Manifest, base: Option<&str>) -> Result<Plan> {
    let fork = fork_point(repo, base.unwrap_or(&repo.config.plan.base));
    let paths = changed_paths(repo, &fork)?;
    select(repo, manifest, paths, &fork, false)
}

fn fork_point(repo: &Repo, base: &str) -> String {
    repo.git(&["merge-base", base, "HEAD"])
        .or_else(|_| repo.git(&["rev-parse", "HEAD"]))
        .unwrap_or_default()
}

/// Paths changed since `fork`, including untracked files.
fn changed_paths(repo: &Repo, fork: &str) -> Result<Vec<String>> {
    let mut paths: Vec<String> = Vec::new();
    if !fork.is_empty() {
        paths.extend(
            repo.git(&["diff", "--name-only", fork])?
                .lines()
                .map(str::to_owned),
        );
    }
    paths.extend(
        repo.git(&["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_owned),
    );
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Declared checks that are new or declared differently than at `before`.
fn edited_checks(repo: &Repo, manifest: &Manifest, before: &str) -> Vec<String> {
    let old = (!before.is_empty())
        .then(|| {
            crate::lang::compile::load_at(&repo.root, Some(before))
                .ok()
                .flatten()
        })
        .flatten()
        .and_then(|(project, sources)| Manifest::from_project(&project, &sources).ok())
        .unwrap_or_default();
    manifest
        .targets
        .values()
        .filter(|target| {
            old.targets
                .get(&target.name)
                .is_none_or(|previous| !previous.same_declaration(target))
        })
        .map(|target| target.name.clone())
        .collect()
}

/// The checks that changes to exactly `paths` would select; `before` is the
/// revision those changes start from (for manifest edits).
pub fn for_paths(
    repo: &Repo,
    manifest: &mut Manifest,
    paths: &[String],
    before: &str,
) -> Result<Plan> {
    let mut plan = for_paths_all(repo, manifest, paths, before)?;
    narrow(&mut plan, repo, manifest);
    manifest.choose_arms(&plan.arms);
    Ok(plan)
}

fn for_paths_all(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    select(repo, manifest, paths.to_vec(), before, true)
}

/// Declared targets owning `paths`; new or edited declarations since `before` too.
/// `explicit`: the paths were given (`--paths-file`, integrate), not diffed from the base.
fn select(
    repo: &Repo,
    manifest: &Manifest,
    paths: Vec<String>,
    fork: &str,
    explicit: bool,
) -> Result<Plan> {
    let mut plan = Plan {
        files: paths.len(),
        ..Plan::default()
    };
    let mut edited_surfaces = String::new();
    let edited: Vec<String> = paths
        .iter()
        .filter(|path| manifest.files.contains(path))
        .cloned()
        .collect();
    if !edited.is_empty() {
        // New or edited declarations are checked right away.
        let changed = edited_checks(repo, manifest, fork);
        let surfaces = if changed.is_empty() {
            "config".to_owned()
        } else {
            changed
                .iter()
                .map(|name| format!("target:{name}"))
                .collect::<Vec<_>>()
                .join(",")
        };
        edited_surfaces = surfaces;
        plan.targets.extend(changed);
    }
    let found = signals(repo, manifest, &paths, fork, explicit)?;
    // Which groups and checks the changed paths touch.
    let mut touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // How many changed paths each group contains, for only() and without().
    let mut group_paths: BTreeMap<String, usize> = BTreeMap::new();
    // Changed paths of each check, for `replaces`.
    let mut owned_paths: Vec<(String, Vec<String>)> = Vec::new();
    for path in &paths {
        // A path a signal claims for a group belongs to it like its own paths.
        let groups: Vec<&str> = manifest
            .groups
            .iter()
            .filter(|group| {
                group.owns(path)
                    || found
                        .claims
                        .iter()
                        .any(|(claimed, name)| claimed == path && *name == group.name)
            })
            .map(|group| group.name.as_str())
            .collect();
        // Its checks: those owning it, and the checks of a group that claimed
        // it which do not narrow the group's paths.
        let mut owners: Vec<&str> = manifest.owners(path);
        for target in manifest.targets.values() {
            let claimed = !target.narrows
                && target
                    .group
                    .as_deref()
                    .is_some_and(|group| groups.contains(&group));
            if claimed && !owners.contains(&target.name.as_str()) {
                owners.push(&target.name);
            }
        }
        touched.extend(owners.iter().map(|name| (*name).to_owned()));
        touched.extend(groups.iter().map(|name| (*name).to_owned()));
        for group in &groups {
            *group_paths.entry((*group).to_owned()).or_insert(0) += 1;
        }
        owned_paths.push((
            path.clone(),
            owners.iter().map(|name| (*name).to_owned()).collect(),
        ));
        if owners.is_empty() && groups.is_empty() {
            // An edited .ci file nobody owns maps to the checks it changed.
            if edited.contains(path) {
                plan.mapped.push((path.clone(), edited_surfaces.clone()));
            } else {
                plan.unmapped.push(path.clone());
            }
            continue;
        }
        plan.mapped.push((
            path.clone(),
            owners
                .iter()
                .map(|owner| format!("target:{owner}"))
                .chain(groups.iter().map(|group| format!("group:{group}")))
                .collect::<Vec<_>>()
                .join(","),
        ));
    }
    let signals = found.names;
    let facts = Facts {
        touched: &touched,
        signals: &signals,
        group_paths: &group_paths,
        changed: paths.len(),
        profile: repo.config.plan.profile.as_deref(),
    };
    // Owners whose condition holds, and checks selected by condition alone;
    // repeated until stable because conditions may name selected checks.
    let mut selected: Vec<String> = plan.targets.clone();
    let mut ordered: Vec<&crate::manifest::Target> = manifest.targets.values().collect();
    ordered.sort_by_key(|target| target.position);
    for _ in 0..manifest.targets.len() + 1 {
        let mut added = false;
        for target in &ordered {
            if selected.contains(&target.name)
                || (!target.inputs.is_empty() && !touched.contains(&target.name))
            {
                continue;
            }
            if target
                .when
                .as_ref()
                .is_none_or(|when| facts.holds(when, &selected))
            {
                selected.push(target.name.clone());
                added = true;
            }
        }
        if !added {
            break;
        }
    }
    // A check that `replaces` others runs instead of them when the change
    // goes beyond what one of them owns; otherwise they run and it does not.
    for target in ordered.iter().filter(|target| !target.replaces.is_empty()) {
        if !selected.contains(&target.name) {
            continue;
        }
        let parts: Vec<&String> = target
            .replaces
            .iter()
            .filter(|part| selected.contains(part))
            .collect();
        let beyond = owned_paths.iter().any(|(_, owners)| {
            owners.contains(&target.name)
                && !owners.iter().any(|owner| target.replaces.contains(owner))
        });
        if beyond || parts.len() > 1 {
            selected.retain(|name| !target.replaces.contains(name));
        } else {
            selected.retain(|name| *name != target.name);
        }
    }
    for name in &selected {
        let Some(target) = manifest.targets.get(name) else {
            continue;
        };
        if let Some(index) = target
            .arms
            .iter()
            .position(|(when, _)| facts.holds(when, &selected))
        {
            plan.arms.insert(name.clone(), index);
        }
    }
    plan.targets = selected;
    plan.groups = manifest
        .groups
        .iter()
        .filter(|group| touched.contains(&group.name))
        .map(|group| group.name.clone())
        .collect();
    plan.labels = manifest
        .labels
        .iter()
        .filter(|(_, when)| facts.holds(when, &plan.targets))
        .map(|(name, _)| name.clone())
        .collect();
    plan.signals = signals.clone();
    plan.status = if plan.unmapped.is_empty() {
        "complete".into()
    } else {
        "partial".into()
    };
    Ok(plan)
}

/// What the project's signal command says about these paths: `SIGNAL <name>`
/// lines, and `CLAIM <path> <group>` lines that put a path into a group.
struct Signals {
    names: Vec<String>,
    claims: Vec<(String, String)>,
}

fn signals(
    repo: &Repo,
    manifest: &Manifest,
    paths: &[String],
    base: &str,
    explicit: bool,
) -> Result<Signals> {
    let Some((program, args)) = manifest.signals.split_first() else {
        return Ok(Signals {
            names: Vec::new(),
            claims: Vec::new(),
        });
    };
    let dir = repo.state_dir().join("tmp");
    crate::repo::private_dir(&dir)?;
    let file = dir.join(format!(
        "signal-paths-{}-{}",
        std::process::id(),
        crate::manifest::now()
    ));
    std::fs::write(
        &file,
        paths
            .iter()
            .map(|path| format!("{path}\n"))
            .collect::<String>(),
    )?;
    let output = Command::new(program)
        .args(args)
        .current_dir(&repo.root)
        .env("CITRUS_PATHS", &file)
        .env("CITRUS_BASE", base)
        .env("CITRUS_PATHS_EXPLICIT", if explicit { "1" } else { "0" })
        .env(
            "CITRUS_PROFILE",
            repo.config.plan.profile.clone().unwrap_or_default(),
        )
        .output();
    let _ = std::fs::remove_file(&file);
    let output = output?;
    if !output.status.success() {
        bail!(
            "signal command {} failed: {}",
            manifest.signals.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(Signals {
        names: stdout
            .lines()
            .filter_map(|line| line.strip_prefix("SIGNAL "))
            .map(|name| name.trim().to_owned())
            .collect(),
        claims: stdout
            .lines()
            .filter_map(|line| line.strip_prefix("CLAIM "))
            .filter_map(|rest| rest.trim().rsplit_once(' '))
            .map(|(path, group)| (path.to_owned(), group.to_owned()))
            .collect(),
    })
}

/// What a plan's conditions are evaluated against.
struct Facts<'a> {
    touched: &'a std::collections::BTreeSet<String>,
    signals: &'a [String],
    group_paths: &'a BTreeMap<String, usize>,
    changed: usize,
    profile: Option<&'a str>,
}

impl Facts<'_> {
    fn holds(&self, when: &crate::lang::compile::Cond, selected: &[String]) -> bool {
        use crate::lang::compile::Cond;
        when.eval(&|fact| match fact {
            Cond::Touched(name) => self.touched.contains(name),
            Cond::Selected(name) => selected.contains(name),
            Cond::Signal(name) => self.signals.contains(name),
            Cond::Profile(name) => self.profile == Some(name.as_str()),
            Cond::Only(name) => {
                self.changed > 0 && self.group_paths.get(name).copied() == Some(self.changed)
            }
            Cond::Without(name) => !self.group_paths.contains_key(name),
            _ => false,
        })
    }
}
