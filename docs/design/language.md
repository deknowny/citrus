# The Citrus language

Citrus organizes a project's checks, tasks and releases: what a change needs,
what is already proven, what runs where and in which order. The language
exists to **decompose and connect** that work — what Make does, with names,
types and functions instead of string targets and prerequisites. It is not a
language for writing tests: tests stay in `cargo test`, Playwright and the
like; a check runs them and judges the result.

The syntax borrows from Rust because people and agents read it fluently. The
semantics are deliberately smaller.

```rust
#![citrus(2)]
#![toolchain("Cargo.lock")]

/// Unit and integration tests: inputs come from Cargo.
check test {
    run!("cargo test --locked")?;
}

/// rustfmt and clippy with warnings as errors.
check lint {
    run!("cargo fmt --all --check")?;
    run!("cargo clippy --locked --all-targets -- -D warnings")?;
}

/// Relative links in Markdown point at existing files.
#[paths("*.md", "docs/**")]
check docs {
    std::docs::check_links("**/*.md")?;
}
```

## Files

A project is `citrus.ci` at the repository root, or `.citrus/*.ci` (one
file per product, `project.ci` first) — not both. Every file starts with
`#![citrus(2)]`. All files share **one namespace**: moving an item from one
file to another changes nothing. `//` is a comment; `///` above an item is
its description (`citrus why`, `citrus targets`, `citrus`). `citrus fmt`
lays files out: four spaces a bracket level (a line opening several, like
`#[paths([`, is one level), a continued expression (`.context(…)`, `&&`)
one more, no trailing spaces, at most one blank line; `--check` only names
the files it would change.

## Decisions

1. **Values are copied, never borrowed.** Strings, lists and structs behave
   as copies: a function cannot change its caller's data, it returns a new
   value. No `&`, `&mut` or lifetimes.
2. **Declarations are items, named by reference.** `group`, `check`, `task`,
   `profile`, `service`, `artifact`, `environment`, `release`, `const`, `fn`,
   `struct`: `#[needs(database)]` names an item, and the name is checked
   when the file loads. Values exist only while code runs.
3. **Mutability is local.** `let` is immutable, `let mut` changes only that
   binding; there is no global mutable state.
4. **One error type.** `Result<T>` is `Result<T, Error>`: a message, the
   place in the file and what it happened in (`.context("…")`).
5. **Types.** `bool`, `int`, `str`, `path`, `glob`, `duration` (`30s`, `5m`,
   `2h`), `Version`, `Cond`, `list<T>`, `Option<T>`, `Result<T>`, `()` and
   structs. Built-in types are generic; user code has no generics, traits,
   closures or async. `"{x}"` interpolates in every string (`{{` for a
   brace).
6. **Structs are plain records** with automatic equality and printing.
7. **Reuse is a function call.** Checks do not call or depend on checks;
   each one is a unit of caching, reporting and parallelism. Shared work is
   a `fn`.

## Phases

| Phase | What is evaluated | What it may use |
|---|---|---|
| Load | `const`, attributes | literals, constants, `const fn`, `std::paths` |
| Plan | `#[when(…)]`, `#![label]` | conditions over the change (no I/O) |
| Run | bodies of `check`, `task`, `step`, `service`, `fn` | everything; I/O only through `run!`, `cmd!` and `std` |

A `const fn` cannot run programs or touch files; the checker refuses it, so
the plan stays deterministic and `citrus why` can explain it.

## Project attributes

```rust
#![citrus(2)]
#![main("origin/main")]                      // what "changed" is measured against
#![toolchain("rust-toolchain.toml")]         // files every fingerprint includes
#![cache(false)]                             // checks reuse a pass only with #[cache]
#![logs(".scratch/citrus")]                  // where run logs go (default .git/citrus/logs)
#![receipts("ci-receipts")]                  // shared pass receipts under the git dir
#![signals(cmd!("python3 scripts/signals.py"))]          // docs/protocol.md
#![free_version(cmd!("scripts/registry.sh free-version"))]  // docs/releases.md
#![after_merge(cmd!("scripts/after-merge.sh --since {{before}}"))]
#![runner(cmd!("make remote-check"), status = cmd!("make builders-status"))]
#![tool("scripts/cargo-test.sh", cmd!("cargo test"))]   // a wrapper Citrus understands
#![label("scope-main", only(main))]          // a named condition reported with the plan
#![command("release", "make deploy", "Roll out the verified release")]
```

## Items

### `check` and `group`

```rust
/// Rust workspace files: every crate is built with them.
const WORKSPACE: list<glob> = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"];

/// The backend crate and everything it is built from.
#[paths(WORKSPACE, "crates/backend/**", ".sqlx/**")]
#[needs(cargo)]
group backend {
    #[profile(fast)]
    #[env(SQLX_OFFLINE = "true")]
    check unit {
        run!("make test-backend")?;
    }

    #[profile(e2e)]
    #[needs(test_db)]
    check database {
        run!("make test-backend-e2e-db")?;
    }
}
```

A **group** is a set of paths and the checks that protect them: a change to
one of its paths selects its checks. A check in a group is `group::check` in the
language and `group.check` outside it. Outside the language every name is
written with `-` for `_`, as Cargo does for crates: `service test_db` is
`test-db` in plans, JSON and on the command line, which takes either. Checks inherit the
group's `paths`, `reads`, `needs`, `env`, `profile`, `cache` and `when`.

| Attribute | Meaning |
|---|---|
| `#[paths(…)]` | Globs (and group names) that select the check; a check's own paths narrow its group's, and a path a check names is that check's alone |
| `#[reads(…)]` | More inputs of its fingerprint that do not select it |
| `#[when(cond)]` | Selected only when the condition holds (and, with paths, one changed) |
| `#[needs(service, …)]` | Resources it runs with |
| `#[profile(name)]` | Belongs to these profiles only |
| `#[cache]` / `#[cache(false)]` | Reuse a pass while inputs and body are unchanged |
| `#[covers(check)]` / `#[replaces(check, …)]` | Plan shape (docs/design/planner.md) |
| `#[env(KEY = "value")]` | Environment of every program the body runs |
| `#[meta(key = value)]` | Data for the project's own tools (`CITRUS_CHECKS`) |

A check whose commands Citrus understands needs no `#[paths]` (see
[Commands](#commands)). `a - b` removes globs: `["scripts/**"] - TOOLS`.

### Conditions

`touched(x)`, `only(x)`, `without(x)` (a group, a check, or globs),
`signal("name")`, `selected(check)`, `profile(name)`, combined with `&&`,
`||` and `!`. A condition is a value of type `Cond`: it can live in a
`const` and be built by a `const fn`.

```rust
const RELEASE_GATE = signal("userbot-release-gate");

const fn part(paths: list<glob>) -> Cond {
    !only(DOCS) && touched(paths)
}

#[when(part(API) || RELEASE_GATE)]
check api { run!("make test-api")?; }
```

When the command depends on the change, write one check per case with
exclusive conditions (`#[when(only(clyer))]`, `#[when(!only(clyer))]`).

### `profile` and `service`

```rust
/// `make check`.
#[env(SQLX_OFFLINE = "true")]
profile fast;

/// A disposable PostgreSQL, started once before the checks that need it and
/// stopped when the run is over, whatever its result.
service database {
    start { run!("docker compose up -d postgres")?; }
    ready { std::wait::tcp("localhost:5432", 1m)?; }
    stop { run!("docker compose down")?; }
}

/// Headless browsers the runner hands out; at most two at a time.
#[limit(2)]
service browser;
```

The first profile is the default (`--profile`, `CITRUS_PROFILE`).

### `task`

```rust
/// Start the database and apply migrations.
task db {
    run!("docker compose up -d postgres")?;
    run!("make migrate")?;
}
```

`citrus do db` runs it.

### `release`, `artifact`, `environment`

```rust
/// The backend image; its inputs are what the build selects.
#[inputs(cmd!("scripts/build-inputs.py backend --list"))]
#[dockerfile("Dockerfile", target = "backend")]
artifact backend_image;

/// Production: one release at a time.
#[kubernetes(context = "prod", namespace = "shop")]
#[record(annotation = "example.com/release")]
#[deploy("backend", backend_image)]
environment production;

#[environment(production)]
#[version(initial = "1.0.0", scope = ["backend"])]
release backend {
    step build(r: Release) { run!("make image RELEASE={r.version}")?; }

    #[production]
    #[recover(reconcile)]
    step deploy(r: Release) { run!("make deploy RELEASE={r.version}")?; }

    rollback(r: Release) { run!("make rollback RELEASE={r.previous}")?; }
}

fn reconcile(r: Release) -> Result<()> {
    run!("make deploy-reconcile RELEASE={r.version}")
}
```

`Release` holds `version`, `previous` (an `Option`), `commit` and `unit`.
An environment without `#[kubernetes]` is only a lock. Details:
docs/releases.md and docs/design/declarative.md.

## Commands

`run!("…")` runs a program and returns `Result<()>` (its output goes to the
log); `cmd!("…")` is the same command as a `Command` (`.output()`, `.env()`,
`.current_dir()`). The line is split into words when the file loads, like a
terminal without a shell:

- whitespace separates words, `'…'` keeps spaces inside one;
- `{x}` is part of the word it stands in and never splits;
- `{list...}` is a word of its own and spreads a list;
- leading `KEY=value` words are the command's environment;
- `|`, `>`, `*` are plain characters: write `run!("sh -c {script}")` for a
  shell.

A check or task whose body is one fixed command line is that process:
`CITRUS_CHECKS` shows its argv to the project's tools.

### Programs Citrus understands

Because the line is read when the file loads, Citrus sees which program it
starts:

- **Cargo** (`cargo test|build|check|clippy|run|bench|doc|fmt|nextest`):
  the check's inputs are the closure of the packages it builds (`-p` names
  them; without it the workspace members, or the root package): crates,
  path dependencies, files their sources `include_str!`, the manifest,
  lockfile and settings. A misspelled subcommand or package is an error
  before anything runs. Everything after `--` belongs to the test binary.

- **Make** (`make [flags] targets…` in the repository's own Makefile): the
  rules it runs (prerequisites and `$(MAKE)` in recipes), the Makefiles
  defining them and the variables their recipes use, and the files the
  recipes name, followed into the scripts those name (code only, files
  only). A target the Makefiles do not define is an error before anything
  runs. These inputs make a pass stale (they join `#[reads]`). What the
  recipes certainly read — the Makefiles and the files the recipes name —
  also selects the check, without taking the path from the checks and
  groups that own it: a change to `scripts/run-tests.sh` runs every check
  whose recipe runs it.

A wrapper script is understood like the command line it stands for once the
project says so: `#![tool("scripts/cargo-test.sh", cmd!("cargo test"))]`
makes `run!("scripts/cargo-test.sh -p api --lib")` read as `cargo test -p api
--lib`, with the script itself among the inputs.

A program Citrus does not know is a plain process: its check declares
`#[paths]`. `citrus why` and `citrus targets` show what each command was
understood as.

A check that may be reused is watched when it runs locally: its Python and
Node programs (audit hook, `--require`) must read no repository file, and
list no directory, outside its paths and `#[reads]`. Otherwise its pass is
not reused and Citrus names what it read. Other programs are not watched.

`citrus deps` checks the inference against the compiler after a build: every
repository file a crate read (Cargo's dep-info, and build scripts'
`rerun-if-changed`) must be among the inputs of each understood check that
builds that crate. It exits 1 and names the file otherwise. Procedural macros
that read files without telling the compiler (SQLx's offline data) are not
seen: declare those with `#[reads(…)]`.

## Tests

A `#[test] fn` states what a change would run. `citrus test [filter]` runs
every test in one process, planning each change like `citrus plan
--paths-file`, signal command included; a change without a profile is
planned in the project's first one.

```rust
#[test]
fn a_script_runs_the_pipeline_contract() -> Result<()> {
    let plan = std::plan::of(["scripts/release.sh"])?;
    assert plan.checks == ["pipeline.contract"];
    assert plan.groups_of("scripts/release.sh") == ["pipeline"];
}

#[test]
fn clyer_alone_is_the_clyer_scope() -> Result<()> {
    let plan = std::plan::change(["clyer/bot.rs"])
        .profile("e2e")
        .env("VALIDATION_SCOPE", "clyer")     // seen by the signal command
        .plan()?;
    assert plan.selects("clyer.e2e"), "the e2e check runs";
    assert plan.labels == ["scope-clyer"];
}
```

A `Plan` has `checks`, `groups`, `labels`, `signals`, `notes` (checks left
out by profile or `covers`) and `unclaimed` (paths no group or check owns);
`owners(path)` are the checks owning a path and `groups_of(path)` its
groups. `selects(name)` fails on a name that is not a check, so a typo
cannot pass. A failed `==` or `!=` shows both sides. Names are the outside
ones (`group.check`, `-` for `_`).

## Statements and expressions

```rust
let x = expr;            let mut xs: list<str> = [];      xs.push("a");
if cond { … } else if cond { … } else { … }
for item in list { … }
match value { Some(x) => …, None => …, "literal" => …, _ => … }
assert cond, "message {x}";
return expr;
expr?                    // Err or None leaves the function with it
```

A body of a check, task, step or service returns `Result<()>`: falling off
the end is `Ok(())`, `?` and a failed `assert` end it with an error, which
the run reports with its place in the file.

## The standard library

| | |
|---|---|
| `run!("…")`, `cmd!("…")` | programs (above) |
| `std::proc::Command::new(program)` | a command whose program is a value |
| `std::fs::read(path) -> Result<str>`, `exists`, `glob`, `copy` | files |
| `std::paths::cargo("pkg")`, `next("@app")`, `package("@lib")` | a package's files, at load |
| `std::env::var(name) -> Option<str>` | |
| `std::env::platform() -> str` | `macos-aarch64`, `linux-x86_64`…: the machine the step runs on |
| `std::wait::http(url, timeout)`, `tcp(address, timeout)`, `file(path, timeout)` | readiness |
| `std::docs::check_links(glob)` | relative Markdown links point at files |
| `std::log::info(message)` | a line in the log |
| `std::plan::of(paths) -> Result<Plan>`, `std::plan::change(paths)` | what a change would run ([Tests](#tests)) |

Methods: `str`/`path` — `len`, `contains`, `starts_with`, `ends_with`,
`find`, `trim`, `lines`, `split`, `count`, `is_empty`, `matches(glob)`;
`list` — `len`, `contains`, `first`, `last`, `join`, `is_empty`, `push` (on
a `let mut`); `Option` — `is_some`, `is_none`, `unwrap_or`, `ok_or`;
`Result` — `is_ok`, `is_err`, `context`, `ok`; `Version` — `bump`;
`Change` — `profile`, `env`, `plan`; `Plan` — `selects`, `owners`, `groups_of`.

## Non-goals

A general-purpose language. Big logic stays in the project's programs;
a body is glue and judgement, a few lines. Features arrive only when they
remove real scripts.
