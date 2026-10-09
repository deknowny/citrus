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
}

/// The plan for the changes since `base`.
pub fn compute(repo: &Repo, manifest: &Manifest, base: Option<&str>) -> Result<Plan> {
    let mut plan = compute_all(repo, manifest, base)?;
    narrow(&mut plan, repo, manifest);
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
    select(repo, manifest, paths, &fork, false, &[])
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
            crate::model::load_at(&repo.root, Some(before))
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
pub fn for_paths(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    let mut plan = for_paths_all(repo, manifest, paths, before)?;
    narrow(&mut plan, repo, manifest);
    Ok(plan)
}

fn for_paths_all(repo: &Repo, manifest: &Manifest, paths: &[String], before: &str) -> Result<Plan> {
    select(repo, manifest, paths.to_vec(), before, true, &[])
}

/// A change a `#[test]` asks about: its paths, the profile it is planned
/// in and what the signal command sees in its environment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    pub paths: Vec<String>,
    pub profile: Option<String>,
    pub env: Vec<(String, String)>,
}

/// Plans changes for `std::plan` in the checkout at `root`: the project is
/// loaded on first use, and a change without a profile is planned in the
/// project's first one, whatever the caller's environment says.
pub fn planner(root: &std::path::Path) -> impl Fn(&Change) -> Result<Plan> + '_ {
    let loaded: std::cell::OnceCell<std::result::Result<(Repo, Manifest, String), String>> =
        std::cell::OnceCell::new();
    move |change| {
        let found = loaded.get_or_init(|| {
            (|| -> Result<(Repo, Manifest, String)> {
                let mut repo = Repo::discover_at(root)?;
                let (project, sources) = crate::model::load(root)
                    .map_err(|rendered| anyhow::anyhow!("{rendered}"))?
                    .ok_or_else(|| anyhow::anyhow!("no Citrus configuration here"))?;
                if let Some(base) = &project.base {
                    repo.config.plan.base = base.clone();
                }
                repo.config.plan.profile = project.profiles.first().cloned();
                let manifest = Manifest::from_project(&project, &sources)?;
                let before = repo
                    .git(&["merge-base", &repo.config.plan.base, "HEAD"])
                    .unwrap_or_default();
                Ok((repo, manifest, before))
            })()
            .map_err(|error| format!("{error:#}"))
        });
        let (repo, manifest, before) =
            found.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?;
        for_change(repo, manifest, change, before)
    }
}

/// The plan of `change` as `citrus plan --paths-file` would make it.
pub fn for_change(repo: &Repo, manifest: &Manifest, change: &Change, before: &str) -> Result<Plan> {
    let mut repo = repo.clone();
    if let Some(profile) = &change.profile {
        repo.config.plan.profile = Some(profile.clone());
    }
    let mut paths = change.paths.clone();
    paths.sort();
    paths.dedup();
    let mut plan = select(&repo, manifest, paths, before, true, &change.env)?;
    narrow(&mut plan, &repo, manifest);
    Ok(plan)
}

/// Declared targets owning `paths`; new or edited declarations since `before` too.
/// `explicit`: the paths were given (`--paths-file`, integrate), not diffed from the base.
fn select(
    repo: &Repo,
    manifest: &Manifest,
    paths: Vec<String>,
    fork: &str,
    explicit: bool,
    env: &[(String, String)],
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
    let found = signals(repo, manifest, &paths, fork, explicit, env)?;
    // Which groups and checks the changed paths touch.
    let mut touched: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // How many changed paths each group contains, for only() and without().
    let mut group_paths: BTreeMap<String, usize> = BTreeMap::new();
    // Changed paths of each check, for `replaces`.
    let mut owned_paths: Vec<(String, Vec<String>)> = Vec::new();
    for path in &paths {
        // A path a signal claims for a group belongs to it like its own paths.
        let mut groups: Vec<&str> = manifest
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
        // A path a check names in its own `paths` is that check's: groups
        // elsewhere, and their checks, do not see it.
        let named = |name: &str| {
            manifest
                .targets
                .get(name)
                .is_some_and(|target| target.narrows || target.group.is_none())
        };
        let homes: Vec<&str> = owners
            .iter()
            .filter(|name| named(name))
            .filter_map(|name| manifest.targets[*name].group.as_deref())
            .collect();
        if owners.iter().any(|name| named(name)) {
            owners.retain(|name| {
                named(name)
                    || manifest.targets[*name]
                        .group
                        .as_deref()
                        .is_some_and(|group| homes.contains(&group))
            });
            groups.retain(|group| homes.contains(group));
        }
        // The signal command made the path one check's alone this time.
        let given: Vec<&str> = found
            .owns
            .iter()
            .filter(|(owned, _)| owned == path)
            .map(|(_, check)| check.as_str())
            .filter(|check| manifest.targets.contains_key(*check))
            .collect();
        if !given.is_empty() {
            owners = given;
            groups.clear();
        } else {
            // Checks whose Make recipes read the path run too; the path
            // stays its owners' as well.
            for target in manifest.targets.values() {
                if target.follows(path) && !owners.contains(&target.name.as_str()) {
                    owners.push(&target.name);
                }
            }
        }
        // Checks that name a group in their paths run for its paths too.
        for target in manifest.targets.values() {
            if target
                .via
                .iter()
                .any(|group| groups.contains(&group.as_str()))
                && !owners.contains(&target.name.as_str())
            {
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
        paths: &paths,
        profile: repo.config.plan.profile.as_deref(),
    };
    // Owners whose condition holds, and checks selected by condition alone;
    // repeated until stable because conditions may name selected checks.
    // An edited declaration joins like a check whose path changed: its
    // condition still decides.
    let edited_checks: Vec<String> = std::mem::take(&mut plan.targets);
    let mut selected: Vec<String> = Vec::new();
    let mut ordered: Vec<&crate::manifest::Target> = manifest.targets.values().collect();
    ordered.sort_by_key(|target| target.position);
    for _ in 0..manifest.targets.len() + 1 {
        let mut added = false;
        for target in &ordered {
            if selected.contains(&target.name)
                || ((!target.inputs.is_empty()
                    || !target.via.is_empty()
                    || target.selected_by_follows)
                    && !touched.contains(&target.name)
                    && !edited_checks.contains(&target.name))
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
/// lines, `CLAIM <path> <group>` lines that put a path into a group, and
/// `OWN <path> <check>` lines that make a path one check's alone.
struct Signals {
    names: Vec<String>,
    claims: Vec<(String, String)>,
    owns: Vec<(String, String)>,
}

fn signals(
    repo: &Repo,
    manifest: &Manifest,
    paths: &[String],
    base: &str,
    explicit: bool,
    env: &[(String, String)],
) -> Result<Signals> {
    let Some((program, args)) = manifest.signals.split_first() else {
        return Ok(Signals {
            names: Vec::new(),
            claims: Vec::new(),
            owns: Vec::new(),
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
        .envs(env.iter().cloned())
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
        owns: stdout
            .lines()
            .filter_map(|line| line.strip_prefix("OWN "))
            .filter_map(|rest| rest.trim().rsplit_once(' '))
            .map(|(path, check)| (path.to_owned(), check.to_owned()))
            .collect(),
    })
}

/// What a plan's conditions are evaluated against.
struct Facts<'a> {
    touched: &'a std::collections::BTreeSet<String>,
    signals: &'a [String],
    group_paths: &'a BTreeMap<String, usize>,
    paths: &'a [String],
    profile: Option<&'a str>,
}

impl Facts<'_> {
    /// Changed paths matching a glob list.
    fn count(&self, globs: &[String]) -> usize {
        crate::manifest::GlobList::new(globs)
            .map(|list| self.paths.iter().filter(|path| list.matches(path)).count())
            .unwrap_or(0)
    }

    /// Changed paths in a group or glob list.
    fn within(&self, set: &crate::model::Paths) -> usize {
        use crate::model::Paths;
        match set {
            Paths::Name(name) => self.group_paths.get(name).copied().unwrap_or(0),
            Paths::Globs(globs) => self.count(globs),
        }
    }

    fn holds(&self, when: &crate::model::Cond, selected: &[String]) -> bool {
        use crate::model::{Cond, Paths};
        when.eval(&|fact| match fact {
            Cond::Touched(Paths::Name(name)) => self.touched.contains(name),
            Cond::Touched(set) => self.within(set) > 0,
            Cond::Selected(name) => selected.contains(name),
            Cond::Signal(name) => self.signals.contains(name),
            Cond::Profile(name) => self.profile == Some(name.as_str()),
            Cond::Only(set) => !self.paths.is_empty() && self.within(set) == self.paths.len(),
            Cond::Without(set) => self.within(set) == 0,
            _ => false,
        })
    }
}
