# `citrus.ci`

A project describes itself in `citrus.ci` at the repository root (it may
`use` other `.ci` files). The language is described in
[design/language.md](design/language.md); this page lists the blocks Citrus
reads and their fields. Unknown blocks and fields are errors with a
suggestion, so typos are caught. `citrus check` validates the file;
`citrus doctor` checks it against the repository.

Without `citrus.ci` Citrus still works: nothing is declared, and a target
passed by name runs as `make <target>` and is reused only for an identical
source snapshot.

```
citrus 1

project {
  base = "origin/main"        # "changed" is measured from the fork point with this ref
  logs = ".citrus/logs"       # run logs; must be ignored by Git
  toolchain = ["rust-toolchain.toml"]   # files every declared fingerprint depends on
  receipts = "citrus/receipts"          # PASS receipts, relative to the Git common directory
  check_env = { "test-api": { SQLX_OFFLINE: "true" } }   # extra environment per check
  after_merge = run("scripts/after-merge", before)       # after `citrus integrate` merged
}

check "test-api" { … }        # docs/manifest.md
task "seed-db" { … }          # `citrus do seed-db`
release "api" { … }           # docs/releases.md
artifact "api" { … }          # docs/design/declarative.md
environment "production" { … }

planner {                     # the project's own planner (optional)
  run = make("ci-plan")       # prints TARGET\tmake:<name> lines (docs/protocol.md)
  base_var = "BASE_REF"       # passed as BASE_REF=<base> for an explicit --base
  paths_var = "PATHS_FILE"    # passed as PATHS_FILE=<file of changed paths>
}

pool "builders" {             # runs the whole planned set elsewhere (optional)
  run = make("remote-check")
  progress = ["LANE"]         # markers besides CITRUS_TARGET (docs/protocol.md)
  waiting = "QUEUED resource="
  acquired = ["ACQUIRED resource="]
  stage = "STAGE "            # human-readable stage shown in status
  log_after = ["full log: "]  # text before the path of a fuller log the runner keeps
  status = make("builders-status")   # slow command describing the pool
  status_prefix = "BUILDER "  # its lines describing one resource each (key=value fields)
  refresh = 1m                # snapshot age before a background refresh
}

command "make deploy" { about = "Roll out the verified release", group = "release" }
```

`before` (and `version`, `next`, `previous`, `release`, `artifact`, `key`,
`tag`, `short`, `commit` in releases and environments) are values Citrus
fills in while running; inside strings they are written `{before}`.

The planner and the pool get `CITRUS_CHECKS`: the path of a JSON file with
the declared checks (docs/manifest.md), so they never parse `citrus.ci`.

## Choosing local or remote

`citrus run` without flags runs locally when it was given target names, when
no pool is declared, or when every check still needed is a declared
`cache = true` check (usually light). Otherwise it uses the pool. `--local`
and `--remote` override this.

## Agents

The agent of a run is taken from `CODEX_THREAD_ID`, `CLAUDECODE` or
`CITRUS_AGENT`, and shown in `status` and `stats`.

## Integration

`citrus integrate` merges `base` (fetching it first when it names a remote
branch). A check that passed on the sources before the merge stays proven
when the planner, given exactly the incoming paths, does not select it. With
the built-in planner that is ownership in `citrus.ci`; an external planner
needs `paths_var`. When any incoming path is claimed by no check, nothing is
carried over. `--push` fast-forwards the base after the remaining checks
pass, and integrates again if the base moved meanwhile.
