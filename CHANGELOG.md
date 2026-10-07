# Changelog

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
