# Protocols

Citrus runs your tools and reads what they print, line by line.

## Runner

A `runner` runs the planned checks elsewhere (shared builders, CI). It runs
with:

- `CITRUS_TARGETS`: a file naming the checks this run needs, one per line;
- `CITRUS_CHECKS`: their declarations, including what each one runs
  (docs/manifest.md);
- `CITRUS_BASE` and `CITRUS_PROFILE`: what the plan was made against.

It reports in these lines:

```
CITRUS_WAIT builder                                    queued for a resource
CITRUS_RUNNING                                         the resource was granted
CITRUS_STAGE [2/5] preparing runner                    a stage shown in `status`
CITRUS_TARGET target=api.unit status=START
CITRUS_TARGET target=api.unit status=PASS exit=0 seconds=41
CITRUS_TARGET target=api.unit status=FAIL exit=2 seconds=12
CITRUS_LOG logs/run-42.log                             a fuller log, read after the run
```

Output between a check's START and its result is that check's log; Citrus
takes the first error from it. A runner that reports checks one by one must
report each of them: a check it stays silent about is `not_run` and the run
fails, even when the runner exits 0. A runner that reports no checks at all
passes or fails them together with its exit code; if it exits non-zero
without failing any check, the run shows a `suite` row with the first error
of the whole output.

## Runner status

The runner's `status` command describes its machines, one per line:

```
CITRUS_RESOURCE host=builder-1 state=busy operation=remote-test owner=agent-b elapsed_seconds=94
```

`host`/`name`, `state`, `operation`, `owner` and `elapsed_seconds` are
shown; anything else is kept in JSON output. `citrus status` shows the last
snapshot at once and refreshes it in the background when it is older than a
minute.

## Signals

The project's `signals` command tells the planner what path globs cannot
(docs/design/planner.md). It runs with `CITRUS_PATHS` (a file of the changed
paths), `CITRUS_BASE`, `CITRUS_PATHS_EXPLICIT` (`1` when the paths were
given rather than diffed) and `CITRUS_PROFILE`, and prints:

```
SIGNAL product:api            a fact conditions can test: signal("product:api")
CLAIM scripts/old.sh pipeline a changed path that belongs to group `pipeline`
```

## Local checks

A check run on this machine prints the same `CITRUS_TARGET` lines into its
run log.
