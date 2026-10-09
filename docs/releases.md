# Releases

A release unit is a named sequence of your own commands. Citrus runs them in
order from a committed source, records every step, holds the unit's
environment for one release at a time, and knows what to do after an
interruption.

```rust
/// The web app's production: one release at a time here, across worktrees.
environment web_production;

/// The web app to production.
#[environment(web_production)]
// The version after the last passed (or running) release, or the first free
// one after it; held for this commit and the names it publishes.
// `citrus release start web --bump minor` (or `major`) raises that part
// instead of the patch; `--version x.y.z` names it by hand.
#[version(initial = "1.4.0", scope = ["web-image"])]     // scope default: the unit's name
release web {
    step build(r: Release) {
        run!("make image VERSION={r.version}")?;
    }

    #[production]                      // needs --approve
    #[recover(reconcile)]              // when the outcome is unknown: reconcile, never repeat
    step deploy(r: Release) {
        run!("make deploy VERSION={r.version}")?;
    }

    step postcheck(r: Release) {
        run!("make smoke VERSION={r.version}")?;
    }

    // `r.version` is the release before the last passed one.
    #[production]
    rollback(r: Release) {
        run!("make deploy VERSION={r.version}")?;
    }
}

fn reconcile(r: Release) -> Result<()> {
    run!("make deploy-reconcile VERSION={r.version}")
}
```

`#[checks(none)]` turns off the gate that every check the plan selects is
proven for the release commit. `Release` holds `version` (being released, or
the rollback target), `previous` (`Option`: the release before), `commit`
and `unit`.

## Versions

A version belongs to one committed source and the names it publishes (its
scope). `citrus version reserve 1.4.0 --scope web-image` holds the first free
version at or after 1.4.0 and prints it; a retry from the same commit gets the
same version, another commit gets the next one. Other names may use the same
number. `citrus version check <version>` fails when the version belongs to
another worktree or commit: put it in front of anything that publishes.
`citrus version source <version>` prints the commit it was reserved for, and
`citrus version list --days 2` shows recent ones. A release's `version` step
reserves the same way.

Versions published outside Citrus are reported by the project's
`free_version` hook:

```rust
// Prints the first version at or after $CITRUS_VERSION that the registry
// does not have for any name in $CITRUS_SCOPE (comma-separated).
#![free_version(cmd!("scripts/registry.sh free-version"))]
```

It prints the version on its last line (a `NAME=` prefix is ignored).

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
  commit (`checks = proven`, or `--unchecked`, which is recorded); `--approve`
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
