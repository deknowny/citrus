//! Declarative releases, read side: artifacts keyed by their inputs at any
//! commit, environments observed through a provider, and `citrus diff`
//! comparing what runs with what HEAD would run (docs/design/declarative.md).

use std::collections::BTreeMap;

use std::process::Command;

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::exec::Context;
use crate::manifest::pattern_matches_any;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// Repository files the artifact is built from (globs); with the build and
    /// publish settings they make up its key.
    #[serde(default)]
    pub inputs: Vec<String>,
    /// Or: a command printing the input paths (one per line), for inputs a
    /// glob cannot express (a dependency closure). Run once at HEAD; the same
    /// paths are compared at every commit.
    #[serde(default)]
    pub inputs_command: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// A multi-stage Dockerfile among the inputs counts only with the stages
    /// this target is built from.
    #[serde(default)]
    pub dockerfile: Option<DockerfileScope>,
    /// Provider-specific build settings (opaque to the core, part of the key).
    #[serde(default)]
    pub build: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub publish: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DockerfileScope {
    pub file: String,
    pub target: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub provider: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub connection: BTreeMap<String, String>,
    /// Where the running release is recorded and how its commit is found.
    #[serde(default)]
    pub record: Record,
    pub workloads: BTreeMap<String, Workload>,
    /// `required` (default): apply needs --approve. `none`: no approval.
    #[serde(default = "required")]
    pub approval: String,
    /// `proven` (default): checks the plan selects must be proven for HEAD.
    #[serde(default = "proven_checks")]
    pub checks: String,
    /// Name recorded for a release; `{short}`, `{commit}` substituted.
    #[serde(default = "short_name")]
    pub release_name: String,
    /// Runs before builds and changes (credentials, logins); may print
    /// `KEY=value` lines that become environment variables of later steps.
    #[serde(default)]
    pub prepare: Vec<String>,
    #[serde(default)]
    pub migrations: Option<Migrations>,
    #[serde(default)]
    pub verify: Verify,
}

fn required() -> String {
    "required".into()
}

fn proven_checks() -> String {
    "proven".into()
}

fn short_name() -> String {
    "{short}".into()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Migrations {
    pub artifact: String,
    /// Kubernetes Job manifest template; `{image}` and `{name}` are substituted.
    pub job: String,
    #[serde(default = "five_minutes")]
    pub timeout: u64,
}

fn five_minutes() -> u64 {
    300
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Verify {
    /// URLs that must answer 2xx.
    pub http: Vec<String>,
    /// Commands that must succeed (run in the repository).
    pub commands: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Record {
    /// Annotation on a workload holding the release name (version).
    pub annotation: String,
    /// Workload carrying the annotation (default: the first workload).
    pub workload: String,
    /// Annotation holding the commit directly, when the deployer writes one.
    pub commit_annotation: String,
    /// Git tag of a release: this prefix + the release name.
    pub tag_prefix: String,
    /// Command printing the commit of a release; `{release}` is substituted.
    pub resolve: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    pub artifact: String,
    #[serde(default = "deployment")]
    pub kind: String,
    /// Container running the artifact (default: the workload name).
    #[serde(default)]
    pub container: String,
    /// A Lease the old instance holds: after the change, wait until a new holder took it.
    #[serde(default)]
    pub fence: Option<String>,
    /// CronJobs: suspended while the environment changes, restored after.
    #[serde(default)]
    pub quiesce: bool,
    /// An environment variable of the container set to the release name
    /// whenever the image changes (the version the program reports).
    #[serde(default)]
    pub version_env: String,
    /// A manifest file holding this workload (and what goes with it): applied
    /// whole with the workload's image set to the build, so changes to its
    /// spec roll out too. Declare it among the artifact's inputs.
    #[serde(default)]
    pub manifest: String,
    #[serde(default = "five_minutes")]
    pub timeout: u64,
}

/// Annotations Citrus writes on every workload it changes.
pub const COMMIT_ANNOTATION: &str = "citrus.dev/commit";
pub const KEY_ANNOTATION: &str = "citrus.dev/key";

fn deployment() -> String {
    "deployment".into()
}

/// Artifacts declared in `citrus.ci`.
pub fn artifacts(context: &Context) -> Result<BTreeMap<String, Artifact>> {
    let artifacts = context
        .project
        .as_ref()
        .map(|project| project.artifacts.clone())
        .unwrap_or_default();
    for (name, artifact) in &artifacts {
        if artifact.inputs.is_empty() == artifact.inputs_command.is_empty() {
            bail!("artifact {name}: give it #[inputs(\"glob\", …)] or #[inputs(cmd!(\"…\"))]");
        }
    }
    Ok(artifacts)
}

/// Environments declared in `citrus.ci`.
pub fn environments(context: &Context) -> Result<BTreeMap<String, Environment>> {
    let environments = context
        .project
        .as_ref()
        .map(|project| project.environments.clone())
        .unwrap_or_default();
    let artifacts = artifacts(context)?;
    for (name, environment) in &environments {
        for (workload, spec) in &environment.workloads {
            if !artifacts.contains_key(&spec.artifact) {
                bail!(
                    "environment {name}: deploy {workload} names unknown artifact {}",
                    spec.artifact
                );
            }
        }
    }
    Ok(environments)
}

/// Input paths of an artifact given by its `inputs_command`, if it has one.
pub fn command_paths(context: &Context, artifact: &Artifact) -> Result<Option<Vec<String>>> {
    let Some((program, args)) = artifact.inputs_command.split_first() else {
        return Ok(None);
    };
    let output = Command::new(program)
        .args(args)
        .current_dir(&context.repo.root)
        .output()?;
    if !output.status.success() {
        bail!(
            "inputs_command {} failed: {}",
            artifact.inputs_command.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    paths.sort();
    paths.dedup();
    Ok(Some(paths))
}

/// Content key of an artifact at `commit`: its input files (path, mode, blob
/// id) and its declaration, from Git objects without a checkout. `paths`
/// replaces the globs when the artifact lists its inputs by command.
pub fn key_at(
    context: &Context,
    name: &str,
    artifact: &Artifact,
    commit: &str,
    paths: Option<&[String]>,
) -> Result<(String, Vec<(String, String)>)> {
    let listing = context
        .repo
        .git(&["ls-tree", "-r", "--full-tree", commit])?;
    let mut tree: BTreeMap<&str, String> = BTreeMap::new();
    for line in listing.lines() {
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or_default();
        let blob = parts.nth(1).unwrap_or_default();
        tree.insert(path, format!("{mode} {blob}"));
    }
    let mut files: Vec<(String, String)> = match paths {
        Some(paths) => paths
            .iter()
            .map(|path| {
                (
                    path.clone(),
                    tree.get(path.as_str())
                        .cloned()
                        .unwrap_or_else(|| "missing".into()),
                )
            })
            .collect(),
        None => tree
            .iter()
            .filter(|(path, _)| {
                artifact.inputs.iter().any(|pattern| {
                    pattern_matches_any(pattern, &[path.to_string()]).unwrap_or(false)
                })
            })
            .map(|(path, identity)| ((*path).to_owned(), identity.clone()))
            .collect(),
    };
    files.sort();
    if let Some(scope) = &artifact.dockerfile {
        for (path, identity) in files.iter_mut() {
            if *path == scope.file && identity != "missing" {
                let text = context.repo.git(&["show", &format!("{commit}:{path}")])?;
                let scoped = crate::dockerfile::scope(&text, &scope.target)
                    .with_context(|| format!("artifact {name}: {path}"))?;
                *identity = format!("stages {}", hex::encode(Sha256::digest(scoped)));
            }
        }
    }
    let mut digest = Sha256::new();
    digest.update(serde_json::to_string(&(name, artifact))?.as_bytes());
    for (path, identity) in &files {
        digest.update(format!("{path}\0{identity}\0").as_bytes());
    }
    Ok((hex::encode(digest.finalize()), files))
}

#[derive(Debug, Serialize)]
pub struct Observed {
    pub workload: String,
    pub image: Option<String>,
    pub digest: Option<String>,
    pub ready: bool,
    pub detail: String,
    pub annotations: BTreeMap<String, String>,
}

/// Ask the environment's provider what runs.
pub fn observe(environment: &Environment) -> Result<Vec<Observed>> {
    match environment.provider.as_str() {
        "kubernetes" => environment
            .workloads
            .iter()
            .map(|(name, workload)| observe_kubernetes(environment, name, workload))
            .collect(),
        other => bail!("provider {other:?} cannot observe yet (available: kubernetes)"),
    }
}

pub fn kubectl(environment: &Environment) -> Command {
    let mut command = Command::new(
        environment
            .connection
            .get("kubectl")
            .map_or("kubectl", String::as_str),
    );
    if let Some(context) = environment.connection.get("context") {
        command.arg("--context").arg(context);
    }
    if let Some(kubeconfig) = environment.connection.get("kubeconfig") {
        command.arg("--kubeconfig").arg(kubeconfig);
    }
    if let Some(namespace) = environment.connection.get("namespace") {
        command.arg("--namespace").arg(namespace);
    }
    command
}

fn observe_kubernetes(
    environment: &Environment,
    name: &str,
    workload: &Workload,
) -> Result<Observed> {
    let output = kubectl(environment)
        .args(["get", &workload.kind, name, "-o", "json"])
        .output()?;
    if !output.status.success() {
        bail!(
            "kubectl get {} {name}: {}",
            workload.kind,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let item: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let container = if workload.container.is_empty() {
        name
    } else {
        workload.container.as_str()
    };
    let pod_spec = if workload.kind == "cronjob" {
        &item["spec"]["jobTemplate"]["spec"]["template"]["spec"]
    } else {
        &item["spec"]["template"]["spec"]
    };
    let image = pod_spec["containers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| entry["name"] == container)
        .and_then(|entry| entry["image"].as_str())
        .map(str::to_owned);
    let digest = image
        .as_deref()
        .and_then(|image| image.split_once('@'))
        .map(|(_, digest)| digest.to_owned());
    let annotations: BTreeMap<String, String> = item["metadata"]["annotations"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, _)| key.as_str() != "kubectl.kubernetes.io/last-applied-configuration")
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
        .collect();
    let (ready, detail) = if workload.kind == "deployment" {
        let wanted = item["spec"]["replicas"].as_i64().unwrap_or(1);
        let ready = item["status"]["readyReplicas"].as_i64().unwrap_or(0);
        let synced = item["status"]["observedGeneration"] == item["metadata"]["generation"];
        (
            ready == wanted && synced,
            format!(
                "{ready}/{wanted} ready{}",
                if synced { "" } else { ", rollout pending" }
            ),
        )
    } else {
        (true, workload.kind.clone())
    };
    Ok(Observed {
        workload: name.to_owned(),
        image,
        digest,
        ready,
        detail,
        annotations,
    })
}

/// The release a declared environment runs now (its record annotation), or
/// None when the environment is not declared, records nothing or cannot be read.
pub fn running_release(context: &Context, name: &str) -> Option<String> {
    let environments = environments(context).ok()?;
    let environment = environments.get(name)?;
    let record = &environment.record;
    if record.annotation.is_empty() {
        return None;
    }
    let observed = observe(environment).ok()?;
    let holder = if record.workload.is_empty() {
        observed.first()
    } else {
        observed
            .iter()
            .find(|item| item.workload == record.workload)
    }?;
    holder.annotations.get(&record.annotation).cloned()
}

/// The commit of the running release, and how it was found.
fn running_commit(
    context: &Context,
    environment_name: &str,
    environment: &Environment,
    observed: &[Observed],
) -> Result<(Option<String>, Option<String>, String)> {
    let record = &environment.record;
    let holder = if record.workload.is_empty() {
        observed.first()
    } else {
        observed
            .iter()
            .find(|item| item.workload == record.workload)
    };
    let Some(holder) = holder else {
        return Ok((None, None, "no workload observed".into()));
    };
    if let Some(commit) = holder.annotations.get(COMMIT_ANNOTATION) {
        return Ok((
            holder.annotations.get(&record.annotation).cloned(),
            Some(commit.clone()),
            format!("annotation {COMMIT_ANNOTATION}"),
        ));
    }
    if !record.commit_annotation.is_empty()
        && let Some(commit) = holder.annotations.get(&record.commit_annotation)
    {
        return Ok((
            None,
            Some(commit.clone()),
            format!("annotation {}", record.commit_annotation),
        ));
    }
    let release = holder.annotations.get(&record.annotation).cloned();
    let Some(release) = release else {
        return Ok((
            None,
            None,
            if record.annotation.is_empty() {
                "no record configured".into()
            } else {
                format!("no annotation {}", record.annotation)
            },
        ));
    };
    if let Some(found) = context
        .store
        .releases_of(environment_name, 50)?
        .into_iter()
        .find(|item| item.version == release && item.state == "passed")
    {
        return Ok((Some(release), Some(found.commit), "citrus history".into()));
    }
    if !record.tag_prefix.is_empty()
        && let Ok(commit) = context.repo.git(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{}{release}^{{commit}}", record.tag_prefix),
        ])
    {
        let found_by = format!("tag {}{}", record.tag_prefix, release);
        return Ok((Some(release), Some(commit), found_by));
    }
    if let Some((program, args)) = record.resolve.split_first() {
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.replace("{release}", &release))
            .collect();
        let output = Command::new(program)
            .args(&args)
            .current_dir(&context.repo.root)
            .output()?;
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if output.status.success() && commit.len() >= 7 {
            let commit = context
                .repo
                .git(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{commit}^{{commit}}"),
                ])
                .unwrap_or(commit);
            return Ok((Some(release), Some(commit), "resolve hook".into()));
        }
    }
    Ok((
        Some(release),
        None,
        "release name has no known commit".into(),
    ))
}

#[derive(Debug, Serialize)]
pub struct WorkloadDiff {
    pub workload: String,
    pub artifact: String,
    pub running_digest: Option<String>,
    pub ready: bool,
    pub detail: String,
    /// `unchanged`, `changed`, or `unknown` (running commit not known).
    pub change: String,
    pub changed_inputs: Vec<String>,
    pub desired_key: String,
}

#[derive(Debug, Serialize)]
pub struct Diff {
    pub environment: String,
    pub head: String,
    pub running_release: Option<String>,
    pub running_commit: Option<String>,
    pub found_by: String,
    pub workloads: Vec<WorkloadDiff>,
    /// What an apply would do, in order.
    pub actions: Vec<String>,
    pub plan_hash: String,
}

pub fn diff(context: &Context, name: &str) -> Result<Diff> {
    let environments = environments(context)?;
    let environment = environments.get(name).with_context(|| {
        format!(
            "no environment {name}; declared: {:?}",
            environments.keys().collect::<Vec<_>>()
        )
    })?;
    let artifacts = artifacts(context)?;
    let observed = observe(environment)?;
    let (running_release, running_commit, found_by) =
        running_commit(context, name, environment, &observed)?;
    let head = context.repo.git(&["rev-parse", "HEAD"])?;
    let mut workloads = Vec::new();
    let mut actions = Vec::new();
    for item in &observed {
        let spec = &environment.workloads[&item.workload];
        let artifact = &artifacts[&spec.artifact];
        let paths = command_paths(context, artifact)?;
        let (desired_key, desired_files) =
            key_at(context, &spec.artifact, artifact, &head, paths.as_deref())?;
        let (change, changed_inputs) = match &running_commit {
            Some(commit) => {
                let (running_key, running_files) =
                    key_at(context, &spec.artifact, artifact, commit, paths.as_deref())?;
                if running_key == desired_key {
                    ("unchanged".to_owned(), Vec::new())
                } else {
                    let mut changed: Vec<String> = desired_files
                        .iter()
                        .filter(|file| !running_files.contains(file))
                        .map(|(path, _)| path.clone())
                        .collect();
                    changed.extend(
                        running_files
                            .iter()
                            .filter(|(path, _)| {
                                !desired_files.iter().any(|(other, _)| other == path)
                            })
                            .map(|(path, _)| format!("{path} (removed)")),
                    );
                    ("changed".to_owned(), changed)
                }
            }
            None => ("unknown".to_owned(), Vec::new()),
        };
        if change != "unchanged" {
            actions.push(format!(
                "build and publish {} ({})",
                spec.artifact,
                &desired_key[..12]
            ));
            actions.push(format!("roll {} {} to it", spec.kind, item.workload));
        }
        if !item.ready {
            actions.insert(
                0,
                format!(
                    "{} {} is not ready ({}): reconcile before changing it",
                    spec.kind, item.workload, item.detail
                ),
            );
        }
        workloads.push(WorkloadDiff {
            workload: item.workload.clone(),
            artifact: spec.artifact.clone(),
            running_digest: item.digest.clone(),
            ready: item.ready,
            detail: item.detail.clone(),
            change,
            changed_inputs,
            desired_key,
        });
    }
    let plan_hash = hex::encode(Sha256::digest(
        serde_json::to_string(&(name, &head, &actions, &running_commit))?.as_bytes(),
    ));
    Ok(Diff {
        environment: name.to_owned(),
        head,
        running_release,
        running_commit,
        found_by,
        workloads,
        actions,
        plan_hash,
    })
}
