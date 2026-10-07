# Protocols Citrus reads

Citrus does not replace your tools; it reads what they print.

## Planner (`plan.command`)

Prints, one per line (tab-separated):

```
PLAN	status=complete	files=12
MAPPED	src/api/user.rs	backend
UNMAPPED	tools/new-script.sh
TARGET	make:test-backend
TARGET	no-heavy:docs
```

Only `TARGET	make:<name>` lines select checks; other `TARGET` entries are shown as
notes. `PLAN`, `MAPPED` and `UNMAPPED` are optional and only improve the
explanation. A non-zero exit without any `TARGET` line is a planner failure.

## Runner progress (`run.remote`, and any local target)

```
CITRUS_TARGET target=<name> status=START
CITRUS_TARGET target=<name> status=PASS exit=0 seconds=41
CITRUS_TARGET target=<name> status=FAIL exit=2 seconds=12
```

Your runner can use its own prefix with the same fields; list it in
`run.progress_prefixes`. Output between a target's START and its result is
that target's log; Citrus takes the first error from it. If the runner exits
non-zero without failing any target, the run shows a `suite` row with the
first error of the whole output.

Optional markers, all configured by prefix:

| Key | Meaning | Example line |
|---|---|---|
| `waiting_prefix` | queued for a resource (first word after the prefix) | `QUEUED resource=builder` |
| `acquired_prefixes` | the resource was granted | `ACQUIRED resource=builder` |
| `stage_prefix` | a stage name shown in `status` | `STAGE [2/5] preparing runner` |
| `linked_log_markers` | the path of a fuller log, read after the run | `… full log: logs/run-42.log` |

## Resources (`status.resources_command`)

Lines starting with `resource_prefix` describe one resource as `key=value`
fields. `host`/`name`, `state`, `operation`, `owner` and `elapsed_seconds` are
shown; anything else is kept in JSON output.

```
BUILDER host=builder-1 state=busy operation=remote-test owner=agent-b elapsed_seconds=94
```
