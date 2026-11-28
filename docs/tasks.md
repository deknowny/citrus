# Tracked work

Tracked work follows one piece of agent work from the card that asked for it
to a verified release, so a lead does not have to remember what is merged and
not released, or who went silent. It lives in the repository's state beside
the queue and releases (`work_tasks`, `work_links`, `work_events`). It is
separate from `citrus task <title>` / `citrus tasks`, which describe worktrees.

```sh
citrus work new "Weekly left counter" --id weekly-left --card Q-436 --unit garvis --about "…"
citrus work claim t-weekly-left --as lead2 --lease 2h   # from the worktree doing it
citrus work report t-weekly-left "migration ready, handler next"
citrus queue add --task t-weekly-left --release garvis  # Citrus takes it from here
citrus work show t-weekly-left
citrus work verify t-weekly-left "hub shows the counter" # when no verify step proved it
citrus work close t-weekly-left
citrus works                     # as stored; --all with closed ones
citrus works --turn lead2        # whose turn it is
citrus works --stuck --exit-code # what nobody moves (JSON for a watcher; 1 when any)
```

A task is named `t-<slug>` (`--id`, else `t-<utc>-<random>`); commands take the
id, the id without `t-`, its card (`Q-436`) or a unique prefix. `work new`
again with the same id and title returns the same task. Every command changes
the task under a row lock; a closed task refuses claims, reports, links,
verification and queue entries.

## Step and turn

| Step | Set by |
|---|---|
| `open` | `work new` |
| `claimed` | `work claim` |
| `in-queue` | `queue add --task` (or `work link --queue`) |
| `merged` | its queue entry landed |
| `released` | every unit it must reach has a passed release carrying its merges |
| `verified` | those releases ran a `verify` or `postcheck` step (an `apply` ends with `verify`), or `work verify` (on a released task; `--force` before) |
| `closed` | `work close` |

`work show` and `works --stuck` bring a task up to date: Citrus moves it
forward (never back) on its queue entries, the releases they started and,
for a unit released later by hand, the first passed (or running) release of
the unit whose commit carries the task's merges. That write happens only if
nobody moved the task since it was read, so it never reopens a closed task.
A release's state is read as stored (`citrus release show` on its machine
settles a lost one). `works` alone shows what is stored.

The turn is always one of `owner`, `lead`, `lead2`, `agent`, `citrus`.
`work claim --as` makes it the claimant's (`lead`, `lead2`, else `agent`);
`work report` returns it to the lead (`--turn` to another, `--done` releases
the claim); the queue and releases make it `citrus`; merged without a release
in flight, released or verified it is the lead's, and the claim is released
once merged. When every queue entry of the task failed, conflicted or was
cancelled, the turn goes back to the claimant (else the lead); a later
`queue add` from the same worktree follows the task without `--task`.

The units a task must reach are those named by `--unit` and those its queue
entries release (`queue add --release`).

## Stuck

`citrus works --stuck` names, for open work:

| Kind | When | To |
|---|---|---|
| `silent` | claimed, the turn is the claimant's, no event, commit on its branches or run in its worktrees within the lease (default 2 h) | lead |
| `queue_failed` | every queue entry failed, conflicted or was cancelled | the turn's holder |
| `release_failed` | a linked release ended without passing and none passed since | the turn's holder |
| `unreleased` | merged more than `--unreleased-after` (15 m) ago, a unit not released, nothing releasing | the turn's holder |
| `unverified` | released more than `--unverified-after` (2 h) ago: the unit has no verify or postcheck step | the turn's holder |
| `idle` | no event, commit or run for `--idle-after` (24 h) | lead |

The turn's holder is the lead while the turn is Citrus's. `--stuck` only
reports; delivering the lines (a watcher every few minutes) is the project's.
