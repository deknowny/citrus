//! The agent's governor: a pool agent shares its machine with production.
//!
//! A fixed CPU share is either too timid on an idle machine or too greedy on a
//! busy one. The governor measures what everything *but* the pool is using,
//! watches the kernel's pressure counters (PSI) for CPU, disk and memory, and
//! moves a CPU budget up slowly and down fast:
//!
//! * the ceiling is `target × cores − foreign load`, never above the share;
//! * pressure (runnable tasks waiting, I/O stalls, low memory) cuts the budget
//!   multiplicatively, whatever the ceiling says;
//! * the budget limits how many checks the agent takes at once and is written
//!   to the pool's cgroup (`cpu.max`) so checks already running slow down too.
//!
//! Only Linux has the counters; elsewhere the agent keeps its fixed share.

use std::path::{Path, PathBuf};
use std::time::Instant;

/// What the governor may do and how careful it is.
#[derive(Debug, Clone)]
pub struct Config {
    /// Logical CPUs of the machine.
    pub cores: f64,
    /// The most the pool may use (the agent's `--share`).
    pub share: f64,
    /// The least it keeps, even under pressure (`--min-cpus`).
    pub floor: f64,
    /// Fraction of the machine the pool and production may use together.
    pub target: f64,
    /// The slice the agent's check containers run in, when it has one. The
    /// cgroup exists only while checks run, so it is looked up on every tick.
    pub slice: Option<String>,
}

/// One measurement over the time since the previous one.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Sample {
    /// CPUs the whole machine kept busy.
    pub busy: f64,
    /// CPUs the pool's checks used (0 when its cgroup is not readable).
    pub pool: f64,
    /// Share of time some task waited for a CPU (PSI `some`, avg10, percent).
    pub cpu_pressure: f64,
    /// Share of time all tasks stalled on I/O (PSI `full`, avg10, percent).
    pub io_pressure: f64,
    /// MemAvailable / MemTotal.
    pub memory_free: f64,
    /// Worst `fdatasync` of a small file on the cache disk over the last few
    /// ticks, milliseconds: what etcd and every database on the machine feel
    /// (0 when there is no probe).
    pub fsync_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Decision {
    /// CPUs the pool may use now.
    pub budget: f64,
    /// Why it is below the share, or empty.
    pub reason: &'static str,
}

const CPU_PRESSURE_LIMIT: f64 = 25.0;
const IO_PRESSURE_LIMIT: f64 = 10.0;
const MEMORY_FREE_LIMIT: f64 = 0.08;
/// A healthy NVMe mirror syncs in a few ms; etcd complains at 100 ms.
pub const FSYNC_LIMIT_MS: f64 = 40.0;
/// Added per tick while there is room (CPUs).
const GROW: f64 = 0.5;
/// The pool is blamed for pressure only when it uses at least this many CPUs.
const BLAME_CPUS: f64 = 1.0;
/// What is kept per tick under pressure.
const KEEP: f64 = 0.6;

/// The next budget given the previous one and a fresh sample.
pub fn decide(config: &Config, previous: f64, sample: &Sample) -> Decision {
    let foreign = (sample.busy - sample.pool).max(0.0);
    let ceiling = (config.cores * config.target - foreign).clamp(config.floor, config.share);
    let (pressured, reason) = if sample.cpu_pressure > CPU_PRESSURE_LIMIT {
        (true, "cpu pressure")
    } else if sample.io_pressure > IO_PRESSURE_LIMIT {
        (true, "disk pressure")
    } else if sample.fsync_ms > FSYNC_LIMIT_MS {
        (true, "disk latency")
    } else if sample.memory_free < MEMORY_FREE_LIMIT {
        (true, "low memory")
    } else {
        (false, "")
    };
    if pressured {
        // Giving CPUs back only helps when the pool is part of the problem: a
        // pool using next to nothing keeps its budget (it just does not grow)
        // while production presses on itself.
        if sample.pool < BLAME_CPUS {
            return Decision {
                budget: previous.min(ceiling).max(config.floor),
                reason,
            };
        }
        return Decision {
            budget: (previous.min(sample.pool + 1.0) * KEEP)
                .max(config.floor)
                .min(ceiling.max(config.floor)),
            reason,
        };
    }
    // Production grew: give the CPUs back at once; otherwise grow slowly.
    let budget = if previous > ceiling {
        ceiling
    } else {
        (previous + GROW).min(ceiling)
    };
    Decision {
        budget,
        reason: if budget + 0.01 < config.share {
            "production load"
        } else {
            ""
        },
    }
}

/// Exponentially smoothed measurements: one busy second must not move the budget.
#[derive(Debug, Default)]
pub struct Smoother {
    busy: Option<f64>,
    pool: Option<f64>,
}

impl Smoother {
    const ALPHA: f64 = 0.2;

    /// The sample with its CPU figures smoothed; pressure stays as measured
    /// (the kernel already averages it over ten seconds).
    pub fn smooth(&mut self, sample: Sample) -> Sample {
        let blend = |old: &mut Option<f64>, new: f64| {
            let value = old.map_or(new, |old| old + Self::ALPHA * (new - old));
            *old = Some(value);
            value
        };
        Sample {
            busy: blend(&mut self.busy, sample.busy),
            pool: blend(&mut self.pool, sample.pool),
            ..sample
        }
    }
}

/// Checks the agent may run at once for a budget: a slot is `share / slots` CPUs.
pub fn allowed_slots(budget: f64, share: f64, slots: usize) -> usize {
    let per_slot = (share / slots as f64).max(0.1);
    ((budget / per_slot).ceil() as usize).clamp(1, slots)
}

// ------------------------------------------------------------ measuring

/// Busy and total jiffies from the first line of /proc/stat.
fn parse_stat(text: &str) -> Option<(f64, f64)> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let fields: Vec<f64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|field| field.parse().ok())
        .collect();
    if fields.len() < 8 {
        return None;
    }
    // user nice system idle iowait irq softirq steal
    let idle = fields[3] + fields[4];
    let total: f64 = fields[..8].iter().sum();
    Some((total - idle, total))
}

/// `avg10` of the `some` or `full` line of a PSI file.
fn parse_pressure(text: &str, kind: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with(kind))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
}

fn parse_memory(text: &str) -> Option<f64> {
    let value = |name: &str| -> Option<f64> {
        text.lines()
            .find(|line| line.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    };
    let total = value("MemTotal:")?;
    (total > 0.0).then(|| value("MemAvailable:").unwrap_or(total) / total)
}

fn parse_usage_usec(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.strip_prefix("usage_usec "))?
        .trim()
        .parse()
        .ok()
}

pub struct Sampler {
    cores: f64,
    slice: Option<String>,
    last: Option<(f64, f64, Option<f64>, Instant)>,
}

impl Sampler {
    pub fn new(cores: f64, slice: Option<String>) -> Sampler {
        Sampler {
            cores,
            slice,
            last: None,
        }
    }

    /// None the first time (nothing to compare with) and off Linux.
    pub fn sample(&mut self) -> Option<Sample> {
        let stat = std::fs::read_to_string("/proc/stat").ok()?;
        let (busy, total) = parse_stat(&stat)?;
        let usage = self
            .slice
            .as_deref()
            .and_then(slice_dir)
            .and_then(|dir| std::fs::read_to_string(dir.join("cpu.stat")).ok())
            .and_then(|text| parse_usage_usec(&text));
        let now = Instant::now();
        let previous = self.last.replace((busy, total, usage, now));
        let (old_busy, old_total, old_usage, then) = previous?;
        let jiffies = total - old_total;
        if jiffies <= 0.0 {
            return None;
        }
        let seconds = now.duration_since(then).as_secs_f64().max(0.001);
        let pool = match (usage, old_usage) {
            (Some(now), Some(then)) => ((now - then) / 1e6 / seconds).max(0.0),
            _ => 0.0,
        };
        Some(Sample {
            busy: (busy - old_busy) / jiffies * self.cores,
            pool,
            cpu_pressure: read(Path::new("/proc/pressure/cpu"), |text| {
                parse_pressure(text, "some")
            })
            .unwrap_or(0.0),
            io_pressure: read(Path::new("/proc/pressure/io"), |text| {
                parse_pressure(text, "full")
            })
            .unwrap_or(0.0),
            memory_free: read(Path::new("/proc/meminfo"), parse_memory).unwrap_or(1.0),
            fsync_ms: 0.0,
        })
    }
}

fn read<T>(path: &Path, parse: impl Fn(&str) -> Option<T>) -> Option<T> {
    parse(&std::fs::read_to_string(path).ok()?)
}

/// Writes the budget as the cgroup's CPU limit; best effort (a service manager
/// may own the file, or the agent may not be root).
pub fn limit_cgroup(dir: &Path, budget: f64) -> bool {
    let period = 100_000u64;
    let quota = (budget * period as f64).round().max(1000.0) as u64;
    std::fs::write(dir.join("cpu.max"), format!("{quota} {period}")).is_ok()
}

/// The cgroup directory of a slice name (`citrus-checks.slice`), if it exists.
/// systemd nests a dashed slice under its prefixes
/// (`citrus.slice/citrus-checks.slice`), and creates it only while it has members.
pub fn slice_dir(slice: &str) -> Option<PathBuf> {
    let stem = slice.strip_suffix(".slice")?;
    let mut dir = PathBuf::from("/sys/fs/cgroup");
    let mut prefix = String::new();
    let parts: Vec<&str> = stem.split('-').collect();
    for (index, part) in parts.iter().enumerate() {
        if !prefix.is_empty() {
            prefix.push('-');
        }
        prefix.push_str(part);
        if index + 1 < parts.len() {
            let parent = dir.join(format!("{prefix}.slice"));
            if parent.is_dir() {
                dir = parent;
            }
        }
    }
    dir = dir.join(slice);
    dir.join("cpu.stat").is_file().then_some(dir)
}

/// Times `fdatasync` of a small file the way a write-ahead log does it. The
/// worst of the last few probes counts: a stall is what hurts, not the mean.
pub struct DiskProbe {
    file: std::fs::File,
    path: PathBuf,
    recent: std::collections::VecDeque<f64>,
}

impl DiskProbe {
    const KEEP: usize = 3;

    /// A probe file in `dir` (which must be on the disk production syncs to).
    pub fn new(dir: &Path) -> Option<DiskProbe> {
        std::fs::create_dir_all(dir).ok()?;
        // Files of agents that were killed.
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(".fsync-probe-")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        let path = dir.join(format!(".fsync-probe-{}", std::process::id()));
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .ok()?;
        Some(DiskProbe {
            file,
            path,
            recent: Default::default(),
        })
    }

    /// One probe; the worst latency of the last few, in milliseconds.
    pub fn probe(&mut self) -> f64 {
        use std::io::{Seek, SeekFrom, Write};
        let started = Instant::now();
        let synced = self
            .file
            .seek(SeekFrom::Start(0))
            .and_then(|_| self.file.write_all(&[0x5a; 8192]))
            .and_then(|_| self.file.sync_data());
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        // A failed write says nothing about latency.
        if synced.is_ok() {
            self.recent.push_back(ms);
            if self.recent.len() > Self::KEEP {
                self.recent.pop_front();
            }
        }
        self.recent.iter().copied().fold(0.0, f64::max)
    }
}

impl Drop for DiskProbe {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The write bandwidth the pool's cgroup may use on the cache disk: halved
/// while the disk's sync latency is high, restored step by step when it calms.
/// It is what keeps a build's dirty pages from starving production's `fsync`.
pub struct WriteLimit {
    device: String,
    cap: f64,
    floor: f64,
    current: f64,
}

impl WriteLimit {
    /// `cap_mib` MiB/s at most, never below a tenth of it (at least 8).
    /// None off Linux or when the disk is not a block device.
    pub fn new(dir: &Path, cap_mib: f64) -> Option<WriteLimit> {
        use std::os::unix::fs::MetadataExt;
        if cap_mib <= 0.0 {
            return None;
        }
        let dev = std::fs::metadata(dir).ok()?.dev();
        // glibc's major()/minor().
        let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
        let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
        if major == 0 {
            return None;
        }
        Some(WriteLimit {
            device: format!("{major}:{minor}"),
            cap: cap_mib,
            floor: (cap_mib / 10.0).max(8.0).min(cap_mib),
            current: cap_mib,
        })
    }

    /// The limit after a tick with the disk this slow (MiB/s).
    pub fn adjust(&mut self, fsync_ms: f64) -> f64 {
        self.current = if fsync_ms > FSYNC_LIMIT_MS {
            (self.current * 0.5).max(self.floor)
        } else if fsync_ms > FSYNC_LIMIT_MS / 2.0 {
            self.current
        } else {
            (self.current * 1.25 + 1.0).min(self.cap)
        };
        self.current
    }

    /// Writes `io.max` for the slice; best effort like the CPU limit.
    pub fn apply(&self, dir: &Path) -> bool {
        let bytes = (self.current * 1024.0 * 1024.0) as u64;
        std::fs::write(dir.join("io.max"), format!("{} wbps={bytes}", self.device)).is_ok()
    }

    /// Lifts the limit (the agent leaves).
    pub fn lift(&self, dir: &Path) -> bool {
        std::fs::write(dir.join("io.max"), format!("{} wbps=max", self.device)).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            cores: 32.0,
            share: 10.0,
            floor: 1.0,
            target: 0.8,
            slice: None,
        }
    }

    fn calm(busy: f64, pool: f64) -> Sample {
        Sample {
            busy,
            pool,
            cpu_pressure: 0.0,
            io_pressure: 0.0,
            memory_free: 0.5,
            fsync_ms: 0.0,
        }
    }

    #[test]
    fn an_idle_machine_grows_the_budget_to_the_share_step_by_step() {
        let mut budget = 2.0;
        for _ in 0..30 {
            budget = decide(&config(), budget, &calm(1.0, 0.0)).budget;
        }
        assert_eq!(budget, 10.0);
        assert_eq!(decide(&config(), 2.0, &calm(1.0, 0.0)).budget, 2.5);
    }

    #[test]
    fn production_load_lowers_the_ceiling_at_once() {
        // 32 × 0.8 = 25.6; production uses 20, the pool 8 of its own.
        let decision = decide(&config(), 10.0, &calm(28.0, 8.0));
        assert!((decision.budget - 5.6).abs() < 1e-9, "{decision:?}");
        assert_eq!(decision.reason, "production load");
    }

    #[test]
    fn the_pool_does_not_count_itself_as_production() {
        // The whole machine is busy, but it is all the pool's own work.
        let decision = decide(&config(), 10.0, &calm(10.0, 10.0));
        assert_eq!(decision.budget, 10.0);
        assert_eq!(decision.reason, "");
    }

    #[test]
    fn pressure_cuts_the_budget_when_the_pool_is_using_cpus() {
        let mut sample = calm(9.0, 8.0);
        sample.cpu_pressure = 40.0;
        let decision = decide(&config(), 10.0, &sample);
        // 60 % of what the pool really uses (8 + 1).
        assert!((decision.budget - 5.4).abs() < 1e-9, "{decision:?}");
        assert_eq!(decision.reason, "cpu pressure");
        sample = calm(9.0, 8.0);
        sample.io_pressure = 30.0;
        assert_eq!(decide(&config(), 10.0, &sample).reason, "disk pressure");
        sample = calm(9.0, 8.0);
        sample.memory_free = 0.02;
        assert_eq!(decide(&config(), 10.0, &sample).reason, "low memory");
    }

    #[test]
    fn pressure_production_makes_itself_does_not_starve_an_idle_pool() {
        // The machine presses on itself; the pool uses 0.2 CPU. Cutting it helps nobody.
        let mut sample = calm(20.0, 0.2);
        sample.cpu_pressure = 60.0;
        let decision = decide(&config(), 5.0, &sample);
        assert_eq!(decision.budget, 5.0);
        assert_eq!(decision.reason, "cpu pressure");
    }

    #[test]
    fn the_floor_holds_under_sustained_pressure() {
        let mut sample = calm(31.0, 20.0);
        sample.cpu_pressure = 90.0;
        let mut budget = 10.0;
        for _ in 0..20 {
            budget = decide(&config(), budget, &sample).budget;
        }
        assert_eq!(budget, 1.0);
    }

    #[test]
    fn a_short_spike_barely_moves_the_smoothed_load() {
        let mut smoother = Smoother::default();
        let mut last = Sample::default();
        for _ in 0..10 {
            last = smoother.smooth(calm(10.0, 0.0));
        }
        assert!((last.busy - 10.0).abs() < 1e-9);
        let spiked = smoother.smooth(calm(30.0, 0.0));
        assert!((spiked.busy - 14.0).abs() < 1e-9, "{spiked:?}");
    }

    #[test]
    fn slots_follow_the_budget() {
        // 10 CPUs over 4 slots: 2.5 CPUs a slot.
        assert_eq!(allowed_slots(10.0, 10.0, 4), 4);
        assert_eq!(allowed_slots(5.0, 10.0, 4), 2);
        assert_eq!(allowed_slots(5.1, 10.0, 4), 3);
        assert_eq!(allowed_slots(0.3, 10.0, 4), 1);
    }

    #[test]
    fn kernel_counters_parse() {
        let stat = "cpu  100 0 50 800 20 5 5 20 0 0\ncpu0 1 1 1 1 1 1 1 1 0 0\n";
        assert_eq!(parse_stat(stat), Some((180.0, 1000.0)));
        let psi = "some avg10=12.50 avg60=3.00 avg300=1.00 total=99\nfull avg10=4.25 avg60=1.00 avg300=0.50 total=9\n";
        assert_eq!(parse_pressure(psi, "some"), Some(12.5));
        assert_eq!(parse_pressure(psi, "full"), Some(4.25));
        let memory = "MemTotal:  1000 kB\nMemFree: 100 kB\nMemAvailable:  250 kB\n";
        assert_eq!(parse_memory(memory), Some(0.25));
        assert_eq!(
            parse_usage_usec("usage_usec 5000000\nuser_usec 1\n"),
            Some(5e6)
        );
    }

    #[test]
    fn slow_syncs_cut_the_pool_and_halve_its_write_limit() {
        let mut sample = calm(8.0, 6.0);
        sample.fsync_ms = 300.0;
        let decision = decide(&config(), 12.0, &sample);
        assert_eq!(decision.reason, "disk latency");
        assert!(decision.budget < 12.0);
        let mut limit = WriteLimit {
            device: "9:1".into(),
            cap: 200.0,
            floor: 20.0,
            current: 200.0,
        };
        assert_eq!(limit.adjust(300.0), 100.0);
        assert_eq!(limit.adjust(300.0), 50.0);
        assert_eq!(limit.adjust(300.0), 25.0);
        assert_eq!(limit.adjust(300.0), 20.0, "never below the floor");
        assert_eq!(
            limit.adjust(30.0),
            20.0,
            "holds while the disk is not yet calm"
        );
        assert!(limit.adjust(3.0) > 20.0, "recovers when it is");
        for _ in 0..40 {
            limit.adjust(3.0);
        }
        assert_eq!(limit.adjust(3.0), 200.0, "back to the cap");
    }

    #[test]
    fn a_probe_on_a_real_disk_reports_a_latency() {
        let dir = std::env::temp_dir().join(format!("citrus-probe-{}", std::process::id()));
        let mut probe = DiskProbe::new(&dir).unwrap();
        let ms = probe.probe();
        assert!(ms > 0.0 && ms < 5000.0, "{ms}");
        drop(probe);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
