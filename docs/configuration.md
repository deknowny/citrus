# `citrus.toml`

All project-specific choices live in `citrus.toml` at the repository root.
Every key is optional; unknown keys are an error, so typos are caught.
`citrus doctor` checks the result against the repository.

```toml
manifest = "ci/targets.toml"     # declared checks (docs/manifest.md)
toolchain_files = []             # files every declared fingerprint depends on
log_dir = ".citrus/logs"         # run logs; must be ignored by Git
target_definitions = ["Makefile", "*.mk", "make/*.mk"]  # where `citrus add` looks for the target; [] skips

[plan]
base = "origin/main"             # "changed" is measured from the fork point with this ref
command = []                     # external planner; [] = built-in (declared owners of changed paths)
base_arg = ""                    # argument for an explicit --base, e.g. "BASE_REF={base}"
paths_arg = ""                   # argument with a file of changed paths, e.g. "PATHS_FILE={file}";
                                 # lets `citrus integrate` carry passes the incoming changes do not affect

[run]
local = ["make", "{target}"]     # one target on this machine
remote = []                      # the whole planned set elsewhere; [] = no remote mode
progress_prefixes = []           # your markers besides CITRUS_TARGET (docs/protocol.md)
waiting_prefix = ""              # "queued for <resource>" marker
acquired_prefixes = []           # "resource granted" markers
stage_prefix = ""                # human-readable stage shown in status
linked_log_markers = []          # text before the path of a fuller log your runner keeps

[run.env.<target>]               # extra environment for one target
NAME = "value"

[receipts]
dir = "citrus/receipts"          # relative to the Git common directory
max_age_days = 7
snapshot_max_age_hours = 24      # reuse window for identical-snapshot passes

[status]
resources_command = []           # slow command describing shared builders/runners
resource_prefix = ""             # its lines that describe one resource each (key=value fields)
refresh_seconds = 0              # snapshot age before a background refresh (min 10)

[integrate]
after_merge = []                 # command after a successful merge; {before} = commit before it

[[catalog]]                      # repeatable: commands shown by `citrus` with no arguments
command = "make deploy"
description = "Roll out the verified release"
group = "release"                # optional heading

[state]
backend = "sqlite"               # one file in the Git common directory
path = "citrus"
```

## Choosing local or remote

`citrus run` without flags runs locally when it was given target names, when no
remote runner is configured, or when every check still needed is a declared
`cache = true` target (usually light). Otherwise it uses `run.remote`.
`--local` and `--remote` override this.

## Agents

The agent of a run is taken from `CODEX_THREAD_ID`, `CLAUDECODE` or
`CITRUS_AGENT`, and shown in `status` and `stats`.

## Integration

`citrus integrate` merges `plan.base` (fetching it first when it names a
remote branch). A check that passed on the sources before the merge stays
proven when the planner, given exactly the incoming paths, does not select it.
With the built-in planner that is ownership in the manifest; an external
planner needs `plan.paths_arg`. When any incoming path is claimed by no check,
nothing is carried over. `--push` fast-forwards the base after the remaining
checks pass, and integrates again if the base moved meanwhile.
