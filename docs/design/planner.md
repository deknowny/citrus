# Planning

Status: implemented. Citrus is the only planner: it maps the changed paths
to checks from the configuration. A project planner such as the ≈800 lines
of bash Garvis once had is replaced by these generic ideas.

## 1. Groups and paths

A group is a set of paths and the checks that protect it. A check in a group
runs when a changed path is in the group's paths, or in its own `paths` when
it narrows them:

```rust
#[paths(std::paths::cargo("backend") - ["crates/backend/src/bots/clyer/**"])]
group backend {
    check unit { run!("cargo test --locked -p backend")?; }
}

#[paths("crates/backend/src/bots/clyer/**", "migrations/clyer/**")]
group clyer {
    check bot { run!("make test-clyerbot")?; }
}
```

A path is matched by a list when the last glob that matches it is a
positive one (like `.gitignore`). A path no group or check claims is
unmapped.

A path a check names in its own `#[paths]` is that check's: groups elsewhere,
and their checks, do not see it (the specific owner wins, as in
CODEOWNERS). A tool's own test runs when the tool changes, not the broad
contract of everything under `scripts/`. Inputs a check shares with others
go to its `#[reads]`, or stay with a group. A check whose commands Citrus
understands (`run!("cargo test -p api")`) owns what they read without
`#[paths]` (docs/design/language.md#commands).

A check can name a group in its paths, `#[paths(platform,
"crates/proto/**")]`: it runs for the platform group's paths, which other
checks share, and for its own files, which it owns alone. A group without checks (documentation) claims its paths and runs
nothing.

## 2. Profiles

Some checks run only in a slower profile (database end-to-end suites before
a release). `citrus run --profile e2e` selects the checks of that profile;
the default is the first profile declared:

```rust
profile fast;
profile e2e;

#[profile(e2e)]
check database {
    run!("make test-backend-e2e-db")?;
}
```

A check without `#[profile]` belongs to all of them. The runner gets the
profile in `CITRUS_PROFILE`.

## 3. Covers and replaces

A broad check sometimes runs a narrower one inside it: `#[covers(bot)]` drops
`bot` from a plan that has the broad one (listed in notes as
`covered:<name>`).

A check that runs several parts at once can be cheaper than the parts:
`#[replaces(users, orders)]` runs it instead of them when the change goes
beyond what one part owns, and runs just that part otherwise.

## 4. Conditions

Some choices depend on the whole change, not on one path. Garvis runs its
Clyer pipeline contract when every change is Clyer's, the main one when none
is, and the combined one otherwise — three checks with exclusive
conditions:

```rust
#[paths("scripts/**", "make/**")]
group pipeline {
    #[when(only(clyer))]
    check contract_clyer { run!("make test-clyer-pipeline-contract")?; }

    #[when(without(clyer))]
    check contract_main { run!("make test-main-pipeline-contract")?; }

    #[when(!only(clyer) && !without(clyer))]
    check contract { run!("make test-pipeline-contract")?; }
}
```

Conditions (values of type `Cond`, usable in a `const` and a `const fn`):

- `touched(x)`: a changed path is in group or check `x`, or in a list of
  globs (a path set that feeds conditions without claiming paths).
- `only(x)`: every changed path is in it; `without(x)`: none is.
- `selected(x)`: check `x` is in the plan (conditions are applied until the
  plan stops changing).
- `profile(e2e)`: the plan is for that profile.
- `signal("x")`: the project's signal command printed `SIGNAL x`.
- `&&`, `||`, `!` combine them. A check with paths and `#[when]` needs both;
  a check with only `#[when]` is chosen by the condition.

`#![label("name", condition)]` reports a named condition with the plan
(`labels`), for tools that need the plan's character.

## 5. Signals

Classification a glob cannot express (Garvis reads file contents to tell
which product a change affects) belongs to the project's signal command,
`#![signals(cmd!("…"))]` (docs/protocol.md). It prints
`SIGNAL <name>` facts and `CLAIM <path> <group>` lines that put a path into
a group (a file that existed at the base and is gone now); a claimed path
selects the group's checks that do not narrow its paths.

`citrus plan --paths-file FILE` plans an explicit list of paths; the plan
lists the touched groups, signals and labels.
