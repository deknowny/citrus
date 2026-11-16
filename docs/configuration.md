# Configuration

A project describes itself in `citrus.ci` at the repository root, or, when
it grows, in `.citrus/*.ci`: `.citrus/project.ci` for shared settings and one
file per product. The language — items, attributes, conditions, commands,
types — is described in [design/language.md](design/language.md); this page
is a whole configuration at a glance and how Citrus uses it. Unknown items,
attributes and names are errors with a suggestion. `citrus check` validates
the configuration; `citrus doctor` checks it against the repository.

```rust
#![citrus(2)]
#![main("origin/main")]                 // "changed" is measured from the fork point with this ref
#![toolchain("rust-toolchain.toml")]    // files every check's fingerprint depends on
#![signals(cmd!("scripts/classify"))]   // prints SIGNAL, CLAIM and OWN lines (docs/design/planner.md)
#![after_merge(cmd!("scripts/after-merge {{before}}"))]   // after `citrus integrate` merged
#![logs(".validation/logs")]            // run logs in the tree (default: .git/citrus/logs)
#![receipts("citrus/receipts")]         // PASS receipts, relative to the Git directory
#![cache(false)]                        // reuse a pass only where a check or group says #[cache]
#![runner(cmd!("make remote-check"), status = cmd!("make builders-status"))]
#![label("scope-api", only(api))]
#![command("release", "make deploy", "Roll out the verified release")]

/// Fast checks read committed SQLx metadata.
#[env(SQLX_OFFLINE = "true")]
profile fast;

profile e2e;

/// PostgreSQL for the e2e checks.
service database {
    start { run!("docker compose up -d postgres")?; }
    ready { std::wait::tcp("localhost:5432", 60s)?; }
}

/// Headless browsers; at most two at a time.
#[limit(2)]
service browser;

/// The API.
#[paths(std::paths::cargo("api") + ["migrations/**"])]
#[env(RUST_LOG = "warn")]
group api {
    check unit {
        run!("cargo test --locked -p api")?;
    }

    #[profile(e2e)]
    #[needs(database)]
    check database {
        run!("make test-api-db")?;
    }
}

#[paths("**/*.md")]
check links {
    std::docs::check_links("**/*.md")?;
}

/// Start the database and apply migrations.
task seed_db {
    run!("docker compose up -d db")?;
    std::wait::tcp("localhost:5432", 60s)?;
    run!("make migrate")?;
}
```

Releases, artifacts and environments: docs/releases.md and
docs/design/declarative.md.

## Values Citrus fills in

`{{before}}` in `#![after_merge]` (the commit before a merge), and in
artifacts and environments `{{release}}`, `{{artifact}}`, `{{key}}`,
`{{tag}}`, `{{short}}`, `{{commit}}`: values Citrus knows only while running,
written with double braces so the language leaves them as text. Release
steps get them as `r: Release` instead.

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
by no group or check, nothing is carried over. `--push` lands the result
through the merge queue (below) and waits for it.

## Merge queue

`citrus queue add [--release <unit>[:minor|major]]` queues the committed
`HEAD` of a worktree for the base and returns: the owner does not wait. One
processor per clone (started by `queue add`, gone when the queue is empty)
merges the waiting branches together onto the tip of the base in its own
checkout (`<checkout>.citrus-queue/queue`, or `CITRUS_QUEUE_DIR`), runs the
checks that batch needs once (`citrus run --base <tip>`, in the pool when
one serves; proven checks are reused) and fast-forwards the base. A red batch
is halved until the branch that breaks it is found; the others land. A branch
that conflicts with the base is set aside as `conflict`.

Each entry is one row in the state and moves only through Citrus:
`queued → checking → merged → releasing → released`, or `failed`,
`conflict`, `cancelled`, `release_failed`. Asking again with the same commit
returns the same entry (`--retry` queues a failed one again); a newer commit
of the worktree replaces the one still waiting. `citrus status`,
`citrus tasks` and `citrus queue` show the queue; `citrus queue wait` waits
for this worktree's entry, `--release` also for its releases.

An entry marked `--release <unit>` starts `citrus release start <unit>
--approve` once merged, from the queue's checkout of that unit at the merged
commit (prepared by the project's `setup-worktree` task, when it has one).
One release per unit at a time: the next waits for the running one, and the
newest merged commit carries the ones before it. Gates, steps and verify are
the release's own.

A run whose check is already being checked by an earlier unfinished run with
the same fingerprint waits for that result (`joined`) instead of checking it
again. A pool check running four times its usual duration and twenty minutes
past it is stopped as hung.
