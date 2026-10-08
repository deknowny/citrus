# The `.ci` language

Status: `citrus 1`, implemented as described here (issue #11). It is
Citrus's only configuration; before 1.0 it changes without compatibility
shims.

A `.ci` file says what a project's code is made of, which checks protect it
and how it is released. A person new to the project reads it and knows what
runs when they change a file; an agent writes it correctly on the first try.
Citrus evaluates the files into a graph and executes the graph; every step
knows the line it came from.

## Principles

1. **Every element does something.** No keyword, field or symbol exists for
   decoration. One concept has one construct; two names for the same
   behaviour are one name too many.
2. **Names and comments explain.** A comment is `#`. A comment directly
   above a declaration is its description: `citrus`, `why` and the dashboard
   show it. There is no `about` field and no special doc-comment syntax.
3. **Symbols carry meaning.** Declared names and references to them are
   bare (`check bot`, `profile = e2e`, `needs = [database]`). Data is quoted:
   paths, package names, values (`"crates/**"`, `crate("clyer")`). `=` gives a
   field its value, `{ }` is a block, `( )` is a call.
4. **A bare name always refers to a declaration.** `citrus check` rejects
   an unknown one and suggests the closest.
5. **Paths are derived, not listed.** `crate("clyer")` is the Rust crate
   and every workspace crate it depends on; `next("@acme/clyer")` is the
   Next.js app and the workspace packages it uses. The function says what
   the thing is. A change in a shared library reaches every product built from
   it without anyone listing it.
6. **Mechanics stay in Citrus.** A file holds what is particular to the
   project, never how Citrus talks to its tools.
7. **Describe, don't execute.** Reading the files runs nothing. The same
   files and repository give the same graph.
8. **Small.** No classes, exceptions, mutation, network or clock at
   evaluation time. `let`, `fn`, `for` and `if` exist for the rare file that
   needs them.

## Files

- A small project: `citrus.ci` at the repository root.
- A larger one: the `.citrus/` directory. Citrus reads every `.citrus/*.ci`;
  their order does not matter, because every declaration has a name. By
  convention `.citrus/project.ci` holds the shared settings and each product
  has its own file. Having both a root `citrus.ci` and `.citrus/` is an
  error.
- Every file starts with `citrus 1`, the language version.
- Declarations are visible in the whole project; declaring a name twice is
  an error that shows both places. `let` and `fn` are local to their file,
  except those in `.citrus/project.ci` (or the root `citrus.ci`), which every
  file sees.
- Citrus keeps no state in the working tree: runs, logs and receipts live in
  the Git directory (`.git/citrus/`).

## Values

```
"text"                       # string; "{expr}" interpolates, "{{" is a literal brace
3, 30s, 5m, 2h               # integers and durations
true, false, none
["a", "b"]                   # list
{ key: "value" }             # map
crate("clyer") + ["x/**"]    # lists join with +
["scripts/**"] - tools       # paths minus paths: the right ones excluded
```

Operators: `+ - * / %`, `== != < <= > >=`, `and or not`, `in`, `??`.

## Declarations

A declaration is `kind name`, optionally `= value`, optionally `{ fields }`.
Names are identifiers (`backend`, `web-host`); a name built from a loop
variable is a string (`group "{vpn}"`).

### `project`

```
project {
  main = "origin/main"   # what "changed" is measured against
  runner = builders      # where checks run unless they say otherwise
  # Versions a registry already has (docs/releases.md#versions).
  free_version = run("scripts/registry.sh", "free-version")
}
```

### `profile`

```
# Fast checks never touch a database: SQLx reads the committed metadata.
profile fast {
  env {
    SQLX_OFFLINE = "true"
    DATABASE_URL = ""
  }
}

# Slow checks against real services.
profile e2e
```

The first profile is the default; `citrus run --profile e2e` picks another.
A check without `profile` belongs to every profile.

### `group` and `check`

```
# Clyer bot, its Mini App and TON contracts.
group clyer {
  paths = crate("clyer") + next("@garvis/clyer") + ["migrations/clyer/**"]

  check bot = cargo.test("clyer")
  check web = pnpm.test("@garvis/clyer")

  # Runs against a real database, so only in the slow profile.
  check database = make("test-clyerbot-db-e2e") {
    profile = e2e
    needs = [database]
  }
}

# Reviewed security paths are exact files, so a new sibling is noticed.
check host-firewall = make("test-host-management-firewall") {
  paths = ["deploy/k3s/hosts/*management-firewall*"]
}
```

- A **group** is a set of paths and the checks that protect it. A check in a
  group runs when a changed path is in the group's paths, or in its own
  `paths` when it narrows them. It is called `clyer.bot`.
- A **check** is `check name = action`. Fields go in `{ }` only when there
  are any; a check outside a group needs `paths`.
- A path no group or check claims is reported as unclaimed; strict plans
  stop on it. A group without checks (documentation) claims its paths and
  runs nothing.
- `paths` take globs: `*` and `?` stay in one directory, `**` crosses them,
  `!glob` excludes (the last matching glob decides).

Check fields:

| Field | Meaning |
|---|---|
| `paths` | narrows the group's paths, or gives a lone check its paths; a group named in it (`[platform, "x/**"]`) selects the check too |
| `reads` | more inputs: they invalidate a cached pass but do not select the check |
| `profile` | the profile it belongs to |
| `needs` | services it needs (below) |
| `env { }` | environment of its steps |
| `covers = [x, y]` | it runs `x` and `y` itself: they are not planned beside it |
| `replaces = [x, y]` | when the change goes beyond what one of them owns, it runs instead of them |
| `cache = false` | never reuse a pass (checks that depend on the outside world) |

A check whose inputs are known (derived paths, globs, `reads`) reuses a
pass while they are unchanged; that is the default.

### `match changed`

When a check's command depends on the whole change, `match` picks it:

```
check pipeline = match changed {
  only(clyer) => make("test-clyer-pipeline-contract")
  without(clyer) => make("test-main-pipeline-contract")
  _ => make("test-pipeline-contract")
}
```

`only(g)`: every changed product path is in `g`; `without(g)`: none is;
`touched(g)`: some is. The first arm that holds is the check's command for
this plan.

### `service`

```
# PostgreSQL for the e2e checks; one per run, shared by the checks that need it.
service database = compose.up("postgres") {
  ready = wait.tcp("localhost:5432")
}

# Headless Chromium; at most two at a time.
service browser {
  limit = 2
}
```

With an action, a local run starts the service once, before the first check
that needs it, and waits for `ready`; the service keeps running after the
run. Without one, it is a resource the runner provides, and `limit` caps how
many checks hold it.

### `runner`

```
# The shared Linux builders.
runner builders = make("check-remote-suite") {
  status = make("workflow-status")
}
```

A runner executes the planned checks elsewhere. It is given the checks to
run (`CITRUS_TARGETS`) and their declarations (`CITRUS_CHECKS`) and reports
each one in the Citrus protocol (docs/protocol.md).

### `release`, `artifact`, `environment`

```
# Clyer bot backend and migrations to k3s.
release clyer {
  environment = clyer-production
  # Held for this commit and these images (docs/releases.md#versions).
  version {
    initial = "0.1.0-clyer"
    scope = ["clyer-backend", "clyer-migrations"]
  }
  step prepare = make("release-prepare-clyer", RELEASE: version)
  step deploy = make("clyer-k3s-release-deploy", RELEASE: version) {
    production = true
    # An interrupted rollout is reconciled, never blindly repeated.
    recover = make("clyer-k3s-release-recover", RELEASE: version)
  }
  rollback = make("clyer-k3s-release-rollback", RELEASE: previous) {
    production = true
  }
}

# Clyer in the production cluster.
environment clyer-production = kubernetes(context: "prod", namespace: "clyer") {
  deploy clyerbot-backend = clyer-backend
}
```

docs/releases.md and docs/design/declarative.md describe them.

### `task` and `commands`

```
# Start the database and apply migrations.
task seed-db = [compose.up("db"), wait.tcp("localhost:5432", timeout: 60s), make("migrate-dev")]

commands release {
  "make verify" = "Validate a release candidate"
}
```

`citrus do seed-db` runs a task. `commands` lists the project's own commands
for people (`citrus` shows them), under a heading when it has a name.

## Actions

Values describing work; Citrus executes them, each with its source line.

| Action | Meaning |
|---|---|
| `run(program, args…)` | a program, without a shell |
| `make(target, VAR: value…)` | `make target` |
| `cargo.test(pkg)`, `cargo.build(…)`, `cargo.fmt(check:)`, `cargo.clippy(deny:)`, `cargo.run(args…)` | Cargo; `citrus check` points a hand-written `run("cargo", …)` at them |
| `pnpm.test(app)`, `pnpm.build(app)` | pnpm workspace scripts |
| `sh("…")` | a shell command; flagged as not portable |
| `compose.up(service)`, `compose.down()` | Docker Compose |
| `wait.tcp(addr)`, `wait.http(url)`, `wait.file(path)` | readiness |
| `copy(from, to)`, `links.check(glob)` | files |
| `kubernetes(namespace, context:)`, `job(manifest)`, `lease(name)` | releases |

## Paths

| Function | Paths |
|---|---|
| `crate("pkg", …)` | Rust crates and every workspace crate they reach through path dependencies, files they include (`include_str!`, `sqlx::migrate!`), `Cargo.toml`, `Cargo.lock` and workspace settings; `"platform-*"` names several |
| `next("@scope/app", …)` | a Next.js app of the pnpm workspace (an error if it does not depend on `next`): its directory, the workspace packages it uses, the lockfile and workspace manifests |
| `package("@scope/lib", …)` | any pnpm workspace package, the same way |
| `glob("pattern")` | the matching files, for comprehensions |

## Evaluation and execution

1. Citrus reads the files, evaluates them and builds the graph: groups,
   checks, services, runners, releases, each with its source span.
2. `citrus check` validates it before anything runs: unknown names, globs
   that match nothing, hand-written commands that have a built-in.
3. `citrus plan` maps the changed paths to checks; `run`, `do`, `release`
   and `apply` execute parts of the graph and record each step.
4. `citrus fmt` writes the canonical layout: two-space indent, one space
   around `=`, no aligned columns.

## Non-goals

- General-purpose programming: what needs more than this is an action or a
  provider written in Citrus, with tests.
- Running anything while files are read.
- Compatibility before 1.0.
