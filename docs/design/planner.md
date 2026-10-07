# Planning

Status: implemented. Citrus is the only planner: it maps the changed paths
to checks from the configuration. A project planner such as the ≈800 lines
of bash Garvis once had is replaced by these generic ideas.

## 1. Groups and paths

A group is a set of paths and the checks that protect it. A check in a group
runs when a changed path is in the group's paths, or in its own `paths` when
it narrows them:

```
group backend {
  paths = crate("backend") + ["!crates/backend/src/bots/clyer/**"]
  check unit = cargo.test("backend")
}

group clyer {
  paths = ["crates/backend/src/bots/clyer/**", "migrations/clyer/**"]
  check bot = make("test-clyerbot")
}
```

A path is matched by a list when the last glob that matches it is a
positive one (like `.gitignore`). A path no group or check claims is
unmapped.

A path a check names in its own `paths` is that check's: groups elsewhere,
and their checks, do not see it (the specific owner wins, as in
CODEOWNERS). A tool's own test runs when the tool changes, not the broad
contract of everything under `scripts/`. Inputs a check shares with others
go to its `reads`, or stay with a group. A group without checks (documentation) claims its paths and runs
nothing.

## 2. Profiles

Some checks run only in a slower profile (database end-to-end suites before
a release). `citrus run --profile e2e` selects the checks of that profile;
the default is the first profile declared:

```
profile fast
profile e2e

check database = make("test-backend-e2e-db") {
  paths = crate("backend")
  profile = e2e
}
```

A check without `profile` belongs to all of them. The runner gets the
profile in `CITRUS_PROFILE`.

## 3. Covers and replaces

A broad check sometimes runs a narrower one inside it: `covers = [bot]` drops
`bot` from a plan that has the broad one (listed in notes as
`covered:<name>`).

A check that runs several parts at once can be cheaper than the parts:
`replaces = [users, orders]` runs it instead of them when the change goes
beyond what one part owns, and runs just that part otherwise.

## 4. Conditions and `match changed`

Some choices depend on the whole change, not on one path. Garvis runs its
Clyer pipeline contract when every change is Clyer's, the main one when none
is, and the combined one otherwise:

```
group pipeline {
  paths = ["scripts/**", "make/**"]
  check contract = match changed {
    only(clyer) => make("test-clyer-pipeline-contract")
    without(clyer) => make("test-main-pipeline-contract")
    _ => make("test-pipeline-contract")
  }
}
```

The arm chosen by the plan is what the check runs, and what its fingerprint
covers. Conditions are also available as `when = …` on a check and on a
`label`:

- `touched(x)`: a changed path is in group or check `x`, or in a list of
  globs (a path set that feeds conditions without claiming paths).
- `only(x)`: every changed path is in it; `without(x)`: none is.
- `selected(x)`: check `x` is in the plan (conditions are applied until the
  plan stops changing).
- `profile(e2e)`: the plan is for that profile.
- `signal("x")`: the project's signal command printed `SIGNAL x`.
- `and`, `or`, `not` combine them. A check with paths and `when` needs both;
  a check with only `when` is chosen by the condition.

`label name { when = … }` reports a named condition with the plan
(`labels`), for tools that need the plan's character.

## 5. Signals

Classification a glob cannot express (Garvis reads file contents to tell
which product a change affects) belongs to the project's signal command,
`project { signals = run(...) }` (docs/protocol.md). It prints
`SIGNAL <name>` facts and `CLAIM <path> <group>` lines that put a path into
a group (a file that existed at the base and is gone now); a claimed path
selects the group's checks that do not narrow its paths.

`citrus plan --paths-file FILE` plans an explicit list of paths; the plan
lists the touched groups, signals and labels.
