//! `citrus apply`: make an environment run what HEAD builds.
//!
//! The plan comes from `diff`; artifacts are built only for input keys not
//! built before; workloads change by digest with Citrus records on them; the
//! steps are recorded like a release, so `release show/log/resume/abandon`
//! work for applies too. Running `apply` again after an interruption observes
//! first, so finished work is not repeated.

use std::collections::BTreeMap;
use std::fs;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use crate::deploy::{self, COMMIT_ANNOTATION, Environment, KEY_ANNOTATION};
use crate::exec::{Context, agent, read_log, segment};
use crate::manifest::now;
use crate::report::{self, compact_utc};
use crate::state::Release;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Build {
    artifact: String,
    key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Roll {
    workload: String,
    kind: String,
    container: String,
    artifact: String,
    key: String,
    fence: Option<String>,
    timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Plan {
    environment: String,
    commit: String,
    release_name: String,
    builds: Vec<Build>,
    migrate: Option<String>,
    quiesce: Vec<String>,
    rolls: Vec<Roll>,
}

#[derive(Debug)]
pub struct Request {
    pub environment: String,
    pub approve: bool,
    pub unchecked: bool,
    pub plan: Option<String>,
}

/// Plan and gates; returns None when the environment already runs HEAD's build.
pub fn start(context: &Context, request: &Request) -> Result<Option<Release>> {
    let environments = deploy::environments(context)?;
    let environment = environments
        .get(&request.environment)
        .with_context(|| format!("no environment {}", request.environment))?;
    let repo = &context.repo;
    if !repo
        .git(&["status", "--porcelain", "--untracked-files=no"])?
        .is_empty()
    {
        bail!("commit the source first: apply builds from a commit");
    }
    if environment.approval != "none" && !request.approve {
        bail!(
            "{} changes a protected environment: pass --approve",
            request.environment
        );
    }
    let diff = deploy::diff(context, &request.environment)?;
    if let Some(expected) = &request.plan
        && !diff.plan_hash.starts_with(expected.as_str())
    {
        bail!(
            "the plan changed since it was reviewed ({} now): run citrus diff {} again",
            &diff.plan_hash[..12],
            request.environment
        );
    }
    if diff.workloads.iter().all(|item| item.change == "unchanged") {
        return Ok(None);
    }
    if environment.checks == "proven" && !request.unchecked {
        let needed = crate::release::unproven_checks(context)?;
        if !needed.is_empty() {
            bail!(
                "checks not proven for this commit: {} — run `citrus run` first (or --unchecked)",
                needed.join(", ")
            );
        }
    }
    let artifacts = deploy::artifacts(context)?;
    let mut builds: Vec<Build> = Vec::new();
    let mut rolls = Vec::new();
    for item in diff
        .workloads
        .iter()
        .filter(|item| item.change != "unchanged")
    {
        let spec = &environment.workloads[&item.workload];
        if !builds.iter().any(|build| build.artifact == spec.artifact) {
            builds.push(Build {
                artifact: spec.artifact.clone(),
                key: item.desired_key.clone(),
            });
        }
        rolls.push(Roll {
            workload: item.workload.clone(),
            kind: spec.kind.clone(),
            container: if spec.container.is_empty() {
                item.workload.clone()
            } else {
                spec.container.clone()
            },
            artifact: spec.artifact.clone(),
            key: item.desired_key.clone(),
            fence: spec.fence.clone(),
            timeout: spec.timeout,
        });
    }
    let migrate = match &environment.migrations {
        Some(migrations) => {
            let artifact = artifacts
                .get(&migrations.artifact)
                .with_context(|| format!("unknown migrations artifact {}", migrations.artifact))?;
            let paths = deploy::command_paths(context, artifact)?;
            let (key, _) = deploy::key_at(
                context,
                &migrations.artifact,
                artifact,
                &diff.head,
                paths.as_deref(),
            )?;
            let running = match &diff.running_commit {
                Some(commit) => Some(
                    deploy::key_at(
                        context,
                        &migrations.artifact,
                        artifact,
                        commit,
                        paths.as_deref(),
                    )?
                    .0,
                ),
                None => None,
            };
            if running.as_deref() == Some(key.as_str()) {
                None
            } else {
                if !builds
                    .iter()
                    .any(|build| build.artifact == migrations.artifact)
                {
                    builds.push(Build {
                        artifact: migrations.artifact.clone(),
                        key,
                    });
                }
                Some(migrations.artifact.clone())
            }
        }
        None => None,
    };
    let quiesce: Vec<String> = environment
        .workloads
        .iter()
        .filter(|(_, spec)| spec.quiesce)
        .map(|(name, _)| name.clone())
        .collect();
    let short = &diff.head[..12.min(diff.head.len())];
    let release_name = environment
        .release_name
        .replace("{short}", short)
        .replace("{commit}", &diff.head);
    let plan = Plan {
        environment: request.environment.clone(),
        commit: diff.head.clone(),
        release_name,
        builds,
        migrate,
        quiesce,
        rolls,
    };

    let mut steps = Vec::new();
    if !environment.prepare.is_empty() {
        steps.push("prepare".to_owned());
    }
    steps.extend(
        plan.builds
            .iter()
            .map(|build| format!("build:{}", build.artifact)),
    );
    if !plan.quiesce.is_empty() {
        steps.push("quiesce".into());
    }
    if plan.migrate.is_some() {
        steps.push("migrate".into());
    }
    steps.extend(
        plan.rolls
            .iter()
            .map(|roll| format!("roll:{}", roll.workload)),
    );
    if !plan.quiesce.is_empty() {
        steps.push("resume".into());
    }
    steps.push("verify".into());

    let started = now();
    let id = format!(
        "apply-{}-{}-{:04x}",
        request.environment,
        compact_utc(started),
        crate::exec::random16()
    );
    crate::repo::private_dir(&repo.log_dir())?;
    let log = repo.log_dir().join(format!("{id}.log"));
    fs::write(
        log.with_extension("plan.json"),
        serde_json::to_string_pretty(&plan)?,
    )?;
    let previous = context
        .store
        .releases_of(&request.environment, 20)?
        .into_iter()
        .find(|item| item.state == "passed")
        .map(|item| item.version)
        .unwrap_or_default();
    let release = Release {
        id: id.clone(),
        unit: request.environment.clone(),
        kind: "apply".into(),
        environment: request.environment.clone(),
        version: plan.release_name.clone(),
        previous,
        commit: plan.commit.clone(),
        worktree: context.worktree(),
        agent: agent(),
        state: "queued".into(),
        note: if request.unchecked {
            "applied without the check gate (--unchecked)".into()
        } else {
            String::new()
        },
        pid: None,
        started: started as i64,
        ended: None,
        log: log.display().to_string(),
    };
    context.store.insert_release(&release, &steps)?;
    if let Some(holder) = context.store.lock_environment(&request.environment, &id)? {
        context.store.finish_release(
            &id,
            "cancelled",
            &format!("{} is held by {holder}", request.environment),
        )?;
        bail!(
            "{} is held by {holder}: wait for it, or `citrus release resume/abandon {holder}`",
            request.environment
        );
    }
    crate::release::spawn(context, &release)?;
    Ok(Some(
        context.store.release(&id)?.context("apply disappeared")?,
    ))
}

/// Runs in the detached worker; stdout and stderr are the apply log.
pub fn work(context: &Context, id: &str) -> Result<()> {
    let release = context.store.release(id)?.context("unknown apply")?;
    let plan: Plan = serde_json::from_str(&fs::read_to_string(
        std::path::Path::new(&release.log).with_extension("plan.json"),
    )?)?;
    let environments = deploy::environments(context)?;
    let environment = environments
        .get(&plan.environment)
        .context("environment no longer declared")?
        .clone();
    let artifacts = deploy::artifacts(context)?;
    let env_file = std::path::Path::new(&release.log).with_extension("env");
    let quiesced_file = std::path::Path::new(&release.log).with_extension("quiesced");
    for mut step in context.store.release_steps(id)? {
        if matches!(step.state.as_str(), "passed" | "recovered") {
            continue;
        }
        println!("CITRUS_STEP target={} status=START", step.name);
        step.state = "running".into();
        context.store.update_step(id, &step)?;
        let started = now();
        let extra_env = read_env(&env_file);
        let outcome = run_step(
            context,
            &environment,
            &artifacts,
            &plan,
            &step.name,
            &extra_env,
            &env_file,
            &quiesced_file,
        );
        step.seconds = Some((now() - started) as i64);
        match outcome {
            Ok(()) => {
                println!(
                    "CITRUS_STEP target={} status=PASS exit=0 seconds={}",
                    step.name,
                    step.seconds.unwrap_or_default()
                );
                step.state = "passed".into();
                step.exit = Some(0);
                context.store.update_step(id, &step)?;
            }
            Err(error) => {
                println!("{error:#}");
                println!(
                    "CITRUS_STEP target={} status=FAIL exit=1 seconds={}",
                    step.name,
                    step.seconds.unwrap_or_default()
                );
                step.state = "failed".into();
                step.exit = Some(1);
                let prefixes = vec!["CITRUS_STEP".to_owned()];
                step.first_error =
                    report::first_error(&segment(&read_log(&release.log), &step.name, &prefixes))
                        .or(Some(format!("{error:#}")));
                context.store.update_step(id, &step)?;
                // Never leave scheduled work suspended because a later step failed.
                if quiesced_file.exists() && step.name != "resume" {
                    let _ = resume_cronjobs(&environment, &quiesced_file, &extra_env);
                }
                context.store.finish_release(
                    id,
                    "failed",
                    &format!("step {} failed", step.name),
                )?;
                context.store.unlock_environment(&release.environment, id)?;
                return Ok(());
            }
        }
    }
    context.store.finish_release(id, "passed", "")?;
    context.store.unlock_environment(&release.environment, id)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_step(
    context: &Context,
    environment: &Environment,
    artifacts: &BTreeMap<String, deploy::Artifact>,
    plan: &Plan,
    step: &str,
    extra_env: &BTreeMap<String, String>,
    env_file: &std::path::Path,
    quiesced_file: &std::path::Path,
) -> Result<()> {
    if step == "prepare" {
        let output = command(&environment.prepare, extra_env, context)?
            .stdout(Stdio::piped())
            .spawn()?
            .wait_with_output()?;
        if !output.status.success() {
            bail!("prepare failed");
        }
        let mut values = extra_env.clone();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Some((key, value)) = line.split_once('=')
                && !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                values.insert(key.to_owned(), value.to_owned());
            } else {
                println!("{line}");
            }
        }
        write_private(
            env_file,
            &values
                .iter()
                .map(|(key, value)| format!("{key}={value}\n"))
                .collect::<String>(),
        )?;
        return Ok(());
    }
    if let Some(name) = step.strip_prefix("build:") {
        let build = plan
            .builds
            .iter()
            .find(|build| build.artifact == name)
            .context("build not in plan")?;
        if let Some(reference) = context.store.artifact_reference(name, &build.key)? {
            println!("reusing {reference}: built before from the same inputs");
            return Ok(());
        }
        let artifact = artifacts.get(name).context("artifact no longer declared")?;
        let reference =
            build_artifact(context, name, artifact, &build.key, &plan.commit, extra_env)?;
        println!("built {reference}");
        context
            .store
            .put_artifact_reference(name, &build.key, &reference)?;
        return Ok(());
    }
    if step == "quiesce" {
        let mut suspended = Vec::new();
        for name in &plan.quiesce {
            let state = kubectl(environment, extra_env)
                .args(["get", "cronjob", name, "-o", "jsonpath={.spec.suspend}"])
                .output()?;
            if String::from_utf8_lossy(&state.stdout).trim() == "true" {
                println!("{name} was already suspended; leaving it so");
                continue;
            }
            run(kubectl(environment, extra_env).args([
                "patch",
                "cronjob",
                name,
                "--type",
                "merge",
                "-p",
                r#"{"spec":{"suspend":true}}"#,
            ]))?;
            suspended.push(name.clone());
        }
        write_private(quiesced_file, &suspended.join("\n"))?;
        return Ok(());
    }
    if step == "resume" {
        return resume_cronjobs(environment, quiesced_file, extra_env);
    }
    if step == "migrate" {
        let migrations = environment
            .migrations
            .as_ref()
            .context("migrations no longer declared")?;
        let artifact = plan.migrate.as_deref().context("no migration in plan")?;
        let key = &plan
            .builds
            .iter()
            .find(|build| build.artifact == artifact)
            .context("migration build missing")?
            .key;
        let image = context
            .store
            .artifact_reference(artifact, key)?
            .context("migration image not built")?;
        let name = format!("{}-migrate-{}", plan.environment, &plan.commit[..8])
            .to_lowercase()
            .replace('_', "-");
        let manifest = fs::read_to_string(context.repo.root.join(&migrations.job))?
            .replace("{image}", &image)
            .replace("{name}", &name);
        let mut apply = kubectl(environment, extra_env);
        apply.args(["apply", "-f", "-"]).stdin(Stdio::piped());
        let mut child = apply.spawn()?;
        std::io::Write::write_all(child.stdin.as_mut().context("stdin")?, manifest.as_bytes())?;
        drop(child.stdin.take());
        if !child.wait()?.success() {
            bail!("kubectl apply of the migration job failed");
        }
        run(kubectl(environment, extra_env).args([
            "wait",
            "--for=condition=complete",
            &format!("job/{name}"),
            &format!("--timeout={}s", migrations.timeout),
        ]))?;
        return Ok(());
    }
    if let Some(name) = step.strip_prefix("roll:") {
        let roll = plan
            .rolls
            .iter()
            .find(|roll| roll.workload == name)
            .context("roll not in plan")?;
        let image = context
            .store
            .artifact_reference(&roll.artifact, &roll.key)?
            .context("image not built")?;
        // Re-running a step whose change already landed must not wait for a
        // takeover that will not happen: nothing restarts for the same image.
        let already = deploy::observe(environment)?
            .into_iter()
            .find(|item| item.workload == name)
            .and_then(|item| item.image)
            .as_deref()
            == Some(image.as_str());
        let holder_before = match &roll.fence {
            Some(lease) if !already => lease_holder(environment, extra_env, lease)?,
            _ => String::new(),
        };
        let mut annotations = serde_json::Map::new();
        annotations.insert(COMMIT_ANNOTATION.into(), plan.commit.clone().into());
        annotations.insert(KEY_ANNOTATION.into(), roll.key.clone().into());
        if !environment.record.annotation.is_empty() {
            annotations.insert(
                environment.record.annotation.clone(),
                plan.release_name.clone().into(),
            );
        }
        let containers = serde_json::json!([{"name": roll.container, "image": image}]);
        let patch = if already {
            // Same image: record only. Touching the pod template would restart it for nothing.
            serde_json::json!({"metadata": {"annotations": annotations}})
        } else if roll.kind == "cronjob" {
            serde_json::json!({"metadata": {"annotations": annotations}, "spec": {"jobTemplate": {"spec": {"template": {"spec": {"containers": containers}}}}}})
        } else {
            serde_json::json!({"metadata": {"annotations": annotations}, "spec": {"template": {"metadata": {"annotations": annotations}, "spec": {"containers": containers}}}})
        };
        if already {
            println!("{name} already runs {image}: recording the release without restarting it");
        }
        run(kubectl(environment, extra_env).args([
            "patch",
            &roll.kind,
            name,
            "--type",
            "strategic",
            "-p",
            &patch.to_string(),
        ]))?;
        if roll.kind == "deployment" {
            run(kubectl(environment, extra_env).args([
                "rollout",
                "status",
                &format!("deployment/{name}"),
                &format!("--timeout={}s", roll.timeout),
            ]))?;
        }
        if let Some(lease) = roll.fence.as_ref().filter(|_| !already) {
            let deadline = now() + roll.timeout;
            loop {
                let holder = lease_holder(environment, extra_env, lease)?;
                if !holder.is_empty() && holder != holder_before {
                    println!("lease {lease} now held by {holder}");
                    break;
                }
                if now() > deadline {
                    bail!(
                        "lease {lease} was not taken over by the new {name} within {}s (holder {holder:?})",
                        roll.timeout
                    );
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        return Ok(());
    }
    if step == "verify" {
        let observed = deploy::observe(environment)?;
        for roll in &plan.rolls {
            let item = observed
                .iter()
                .find(|item| item.workload == roll.workload)
                .context("workload disappeared")?;
            let expected = context
                .store
                .artifact_reference(&roll.artifact, &roll.key)?
                .unwrap_or_default();
            if item.image.as_deref() != Some(expected.as_str()) {
                bail!(
                    "{} runs {:?}, expected {expected}",
                    roll.workload,
                    item.image
                );
            }
            if !item.ready {
                bail!("{} is not ready ({})", roll.workload, item.detail);
            }
        }
        for url in &environment.verify.http {
            run(Command::new("curl").args(["-fsS", "--max-time", "15", "-o", "/dev/null", url]))?;
        }
        for check in &environment.verify.commands {
            let status = command(check, extra_env, context)?.status()?;
            if !status.success() {
                bail!("verify command failed: {}", check.join(" "));
            }
        }
        return Ok(());
    }
    bail!("unknown step {step}")
}

fn build_artifact(
    context: &Context,
    name: &str,
    artifact: &deploy::Artifact,
    key: &str,
    commit: &str,
    extra_env: &BTreeMap<String, String>,
) -> Result<String> {
    let text = |table: &BTreeMap<String, toml::Value>, field: &str| {
        table
            .get(field)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    };
    let provider = text(&artifact.build, "provider").unwrap_or_else(|| "command".into());
    let tag = &key[..16];
    match provider.as_str() {
        "docker" => {
            let registry = text(&artifact.publish, "registry")
                .context("docker artifacts need publish.registry")?;
            let metadata = context
                .repo
                .log_dir()
                .join(format!("build-{name}-{tag}.json"));
            let mut docker = Command::new("docker");
            docker
                .args([
                    "buildx",
                    "build",
                    "--push",
                    "--tag",
                    &format!("{registry}:{tag}"),
                    "--metadata-file",
                ])
                .arg(&metadata);
            if let Some(file) = text(&artifact.build, "dockerfile") {
                docker.args(["--file", &file]);
            }
            if let Some(target) = text(&artifact.build, "target") {
                docker.args(["--target", &target]);
            }
            if let Some(platform) = text(&artifact.build, "platform") {
                docker.args(["--platform", &platform]);
            }
            docker
                .arg(text(&artifact.build, "context").unwrap_or_else(|| ".".into()))
                .current_dir(&context.repo.root)
                .envs(extra_env);
            run(&mut docker)?;
            let meta: serde_json::Value = serde_json::from_str(&fs::read_to_string(&metadata)?)?;
            let digest = meta["containerimage.digest"]
                .as_str()
                .context("build metadata has no containerimage.digest")?;
            Ok(format!("{registry}@{digest}"))
        }
        "command" => {
            let template: Vec<String> = artifact
                .build
                .get("run")
                .and_then(|value| value.as_array())
                .context("command artifacts need build.run")?
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect();
            let argv: Vec<String> = template
                .iter()
                .map(|part| {
                    part.replace("{key}", key)
                        .replace("{tag}", tag)
                        .replace("{commit}", commit)
                        .replace("{artifact}", name)
                })
                .collect();
            let output = command(&argv, extra_env, context)?
                .stdout(Stdio::piped())
                .spawn()?
                .wait_with_output()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            print!("{stdout}");
            if !output.status.success() {
                bail!("build of {name} failed");
            }
            stdout
                .lines()
                .rev()
                .find_map(|line| line.trim().strip_prefix("IMAGE="))
                .filter(|reference| reference.contains("@sha256:"))
                .map(str::to_owned)
                .context("the build printed no IMAGE=<registry>@sha256:<digest> line")
        }
        other => bail!("unknown build provider {other:?} (docker, command)"),
    }
}

fn command(
    argv: &[String],
    extra_env: &BTreeMap<String, String>,
    context: &Context,
) -> Result<Command> {
    let (program, args) = argv.split_first().context("empty command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .envs(extra_env)
        .current_dir(&context.repo.root)
        .stdin(Stdio::null());
    Ok(command)
}

fn kubectl(environment: &Environment, extra_env: &BTreeMap<String, String>) -> Command {
    let mut command = deploy::kubectl(environment);
    command.envs(extra_env);
    command
}

fn lease_holder(
    environment: &Environment,
    extra_env: &BTreeMap<String, String>,
    lease: &str,
) -> Result<String> {
    let output = kubectl(environment, extra_env)
        .args([
            "get",
            "lease",
            lease,
            "-o",
            "jsonpath={.spec.holderIdentity}",
        ])
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn resume_cronjobs(
    environment: &Environment,
    quiesced_file: &std::path::Path,
    extra_env: &BTreeMap<String, String>,
) -> Result<()> {
    let names = fs::read_to_string(quiesced_file).unwrap_or_default();
    for name in names.lines().filter(|name| !name.is_empty()) {
        run(kubectl(environment, extra_env).args([
            "patch",
            "cronjob",
            name,
            "--type",
            "merge",
            "-p",
            r#"{"spec":{"suspend":false}}"#,
        ]))?;
    }
    let _ = fs::remove_file(quiesced_file);
    Ok(())
}

fn run(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if !status.success() {
        bail!("{:?} failed ({status})", command.get_program());
    }
    Ok(())
}

fn read_env(path: &std::path::Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| {
            line.split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
        })
        .collect()
}

fn write_private(path: &std::path::Path, text: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    Ok(())
}
