//! Starting runs, deciding what can be reused, and the detached worker that
//! executes the rest and records evidence.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

use crate::config::substitute;
use crate::manifest::{Manifest, fingerprint, now};
use crate::plan;
use crate::repo::Repo;
use crate::report::{self, compact_utc, strip_ansi};
use crate::state::{Run, RunTarget, Store};

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
}

#[derive(Debug)]
pub struct Context {
    pub repo: Repo,
    pub store: Store,
    pub manifest: Manifest,
}

impl Context {
    pub fn open() -> Result<Context> {
        let repo = Repo::discover()?;
        let store = Store::open(&repo.state_dir())?;
        let manifest = Manifest::load(&repo.manifest_path())?;
        Ok(Context {
            repo,
            store,
            manifest,
        })
    }

    pub fn worktree(&self) -> String {
        self.repo.root.display().to_string()
    }

    fn snapshot_max_age(&self) -> i64 {
        (self.repo.config.receipts.snapshot_max_age_hours * 3600) as i64
    }

    fn declared_cache(&self, target: &str) -> bool {
        self.manifest
            .targets
            .get(target)
            .is_some_and(|entry| entry.cache)
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
                decision.reason = "same_snapshot".into();
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

    pub fn start(&self, request: &Request) -> Result<Run> {
        if let Some(key) = &request.key
            && let Some(existing) = self.store.run_by_key(key)?
        {
            return Ok(existing);
        }
        let explicit = !request.targets.is_empty();
        let remote_available = !self.repo.config.run.remote.is_empty();
        if request.mode == Mode::Remote && !remote_available {
            bail!("no remote runner configured (run.remote in citrus.toml)");
        }
        if explicit && request.mode == Mode::Remote {
            bail!("--remote runs the planned set; drop the target names or use --local");
        }
        let names = if explicit {
            request.targets.clone()
        } else {
            plan::compute(&self.repo, &self.manifest, request.base.as_deref())?.targets
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
            Mode::Auto
                if explicit
                    || !remote_available
                    || pending
                        .iter()
                        .all(|decision| self.declared_cache(&decision.target)) =>
            {
                "local"
            }
            Mode::Auto => "remote",
        };
        let started = now();
        let id = format!("r-{}-{:04x}", compact_utc(started), random16());
        fs::create_dir_all(self.repo.log_dir())?;
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
    pub fn work(&self, id: &str) -> Result<()> {
        let run = self.store.run(id)?.context("unknown run")?;
        self.store.set_state(id, "running")?;
        let targets = self.store.targets(id)?;
        let passed = if run.mode == "remote" {
            self.work_remote(&run, &targets)?
        } else {
            self.work_local(&run, &targets)?
        };
        let finished = self.store.targets(id)?;
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
        for target in targets.iter().filter(|target| target.result == "pending") {
            let mut current = target.clone();
            current.result = "running".into();
            self.store.update_target(&run.id, &current)?;
            println!("CITRUS_TARGET target={} status=START", target.target);
            let started = now();
            let argv = substitute(&self.repo.config.run.local, "{target}", &target.target);
            let mut command = Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .current_dir(&self.repo.root)
                .stdin(Stdio::null());
            if let Some(env) = self.repo.config.run.env.get(&target.target) {
                command.envs(env);
            }
            let status = command.status()?;
            let code = i64::from(status.code().unwrap_or(-1));
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
                self.record_inputs(run, &target.target)?;
                passed.push(target.target.clone());
            } else {
                current.result = "failed".into();
                current.reason = "ran".into();
                current.first_error =
                    report::first_error(&segment(&read_log(&run.log), &target.target, &[]));
            }
            self.store.update_target(&run.id, &current)?;
        }
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

    fn work_remote(&self, run: &Run, targets: &[RunTarget]) -> Result<Vec<String>> {
        let config = &self.repo.config;
        let mut argv = config.run.remote.clone();
        if let Some(base) = run
            .base
            .as_deref()
            .filter(|_| !config.plan.base_arg.is_empty())
        {
            argv.push(config.plan.base_arg.replace("{base}", base));
        }
        // One pipe for stdout and stderr keeps the log in order.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("exec \"$@\" 2>&1")
            .arg("citrus-remote")
            .args(&argv)
            .current_dir(&self.repo.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        let mut child = command.spawn()?;
        let reader = BufReader::new(child.stdout.take().context("no stdout")?);
        let mut lines: Vec<String> = Vec::new();
        let mut out = std::io::stdout().lock();
        let mut passed = Vec::new();
        for line in reader.lines() {
            let line = strip_ansi(&line.unwrap_or_default());
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
        }
        let status = child.wait()?;
        let code = i64::from(status.code().unwrap_or(-1));
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
        let rows = self.store.targets(&run.id)?;
        for mut row in rows
            .into_iter()
            .filter(|row| matches!(row.result.as_str(), "pending" | "running"))
        {
            if code == 0 {
                row.result = "passed".into();
                row.reason = "suite_passed".into();
                passed.push(row.target.clone());
            } else {
                row.result = "not_run".into();
                row.reason = "suite_failed".into();
            }
            self.store.update_target(&run.id, &row)?;
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

fn alive(pid: i64) -> bool {
    // SAFETY: signal 0 only checks for existence.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn agent() -> String {
    if let Ok(thread) = std::env::var("CODEX_THREAD_ID") {
        return format!("codex:{}", thread.chars().take(8).collect::<String>());
    }
    if std::env::var_os("CLAUDECODE").is_some() {
        return "claude".into();
    }
    std::env::var("CITRUS_AGENT").unwrap_or_else(|_| "human".into())
}

fn random16() -> u16 {
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
