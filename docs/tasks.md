# Tracked tasks

A tracked task follows one piece of agent work from the card that asked for it
to a verified release, so a lead does not have to remember what is merged and
not released, or who went silent. It lives in the repository's state beside
the queue and releases (`work_tasks`, `work_links`, `work_events`).

```sh
citrus task new "Weekly left counter" --id weekly-left --card Q-436 --unit garvis --about "…"
citrus task claim t-weekly-left --as lead2 --lease 2h   # from the worktree doing it
citrus task report t-weekly-left "migration ready, handler next"
citrus queue add --task t-weekly-left --release garvis   # Citrus takes it from here
citrus task show t-weekly-left
citrus task verify t-weekly-left "hub shows the counter"  # when no verify step proved it
citrus task close t-weekly-left
citrus tasks                     # tracked tasks first, then worktrees
citrus tasks --turn lead2        # whose turn it is
citrus tasks --stuck             # what nobody moves (JSON for a watcher)
```

A task is named `t-<slug>` (`--id`, else `t-<utc>-<random>`); commands take the
id, the id without `t-`, a unique prefix, or its card (`Q-436`). `task new`
again with the same id and title returns the same task.

## Step and turn

| Step | Set by |
|---|---|
| `open` | `task new` |
| `claimed` | `task claim` |
| `in-queue` | `queue add --task` (or `task link --queue`) |
| `merged` | its queue entry landed |
| `released` | every unit it must reach has a passed release carrying its merges |
| `verified` | those releases ran a `verify` or `postcheck` step (an `apply` ends with `verify`), or `task verify` |
| `closed` | `task close` |

Citrus moves a task forward on what it knows (never back) whenever it reads
it: queue entries linked to it, the releases they started, and, for a unit
released later by hand, the first passed release of the unit whose commit
carries the task's merges. Each move is an event in `task show`.

The turn is always one of `owner`, `lead`, `lead2`, `agent`, `citrus`.
`task claim --as` makes it the claimant's (`lead`, `lead2`, else `agent`);
`task report` returns it to the lead (`--turn` to someone else, `--done`
releases the claim); the queue and releases make it `citrus`; once merged
without a release in flight, released or verified it is the lead's again.

The units a task must reach are those named by `--unit` and those its queue
entries release (`queue add --release`).

## Stuck

`citrus tasks --stuck` names, for open tasks:

| Kind | When | To |
|---|---|---|
| `silent` | claimed, the turn is the claimant's, and no event, commit on its branches or run in its worktrees within the lease (default 2 h) | lead |
| `unreleased` | merged more than `--unreleased-after` (15 m) ago, a unit not released, no release running | the turn's holder, else the lead |
| `release_failed` | a linked release ended without passing and none passed since | the turn's holder, else the lead |
| `unverified` | released more than `--unreleased-after` ago and not verified | the turn's holder, else the lead |
| `idle` | no event for `--idle-after` (24 h) | lead |

It only reports; delivering the lines (a watcher every few minutes) is the
project's.
