//! Turning long logs into the few lines an agent or a person needs.

/// Lines that name a concrete failure.
const STRONG: [&str; 14] = [
    "error[E",
    "error:",
    "Error:",
    "ERROR",
    "panicked at",
    "Traceback (most recent call last)",
    "AssertionError",
    "assertion",
    "✖",
    "✗",
    "not ok ",
    "FAILED",
    "FAIL:",
    "Exception",
];
/// Summary lines that only say "something failed"; used when nothing better exists.
const GENERIC: [&str; 4] = ["FAIL ", "make: *** ", "Error ", "exit status"];
/// Lines that look like failures but are progress or passing output.
const NOISE: [&str; 9] = [
    "✔",
    "ok ",
    "PASS",
    "CI_PARALLEL_LANE",
    "STAGE_",
    "RUN ",
    "# ",
    "0 errors",
    "failed=0",
];

pub const EXCERPT_LINES: usize = 40;
const LINE_WIDTH: usize = 240;

pub fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        if c != '\r' {
            out.push(c);
        }
    }
    out
}

fn clip(line: &str) -> String {
    if line.chars().count() <= LINE_WIDTH {
        return line.to_owned();
    }
    let mut clipped: String = line.chars().take(LINE_WIDTH).collect();
    clipped.push('…');
    clipped
}

/// Index of the first line describing the failure, if any.
pub fn first_error_index(lines: &[String]) -> Option<usize> {
    let quiet = |line: &str| {
        let trimmed = line.trim_start();
        NOISE.iter().any(|noise| trimmed.starts_with(noise))
    };
    lines
        .iter()
        .position(|line| !quiet(line) && STRONG.iter().any(|marker| line.contains(marker)))
        .or_else(|| {
            lines
                .iter()
                .position(|line| !quiet(line) && GENERIC.iter().any(|marker| line.contains(marker)))
        })
}

/// Lines Make prints when a recipe fails: `make: *** [...] Error N`.
fn is_make_failure(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("make: *** ")
        || trimmed
            .strip_prefix("make[")
            .and_then(|rest| rest.split_once("]: *** "))
            .is_some_and(|(level, _)| level.chars().all(|c| c.is_ascii_digit()))
}

/// Output just before Make reports the failed recipe: what the failing command
/// printed last. Test runners print their verdict there, while words like
/// "error:" earlier are often expected output of passing tests.
const BEFORE_MAKE_FAILURE: usize = 15;

/// A short excerpt describing the failure, or the tail when nothing matches.
pub fn first_error(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    if let Some(anchor) = lines.iter().position(|line| is_make_failure(line)) {
        let end = lines[anchor..]
            .iter()
            .take_while(|line| is_make_failure(line))
            .count()
            + anchor;
        let start = anchor.saturating_sub(BEFORE_MAKE_FAILURE);
        let excerpt: Vec<String> = lines[start..end]
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| clip(line))
            .collect();
        return Some(excerpt.join("\n"));
    }
    let (start, end) = match first_error_index(lines) {
        Some(index) => (
            index.saturating_sub(3),
            (index + EXCERPT_LINES - 3).min(lines.len()),
        ),
        None => (lines.len().saturating_sub(15), lines.len()),
    };
    let excerpt: Vec<String> = lines[start..end]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| clip(line))
        .collect();
    Some(excerpt.join("\n"))
}

pub fn age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h{:02}m", seconds / 3600, seconds % 3600 / 60),
        _ => format!("{}d", seconds / 86400),
    }
}

/// `YYYYMMDD-HHMMSS` in UTC.
pub fn compact_utc(epoch: u64) -> String {
    let days = (epoch / 86400) as i64;
    let rem = epoch % 86400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Howard Hinnant's days → civil date conversion.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn picks_specific_failure_over_make_summary() {
        let log = lines(
            "RUN lane=x\n✔ handles error: gracefully\nbuilding\nTraceback (most recent call last):\n  File \"a.py\"\nAssertionError: boom\nmake: *** [x] Error 1",
        );
        assert_eq!(first_error_index(&log), Some(3));
        assert!(first_error(&log).unwrap().contains("AssertionError: boom"));
    }

    #[test]
    fn prefers_what_ran_last_before_make_reports_the_failure() {
        let mut log = vec![
            "test git push ... ".to_owned(),
            "error: failed to push some refs (expected)".to_owned(),
        ];
        log.extend((0..30).map(|index| format!("ok {index}")));
        log.extend(lines("PASS end_to_end duration=38s\nSTALE suite=db-e2e before=a after=b\nmake[1]: *** [x.mk:51: db-e2e] Error 75\nmake: *** [y.mk:2: contract] Error 2"));
        let excerpt = first_error(&log).unwrap();
        assert!(excerpt.contains("STALE suite=db-e2e"), "{excerpt}");
        assert!(excerpt.ends_with("make: *** [y.mk:2: contract] Error 2"));
        assert!(!excerpt.contains("failed to push"));
    }

    #[test]
    fn falls_back_to_generic_then_tail() {
        assert_eq!(
            first_error_index(&lines("a\nb\nmake: *** [t] Error 2")),
            Some(2)
        );
        assert_eq!(first_error(&lines("a\nb")).unwrap(), "a\nb");
    }

    #[test]
    fn strips_terminal_colours() {
        assert_eq!(strip_ansi("\u{1b}[31merror\u{1b}[0m: x\r"), "error: x");
    }

    #[test]
    fn formats_utc() {
        assert_eq!(compact_utc(0), "19700101-000000");
        assert_eq!(compact_utc(1_791_380_000), "20261007-133320");
    }
}
