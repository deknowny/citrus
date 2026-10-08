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
by no group or check, nothing is carried over. `--push` fast-forwards the
base after the remaining checks pass, and integrates again if the base moved
meanwhile.
