# Planning in `citrus.ci`

Status: exclusions implemented; profiles and `covered_by` next. Issue #4.

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

A check without `profiles` belongs to all of them.

## 3. Covered checks

A broad check sometimes runs a narrower one inside it (Garvis' Clyer
pipeline contract runs the Clyer bot tests). When both are selected the
narrower one is redundant:

```
check "test-clyerbot" { owns = clyer, covered_by = ["test-clyer-pipeline-contract"] }
```

`covered_by` drops the check from a plan that already contains one of the
named checks; reuse, receipts and `why` explain it as covered.

## Migration path

Surface by surface, each move deletes its bash branch and its `add_target`
line, and keeps the planner fixtures (exact paths → exact checks) passing.
Paths shared by several surfaces move when the last of those surfaces moves:
then every declared check owns them and the bash map no longer needs them.
