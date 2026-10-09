# Design: the pool — every machine that runs `citrus agent`

Status: in progress. Issues #9 (runners), #7 (resource status), #10 (shared state).

## Why

`citrus run --remote` hands the planned checks to a consumer script
(`#![runner(cmd!(...))]`). In the first consumer that script is the last piece
of the old CI: about 4,500 lines (builder pool, leases, transport, snapshot
proof, adapter). It also splits machines into "local" and "remote", although a
check does not care where it runs.

Measured (run r-20261008-161750-c562, 11 checks): 8 minutes end to end, of
which the checks took 2.6 minutes. The other 5 minutes are the transport:
probes, lease, snapshot archive, image resolve, dependency setup, container
start.

## Model

- **A machine joins by running `citrus agent`.** It reports what it is (OS,
  architecture, CPUs, memory, Docker), how much it shares (`--share 6`) and
  its load. Stopping the agent leaves the pool. Nothing is registered by hand.
- **Agents pull work.** `citrus run` puts the checks in a shared queue; every
  agent takes what fits it. Laptops behind NAT work the same as servers: no
  inbound connections, no SSH into anyone's machine.
- **The coordinator is a Postgres database.** Queue, agents, run events: three
  tables. Claims use `FOR UPDATE SKIP LOCKED`; wake-ups use `LISTEN/NOTIFY`.
  There is no Citrus server process. The pool URL is per person, not per
  repository: `CITRUS_POOL` or `~/.config/citrus/pool`.
- **Snapshots travel through Git.** The requester records its working tree
  (uncommitted changes included, ignored files and `#![private(...)]` paths
  excluded) as a commit and pushes it to `refs/citrus/runs/<run>` of the
  repository's own remote. Agents fetch it into a cached mirror: Git sends only
  the difference. The ref is deleted when the run ends.
- **Checks are split across agents.** One queue row per check. An agent claims
  as many checks of a run as it has free slots and runs them with one
  `citrus run --local --jobs N`, so services and `#[limit]` still apply per
  machine. When it finishes, it claims more. A run of 11 checks on four
  machines takes about the time of its slowest check.
- **The configuration says what checks need, not where they run.**
  `#[meta(linux = true)]` (and `#[requires("docker")]`) match agent labels.
  `#![image(dockerfile = "...", target = "...", context = "...")]` names the
  image the checks run in; an agent with Docker builds it (Docker's cache makes
  that cheap) and runs the executor inside, with the snapshot mounted at its
  own path and the machine's cache for the repository at `/citrus-cache`
  (`CITRUS_POOL_CACHE`), so the image can point Cargo, pnpm and others there. An agent without
  Docker runs natively and only takes checks that need nothing more.
- **What a snapshot leaves out, a machine brings.** `#![private(...)]` paths
  never travel. Each machine can keep its own copies (local test credentials)
  under `~/.config/citrus/files/<repository>/` (or `CITRUS_AGENT_FILES`); the
  agent copies them into every tree of that repository.
- **`#![prepare(cmd!("…"))]` runs in the tree before the checks**, inside
  the image, with `CITRUS_POOL_RUN` set to the run id so per-run resources
  (Compose projects, ports) do not collide on a shared machine. A command
  that starts with `citrus` runs the run's own executor; `citrus ports FILE
  NAME… [--url KEY=NAME]…` moves the ports named in an env file to a free
  block picked from the run id (`NAME` gets block + its position, `NAME+` is
  added when missing, and `--url` follows the port inside another value).
- **Checks run in slot trees.** Cargo trusts source modification times and
  bakes absolute paths into what it builds (`env!("CARGO_MANIFEST_DIR")`). A tree
  at a new path for every run would recompile the workspace every time, and one
  shared build directory beside such trees would reuse artifacts that point into
  trees long gone. So an agent keeps as many slots per repository as it has check
  slots: each is a tree at a path that never changes, with its own Cargo target
  directories (every `/citrus-cache/cargo-target*` an image's environment names).
  A batch claims a free slot, `git checkout`s its commit there (unchanged files
  keep their times, so Cargo rebuilds exactly what changed) and `git clean -fdx`es
  what the previous run left. Slots idle for two weeks are deleted.
- **The executor is the requester's Citrus version.** Agents keep a cache of
  Citrus binaries by commit (built or downloaded as `bin/citrus` does) and run
  the job with the matching one; for a container, the Linux build of it.

## Each machine keeps its own rules

`citrus agent --share 6 --labels ryzen --only-on-power --idle` — a laptop gives
at most 6 CPUs, only on mains, and stops taking work while its owner is
active. A production host can join with `--share 4 --quiet-hours 01-07` or not
at all: heavy builds next to production workloads have hurt before.

### Sharing a machine with production

On Linux an agent adapts by itself (`--fixed` turns it off). Every two seconds
its governor measures what everything *except* the pool is using, and the
kernel's pressure counters (PSI) for CPU, disk and memory:

- the pool may use `target × cores − foreign load` CPUs, never more than
  `--share` and never less than `--min-cpus` (defaults: target 0.8, floor 1);
- CPU or disk pressure, or low free memory, cuts the budget to 60 % per tick
  until the machine recovers; with room the budget grows by half a CPU a tick;
- the budget sets how many checks run at once (a slot is `share / slots` CPUs)
  and is written to the pool's cgroup (`cpu.max` of `CITRUS_AGENT_CGROUP_PARENT`),
  so checks already running slow down too, and the agent restores the full
  share when it stops;
- `citrus pool` shows `limited to N CPUs (reason)` while an agent holds back.

A production node can therefore offer a generous `--share` and still get out of
the way when the node's own workloads need the CPUs.

## Commands

```
citrus agent [--share N] [--labels a,b] [--name NAME]   # join the pool
             [--min-cpus N] [--target-util F] [--fixed]  # adapt to the machine's own load
citrus pool                     # agents, their load, queued and running checks
citrus pool drain NAME          # finish current checks, take no more
citrus run --remote             # through the pool when one is configured
                                # (CITRUS_REMOTE=runner: the declared runner script)
```

`citrus status` shows pool state; no consumer status script is needed (#7).

## Security

- The pool is for a team that trusts each other's code: an agent runs the
  checks of whoever submits them, inside the image when there is one.
- Postgres with TLS and a role that can only use the `citrus` schema.
- Snapshots go only to the repository's own remote, which every agent must be
  able to read anyway.

## Migration in the first consumer

1. Pool, agent, client, image execution in Citrus; tests with a Postgres in CI.
2. A small Postgres for the pool; agents on the two builders and the Mac.
3. Shadow: `citrus run --remote --pool` beside the current runner on real
   changes; compare times and results.
4. Make the pool the default; delete the builder pool, coordinator, transport,
   snapshot proof, bundle and adapter.
5. Release builds (`citrus apply`) take the same agents.
