# Configuration

A project describes itself in `citrus.ci` at the repository root, or, when
it grows, in `.citrus/*.ci`: `.citrus/project.ci` for shared settings and one
file per product. The language is described in
[design/language.md](design/language.md); this page lists the declarations
Citrus reads and their fields. Unknown declarations, fields and names are
errors with a suggestion. `citrus check` validates the configuration;
`citrus doctor` checks it against the repository.

A comment directly above a declaration is its description: `citrus`,
`citrus targets` and `why` show it.

```
citrus 1

project {
  main = "origin/main"                 # "changed" is measured from the fork point with this ref
  runner = builders                    # with several runners: the one checks run on
  toolchain = ["rust-toolchain.toml"]  # files every check's fingerprint depends on
  signals = run("scripts/classify")    # prints SIGNAL and CLAIM lines (docs/design/planner.md)
  after_merge = run("scripts/after-merge", before)   # after `citrus integrate` merged
  logs = ".validation/logs"            # run logs in the tree (default: .git/citrus/logs)
  receipts = "citrus/receipts"         # PASS receipts, relative to the Git directory
  cache = false                        # reuse a pass only where a check or group says `cache = true`
}

# Fast checks read committed SQLx metadata.
profile fast {
  env {
    SQLX_OFFLINE = "true"
  }
}

profile e2e

# PostgreSQL for the e2e checks.
service database = compose.up("postgres") {
  ready = wait.tcp("localhost:5432", timeout: 60s)
}

# Headless browsers; at most two at a time.
service browser {
  limit = 2
}

# The shared builders.
runner builders = make("remote-check") {
  status = make("builders-status")
}

# The API.
group api {
  paths = crate("api") + ["migrations/**"]
  env {
    RUST_LOG = "warn"
  }
  check unit = cargo.test("api")
  check database = make("test-api-db") {
    profile = e2e
    needs = [database]
  }
}

check links = links.check("**/*.md") {
  paths = ["**/*.md"]
}

label scope-api {
  when = only(api)
}

# Start the database and apply migrations.
task seed-db = [compose.up("db"), wait.tcp("localhost:5432"), make("migrate")]

commands release {
  "make deploy" = "Roll out the verified release"
}

release api { … }                      # docs/releases.md
artifact api { … }                     # docs/design/declarative.md
environment production = kubernetes(context: "prod", namespace: "api") { … }
```

## Declarations

| Declaration | |
|---|---|
| `project { … }` | settings above |
| `profile name` | a set of checks run together (`--profile`); the first one is the default; `env { }` applies to its checks |
| `service name [= action] { ready, limit }` | something checks `need`: started by Citrus when it has an action, otherwise a resource the runner provides, `limit` at a time |
| `runner name = action { status }` | runs the planned checks elsewhere (docs/protocol.md) |
| `group name { paths, needs, cache, env, check … }` | a set of paths and the checks that protect it; its checks inherit `needs`, `cache` and `env` |
| `check name = action { … }` | a check; inside a group it is called `group.name` |
| `label name { when }` | a named condition reported with the plan |
| `task name = actions` | `citrus do name` |
| `commands [heading] { "command" = "what it does" }` | the project's own commands, listed by `citrus` |
| `release`, `artifact`, `environment` | releases and deployments |

## Checks

| Field | |
|---|---|
| `paths` | changed paths that select the check; inside a group they narrow the group's paths. A glob named here is this check's alone: groups elsewhere do not see it. A group named here (`paths = [platform, "x/**"]`) selects the check with its paths, which stay shared |
| `reads` | more inputs: they invalidate a pass but do not select the check |
| `profile` | the profile it belongs to (without one: every profile) |
| `needs = [service, …]` | services it needs |
| `env { NAME = "value" }` | environment of its steps, after the group's and the profile's |
| `covers = [check, …]` | it runs them itself: with it in the plan they are dropped |
| `replaces = [check, …]` | when the change goes beyond one of them, it runs instead of them |
| `when = condition` | selected only when this holds |
| `cache` | reuse a pass while its inputs are unchanged (default: the group's, else the project's, else `true`); a check with no paths or reads is never reused |
| `meta = { … }` | data for the project's own tools, passed through `CITRUS_CHECKS` |

The action is one step or a list of steps, or `match changed { … }`: the
first arm whose condition holds is what the check runs for this plan, `_`
otherwise.

Conditions: `touched(x)` (a changed path is in group or check `x`, or in a
list of globs), `only(x)` (every changed path is), `without(x)` (none is),
`selected(check)`, `signal("name")`, `profile(name)`, combined with `and`,
`or`, `not`. A list of globs serves conditions without claiming paths:
`let main = ["**", "!clyer/**"]` then `when = touched(main)`.

`paths - other` is `paths` with every glob of `other` excluded: a group can
leave the files a narrower check owns to that check
(`paths = ["scripts/**"] - tools`).

## Values Citrus fills in

`before` (the commit before a merge), and in releases and environments
`version`, `previous`, `release`, `artifact`, `key`, `tag`, `short`,
`commit`: values Citrus knows while running; inside strings they are written
`{before}`.

The runner gets `CITRUS_CHECKS`, the path of a JSON file with the declared
checks (docs/manifest.md), so it never parses the configuration.

## Choosing local or remote

`citrus run` without flags runs locally when it was given check names, when
there is no runner, or when every check still needed passed within a minute
in its last five passes here. A check that never passed is assumed heavy and
goes to the runner. `--local` and `--remote` override this.

## Agents

The agent of a run is taken from `CODEX_THREAD_ID`, `CLAUDECODE` or
`CITRUS_AGENT`, and shown in `status` and `stats`.

## Integration

`citrus integrate` merges `main` (fetching it first when it names a remote
branch). A check that passed on the sources before the merge stays proven
when the incoming paths do not select it. When any incoming path is claimed
by no group or check, nothing is carried over. `--push` fast-forwards the
base after the remaining checks pass, and integrates again if the base moved
meanwhile.
