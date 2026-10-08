//! Starting runs, deciding what can be reused, and the detached worker that
//! executes the rest and records evidence.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use crate::manifest::{Manifest, fingerprint, now};
use crate::plan;
use crate::repo::Repo;
use crate::report::{self, compact_utc, strip_ansi};
use crate::state::{Run, RunTarget, Store};

/// A check this fast runs locally rather than waiting for a pool.
const QUICK_SECONDS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Local,
    Remote,
}

#[derive(Debug)]
pub struct Request {
    pub targets: Vec<String>,
    pub base: Option<String>,
    pub mode: Mode,
    pub key: Option<String>,
    pub force: bool,
    /// Checks run at once locally.
    pub jobs: usize,
    /// Plan for exactly these changed paths instead of the diff with the base.
    pub paths: Option<Vec<String>>,
}

#[derive(Debug)]
pub struct Context {
    pub repo: Repo,
    pub store: Store,
    pub manifest: Manifest,
    /// The compiled `citrus.ci`, when the repository has one.
    pub project: Option<crate::model::Project>,
}

impl Context {
    /// `profile`: `--profile`; otherwise `CITRUS_PROFILE`, otherwise the
    /// project's first profile.
    pub fn open(profile: Option<String>) -> Result<Context> {
        let mut repo = Repo::discover()?;
        let (manifest, project) = match crate::model::load(&repo.root)
            .map_err(|rendered| anyhow::anyhow!("{rendered}"))?
        {
            // `citrus.ci` is the whole configuration; without it, generic defaults.
            Some((project, sources)) => {
                if let Some(base) = &project.base {
                    repo.config.plan.base = base.clone();
                }
                if let Some(logs) = &project.logs {
                    repo.config.log_dir = Some(logs.clone());
                }
                if !project.toolchain.is_empty() {
                    repo.config.toolchain_files = project.toolchain.clone();
                }
                let config = &mut repo.config;
                if let Some(receipts) = &project.receipts {
                    config.receipts.dir = receipts.clone();
                }
                let requested = profile.or_else(|| {
                    std::env::var("CITRUS_PROFILE")
                        .ok()
                        .filter(|value| !value.is_empty())
                });
                config.plan.profile = match requested {
                    Some(name) if project.profiles.contains(&name) => Some(name),
                    Some(name) => bail!(
                        "no profile `{name}` in citrus.ci; declared: {}",
                        if project.profiles.is_empty() {
                            "none".to_owned()
                        } else {
                            project.profiles.join(", ")
                        }
                    ),
                    None => project.profiles.first().cloned(),
                };
                if let Some(pool) = &project.pool {
                    config.run.remote = pool.argv.clone();
                    config.status.resources_command = pool.status.clone();
                    config.status.resource_prefix = "CITRUS_RESOURCE ".into();
                    config.status.refresh_seconds = 60;
                }
                config.run.image = project.image.clone();
                config.run.private = project.private.clone();
                config.run.prepare = project.prepare.clone();
                if !project.after_merge.is_empty() {
                    config.integrate.after_merge = project.after_merge.clone();
                }
                for (command, about, group) in &project.commands {
                    config.catalog.push(crate::config::CatalogEntry {
                        command: command.clone(),
                        description: about.clone(),
                        group: group.clone(),
                    });
                }
                let mut manifest = Manifest::from_project(&project, &sources)?;
                // The active profile's environment reaches its checks' steps.
                let profile_env: Vec<(String, String)> = repo
                    .config
                    .plan
                    .profile
                    .as_ref()
                    .and_then(|profile| {
                        project.profile_env.iter().find(|(name, _)| name == profile)
                    })
                    .map(|(_, env)| env.clone())
                    .unwrap_or_default();
                if !profile_env.is_empty() {
                    let active = repo.config.plan.profile.clone().unwrap_or_default();
                    for target in manifest.targets.values_mut() {
                        if !target.profiles.is_empty() && !target.profiles.contains(&active) {
                            continue;
                        }
                        for (name, value) in &profile_env {
                            target
                                .env
                                .entry(name.clone())
                                .or_insert_with(|| value.clone());
                        }
                        for step in &mut target.steps {
                            if let crate::model::Work::Process { env, .. }
                            | crate::model::Work::Script { env, .. } = &mut step.work
                            {
                                for (name, value) in &profile_env {
                                    if !env.iter().any(|(known, _)| known == name) {
                                        env.insert(0, (name.clone(), value.clone()));
                                    }
                                }
                            }
                        }
                    }
                }
                (manifest, Some(project))
            }
            None => (Manifest::default(), None),
        };
        let store = Store::open(&repo.state_dir())?;
        Ok(Context {
            repo,
            store,
            manifest,
            project,
        })
    }

    pub fn worktree(&self) -> String {
        self.repo.root.display().to_string()
    }

    fn snapshot_max_age(&self) -> i64 {
        (self.repo.config.receipts.snapshot_max_age_hours * 3600) as i64
    }

    /// Every check passed before within a minute: not worth a pool's queue.
    /// A check that never passed is assumed heavy.
    fn all_quick(&self, pending: &[&RunTarget]) -> Result<bool> {
        for decision in pending {
            match self.store.typical_seconds(&decision.target)? {
                Some(seconds) if seconds <= QUICK_SECONDS => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    /// Whether `target` is already proven for the current sources, and why not otherwise.
    pub fn decide(
        &self,
        files: &[String],
        snapshot: &str,
        target: &str,
        force: bool,
    ) -> Result<RunTarget> {
        let mut decision = RunTarget {
            target: target.to_owned(),
            result: "pending".into(),
            reason: if force {
                "forced".into()
            } else {
                "no_evidence".into()
            },
            fingerprint: None,
            evidence_run: None,
            seconds: None,
            exit: None,
            first_error: None,
        };
        if let Some(entry) = self
            .manifest
            .targets
            .get(target)
            .filter(|entry| entry.cache)
        {
            let current = fingerprint(
                &self.repo.root,
                files,
                entry,
                &self.repo.config.toolchain_files,
            )?;
            decision.fingerprint = Some(current.value.clone());
            if force {
                return Ok(decision);
            }
            if self.repo.receipts().valid(target, &current.value) {
                decision.result = "reused".into();
                decision.reason = "inputs_unchanged".into();
                decision.evidence_run = self
                    .store
                    .evidence(target, "inputs", &current.value)?
                    .map(|evidence| evidence.run);
            } else if self.store.latest_evidence(target, "inputs")?.is_some() {
                decision.reason = "input_changed".into();
            }
            return Ok(decision);
        }
        decision.fingerprint = Some(snapshot.to_owned());
        if force {
            return Ok(decision);
        }
        match self.store.evidence(target, "snapshot", snapshot)? {
            Some(evidence)
                if evidence.result == "passed"
                    && now() as i64 - evidence.created < self.snapshot_max_age() =>
            {
                decision.result = "reused".into();
                decision.reason = if evidence.detail.starts_with("carried") {
                    "carried_over".into()
                } else {
                    "same_snapshot".into()
                };
                decision.evidence_run = Some(evidence.run);
            }
            Some(_) => decision.reason = "evidence_expired".into(),
            None if self.store.latest_evidence(target, "snapshot")?.is_some() => {
                decision.reason = "input_changed".into()
            }
            None => {}
        }
        Ok(decision)
    }

    pub fn start(&mut self, request: &Request) -> Result<Run> {
        if let Some(key) = &request.key
            && let Some(existing) = self.store.run_by_key(key)?
        {
            return Ok(existing);
        }
        let explicit = !request.targets.is_empty();
        let remote_available = !self.repo.config.run.remote.is_empty() || self.uses_pool();
        if request.mode == Mode::Remote && !remote_available {
            bail!(
                "no remote runner: join a pool (CITRUS_POOL) or declare a `runner` in the configuration"
            );
        }
        if explicit && request.mode == Mode::Remote {
            bail!("--remote runs the planned set; drop the target names or use --local");
        }
        let names = if explicit {
            request.targets.clone()
        } else {
            let plan = match &request.paths {
                Some(paths) => {
                    let base = request
                        .base
                        .as_deref()
                        .unwrap_or(&self.repo.config.plan.base);
                    let before = self
                        .repo
                        .git(&["merge-base", base, "HEAD"])
                        .unwrap_or_default();
                    plan::for_paths(&self.repo, &self.manifest, paths, &before)?
                }
                None => plan::compute(&self.repo, &self.manifest, request.base.as_deref())?,
            };
            // The planner itself says it cannot tell what these changes need.
            if plan.status == "incomplete" && !plan.unmapped.is_empty() {
                let shown: Vec<&str> = plan.unmapped.iter().take(5).map(String::as_str).collect();
                bail!(
                    "the plan is incomplete: {} changed paths no check claims ({}{}); claim them in citrus.ci (`owns`) or in the planner, or run named checks",
                    plan.unmapped.len(),
                    shown.join(", "),
                    if plan.unmapped.len() > shown.len() {
                        ", …"
                    } else {
                        ""
                    }
                );
            }
            plan.targets
        };
        let files = self.repo.files()?;
        let snapshot = self.repo.snapshot()?;
        if !request.force
            && let Some(running) = self.running_for(&snapshot, &names)?
        {
            let joined: i64 = self
                .store
                .fact("joined_runs")?
                .and_then(|(value, _)| value.parse().ok())
                .unwrap_or_default();
            self.store
                .set_fact("joined_runs", &(joined + 1).to_string())?;
            return Ok(running);
        }
        let decisions = names
            .iter()
            .map(|name| self.decide(&files, &snapshot, name, request.force))
            .collect::<Result<Vec<_>>>()?;
        let pending: Vec<&RunTarget> = decisions
            .iter()
            .filter(|decision| decision.result == "pending")
            .collect();
        let mode = match request.mode {
            Mode::Local => "local",
            Mode::Remote => "remote",
            Mode::Auto if explicit || !remote_available || self.all_quick(&pending)? => "local",
            Mode::Auto => "remote",
        };
        let started = now();
        let id = format!("r-{}-{:04x}", compact_utc(started), random16());
        crate::repo::private_dir(&self.repo.log_dir())?;
        let log = self.repo.log_dir().join(format!("{id}.log"));
        let mut run = Run {
            id: id.clone(),
            key: request.key.clone(),
            worktree: self.worktree(),
            branch: self.repo.branch(),
            agent: agent(),
            mode: mode.into(),
            state: if pending.is_empty() {
                "passed".into()
            } else {
                "queued".into()
            },
            note: if names.is_empty() {
                "nothing to check for these changes".into()
            } else {
                String::new()
            },
            snapshot,
            base: request.base.clone(),
            pid: None,
            started: started as i64,
            ended: pending.is_empty().then_some(started as i64),
            exit: pending.is_empty().then_some(0),
            log: log.display().to_string(),
            linked_log: None,
        };
        self.store.insert_run(&run, &decisions)?;
        if pending.is_empty() {
            return Ok(run);
        }
        if request.jobs > 1 {
            self.store
                .set_fact(&format!("jobs:{id}"), &request.jobs.to_string())?;
        }
        if let Some(paths) = &request.paths {
            self.store
                .set_fact(&format!("paths:{id}"), &serde_json::to_string(paths)?)?;
        }
        let output = OpenOptions::new().create(true).append(true).open(&log)?;
        let pid = self
            .spawn_detached(&["worker", &id], output)
            .context("start citrus worker")?;
        self.store.set_pid(&id, pid)?;
        run.pid = Some(pid);
        Ok(run)
    }

    /// Start this binary with `args` in its own session: it survives the caller's terminal.
    pub fn spawn_detached(&self, args: &[&str], output: fs::File) -> Result<i64> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args(args)
            .env(
                "CITRUS_PROFILE",
                self.repo.config.plan.profile.clone().unwrap_or_default(),
            )
            .current_dir(&self.repo.root)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        Ok(i64::from(command.spawn()?.id()))
    }

    /// "Nothing to check" is only true if HEAD has nothing the base lacks. A
    /// planner comparing against another base (or HEAD itself) must not turn
    /// unmerged work into a green result.
    /// An unfinished run over the same sources that already covers `names`:
    /// asking again joins it instead of starting a duplicate.
    fn running_for(&self, snapshot: &str, names: &[String]) -> Result<Option<Run>> {
        for run in self.store.active()? {
            let run = self.reconcile(run)?;
            if run.finished() || run.snapshot != snapshot {
                continue;
            }
            let covered: Vec<String> = self
                .store
                .targets(&run.id)?
                .into_iter()
                .map(|row| row.target)
                .collect();
            if names.iter().all(|name| covered.contains(name)) {
                return Ok(Some(run));
            }
        }
        Ok(None)
    }

    /// Mark runs whose worker vanished without a result.
    pub fn reconcile(&self, run: Run) -> Result<Run> {
        if run.finished() {
            return Ok(run);
        }
        let lost = match run.pid {
            Some(pid) => !alive(pid),
            None => now() as i64 - run.started > 60,
        };
        if lost {
            self.store.set_note(
                &run.id,
                "worker disappeared without a result; check the log before rerunning",
            )?;
            self.store.finish(&run.id, "unknown", None)?;
            for mut target in self.store.targets(&run.id)? {
                if matches!(target.result.as_str(), "pending" | "running") {
                    target.result = "unknown".into();
                    target.reason = "process_lost".into();
                    self.store.update_target(&run.id, &target)?;
                }
            }
            return self.store.run(&run.id)?.context("run disappeared");
        }
        Ok(run)
    }

    pub fn cancel(&self, run: &Run) -> Result<()> {
        // Queued pool checks are dropped and agents stop the running ones.
        if self.store.fact(&format!("pool:{}", run.id))?.is_some() {
            let _ = crate::pool::cancel(&run.id);
        }
        if let Some(pid) = run.pid.filter(|pid| alive(*pid)) {
            // SAFETY: plain signal delivery to the worker's own process group.
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGTERM) };
            for _ in 0..20 {
                if !alive(pid) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            if alive(pid) {
                // SAFETY: as above.
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
        }
        self.store.finish(&run.id, "cancelled", None)?;
        for mut target in self.store.targets(&run.id)? {
            if matches!(target.result.as_str(), "pending" | "running") {
                target.result = "cancelled".into();
                target.reason = "cancelled".into();
                self.store.update_target(&run.id, &target)?;
            }
        }
        Ok(())
    }

    /// Runs inside the detached worker process; stdout and stderr are the run log.
    pub fn work(&mut self, id: &str) -> Result<()> {
        let run = self.store.run(id)?.context("unknown run")?;
        self.store.set_state(id, "running")?;
        let targets = self.store.targets(id)?;
        let jobs: usize = self
            .store
            .fact(&format!("jobs:{id}"))?
            .and_then(|(value, _)| value.parse().ok())
            .unwrap_or(1);
        let passed = if run.mode == "remote" {
            self.work_remote(&run, &targets)?
        } else if jobs > 1 {
            self.work_parallel(&run, &targets, jobs)?
        } else {
            self.work_local(&run, &targets)?
        };
        let finished = self.store.targets(id)?;
        if run.mode == "remote" {
            // A check the pool reported passing proves its inputs here too,
            // as long as they are what they were when the run started.
            for row in finished
                .iter()
                .filter(|row| row.result == "passed" && row.reason == "ran")
            {
                self.record_inputs(&run, &row.target)?;
            }
        }
        let ok = finished
            .iter()
            .all(|target| matches!(target.result.as_str(), "passed" | "reused"));
        if !passed.is_empty() && self.repo.snapshot().ok().as_deref() == Some(run.snapshot.as_str())
        {
            for target in &passed {
                self.store
                    .put_evidence(target, "snapshot", &run.snapshot, "passed", id, "")?;
            }
        } else if !passed.is_empty() {
            println!("CITRUS_NOTE sources changed during the run; snapshot evidence not recorded");
        }
        self.store.finish(
            id,
            if ok { "passed" } else { "failed" },
            Some(if ok { 0 } else { 1 }),
        )?;
        Ok(())
    }

    fn work_local(&self, run: &Run, targets: &[RunTarget]) -> Result<Vec<String>> {
        let mut passed = Vec::new();
        // Services a check needs start once, before the first check needing them.
        let mut started_services: Vec<String> = Vec::new();
        for target in targets.iter().filter(|target| target.result == "pending") {
            let mut current = target.clone();
            current.result = "running".into();
            self.store.update_target(&run.id, &current)?;
            println!("CITRUS_TARGET target={} status=START", target.target);
            let started = now();
            let declared = self.manifest.targets.get(&target.target);
            if let Some(entry) = declared {
                let mut code = 0;
                let services = self.project.iter().flat_map(|project| &project.services);
                for service in services.filter(|service| entry.resources.contains(&service.name)) {
                    if code != 0 || started_services.contains(&service.name) {
                        continue;
                    }
                    started_services.push(service.name.clone());
                    for step in service.start.iter().chain(&service.ready) {
                        println!("── {}  (service {})", step.label, service.name);
                        code = i64::from(crate::model::execute(step, &self.repo.root, false)?);
                        if code != 0 {
                            println!("service {} did not start", service.name);
                            break;
                        }
                    }
                }
                let steps = if code == 0 {
                    entry.steps.as_slice()
                } else {
                    &[]
                };
                // A check that may be reused is watched: its Python and Node
                // programs must read nothing outside its inputs.
                let watch = entry.cache.then(|| {
                    let dir = self.repo.state_dir().join("observe");
                    let log = self
                        .repo
                        .state_dir()
                        .join("tmp")
                        .join(format!("observe-{}-{}", run.id, target.target));
                    let _ = std::fs::remove_file(&log);
                    let ready = log
                        .parent()
                        .map_or(Ok(()), crate::repo::private_dir)
                        .and_then(|()| crate::observe::environment(&dir, &log));
                    (log.clone(), ready)
                });
                let extra = match &watch {
                    Some((_, Ok(env))) => env.clone(),
                    _ => Vec::new(),
                };
                for step in steps {
                    println!(
                        "── {}  ({})",
                        step.label,
                        entry.source.as_deref().unwrap_or("citrus.ci")
                    );
                    code = i64::from(crate::model::execute_env(
                        step,
                        &self.repo.root,
                        false,
                        &extra,
                    )?);
                    if code != 0 {
                        break;
                    }
                }
                let mut outside = Vec::new();
                if let Some((log, Ok(_))) = &watch {
                    let read = crate::observe::in_repository(
                        crate::observe::read(log, &self.repo.root),
                        &self.repo.files()?,
                    );
                    let globs: Vec<String> = entry
                        .inputs
                        .iter()
                        .chain(&entry.extra_inputs)
                        .chain(&self.repo.config.toolchain_files)
                        .cloned()
                        .collect();
                    if let Ok(inputs) = crate::manifest::GlobList::new(&globs) {
                        outside = crate::observe::outside(&read, &inputs);
                    }
                    let _ = std::fs::remove_file(log);
                }
                current.seconds = Some((now() - started) as i64);
                current.exit = Some(code);
                println!(
                    "CITRUS_TARGET target={} status={} exit={code} seconds={}",
                    target.target,
                    if code == 0 { "PASS" } else { "FAIL" },
                    current.seconds.unwrap_or_default()
                );
                if code == 0 {
                    current.result = "passed".into();
                    current.reason = "ran".into();
                    if outside.is_empty() {
                        self.record_inputs(run, &target.target)?;
                    } else {
                        let shown: Vec<&str> = outside.iter().take(5).map(String::as_str).collect();
                        println!(
                            "CITRUS_NOTE {} read {}{} outside its inputs; this pass is not reused (add them to #[reads])",
                            target.target,
                            shown.join(", "),
                            if outside.len() > shown.len() {
                                ", …"
                            } else {
                                ""
                            }
                        );
                        current.reason =
                            format!("ran; read outside its inputs: {}", shown.join(", "));
                    }
                    passed.push(target.target.clone());
                } else {
                    current.result = "failed".into();
                    current.reason = "ran".into();
                    current.first_error =
                        report::first_error(&segment(&read_log(&run.log), &target.target, &[]));
                }
                self.store.update_target(&run.id, &current)?;
                continue;
            }
            println!(
                "CITRUS_TARGET target={} status=FAIL exit=127",
                target.target
            );
            current.result = "failed".into();
            current.reason = "not declared".into();
            current.first_error =
                Some(format!("no check `{}` in the configuration", target.target));
            self.store.update_target(&run.id, &current)?;
        }
        self.stop_services(&started_services)?;
        Ok(passed)
    }

    /// Stop what the run started, last first, whatever the checks did.
    fn stop_services(&self, started: &[String]) -> Result<()> {
        let services = self.project.iter().flat_map(|project| &project.services);
        let services: Vec<&crate::model::Service> = services.collect();
        for name in started.iter().rev() {
            let Some(service) = services.iter().find(|service| &service.name == name) else {
                continue;
            };
            for step in &service.stop {
                println!("── {}  (service {} stop)", step.label, service.name);
                if crate::model::execute(step, &self.repo.root, false)? != 0 {
                    println!("service {} did not stop cleanly", service.name);
                }
            }
        }
        Ok(())
    }

    /// Up to `jobs` checks at once, each in its own `citrus exec-steps`
    /// process writing its own log; the run log gets each check's output whole,
    /// between its START and result lines, when it ends. Services start once,
    /// before any check; a service's `limit` caps the checks using it at once.
    fn work_parallel(&self, run: &Run, targets: &[RunTarget], jobs: usize) -> Result<Vec<String>> {
        let mut passed = Vec::new();
        let pending: Vec<&RunTarget> = targets
            .iter()
            .filter(|target| target.result == "pending")
            .collect();
        let services: Vec<&crate::model::Service> = self
            .project
            .iter()
            .flat_map(|project| &project.services)
            .collect();
        // Services the pending checks need, started in declaration order.
        let mut failed_services: Vec<String> = Vec::new();
        let mut started_services: Vec<String> = Vec::new();
        for service in &services {
            let needed = pending.iter().any(|target| {
                self.manifest
                    .targets
                    .get(&target.target)
                    .is_some_and(|entry| entry.resources.contains(&service.name))
            });
            if !needed {
                continue;
            }
            started_services.push(service.name.clone());
            for step in service.start.iter().chain(&service.ready) {
                println!("── {}  (service {})", step.label, service.name);
                if crate::model::execute(step, &self.repo.root, false)? != 0 {
                    println!("service {} did not start", service.name);
                    failed_services.push(service.name.clone());
                    break;
                }
            }
        }
        let tmp = self.repo.state_dir().join("tmp");
        crate::repo::private_dir(&tmp)?;
        struct Running {
            target: RunTarget,
            child: std::process::Child,
            log: std::path::PathBuf,
            spec: std::path::PathBuf,
            started: u64,
            watch: Option<std::path::PathBuf>,
            resources: Vec<String>,
        }
        let mut queue: std::collections::VecDeque<&RunTarget> = pending.into_iter().collect();
        let mut running: Vec<Running> = Vec::new();
        let limit_of = |name: &str| {
            services
                .iter()
                .find(|service| service.name == name)
                .and_then(|service| service.limit)
                .map(|limit| limit.max(1) as usize)
        };
        while !queue.is_empty() || !running.is_empty() {
            // Start what fits: free slots, and services under their limit.
            while running.len() < jobs {
                let Some(position) = queue.iter().position(|target| {
                    let resources = self
                        .manifest
                        .targets
                        .get(&target.target)
                        .map(|entry| entry.resources.clone())
                        .unwrap_or_default();
                    resources.iter().all(|name| {
                        limit_of(name).is_none_or(|limit| {
                            running
                                .iter()
                                .filter(|other| other.resources.contains(name))
                                .count()
                                < limit
                        })
                    })
                }) else {
                    break;
                };
                let Some(target) = queue.remove(position) else {
                    break;
                };
                let mut current = target.clone();
                let Some(entry) = self.manifest.targets.get(&target.target) else {
                    println!("CITRUS_TARGET target={} status=START", target.target);
                    println!(
                        "CITRUS_TARGET target={} status=FAIL exit=127",
                        target.target
                    );
                    current.result = "failed".into();
                    current.reason = "not declared".into();
                    current.first_error =
                        Some(format!("no check `{}` in the configuration", target.target));
                    self.store.update_target(&run.id, &current)?;
                    continue;
                };
                if let Some(service) = entry
                    .resources
                    .iter()
                    .find(|name| failed_services.contains(name))
                {
                    println!("CITRUS_TARGET target={} status=START", target.target);
                    println!("service {service} did not start");
                    println!(
                        "CITRUS_TARGET target={} status=FAIL exit=1 seconds=0",
                        target.target
                    );
                    current.result = "failed".into();
                    current.reason = "ran".into();
                    current.exit = Some(1);
                    current.first_error = Some(format!("service {service} did not start"));
                    self.store.update_target(&run.id, &current)?;
                    continue;
                }
                current.result = "running".into();
                self.store.update_target(&run.id, &current)?;
                let stem = format!("{}-{}", run.id, target.target);
                let watch = entry.cache.then(|| tmp.join(format!("observe-{stem}")));
                let mut env: Vec<(String, String)> = Vec::new();
                if let Some(watch) = &watch {
                    let _ = fs::remove_file(watch);
                    if let Ok(found) =
                        crate::observe::environment(&self.repo.state_dir().join("observe"), watch)
                    {
                        env = found;
                    }
                }
                let spec = tmp.join(format!("steps-{stem}.json"));
                fs::write(
                    &spec,
                    serde_json::to_string(&serde_json::json!({
                        "root": self.repo.root,
                        "source": entry.source.clone().unwrap_or_else(|| "citrus.ci".into()),
                        "steps": entry.steps,
                        "env": env,
                    }))?,
                )?;
                let log = tmp.join(format!("log-{stem}"));
                let output = fs::File::create(&log)?;
                let child = Command::new(std::env::current_exe()?)
                    .args(["exec-steps"])
                    .arg(&spec)
                    .current_dir(&self.repo.root)
                    .stdin(Stdio::null())
                    .stdout(output.try_clone()?)
                    .stderr(output)
                    .spawn()
                    .context("start a check process")?;
                running.push(Running {
                    target: current,
                    child,
                    log,
                    spec,
                    started: now(),
                    watch,
                    resources: entry.resources.clone(),
                });
            }
            // Collect what ended, and write its output whole.
            let mut index = 0;
            let mut ended = false;
            while index < running.len() {
                let Some(status) = running[index].child.try_wait()? else {
                    index += 1;
                    continue;
                };
                ended = true;
                let done = running.remove(index);
                let code = i64::from(status.code().unwrap_or(-1));
                let mut current = done.target;
                let output = fs::read_to_string(&done.log).unwrap_or_default();
                let _ = fs::remove_file(&done.log);
                let _ = fs::remove_file(&done.spec);
                current.seconds = Some((now() - done.started) as i64);
                current.exit = Some(code);
                println!("CITRUS_TARGET target={} status=START", current.target);
                print!("{output}");
                if !output.is_empty() && !output.ends_with('\n') {
                    println!();
                }
                println!(
                    "CITRUS_TARGET target={} status={} exit={code} seconds={}",
                    current.target,
                    if code == 0 { "PASS" } else { "FAIL" },
                    current.seconds.unwrap_or_default()
                );
                let mut outside = Vec::new();
                if let (Some(watch), Some(entry)) =
                    (&done.watch, self.manifest.targets.get(&current.target))
                {
                    let read = crate::observe::in_repository(
                        crate::observe::read(watch, &self.repo.root),
                        &self.repo.files()?,
                    );
                    let globs: Vec<String> = entry
                        .inputs
                        .iter()
                        .chain(&entry.extra_inputs)
                        .chain(&self.repo.config.toolchain_files)
                        .cloned()
                        .collect();
                    if let Ok(inputs) = crate::manifest::GlobList::new(&globs) {
                        outside = crate::observe::outside(&read, &inputs);
                    }
                    let _ = fs::remove_file(watch);
                }
                if code == 0 {
                    current.result = "passed".into();
                    current.reason = "ran".into();
                    if outside.is_empty() {
                        self.record_inputs(run, &current.target)?;
                    } else {
                        let shown: Vec<&str> = outside.iter().take(5).map(String::as_str).collect();
                        println!(
                            "CITRUS_NOTE {} read {}{} outside its inputs; this pass is not reused (add them to #[reads])",
                            current.target,
                            shown.join(", "),
                            if outside.len() > shown.len() {
                                ", …"
                            } else {
                                ""
                            }
                        );
                        current.reason =
                            format!("ran; read outside its inputs: {}", shown.join(", "));
                    }
                    passed.push(current.target.clone());
                } else {
                    current.result = "failed".into();
                    current.reason = "ran".into();
                    let lines: Vec<String> = output.lines().map(str::to_owned).collect();
                    current.first_error = report::first_error(&lines);
                }
                self.store.update_target(&run.id, &current)?;
            }
            if !ended {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        self.stop_services(&started_services)?;
        Ok(passed)
    }

    /// PASS of a declared target: receipt and evidence only if inputs did not change while it ran.
    fn record_inputs(&self, run: &Run, target: &str) -> Result<()> {
        let Some(entry) = self
            .manifest
            .targets
            .get(target)
            .filter(|entry| entry.cache)
        else {
            return Ok(());
        };
        let expected = self
            .store
            .targets(&run.id)?
            .into_iter()
            .find(|row| row.target == target)
            .and_then(|row| row.fingerprint);
        let current = fingerprint(
            &self.repo.root,
            &self.repo.files()?,
            entry,
            &self.repo.config.toolchain_files,
        )?;
        if expected.as_deref() != Some(current.value.as_str()) {
            println!("CITRUS_NOTE inputs of {target} changed during the run; no receipt");
            return Ok(());
        }
        self.repo.receipts().write(target, &current.value)?;
        let detail = serde_json::to_string(&current.files)?;
        self.store
            .put_evidence(target, "inputs", &current.value, "passed", &run.id, &detail)?;
        Ok(())
    }

    /// `--remote` goes to the pool whenever one is configured;
    /// CITRUS_REMOTE=runner asks for the declared runner script instead.
    pub fn uses_pool(&self) -> bool {
        crate::pool::url().is_some()
            && !(std::env::var("CITRUS_REMOTE").as_deref() == Ok("runner")
                && !self.repo.config.run.remote.is_empty())
    }

    fn work_remote(&self, run: &Run, targets: &[RunTarget]) -> Result<Vec<String>> {
        let config = &self.repo.config;
        let argv = config.run.remote.clone();
        // The checks this run needs, one per line: the pool runs these.
        let wanted = self
            .repo
            .state_dir()
            .join("tmp")
            .join(format!("targets-{}", run.id));
        crate::repo::private_dir(&self.repo.state_dir().join("tmp"))?;
        fs::write(
            &wanted,
            targets
                .iter()
                .filter(|target| target.result == "pending")
                .map(|target| format!("{}\n", target.target))
                .collect::<String>(),
        )?;
        // The changed paths a run was asked about (`--paths-file`); empty: the diff.
        let paths_file = wanted.with_extension("paths");
        let given: Vec<String> = self
            .store
            .fact(&format!("paths:{}", run.id))?
            .and_then(|(value, _)| serde_json::from_str(&value).ok())
            .unwrap_or_default();
        fs::write(
            &paths_file,
            given
                .iter()
                .map(|path| format!("{path}\n"))
                .collect::<String>(),
        )?;
        let mut lines: Vec<String> = Vec::new();
        let mut out = std::io::stdout().lock();
        let mut passed = Vec::new();
        let mut handle = |line: String| -> Result<()> {
            let line = strip_ansi(&line);
            writeln!(out, "{line}")?;
            out.flush()?;
            lines.push(line.clone());
            let markers = &config.run;
            if let Some(rest) = prefixed(&line, &markers.waiting_prefix) {
                let resource = rest.split_whitespace().next().unwrap_or(rest);
                self.store.set_state(&run.id, "waiting")?;
                self.store
                    .set_note(&run.id, &format!("waiting for {resource}"))?;
            } else if markers
                .acquired_prefixes
                .iter()
                .any(|prefix| line.starts_with(prefix.as_str()))
            {
                self.store.set_state(&run.id, "running")?;
                self.store.set_note(&run.id, "")?;
            } else if let Some(rest) = prefixed(&line, &markers.stage_prefix) {
                self.store.set_note(&run.id, rest.trim())?;
            } else if let Some(lane) = parse_lane(&line, &markers.progress_prefixes) {
                self.apply_lane(run, targets, &lane, &lines, &mut passed)?;
            }
            Ok(())
        };
        let code = if self.uses_pool() {
            let checks: Vec<String> = targets
                .iter()
                .filter(|target| target.result == "pending")
                .map(|target| target.target.clone())
                .collect();
            crate::pool::run(self, &run.id, &checks, &mut handle)?
        } else {
            // One pipe for stdout and stderr keeps the log in order.
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg("exec \"$@\" 2>&1")
                .arg("citrus-remote")
                .args(&argv)
                .current_dir(&self.repo.root)
                .env("CITRUS_CHECKS", self.manifest.export_file(&self.repo)?)
                .env("CITRUS_TARGETS", &wanted)
                .env("CITRUS_PATHS", &paths_file)
                .env(
                    "CITRUS_PROFILE",
                    self.repo.config.plan.profile.clone().unwrap_or_default(),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            // The base a run was asked to compare with (`citrus run --base`).
            if let Some(base) = &run.base {
                command.env("CITRUS_BASE", base);
            }
            let mut child = command.spawn()?;
            let reader = BufReader::new(child.stdout.take().context("no stdout")?);
            for line in reader.lines() {
                handle(line.unwrap_or_default())?;
            }
            let status = child.wait()?;
            i64::from(status.code().unwrap_or(-1))
        };
        // The runner may keep the detailed output in its own log and print only its path.
        let linked = linked_log(&lines, &config.run.linked_log_markers, &self.repo.root);
        let mut detail: Vec<String> = Vec::new();
        if let Some(path) = &linked {
            self.store.set_linked_log(&run.id, path)?;
            detail = read_log(path);
            let settled: Vec<String> = self
                .store
                .targets(&run.id)?
                .into_iter()
                .filter(|row| matches!(row.result.as_str(), "passed" | "failed"))
                .map(|row| row.target)
                .collect();
            for (index, line) in detail.iter().enumerate() {
                if let Some(lane) = parse_lane(line, &config.run.progress_prefixes)
                    && !settled.contains(&lane.target)
                {
                    self.apply_lane(run, targets, &lane, &detail[..=index], &mut passed)?;
                }
            }
        }
        let _ = fs::remove_file(&wanted);
        let rows = self.store.targets(&run.id)?;
        // A pool that reports checks one by one and is silent about one did not run it.
        let reports_targets = rows
            .iter()
            .any(|row| matches!(row.result.as_str(), "passed" | "failed"));
        let mut code = code;
        let mut silent = Vec::new();
        for mut row in rows
            .into_iter()
            .filter(|row| matches!(row.result.as_str(), "pending" | "running"))
        {
            if code == 0 && reports_targets {
                row.result = "not_run".into();
                row.reason = "not_reported".into();
                silent.push(row.target.clone());
            } else if code == 0 {
                row.result = "passed".into();
                row.reason = "suite_passed".into();
                passed.push(row.target.clone());
            } else {
                row.result = "not_run".into();
                row.reason = "suite_failed".into();
            }
            self.store.update_target(&run.id, &row)?;
        }
        if !silent.is_empty() {
            code = 1;
            self.store.update_target(
                &run.id,
                &RunTarget {
                    target: "suite".into(),
                    result: "failed".into(),
                    reason: "suite_error".into(),
                    fingerprint: None,
                    evidence_run: None,
                    seconds: None,
                    exit: Some(code),
                    first_error: Some(format!(
                        "the pool passed without running {} (CITRUS_TARGETS lists what to run)",
                        silent.join(", ")
                    )),
                },
            )?;
        }
        if code != 0
            && !self
                .store
                .targets(&run.id)?
                .iter()
                .any(|row| row.result == "failed")
        {
            // The suite failed outside any target (transport, builder, planner).
            self.store.update_target(
                &run.id,
                &RunTarget {
                    target: "suite".into(),
                    result: "failed".into(),
                    reason: "suite_error".into(),
                    fingerprint: None,
                    evidence_run: None,
                    seconds: None,
                    exit: Some(code),
                    first_error: report::first_error(&detail)
                        .or_else(|| report::first_error(&lines)),
                },
            )?;
        }
        Ok(passed)
    }
}

impl Context {
    fn apply_lane(
        &self,
        run: &Run,
        targets: &[RunTarget],
        lane: &Lane,
        lines: &[String],
        passed: &mut Vec<String>,
    ) -> Result<()> {
        let mut row = self
            .store
            .targets(&run.id)?
            .into_iter()
            .find(|target| target.target == lane.target)
            .or_else(|| {
                targets
                    .iter()
                    .find(|target| target.target == lane.target)
                    .cloned()
            })
            .unwrap_or(RunTarget {
                target: lane.target.clone(),
                result: "pending".into(),
                reason: "planned_by_suite".into(),
                fingerprint: Some(run.snapshot.clone()),
                evidence_run: None,
                seconds: None,
                exit: None,
                first_error: None,
            });
        match lane.status.as_str() {
            "START" => row.result = "running".into(),
            "PASS" => {
                row.result = "passed".into();
                row.reason = "ran".into();
                passed.push(lane.target.clone());
            }
            _ => {
                row.result = "failed".into();
                row.reason = "ran".into();
                let prefixes = &self.repo.config.run.progress_prefixes;
                row.first_error =
                    report::first_error(&failure_segment(lines, &lane.target, prefixes));
            }
        }
        row.seconds = lane.seconds.or(row.seconds);
        row.exit = lane.exit.or(row.exit);
        self.store.update_target(&run.id, &row)
    }
}

/// Path printed after one of `markers` (last occurrence wins), if that file exists.
fn linked_log(lines: &[String], markers: &[String], root: &std::path::Path) -> Option<String> {
    for line in lines.iter().rev() {
        for marker in markers.iter().filter(|marker| !marker.is_empty()) {
            if let Some((_, rest)) = line.rsplit_once(marker.as_str()) {
                let path = rest
                    .split(|c: char| c.is_whitespace() || c == ';')
                    .next()
                    .unwrap_or_default();
                let full = root.join(path);
                if !path.is_empty() && full.is_file() {
                    return Some(full.display().to_string());
                }
            }
        }
    }
    None
}

#[derive(Debug)]
struct Lane {
    target: String,
    status: String,
    exit: Option<i64>,
    seconds: Option<i64>,
}

fn prefixed<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    if prefix.is_empty() {
        None
    } else {
        line.strip_prefix(prefix)
    }
}

/// `CITRUS_TARGET target=<name> status=START|PASS|FAIL [exit=N] [seconds=N]`, or the
/// same fields after one of the project's own progress prefixes.
fn parse_lane(line: &str, prefixes: &[String]) -> Option<Lane> {
    let rest = std::iter::once("CITRUS_TARGET")
        .chain(prefixes.iter().map(String::as_str))
        .find_map(|prefix| {
            line.strip_prefix(prefix)
                .and_then(|rest| rest.strip_prefix(' '))
        })?;
    let field = |name: &str| {
        rest.split_whitespace()
            .find_map(|part| part.strip_prefix(name))
            .map(str::to_owned)
    };
    Some(Lane {
        target: field("target=")?,
        status: field("status=")?,
        exit: field("exit=").and_then(|value| value.parse().ok()),
        seconds: field("seconds=").and_then(|value| value.parse().ok()),
    })
}

pub fn read_log(path: &str) -> Vec<String> {
    fs::read(path)
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .lines()
                .map(strip_ansi)
                .collect()
        })
        .unwrap_or_default()
}

/// Log lines that explain a failed target: its own segment, or the segment of
/// the innermost target that failed inside it (suites run nested targets).
pub fn failure_segment(lines: &[String], target: &str, prefixes: &[String]) -> Vec<String> {
    let mut current = segment(lines, target, prefixes);
    let mut seen = vec![target.to_owned()];
    loop {
        let nested = current.iter().find_map(|line| {
            parse_lane(line, prefixes)
                .filter(|lane| lane.status == "FAIL" && !seen.contains(&lane.target))
        });
        let Some(lane) = nested else { return current };
        let inner = segment(&current, &lane.target, prefixes);
        if inner.is_empty() {
            return current;
        }
        seen.push(lane.target);
        current = inner;
    }
}

/// Log lines of one target: from its START marker to its result marker.
/// `citrus exec-steps <file>`: run one check's steps (written by a parallel
/// worker) in order, with the given environment; exit with the first failure.
pub fn exec_steps(file: &std::path::Path) -> Result<i32> {
    #[derive(serde::Deserialize)]
    struct Spec {
        root: std::path::PathBuf,
        source: String,
        steps: Vec<crate::model::Step>,
        env: Vec<(String, String)>,
    }
    let spec: Spec = serde_json::from_str(&fs::read_to_string(file)?)?;
    for step in &spec.steps {
        println!("── {}  ({})", step.label, spec.source);
        let code = crate::model::execute_env(step, &spec.root, false, &spec.env)?;
        if code != 0 {
            return Ok(code);
        }
    }
    Ok(0)
}

pub fn segment(lines: &[String], target: &str, prefixes: &[String]) -> Vec<String> {
    let markers: Vec<String> = std::iter::once("CITRUS_TARGET")
        .chain(prefixes.iter().map(String::as_str))
        .map(|prefix| format!("{prefix} target={target} "))
        .collect();
    let is_marker = |line: &String| {
        markers
            .iter()
            .any(|marker| line.starts_with(marker.as_str()))
    };
    let start = lines
        .iter()
        .rposition(|line| is_marker(line) && line.contains("status=START"));
    let Some(start) = start else {
        return Vec::new();
    };
    let end = lines[start + 1..]
        .iter()
        .position(is_marker)
        .map_or(lines.len(), |offset| start + 1 + offset);
    lines[start + 1..end].to_vec()
}

pub fn alive(pid: i64) -> bool {
    // A worker this process started stays a zombie until reaped, and a zombie
    // still "exists" for kill(0): reap it first so its exit is noticed.
    let mut status = 0;
    // SAFETY: WNOHANG never blocks; for a pid that is not our child it returns -1.
    let reaped = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, libc::WNOHANG) };
    if reaped == pid as libc::pid_t {
        return false;
    }
    // SAFETY: signal 0 only checks for existence.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub fn agent() -> String {
    if let Ok(thread) = std::env::var("CODEX_THREAD_ID") {
        return format!("codex:{}", thread.chars().take(8).collect::<String>());
    }
    if std::env::var_os("CLAUDECODE").is_some() {
        return "claude".into();
    }
    std::env::var("CITRUS_AGENT").unwrap_or_else(|_| "human".into())
}

pub fn random16() -> u16 {
    let mut bytes = [0u8; 2];
    if fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes))
        .is_err()
    {
        return (std::process::id() ^ now() as u32) as u16;
    }
    u16::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lane_markers() {
        let prefixes = vec!["CI_PARALLEL_LANE".to_owned()];
        let lane = parse_lane(
            "CI_PARALLEL_LANE target=test-x status=FAIL exit=2 seconds=41",
            &prefixes,
        )
        .unwrap();
        assert_eq!(
            (
                lane.target.as_str(),
                lane.status.as_str(),
                lane.exit,
                lane.seconds
            ),
            ("test-x", "FAIL", Some(2), Some(41))
        );
        assert!(parse_lane("RUN lane=x", &prefixes).is_none());
        assert!(parse_lane("CI_PARALLEL_LANE target=x status=PASS", &[]).is_none());
        assert!(parse_lane("CITRUS_TARGET target=x status=PASS", &[]).is_some());
    }

    #[test]
    fn failure_segment_descends_into_the_failed_nested_target() {
        let prefixes = vec!["LANE".to_owned()];
        let lines: Vec<String> = "LANE target=suite status=START\nLANE target=a status=START\nnoise error: expected\nLANE target=a status=PASS\nLANE target=b status=START\nreal failure\nLANE target=b status=FAIL exit=1\nLANE target=suite status=FAIL exit=2"
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            failure_segment(&lines, "suite", &prefixes),
            vec!["real failure"]
        );
    }

    #[test]
    fn segments_one_target() {
        let lines: Vec<String> = "a\nCI_PARALLEL_LANE target=t status=START\nboom\nCI_PARALLEL_LANE target=t status=FAIL exit=1\nz"
            .lines()
            .map(str::to_owned)
            .collect();
        let prefixes = vec!["CI_PARALLEL_LANE".to_owned()];
        assert_eq!(segment(&lines, "t", &prefixes), vec!["boom"]);
        assert!(segment(&lines, "other", &prefixes).is_empty());
    }
}
