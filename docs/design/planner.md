# Planning in `citrus.ci`

Status: implemented (exclusions, profiles, `covered_by`, groups, `when`, signals). Issue #4.

A project planner such as Garvis' `scripts/ci-plan.sh` (≈800 lines of bash
plus a 780-line product-impact script) answers one question: which checks do
these changed paths need. Moving its first surfaces into `check` blocks
showed that what is left needs three generic ideas, not more bash.

## 1. Exclusions in globs

Ownership is often "this directory except that part": the Garvis backend
crate belongs to the Garvis product except its Clyer bot module, which
belongs to Clyer. A glob starting with `!` removes matches of the globs
before it:

```
check "test-backend" {
  owns = ["crates/backend/**", "!crates/backend/src/bots/clyer/**"]
}
check "test-clyerbot" {
  owns = ["crates/backend/src/bots/clyer/**", "migrations/clyer/**"]
}
```

A path is matched by a list when the last glob that matches it is a
positive one (like `.gitignore`). Exclusions apply to `owns` and `reads`
alike; the fingerprint hashes the list as written (docs/manifest.md).

## 2. Profiles

Some checks run only in a slower profile (database end-to-end suites before
a release). A check names the profiles it belongs to; `citrus run --profile
e2e` selects checks of that profile, and the default profile is the first
one the project declares:

```
project { profiles = ["fast", "e2e"] }
check "test-backend" { owns = backend, profiles = ["fast"], run = make("test-backend") }
check "test-backend-e2e-db" { owns = backend, profiles = ["e2e"], run = make("test-backend-e2e-db") }
```

A check without `profiles` belongs to all of them. `--profile` (or
`CITRUS_PROFILE`) picks the profile; the planner gets it as `profile_var`
(`planner { profile_var = "MODE" }` passes `MODE=e2e`) and in
`CITRUS_PROFILE`, and so does the pool.

## 3. Covered checks

A broad check sometimes runs a narrower one inside it (Garvis' Clyer
pipeline contract runs the Clyer bot tests). When both are selected the
narrower one is redundant:

```
check "test-clyerbot" { owns = clyer, covered_by = ["test-clyer-pipeline-contract"] }
```

`covered_by` drops the check from a plan that already contains one of the
named checks (declared or chosen by the project's planner); the plan lists
it under notes as `covered:<name>`.

## 4. Groups, conditions and signals

Some choices depend on the whole change, not on one path: Garvis runs its
Clyer pipeline contract when pipeline files changed and every other change
is Clyer's, the main one when none is, and the combined one otherwise.
Named path sets (`group`) and plan-time conditions (`when`) express that:

```
group "pipeline" { owns = ["scripts/**", "make/**"] }
group "clyer" { owns = ["crates/clyer/**", "migrations/clyer/**"] }
group "main" { owns = ["**", "!crates/clyer/**", "!migrations/clyer/**"] }
group "docs" { owns = ["**/*.md"], note = "no-heavy:docs" }

check "test-clyer-pipeline-contract" {
  when = touched("pipeline") and touched("clyer") and not touched("main")
  run = make("test-clyer-pipeline-contract")
}
```

- `touched("x")`: a changed path is owned by group or check `x`.
- `selected("x")`: check `x` is in the plan (conditions are applied until
  the plan stops changing).
- `signal("x")`: the project's signal command printed `SIGNAL x`. It runs
  with `CITRUS_PATHS` (a file of the changed paths) and is the place for
  classification a glob cannot express (Garvis reads file contents to tell
  which product a change affects): `project { signals = run(...) }`. It may
  also print `CLAIM <path> <group>` to put one path into a group (a file
  that existed at the base and is gone now).
- `and`, `or`, `not` combine them. A check with `owns` and `when` needs
  both; a check with only `when` is chosen by the condition.
- A touched group's `note` is listed with the plan's targets.
- `claims = false`: the group feeds conditions only; a path that only such
  groups contain stays unmapped (`group "main" { owns = ["**", "!clyer/**"],
  claims = false }`).
- A path a check owns belongs to that check: it touches only groups with
  `claims = false`, never the claiming ones.
- `label "name" { when = … }` reports a named condition with the plan
  (`labels`), for tools that need the plan's character (Garvis' scope).
- Declaring a group again adds paths: a path belongs to the group when any
  declaration matches it (each with its own `!` exclusions).

`citrus plan --paths-file FILE` plans an explicit list of paths; the plan
lists the touched groups and signals for tools that adapt it.
`CITRUS_PLANNER=builtin` plans from `citrus.ci` even when a project planner
is declared, to compare both before switching.

## Migration path

Surface by surface, each move deletes its bash branch and its `add_target`
line, and keeps the planner fixtures (exact paths → exact checks) passing.
Paths shared by several surfaces move when the last of those surfaces moves:
then every declared check owns them and the bash map no longer needs them.
