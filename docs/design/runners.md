# Design: runners — checks on your own machines (`#![runner]` built in)

Status: proposal, for review before implementation. Issue #9.

## Why

`citrus run --remote` hands the planned checks to a consumer script
(`#![runner(cmd!(...))]`). In the first consumer that script is the last
piece of the old CI. It has about 4,500 lines in seven files:

| Piece | Lines | What it does |
|---|---|---|
| builder pool | 630 | probe hosts over SSH (CPU, memory, pressure, disk, leases), pick the best one, wait while all are busy |
| lease and coordinator | 1,060 | one exclusive kernel `flock` per builder and resource; records who holds it |
| transport | 1,820 | describe the source snapshot, ship it (git archive + gitlinks + a filtered tarball), build a runner image, start a container with CPU/memory limits, stream its log, clean up |
| snapshot proof | 540 | before and after the run, prove the builder ran exactly the described files |
| local Docker host | 190 | the developer's Mac as one more builder |
| bundle, adapter | 300 | tarball filter; translate the transport's log into Citrus lines |

Most of this is generic: every team that runs checks on its own machines
needs to pick a machine, lease it, ship exactly the snapshot, run inside a
limit, and report. The consumer-specific part is small: host addresses, the
image to run in, and resource limits.

Measured cost today (run r-20261008-161750-c562, 11 checks): 8 minutes end to
end, of which the checks themselves took 2.6 minutes (`citrus run --local` in
the container). The other 5 minutes are the transport: probes, lease,
snapshot archive, runner image resolve, dependency setup and container start.
That overhead, not the checks, is what a built-in runner removes.

## Model

A runner is a machine Citrus can lease and run checks on. It is declared in
`.ci`, next to services:

```rust
/// The Ryzen builders: Linux, Docker, the shared Cargo and pnpm caches.
#[hosts("root@81.90.20.4", "root@81.90.20.5")]
#[budget(cpus = 30, memory = "96G", disk_free = "80G")]
#[image(dockerfile = "deploy/ci/runner.Dockerfile", target = "runner")]
#[caches("/cargo", "/pnpm-store")]
runner ryzen {}

/// The developer's Mac, through Docker Desktop's Linux VM.
#[hosts("local")]
#[budget(cpus = "all - 1")]
#[platform("linux/arm64")]
runner mac {}
```

- `#[hosts]`: SSH destinations (`ssh` config applies) or `local`.
- `#[budget]`: what one run may take. Citrus starts the container with these
  limits, and with `--jobs` from the CPU budget.
- `#[image]`: the image checks run in. Citrus builds it on the runner with
  BuildKit, keyed by the hash of its inputs (the same key as artifacts), and
  keeps the last 3.
- `#[caches]`: volumes kept between runs on that host.
- Checks choose runners like they choose services: `#[on(ryzen)]`, or
  `#[meta(linux = true)]` keeps them off a runner whose platform is not Linux.
  A run with no requirement goes to any runner.

`citrus run --remote` then needs no consumer script. The `#![runner(cmd!)]`
escape hatch stays for setups Citrus cannot model.

## What a run does

1. **Pick.** Read each host's state from `citrusd` (below) — one SSH round
   trip, under 1 s, instead of a probe per host. Choose the least loaded host
   that fits the budget; if all are busy, queue and print the position.
2. **Lease.** A lease is a `flock` held by `citrusd` on that host, tied to the
   SSH connection: if the agent dies, the lease is released. The lease records
   run id, agent, branch, start time; `citrus status` shows it.
3. **Ship.** The snapshot is the set of files `citrus` already fingerprints.
   Citrus sends only blobs the host does not have yet (content-addressed
   store on the host, `~/.citrus/blobs`), then a manifest. A typical change
   ships kilobytes instead of a full archive. Gitlinks ship as their commit
   and are fetched on the host.
4. **Run.** In the runner image: `citrus run --local --jobs N <checks>` on the
   shipped tree, with `CITRUS_GIT_DIR` as today. Services start inside the
   same container network.
5. **Prove.** `citrusd` hashes the tree before and after; a change during the
   run fails it with status 75 (drift), as today.
6. **Report.** The child Citrus speaks `CITRUS_PROTOCOL=1` already; the parent
   reads it directly. Logs stay on the host and are fetched on
   `citrus log --full`.
7. **Clean.** Container, network and tree are removed; the lease ends.

## `citrusd`

A small process Citrus starts on demand over SSH (`citrus daemon --stdio`),
the same binary. It is not a long-lived server: it lives for the lease. It
answers state queries, holds the lease, receives blobs, starts the container
and streams events. Installing it is copying the binary, which `citrus` does
itself when the versions differ (by checksum).

This also closes issue #7: `citrus status` reads runner state through it,
with no consumer status script.

## Security

- Only SSH. Citrus adds no listening port.
- The shipped tree excludes what the planner already excludes (secrets,
  local hooks, certificates); the manifest is checked against the same rules
  on the host.
- The container gets no host credentials. Registry access for building the
  runner image uses the host's existing Docker login.

## Migration in the first consumer

1. Implement `runner` declarations, `citrusd`, pick/lease/ship/run/prove for
   one host. Test against a local Docker and a container that plays a host
   (sshd in a container) in `tests/`.
2. Shadow: the consumer keeps `#![runner(cmd!)]`; `citrus run --remote
   --runner ryzen` uses the built-in runner on demand. Compare time and results
   on real changes for a few days.
3. Switch the default; delete the builder pool, coordinator, transport,
   snapshot proof, bundle and adapter (about 4,500 lines). The local Docker
   host script becomes `#[hosts("local")]`.
4. Release builds (`citrus apply`) lease the same runners, so the separate
   release lease and the release-build lock go too.

## Not in the first version

- Other platforms' sandboxing (cgroups outside Docker, macOS power budgets).
- A shared server for several people's state (issue #10).
- Autoscaling or cloud hosts.

## Open questions

- Blob store garbage collection: keep blobs referenced by the last N
  manifests per host, or a size cap?
- Should the runner image be one per repository, or per check group
  (Rust-only checks do not need Node)?
