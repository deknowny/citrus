# Changelog

## Unreleased

Breaking: `citrus.ci` is the only configuration. Citrus no longer reads
`citrus.toml`, `ci/targets.toml`, `ci/releases.toml`, `ci/artifacts.toml` or
`ci/environments.toml`; `doctor` warns when a `citrus.toml` is left over.

- The `.ci` language (docs/design/language.md): `citrus 1`, `let`, functions,
  `for`/`if`, comprehensions, string interpolation, durations, `use`.
- Blocks: `project`, `check`, `task`, `release` (with `step` blocks),
  `artifact`, `environment` (with `deploy` blocks), `planner`, `pool`,
  `command` (docs/configuration.md). Values Citrus fills in while running —
  `before`, `version`, `next`, `previous`, `artifact`, `key`, … — are names,
  written `{version}` inside strings.
- Steps `run`, `make`, `sh` (flagged non-portable), `cargo.*`, `compose.*`,
  and built-in `wait.tcp|http|file` and `copy` that need no shell.
- `citrus check` validates `citrus.ci` before anything runs; errors show the
  file, line, an excerpt with a caret and a "did you mean" hint.
- `citrus do [task]` runs a task's steps with their source lines.
- Checks take `meta = { … }`: data for the project's own tools.
- The planner and the pool get `CITRUS_CHECKS`, a JSON file with the
  declared checks; `citrus targets --json` prints the same (docs/manifest.md).
- The input fingerprint hashes a check's `declaration` (globs, steps,
  environment) as documented JSON, so other tools can compute it; receipts
  of earlier versions no longer match.
- Editing a check in `citrus.ci` selects it on the next plan — also when a
  project planner makes the plan; other edits of `citrus.ci` map to `config`.
- `citrus run` refuses a plan its planner reports as `incomplete` with
  unclaimed paths, before anything runs, and names the paths.
- Artifacts take `dockerfile = { file:, target: }`: a shared multi-stage
  Dockerfile counts in the key only with the stages the target is built from
  (`FROM <stage>`, `COPY --from=`, `--mount=…,from=`), so another product's
  stage no longer asks for a rebuild.
- `cargo.closure("package")`: the files a Cargo package is built from
  (path-dependency closure, included files, manifests and workspace
  settings), so Rust checks can be cached without listing crates by hand.
- A declared cached check that a pool reported passing now counts locally
  too (evidence and receipt by inputs), so a remote PASS is reused after
  unrelated edits — before, only local passes were, and most runs are remote.
- Plan-time choice: `group` blocks (named path sets, optional `note`),
  `when` on checks with `touched`, `selected`, `signal`, `and/or/not`, a
  project `signals` command (`SIGNAL x`, `CLAIM <path> <group>`);
  `CITRUS_PLANNER=builtin` for comparing planners; `claims = false` and
  repeated declarations for groups; `label` blocks; plans list touched
  groups and signals, keep
  declaration order, and `citrus plan --paths-file` plans an explicit list.
- Profiles: `project { profiles = [...] }`, `check { profiles = [...] }`,
  `--profile` (also `CITRUS_PROFILE`), passed to the planner (`profile_var`)
  and the pool. `covered_by` drops a check whose covering check is planned.
- `!glob` in `owns`/`reads` excludes paths; the last matching glob decides
  (docs/design/planner.md).
- Submodule paths (gitlinks) are valid globs for `owns`/`reads` in `check`,
  `doctor` and `add`.
- `citrus fmt [--check]`: the canonical `.ci` layout — one indent level per
  line however many brackets it opens, two-space nesting,
  one space around `=` and after `,`/`:`, no aligned columns; comments,
  line breaks and strings stay as written.
- A pool gets `CITRUS_TARGETS` (the checks to run). When it reports checks
  one by one, a planned check it is silent about is `not_run` and the run
  fails — before, it was "passed with the suite" (found in Garvis: the
  suite planned on its own and skipped three checks).
- `citrus run` picks local or pool by history: local only when every needed
  check passed within a minute recently (declared caching no longer implies
  "light").
- Release steps run built-in actions too (`wait.http`, `copy`,
  `links.check`, …), not only commands; `release start --version X` releases
  a version given by hand (no `version` step).
- `links.check(glob)`: relative links in Markdown point at existing files.
- `integrate --push` retries a push the remote failed (GitHub 5xx) and no
  longer reports it as a moved base.
- Citrus releases itself through `release "github"` in its own `citrus.ci`;
  its Makefile and scripts are gone.
- `citrus add` appends a `check` block to `citrus.ci` (and creates the file).
- `apply` records a release on a workload that already runs the built image
  without touching its pod template, so nothing restarts for an identical build.

## 0.3.0 — 2026-10-07

- `citrus apply <env> --approve [--plan HASH]`: builds artifacts whose input
  key was never built (build providers `docker`, `command`; key → image cache),
  suspends quiesced CronJobs, runs the migration Job, rolls workloads by digest
  with `citrus.dev/commit` / `citrus.dev/key` records, waits for a Lease
  takeover (`fence`), resumes CronJobs, verifies images, readiness, HTTP and
  commands. Steps are recorded like releases (`release show/log/resume/abandon`);
  a failure never leaves CronJobs suspended.
- A worker that exits without a result no longer leaves `wait` hanging (its
  zombie process is reaped before the liveness check).

- Declarative releases, read side: `ci/artifacts.toml` (inputs as globs or an
  `inputs_command`; keys from Git objects at any commit), `ci/environments.toml`
  (provider `kubernetes`, workloads, release record by annotation / tag / resolve
  hook), `citrus artifacts [--at REV]` and `citrus diff <env>` — what runs versus
  what HEAD would build, per workload, with the inputs that changed.

- Log, state and receipt directories are created owner-only (0700); a group-readable state dir broke a consumer's builder pool in fresh worktrees.
- `run` refuses a "nothing to check" from an external planner that saw no changed files while HEAD has commits the base lacks (it compared against another base) instead of passing.

- `citrus --version` and `citrus doctor` show the commit the binary was built from (`CITRUS_BUILD_COMMIT`, or Git at build time; `-dirty` for local changes).

- `[integrate] after_merge` hook (`{before}`); `--push` never forces, follows tags or recurses into submodules.

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
