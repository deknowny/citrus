# Design: declarative releases (`citrus diff` / `citrus apply`)

Status: implemented for the `kubernetes` provider — `citrus artifacts`, `citrus diff`, `citrus apply` (builds by input key with the `docker` and `command` build providers, quiesce, migration Job, roll by digest with records, lease fence, verify). Issue #2. Releases today are imperative steps — run these
commands in order. This design replaces them with desired state: what each
environment should run, observed against what it runs, reconciled by one
generic mechanism.

## Why

The first consumer (a monorepo with several products on k3s) carries about
12,000 lines of release code. Read side by side, most of it is the same
handful of mechanisms written again per product:

| Mechanism | How often it is re-implemented today |
|---|---|
| One release at a time (a lock) | in the shared release script and in each of 15 component release scripts |
| Build from the committed tree only, keyed by its content | per product |
| Image identity by registry digest, never by tag | per product |
| Plan → reviewed diff → apply exactly that plan (`PLAN_SHA`) | per component |
| Immutable release records and "previous release" for rollback | per product, stored in different places |
| Migrations before workloads, forward-only, compatible with the old version | per product |
| Recovery after an interrupted rollout (observe what runs, continue) | a separate `recover` path per product |
| Postcheck: digests running, health, the changed scenario | per product |

What is genuinely specific is smaller: Kubernetes manifests, Dockerfiles,
migration SQL, one-off data cutovers, and a few invariants such as "the
Telegram userbot runs once: stop the old one before the new one starts".
Those invariants turn out to be generic options with parameters (below).

## Model

Artifacts and environments, next to the checks in the configuration
(fields as implemented; `strategy` and verify-by-check are still planned):

```
# What is built, from what. Key = hash of the inputs + this declaration.
artifact api {
  inputs = ["crates/api/**", "Cargo.lock", "Dockerfile"]
  # A shared Dockerfile counts only with the stages `runtime` is built from.
  dockerfile = { file: "Dockerfile", target: "runtime" }
  build = { provider: "docker", dockerfile: "Dockerfile", target: "runtime" }
  publish = { registry: "registry.example.com/shop/api" }    # identity = pushed digest
}

artifact api-migrations {
  inputs = ["migrations/api/**", "Dockerfile.migrations"]
  build = { provider: "docker", dockerfile: "Dockerfile.migrations" }
  publish = { registry: "registry.example.com/shop/api-migrations" }
}

# What runs where. Credentials are referenced, never stored.
environment shop-production = kubernetes(context: "prod", namespace: "shop") {
  approval = required             # apply needs --approve
  checks = proven                 # planned checks must be proven for the commit
  # Runs to completion before workloads change.
  migrations = { artifact: "api-migrations", job: "deploy/migrate.yaml", timeout: 5m }
  record = { annotation: "example.com/release" }

  deploy api = api
  # Never two at once: wait until the old one released its lease.
  deploy bot = bot {
    fence = "bot-session"
  }
  # Suspended while the environment changes, restored after.
  deploy backup = backup {
    kind = cronjob
    quiesce = true
  }

  verify = { http: ["https://shop.example.com/health"] }
}
```

Values never hold secrets; they name where the provider finds them.

## Lifecycle

**`citrus diff <env>`** computes a plan and prints it:

1. *Desired:* for HEAD, each artifact's input key → a known digest (built
   before from the same inputs) or "needs build".
2. *Observed:* the provider reports what runs — digest per workload, the
   recorded release, pending migrations.
3. *Plan:* builds, publishes, migrations and per-workload changes, with the
   strategy each one uses, and a plan hash. Nothing runs.

**`citrus apply <env> [--plan HASH] --approve`** executes that plan (or refuses
if the plan changed since `--plan` was reviewed):

1. lock the environment; check the gates (committed tree, proven checks, approval);
2. build missing artifacts by key, publish, record digests;
3. quiesce, run migrations, change workloads by strategy and fence, resume;
4. verify (provider readiness, health, postcheck targets);
5. record the release (commit, digests, plan hash) — in Citrus state and, when
   the provider supports it, in the environment itself so other machines see it.

**After an interruption** the same `apply` is run again. It observes first,
so finished work is not repeated and an unknown step resolves itself by what
the environment actually runs. There are no per-product `recover` commands.

**Rollback** is `apply` with desired = the previous release record.
**History** is the list of records.

## Providers

The core knows artifacts, environments, plans, locks, records and gates.
Everything platform-specific is a provider:

| Provider | observe | apply | verify |
|---|---|---|---|
| `kubernetes` (built in, via kubectl) | workload images by digest, lease holders, Job status, release record ConfigMap | set image by digest; recreate/rolling; fence on Lease; suspend/resume CronJobs; migration Job | rollout status, readiness, HTTP |
| `compose` (built in) | `docker compose ps` images | `up -d` per service | health |
| `github-release` (built in) | assets of the tag | create release, upload, SHA256SUMS | checksums |
| `command` (built in) | any program speaking the provider protocol | | |

The **provider protocol** is a process boundary: the provider is a program
receiving a JSON request on stdin (`observe`, `apply`, `verify`, `quiesce`,
`resume`) and answering JSON on stdout. Project-specific behaviour — a data
cutover, a network controller — is a small provider or a hook, not a change
in Citrus.

Argo CD and Flux already reconcile Kubernetes well. Where a team uses them,
an `argocd` provider sets the desired revision and waits for sync; Citrus is
not a second in-cluster controller.

## Fit with real consumers

| Consumer | Fits as | Stays outside Citrus |
|---|---|---|
| Clyer (k3s, backend + migrations) | two artifacts; environment with a migration job before a `recreate` deployment; verify by status + health | Kubernetes manifests |
| Garvis (backend, userbot, webapp, backup) | four artifacts; userbot `recreate` + `fence = lease`; backup CronJob `quiesce`; release records in a ConfigMap | one-time secret cutovers (a provider hook), host aliases (manifests) |
| MT3S components (15 release scripts) | artifact + environment each; their reviewed-plan flow is exactly `diff` + `apply --plan` | network controllers, BGP and listener management — not deployments |
| Citrus itself | three binary artifacts; environment `github-release` | — |

Two of these are deliberately unlike each other (a Kubernetes product and a
GitHub release of a CLI): the model is accepted only if both work without
special cases in the core.

## Non-goals

- General-purpose logic in declarations. When one needs more than the `.ci`
  language offers, it needs a provider or a hook.
- Secret management. Environments reference credentials; they never contain them.
- Replacing in-cluster GitOps controllers.
- Building arbitrary software. Builds are provider calls (`docker`, `command`);
  Citrus decides *whether* to build (by input key), not *how*.

## Path from the imperative releases

1. `citrus diff` for one product while its old release path stays — compare
   plans with what the old path does on every release.
2. Before deleting old code, write its guarantees as provider tests:
   digest pinning, migrations before workloads, recreate + fence, quiesce and
   restore, refusal on a changed plan, recovery by observation.
3. `citrus apply` for that product; delete its part of the old release code.
4. Next product. Citrus releasing itself through `github-release` comes early,
   as the second, different consumer.

Progress is measured in deleted lines of release code and in releases that
needed a human to recover.

## Open questions

- Build location: local, a runner, or CI — artifacts are keyed by inputs, so
  any builder can produce them; who builds is a runner concern (#9).
- Release record location for teams without a shared environment store.
- Partial plans: releasing one workload of an environment on purpose.
