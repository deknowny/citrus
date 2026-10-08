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

/// What test runners and compilers said failed, read from their own
/// formats: failed Rust tests with the place they panicked, rustc and clippy
/// errors with their place, failed Python and Node tests. At most a few
/// lines; none when nothing is recognised.
pub fn diagnose(lines: &[String]) -> Vec<String> {
    const MAX: usize = 6;
    let mut found: Vec<String> = Vec::new();
    let push = |line: String, found: &mut Vec<String>| {
        if found.len() < MAX && !found.contains(&line) {
            found.push(line);
        }
    };
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        // cargo test: `test path::name ... FAILED`, then the panic.
        if let Some(name) = trimmed
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            let panic = lines.iter().find_map(|line| {
                let rest = line.split_once(&format!("'{name}'"))?.1;
                let place = rest.split_once("panicked at ")?.1.trim_end_matches(':');
                Some(place.to_owned())
            });
            push(
                match panic {
                    Some(place) => format!("test {name} failed at {place}"),
                    None => format!("test {name} failed"),
                },
                &mut found,
            );
        }
        // rustc, clippy: `error[E0308]: …` or `error: …`, then `--> file:line:col`.
        if (trimmed.starts_with("error[") || trimmed.starts_with("error: "))
            && !trimmed.starts_with("error: could not compile")
            && !trimmed.starts_with("error: test failed")
            && !trimmed.starts_with("error: aborting")
            && let Some(place) = lines
                .iter()
                .skip(index + 1)
                .take(3)
                .find_map(|next| next.trim().strip_prefix("--> "))
        {
            push(format!("{trimmed} ({place})"), &mut found);
        }
        // unittest: `FAIL: test_x (module.Case.test_x)` / `ERROR: …`.
        if (trimmed.starts_with("FAIL: ") || trimmed.starts_with("ERROR: "))
            && trimmed.contains('(')
        {
            // Under the header's `-----`, the traceback runs to the next one.
            let reason = lines
                .iter()
                .skip(index + 1)
                .skip_while(|next| next.starts_with("-----"))
                .take(40)
                .take_while(|next| !next.starts_with("-----") && !next.starts_with("====="))
                .filter(|next| {
                    let next = next.trim();
                    !next.is_empty() && !next.starts_with("File ") && !next.starts_with("Traceback")
                })
                .last()
                .map(|next| next.trim().to_owned());
            push(
                match reason {
                    Some(reason) => format!("{trimmed}: {}", clip(&reason)),
                    None => trimmed.to_owned(),
                },
                &mut found,
            );
        }
        // pytest: `FAILED path::test - Error`.
        if trimmed.starts_with("FAILED ") && trimmed.contains("::") {
            push(clip(trimmed), &mut found);
        }
        // node --test (TAP): `not ok 3 - name`.
        if let Some(rest) = trimmed.strip_prefix("not ok ")
            && let Some((_, name)) = rest.split_once(" - ")
        {
            push(format!("node test failed: {name}"), &mut found);
        }
    }
    found
}

/// A short excerpt describing the failure, or the tail when nothing matches.
/// What the tools themselves reported comes first.
pub fn first_error(lines: &[String]) -> Option<String> {
    let excerpt = excerpt(lines)?;
    let diagnosed = diagnose(lines);
    if diagnosed.is_empty() {
        return Some(excerpt);
    }
    Some(format!("{}\n{excerpt}", diagnosed.join("\n")))
}

fn excerpt(lines: &[String]) -> Option<String> {
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

#[cfg(test)]
mod diagnose_tests {
    use super::diagnose;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn tools_say_what_failed() {
        let cargo = lines(
            "running 2 tests\ntest api::ok ... ok\ntest api::sums ... FAILED\n\nfailures:\n\n---- api::sums stdout ----\n\nthread 'api::sums' (77) panicked at src/lib.rs:9:5:\nassertion `left == right` failed\n",
        );
        assert_eq!(
            diagnose(&cargo),
            vec!["test api::sums failed at src/lib.rs:9:5"]
        );
        let rustc = lines(
            "   Compiling api v0.1.0\nerror[E0308]: mismatched types\n  --> src/lib.rs:3:17\n   |\nerror: could not compile `api`\n",
        );
        assert_eq!(
            diagnose(&rustc),
            vec!["error[E0308]: mismatched types (src/lib.rs:3:17)"]
        );
        let unittest = lines(
            "F.\n======================================================================\nFAIL: test_limits (test_tool.ToolTest.test_limits)\n----------------------------------------------------------------------\nTraceback (most recent call last):\n  File \"x.py\", line 3, in test_limits\n    self.assertEqual(1, 2)\nAssertionError: 1 != 2\n\n----------------------------------------------------------------------\nRan 2 tests\n",
        );
        assert_eq!(
            diagnose(&unittest),
            vec!["FAIL: test_limits (test_tool.ToolTest.test_limits): AssertionError: 1 != 2"]
        );
        let node = lines("TAP version 13\nok 1 - parses\nnot ok 2 - renders the plan\n");
        assert_eq!(diagnose(&node), vec!["node test failed: renders the plan"]);
        assert!(diagnose(&lines("all good\n")).is_empty());
    }
}
