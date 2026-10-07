# The `.ci` language

Status: `citrus 1`, first part implemented — values, `let`, functions, `for`/`if`, comprehensions, interpolation, durations, `use`; `project`, `check`, `task`; `citrus check`, `citrus do`. Artifacts, environments, pools and events follow. Issue #11.

A `.ci` file describes a project's CI/CD — checks, artifacts, environments,
tasks — in a form a person reads at a glance and an agent writes correctly
on the first try. Citrus evaluates the files into a graph and executes the
graph. Every executed step knows the line it came from, so the CLI, the
dashboard and agents all see *where in the recipe* things are.

## Why a language, and why our own

Citrus first grew a set of TOML files. Each new case added a field or a
template inside a string: `inputs_command`, `after_merge = [... "{before}"]`,
`resolve = [... "{release}"]`, Job manifests with `{image}`. Logic leaked into
strings and into external shell and Python scripts, which only run on some
machines and which nobody reads. The model behind it (a graph of checks,
artifacts, environments; keys by inputs; plans; recorded steps) was right;
static data was the wrong way to describe it.

The language is ours because the runtime is ours: evaluation, execution,
events, the dashboard and agent output are designed together, and the
language can be exactly as small as the job requires.

## Principles

1. **Describe, don't execute.** Evaluating `.ci` files has no side effects.
   It produces a graph; Citrus executes the graph. Nothing runs while a file
   is being read.
2. **Readable first.** A file reads like a description of the pipeline.
   Blocks name things; expressions compute values.
3. **Portable by construction.** Actions are built into Citrus (`run`,
   `docker`, `kubectl`, `wait`, `copy`, `http` …) and behave the same on
   macOS, Linux and Windows. `sh(...)` exists and is visibly non-portable.
4. **Deterministic.** The same files and the same repository give the same
   graph. Reading files (`glob`, `read`) is allowed only through builtins that
   record what was read; those files become inputs.
5. **Small.** No classes, exceptions, mutation of shared state, network or
   clock at evaluation time. Every feature must pay for itself in real files.
6. **Every step has a source.** Spans (file, line, column), the declaring
   block and the loop instance travel with each node into execution events.

## Files

- `citrus.ci` at the repository root; it may `use` other files
  (`use "ci/mt3s.ci"`). Paths are repository-relative.
- The first line declares the language version: `citrus 1`. A file without it
  is rejected; a newer major version is rejected with a clear message.
- `#` starts a comment.

## Values and expressions

```
let name    = "api"                       # string; "{expr}" interpolates, "{{" is a literal brace
let count   = 3                           # integer
let enabled = true                        # bool
let none_   = none                        # absence
let items   = ["a", "b"]                  # list
let opts    = { file: "Dockerfile", target: name }   # map with identifier or string keys

let doubled = [x * 2 for x in [1, 2, 3] if x > 1]     # comprehension
let label   = if enabled { "on" } else { "off" }      # if is an expression
let joined  = items.join(", ")                       # methods on built-in types
let line    = """
  multi-line string, common indentation removed
"""
```

Operators: `+ - * / %`, `== != < <= > >=`, `and or not`, `in`, `??` (default
for `none`), field access `opts.file`, indexing `items[0]`.

## Functions

```
fn image(product, suffix = "") {
  "registry.example.com/{product}{suffix}"
}
```

Functions are pure; the last expression is the result. Named arguments at the
call site: `docker(file: "deploy/Dockerfile", target: name)`.

## Declarations

Declarations are blocks: `kind "name" { field = expr … nested blocks }`.
Repeating a declaration in a `for` creates one node per iteration; the
dashboard shows each instance separately with the same source line.

### `project`

```
project {
  base    = "origin/main"          # what "changed" is measured against
  logs    = ".citrus/logs"         # must be ignored by Git
  toolchain = ["rust-toolchain.toml", ".python-version"]   # inputs of every cached check
}
```

### `check`

```
check "test-api" {
  owns    = ["crates/api/**"]       # changing these selects the check
  reads   = ["Cargo.lock"]          # these invalidate reuse but do not select it
  run     = cargo.test("api")       # an action
  cache   = true                    # reuse a PASS while owns+reads are unchanged
  env     = { SQLX_OFFLINE: "true" }
  on      = local                   # local | remote(pool) | linux
}
```

### `artifact`

```
artifact "api" {
  inputs = rust_closure("api") + ["Dockerfile"]  # key = content of inputs + this declaration
  build  = docker(file: "Dockerfile", target: "api")
  push   = "registry.example.com/shop/api"
}
```

### `environment` and `deploy`

```
environment "shop-production" {
  on       = kubernetes(context: "prod", namespace: "shop")
  approval = required
  checks   = proven
  migrate  = job("deploy/migrate.yaml", artifact: "api-migrations", timeout: 5m)

  deploy "api" { artifact = "api", strategy = rolling }
  deploy "bot" { artifact = "bot", strategy = recreate, fence = lease("bot-session") }
  deploy "backup" { artifact = "backup", kind = cronjob, quiesce = true }

  record   = annotation("example.com/release", name: "{short}")
  verify   = [http("https://shop.example.com/health"), check("smoke-shop")]
}
```

### `task`

Named procedures people and agents run with `citrus do <task>`, replacing
one-off scripts.

```
task "seed-dev-db" {
  about = "Start the dev database and apply migrations"
  steps = [
    compose.up("db"),
    wait.tcp("localhost:5432", timeout: 60s),
    make("migrate-dev"),
  ]
}
```

### `pool` and `remote`

```
pool "builders" {
  run    = make("check-remote-suite")          # the project's own remote runner, for now
  report = progress(prefix: "CI_PARALLEL_LANE", log: after("full log: "))
}
```

### `use`, `planner`, `hook`

```
use "ci/mt3s.ci"
planner = external(make("ci-plan"), base: arg("BASE_REF={base}"), paths: arg("GARVIS_CHANGED_PATHS_FILE={file}"))
hook after_merge = run("python3", "-B", "scripts/task-flow.py", "retire-gitlinks", "--since", before)
```

## Standard library

**Actions** (values describing work; Citrus executes them, each carries its span):

| Action | Meaning |
|---|---|
| `run(program, args…)` | run a program without a shell |
| `make(target, vars…)` | `make <target>` |
| `cargo.test(pkg)`, `cargo.build(…)` | Cargo commands |
| `sh("…")` | a shell command — explicit, flagged non-portable by `citrus check` |
| `docker(file:, target:, platform:)` | build and push, result is a digest |
| `kubectl(…)`, `kubernetes(context:, namespace:)` | cluster access |
| `compose.up(service)`, `compose.down()` | Docker Compose |
| `job(template, artifact:, timeout:)` | run a Kubernetes Job from a template |
| `wait.tcp(addr)`, `wait.http(url)`, `wait.file(path)` | readiness |
| `http(url)` | a verification request |
| `copy(from, to)`, `archive(dir, to)` | files |
| `check(name)` | run a declared check as a step |
| `lease(name)`, `annotation(key, name:)` | fences and records |

**Inputs:** `glob(pattern)`, `read(path)`, `rust_closure(package)` (a Cargo
dependency closure), `inputs_of(command)` (a command printing paths; an
escape hatch, flagged by `check`).

**Values:** `secret(name)` — resolved only during execution from the
environment or a configured store; never printed, logged, shown in the
dashboard or returned to agents. `env(name)` — a non-secret variable.

**Durations:** `30s`, `5m`, `2h` are literals.

## Evaluation and execution

1. Citrus parses `citrus.ci` and its `use`d files, evaluates them and builds
   the graph: nodes (checks, artifacts, environments, deploys, tasks, steps)
   with their spans, inputs and dependencies.
2. `citrus check` validates the graph before anything runs: unknown names,
   dependency cycles, globs matching nothing, cached checks whose declared
   inputs miss files they read (when detectable), non-portable actions.
3. Commands (`status`, `run`, `diff`, `apply`, `do`) select a part of the
   graph and execute it. The runtime records every transition as an event.
4. `citrus fmt` prints the canonical layout, so files written by different
   agents look the same.

## Execution events

One stream, one schema, read by the CLI, the dashboard, agents (`--json`) and history:

```json
{"seq": 812, "time": "2026-10-07T18:04:11Z", "run": "apply-shop-production-…",
 "node": "deploy shop-production/bot", "instance": null,
 "source": {"file": "citrus.ci", "line": 14, "column": 3},
 "state": "running", "phase": "fence", "detail": "waiting for lease bot-session",
 "inputs_key": "4bebc33…", "log": {"path": "…", "offset": 18234}}
```

- `node` is stable across runs; `instance` names the loop iteration
  (`{"name": "alerts"}`) when the node came from a `for`.
- States: `pending`, `waiting`, `running`, `passed`, `reused`, `failed`,
  `unknown`, `cancelled`, `skipped`.
- Secrets never appear in events; values derived from `secret(...)` are
  redacted at the source.

## How it looks while running

CLI:

```
apply shop-production                         ● running · 2m10s
  build api            ≡ reused   key 4bebc33           citrus.ci:21
  migrate              ✓ 41s      job shop-migrate-…    citrus.ci:30
  deploy bot           ● fence    waiting lease bot-session   citrus.ci:34
  verify               ○
```

Dashboard: the same graph, with the recipe beside it and the running line
highlighted; loop instances as separate rows; each node opens its log, its
inputs and why it was selected or reused.

## What it replaces — measured

| Today | Lines | In `.ci` (sketch below, to be measured on the implementation) |
|---|---|---|
| Citrus's own setup: Makefile, citrus.toml, ci/targets.toml, two scripts | 96 | ~30 |
| Garvis Citrus config: citrus.toml + 4 TOML files | 284 | ~110 |
| 15 MT3S component release scripts | 4,761 | one loop over components + shared provider code |

Citrus's own CI as a sketch:

```
citrus 1

project { base = "origin/main", toolchain = ["Cargo.lock"] }

check "test" { owns = ["src/**", "tests/**", "examples/**"], reads = ["Cargo.*"], run = cargo.test(), cache = true }
check "lint" { owns = ["src/**", "tests/**"], reads = ["Cargo.*"], run = [cargo.fmt(check: true), cargo.clippy(deny: "warnings")], cache = true }
check "docs" { owns = ["*.md", "docs/**", "skills/**"], run = links.check("**/*.md"), cache = true }

for target in ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"] {
  artifact "citrus-{target}" {
    inputs = ["src/**", "Cargo.*"]
    build  = cargo.build(release: true, target: target)
  }
}

environment "github" {
  on     = github.release(repository: "deknowny/citrus", tag: "v{version}")
  checks = proven
  verify = [checksums()]
}
```

## Non-goals

- General-purpose programming. If a recipe needs more, it is a provider or
  an action written in Rust inside Citrus, with tests.
- Running commands during evaluation.
- Templating other formats with string concatenation (Kubernetes manifests
  stay manifests; `job()` and `kubernetes()` substitute declared fields).
- Backwards compatibility with TOML forever: TOML stays readable while
  consumers move, then goes.

## Plan

1. This document, reviewed against real files (Citrus, Garvis config,
   MT3S releases, a few shell scripts).
2. Lexer, parser with spans, evaluator into the existing graph types;
   `citrus check`, `citrus fmt`; errors with source excerpts.
3. Citrus describes itself in `citrus.ci`; TOML remains a supported input.
4. Execution events with spans; the CLI live view; `citrus do <task>`.
5. Garvis moves product by product, deleting scripts as it goes.
6. Dashboard on events; syntax highlighting; LSP.
