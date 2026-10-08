# Citrus language v2 (prototype)

Citrus organizes a project's checks, tasks and releases: what a change
needs, what is already proven, what runs where and in which order. The
language exists to **decompose and connect** that work — the job Make does
today, with names, types and functions instead of string targets and
prerequisites. It is not a language for writing tests: tests stay in
`cargo test`, Playwright and the like; a check runs them and judges the
result.

The syntax borrows from Rust because people and agents read it fluently.
The semantics are deliberately smaller.

```rust
#![citrus(2)]
#![toolchain("Cargo.lock")]

/// Sources every Rust check reads.
const RUST: list<glob> = ["src/**", "tests/**", "Cargo.toml", "Cargo.lock", "build.rs"];

/// Unit and integration tests on throwaway Git repositories.
#[paths(RUST, "examples/**")]
check test {
    std::cargo::test().run()?;
}

/// rustfmt and clippy with warnings as errors.
#[paths(RUST, "rustfmt.toml")]
check lint {
    std::cargo::fmt().arg("--check").run()?;
    std::cargo::clippy().args(["--all-targets", "--", "-D", "warnings"]).run()?;
}
```

## Decisions

1. **Values are copied, never borrowed.** Strings, lists and structs behave
   as copies: a function cannot change its caller's data, it returns a new
   value. No `&`, `&mut` or lifetimes. (Internally shared and copied on
   write.)
2. **Declarations are items, referenced by name.** `group`, `check`,
   `task`, `release`, `environment`, `const`, `fn`, `struct` are items, like
   `fn` and `struct` in Rust: `#[needs(database)]` names an item and the
   name is checked when the file loads. Values exist only while code runs.
3. **Mutability is local.** `let` is immutable, `let mut` changes only that
   binding. There is no global mutable state; `const` is computed at load.
4. **One error type.** `Result<T>` is `Result<T, Error>`; an `Error` has a
   message, the place in the file and a chain of causes (`.context("…")`).
   No user error enums: the only reader of an error is the run report.
5. **One namespace per project.** Files under `.citrus/` organize the
   configuration for people; moving an item between files changes nothing.
   Built-ins live in `std::`. A check in a group is `group::check`, in code
   and on the command line.
6. **Types.** `bool`, `int`, `str`, `path`, `glob`, `duration` (`30s`,
   `5m`), `Version`, `list<T>`, `Option<T>`, `Result<T>`, `()` and
   structs. Built-in types are generic; user code has no generics, traits,
   closures or async. `"{x}"` interpolates in every string (`{{` for a
   brace).
7. **Structs are plain records** with automatic equality and printing. No
   user `impl` yet; methods belong to `std` types.
8. **Reuse is a function call.** Checks do not call or depend on checks:
   each one is a unit of caching, reporting and parallelism. Shared work is a
   `fn`; order, when it really matters, is `#[after(other)]`.

## Phases

| Phase | What is evaluated | What it may use |
|---|---|---|
| Load | `const`, attributes | literals, `const` items, `const fn` |
| Plan | `#[when(…)]` | `const fn` of the change (no I/O) |
| Run | bodies of `check`, `task`, `step`, `fn` | everything, I/O only through `std::` |

A `const fn` cannot call a plain `fn` or any `std` function with effects;
the checker refuses it. The plan therefore stays deterministic and `citrus
why` can explain it.

## Items

```rust
#![citrus(2)]                     // first line: this file is language v2
#![main("origin/main")]           // project settings are inner attributes
#![toolchain("Cargo.lock")]

const NAME: type = expr;
const fn name(param: type, …) -> type { … }
fn name(param: type, …) -> type { … }
struct Name { field: type, … }

/// Doc comment: the item's description (`citrus why`, `citrus targets`).
#[paths(glob, …)] #[reads(glob, …)] #[cache] #[needs(item, …)]
group name { check … }

#[paths(…)] #[cache] #[after(check)]
check name { statements }

task name { statements }          // `citrus do name`

environment name;                 // one release at a time holds it

#[environment(name)]
#[version(initial = "1.0.0", scope = ["image"])]
release name {
    step name(r: Release) { … }   // r.version, r.previous, r.commit, r.unit
    #[production] #[recover(fn_name)]
    step name(r: Release) { … }
    rollback(r: Release) { … }
}
```

`#[paths]` on a group selects its checks; a check's own `#[paths]` narrows
it (the old named-path ownership). `#[cache]` reuses a pass while the
declared inputs and the body are unchanged.

## Statements and expressions

```rust
let x = expr;            let mut xs: list<str> = [];      xs = xs + ["a"];
if cond { … } else if cond { … } else { … }
for item in list { … }
match value { Some(x) => …, None => …, "literal" => …, _ => … }
assert cond, "message {x}";
return expr;
expr?                    // Err or None: leave the function with it
```

A check or step body returns `Result<()>`: falling off the end is `Ok(())`,
`?` and a failed `assert` end it with an error, and the run reports that
error with its place in the file.

## The standard library (first cut)

| | |
|---|---|
| `std::proc::Command::new(program)` | `.arg(s)`, `.args(list)`, `.env(k, v)`, `.run() -> Result<()>` (streams to the log), `.output() -> Result<Output>` (`code`, `stdout`, `stderr`) |
| `std::cargo::test()` / `fmt()` / `clippy()` / `run()` | `cargo <sub>` as a `Command` |
| `std::fs::read(path) -> Result<str>`, `exists(path) -> bool`, `glob(glob) -> list<path>` | |
| `std::env::var(name) -> Option<str>` | |
| `std::wait::http(url, timeout) -> Result<()>` | |
| `std::docs::check_links(glob) -> Result<()>` | relative Markdown links point at files |
| `std::log::info(message)` | a line in the run log |

Methods: `str` — `len`, `contains`, `starts_with`, `ends_with`, `find`,
`trim`, `lines`, `split`, `count`, `is_empty`; `list` — `len`, `contains`,
`is_empty`, `push` (on a `let mut`); `Option` — `is_some`, `is_none`,
`unwrap_or`, `ok_or`; `Result` — `is_ok`, `is_err`, `context`; `path` —
`matches(glob)`; `Version` — `bump()`.

## Not in the prototype

Groups' `when` with signals, services, runners, profiles, artifacts and
environments with Kubernetes deploys, input tracing of `std::fs` reads,
`citrus fmt` for v2. The prototype is judged on Citrus's own pipeline
first.
