---
name: citrus
description: Check changes with Citrus — what needs checking, what is already proven, running with reuse, following a run after a lost session, and the first error instead of a full log. Use before running any check, when a check fails, and instead of asking other agents about status.
---

# Citrus

`citrus` is the one command for checks in this repository. It picks the
checks a change needs, skips the ones proven for the same inputs, runs the
rest in a detached process and answers briefly. Output is JSON when stdout is
not a terminal (`--text` for short text); every answer lists valid follow-up
commands in `next`.

1. **Before checking** — `citrus status`: needed vs proven (`reused`) checks,
   runs in progress in every worktree, who holds the builders. This answers
   "is the builder free?" and "did you check it?" without messages. A `!`
   warning means the sources changed under your own running run.
2. **Check** — `citrus run`. Proven results are reused; light declared checks
   run locally, the rest through the configured remote runner. Specific
   checks: `citrus run <target>...`. Use `--force` only to re-check
   something that depends on the outside world.
3. **Lost session or long run** — the run keeps going: `citrus wait last` or
   `citrus show <run>`. Do not start it again to learn its status. `unknown`
   means the process vanished without a result: read `citrus log <run> --full`
   before re-running.
4. **Failure** — `citrus log last`: the first error of each failed check.
   `--target <name>` for one check, `--full` only if the first error is not enough.
5. **Unclear why a check is needed** — `citrus why <target>`.
6. **New check or task** — declare it in `citrus.ci` (or the product's
   `.citrus/*.ci`): `check name = make("target") { paths = [...] }`, inside
   the product's `group` when it protects the group's paths; `task name =
   [...]`. Steps are actions such as `make(...)`, `cargo.test(...)`,
   `pnpm.test(...)`, `wait.tcp(...)`, `copy(...)` — no shell scripts. A
   comment above it is its description. Then `citrus fmt` and
   `citrus check`; `citrus do <task>` runs a task.
7. **Bring in the base branch** — `citrus integrate` (merge, keep what is still
   proven, re-check the rest); `citrus integrate --push` to publish when green.
8. **Other tasks** — `citrus tasks` shows every worktree and the agreements
   between tasks. `citrus task "<title>" --scope …` says what you are doing;
   `--blocked "<action>" --needs "<decision or data, from whom>"` what you wait
   for (`--clear-blocker` when it is resolved). `citrus agree <key> --terms …
   --reopen … --evidence …` records who does what (`--revision N` to change
   one you read). Instead of messages.
9. **Release** — `citrus release` lists units; `citrus release start <unit>
   --dry-run` shows the exact commands; `citrus release start <unit> --approve`
   runs them. After a failure or a lost session: `citrus release resume <id>
   --approve` (never start a second release over an `unknown` one).
10. **Is it helping** — `citrus stats`. `citrus` alone lists what can be done here.

Exit codes: 0 passed or still running (`--detach`), 1 a check failed,
2 usage or environment error, 3 cancelled or unknown result.

Do not edit files of a worktree while its run is in progress — the result
would describe the earlier sources. If Citrus gets in the way, use the
project's previous commands and say why in one sentence in your final answer,
so the tool can be fixed.
