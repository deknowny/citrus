# Citrus

**One command for checks — for people and for AI coding agents.**

Citrus tells you which checks your change needs, skips the ones already proven
for these exact inputs, runs the rest in a process that survives a closed
terminal, and answers with the first error instead of a 2,000-line log.

```
$ citrus status
feature-x @ 3f2a91c · 4 changed files · plan complete
  ≡ api.unit                    proven: declared inputs unchanged (r-20261007-114218-0354)
  ○ web.unit                    needed: inputs changed since the last pass
running now:
  r-20261007-114837-191e running remote · other-worktree · agent-b · 2m · waiting for builder
resources (12s ago):
  builder-1 · busy · remote-test · agent-b · 2m
  builder-2 · free
next: citrus run

$ citrus run
✗ r-20261007-121149-2005 failed · 41s
  ≡ api.unit                    proven: declared inputs unchanged (r-20261007-114218-0354)
  ✗ web.unit                    38s
      AssertionError: expected 3 rows, got 2
        at tests/report.test.ts:88
next: citrus log r-20261007-121149-2005 · citrus why web.unit · citrus run
```

## Why

When several agents (and people) work on one repository, CI turns into a pile
of wrapper scripts, repeated runs and status questions. In the repository
where Citrus was born, with 5–10 coding agents working in parallel, one week
before Citrus looked like this:

- 147 new script and Make files — agents wrote a new wrapper for almost every check;
- 535 checks re-run on byte-identical sources;
- ~30% of all tool output agents read was CI logs (median failed log: 1,775 lines);
- 538 messages between agents, mostly “is the builder free?” and “did you check it?”.

Citrus removes the reasons for that work instead of adding another layer:
it reuses the existing commands (Make, npm, your remote runner), and makes
their results shared, reusable and short.

## What it does

| Command | |
|---|---|
| `citrus status` | What the current changes need, what is already proven, what runs right now (all worktrees), shared builders |
| `citrus run [targets…]` | Run only what is not proven; `--remote` uses your runner; `--detach` returns at once; the same sources join a run already in progress |
| `citrus wait <run>` / `show` | Follow a run after a lost terminal or a new agent session; a vanished process becomes `unknown`, not “running forever” |
| `citrus log <run>` | First error of each failed target; `--target`, `--full` when needed |
| `citrus why <target>` | Why it is needed, and which inputs changed since its last pass |
| `citrus integrate [--push]` | Merge the base branch, keep checks the incoming changes do not touch, re-check the rest, and fast-forward the base when green |
| `citrus tasks` / `citrus note <text>` | Every worktree as a task — branch, unmerged commits, runs — and what its owner wants others to know |
| `citrus diff <env>` / `citrus apply <env> --approve` | What an environment runs versus what HEAD builds; build what is missing by input key and roll it out by digest ([docs/design/declarative.md](docs/design/declarative.md)) |
| `citrus release start <unit> --approve` | Release a unit from HEAD: version → build → deploy → postcheck with gates, an environment lock, recovery of interrupted steps and rollback ([docs/releases.md](docs/releases.md)) |
| `citrus` | What can be done here: Citrus commands plus the project's own `commands` |
| `citrus targets` | Declared checks and when they last passed |
| `citrus stats` | Runs, reuse rate, time not spent thanks to reuse |
| `citrus doctor` | Is this repository set up so the answers can be trusted |
| `citrus cancel <run>` | Stop a run and everything it started |

Output is short text in a terminal and JSON (`citrus/v1`) otherwise, so agents
get structured results with a `next` list of valid follow-up commands.

## How reuse stays honest

- A check is reused while every file it owns or reads, what it runs and the
  toolchain files are unchanged (7 days). A PASS is recorded only if the
  inputs did not change while the check ran.
- A check with `cache = false` (it depends on the outside world) is reused
  only for the byte-identical source tree (Git tree of all non-ignored
  files), for 24 hours.
- `--force` re-runs anyway.

See [docs/manifest.md](docs/manifest.md) for the exact format.

## Quickstart

```sh
cargo install --git https://github.com/deknowny/citrus --locked   # or a release binary
cd your-repo
cat > citrus.ci <<'CI'
citrus 1

# Unit tests.
check test = make("test") {
  paths = ["src/**", "tests/**"]
}

# Start the database and wait for it.
task dev-db = [compose.up("db"), wait.tcp("localhost:5432", timeout: 60s)]
CI
citrus check     # validates the configuration, points at the line if something is wrong
citrus status
citrus run
citrus do dev-db
```

`citrus.ci` is the whole configuration, in a small language where every
element does something: declarations (`check test`, `group api`,
`profile e2e`), references to them by bare name, data in quotes, and a
comment above a declaration as its description. Paths are derived from the
workspace — `crate("api")` is a Rust crate and every crate it depends on,
`next("@acme/shop")` a Next.js app and its packages — and steps are built
into Citrus so they behave the same on macOS, Linux and Windows. A larger
project splits it into `.citrus/*.ci`, one file per product — see
[docs/design/language.md](docs/design/language.md). This repository's own
[`citrus.ci`](citrus.ci) is a short real example; every declaration is
listed in [docs/configuration.md](docs/configuration.md), and
[examples/](examples/) shows a Make project, a pnpm workspace and a monorepo
with remote builders.

## Pinning Citrus in a repository

Pin an exact commit and build it once per machine — no release needed:

```sh
cargo install --git https://github.com/deknowny/citrus --rev <full-commit-sha> --locked --root .git/citrus/bin
```

or download a release binary and check it against the release's `SHA256SUMS`.
A small launcher script in your repository can do either and cache the result.

## For AI agents

Give your agents [skills/citrus/SKILL.md](skills/citrus/SKILL.md) (Claude Code,
Codex and similar) or the short [AGENTS.md snippet](skills/AGENTS-snippet.md).
The rules they need are five lines: read `status` before checking, `run` instead
of custom scripts, `log` instead of raw logs, `wait` instead of re-running to
learn a status, a declared check instead of a new wrapper.

## State

One SQLite file, receipts and run logs in the Git common directory
(`.git/citrus/`), shared by every worktree of a clone; nothing in the work
tree, nothing to host. Runs execute in their own session (`setsid`), so they
outlive the terminal or agent that started them. A shared server backend for
several machines and people is planned.

## Where it is going

Releases are moving from ordered steps to desired state: artifacts keyed by
their inputs, environments with providers, `citrus diff` and `citrus apply`
that observe what runs and reconcile it — see
[docs/design/declarative.md](docs/design/declarative.md).

## Status

Early, used daily in a multi-agent monorepo. The CLI contract (`citrus/v1`)
and the receipt format are kept stable; see [CHANGELOG.md](CHANGELOG.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
