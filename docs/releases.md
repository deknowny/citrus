# Releases: `ci/releases.toml`

A release unit is a named sequence of your own commands. Citrus runs them in
order from a committed source, records every step, holds the unit's
environment for one release at a time, and knows what to do after an
interruption.

```toml
[releases.web]
description = "Web app to production"
environment = "web-production"   # one release at a time here, across worktrees
checks = "proven"                # default: every planned check must be proven for the commit

[releases.web.version]
reserve = ["make", "version-reserve", "START={next}"]   # prints RELEASE=<version>
prefix = "RELEASE="              # default
initial = "1.4.0"                # first {next} when Citrus has no passed release yet

[[releases.web.steps]]
name = "build"
run = ["make", "image", "VERSION={version}"]

[[releases.web.steps]]
name = "deploy"
production = true                # needs --approve
run = ["make", "deploy", "VERSION={version}"]
recover = ["make", "deploy-reconcile", "VERSION={version}"]   # used when the outcome is unknown

[[releases.web.steps]]
name = "postcheck"
run = ["make", "smoke", "VERSION={version}"]

[releases.web.rollback]
production = true
run = ["make", "deploy", "VERSION={version}"]   # {version} = the release before the last passed one
```

Values in commands: `{version}` (reserved, or the rollback target), `{previous}`
(last passed release), `{next}` (after `{previous}`, or `initial`), `{commit}`, `{unit}`.

## Commands

| | |
|---|---|
| `citrus release` | Units, their steps (`*` changes production) and last release |
| `citrus release start <unit> --approve` | Release from HEAD; `--dry-run` shows gates and exact commands without running |
| `citrus release wait / show / log <id\|last>` | Follow a release after a lost session; first error of the failed step |
| `citrus release resume <id> --approve` | Continue a failed or unknown release from its first unfinished step |
| `citrus release abandon <id> --reason …` | Give up an unfinished release after checking the environment by hand |
| `citrus release rollback <unit> --approve` | Run `rollback` towards the release before the last passed one |
| `citrus release history <unit>` | Past releases with version, commit, agent and outcome |

## Guarantees

- **Gates before anything runs:** a committed tree; checks proven for this
  commit (`checks = "proven"`, or `--unchecked`, which is recorded); `--approve`
  when any step changes production; the environment not held by an unfinished
  or unknown release.
- **One release per environment.** A failed release frees it; a release whose
  worker vanished mid-step becomes `unknown` and keeps it — nobody can deploy
  over a state nobody has verified.
- **Resume, not repeat.** Passed steps are never run again. An unknown step runs
  its `recover` command (reconcile the real state) instead of being repeated;
  without `recover` it is run again.
- **History** keeps the version, commit, agent and outcome of every release
  and rollback.
