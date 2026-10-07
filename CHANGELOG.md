# Changelog

## Unreleased

- Releases (`ci/releases.toml`, `citrus release …`): ordered steps with a
  version reservation, gates (committed source, proven checks, `--approve` for
  production), one release per environment, `unknown` after an interrupted
  step with `recover` on resume, rollback, history, `--dry-run`.

- `citrus integrate [--push]`: merge the base, carry passes of checks the
  incoming changes do not select (`plan.paths_arg` for external planners),
  re-check the rest, fast-forward the base when green.
- `citrus tasks` and `citrus note`: every worktree with branch, unmerged
  commits, last run and its owner's note; notes appear in `status`.
- `citrus` without arguments: overview and the project's `[[catalog]]`.
- `citrus targets`: declared checks with their last pass.
- Progress notes are not repeated while a run waits.

## 0.2.1 — 2026-10-07

- Failure excerpts: a suite's failure is explained by the innermost failed
  target inside it, and by what the failing command printed just before Make
  reported it — not by "error:" lines in the expected output of passing tests.

## 0.2.0 — 2026-10-07

First public release.

- `status`, `plan`, `run` (`--local`, `--remote`, `--detach`, `--key`, `--force`),
  `wait`, `show`, `log`, `why`, `cancel`, `add`, `stats`, `doctor`.
- Reuse of declared checks by input fingerprint and of any check by identical
  source snapshot; receipts in a documented format.
- Detached runs that survive the terminal; a new `run` on the same sources
  joins the one in progress; vanished workers become `unknown`.
- Runner protocol with configurable progress markers and linked logs;
  background snapshot of shared builders in `status`.
- `citrus.toml` for every project-specific choice; SQLite state shared by the
  worktrees of a clone.
