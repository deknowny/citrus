# Checks, fingerprints and receipts

How checks are declared is in [configuration.md](configuration.md). This
page is the contract other tools rely on: paths, the `CITRUS_CHECKS` file,
the input fingerprint and receipts.

## Paths

- Check names: `[a-z0-9][a-z0-9._-]*`; a check in a group is `group.name`.
- Globs are repository-relative: `*` and `?` stay inside one path segment,
  `**` matches anything, `**/` matches zero or more whole directories.
  `dir/**` also matches `dir` itself (a gitlink, or a directory removed in a
  diff).
  Absolute paths and `..` are rejected; a glob matching no file is reported.
- `!glob` excludes: in each of `paths` and `reads`, the last glob that
  matches a path decides, as in `.gitignore`
  (`["crates/backend/**", "!crates/backend/src/bots/clyer/**"]`). Tools
  computing the fingerprint apply the same rule when they select files.
- A pass is reused while `paths` + `reads` are unchanged, so they must list
  **everything** the check reads; `crate("pkg")`, `next("@app")` and
  `package("@lib")` derive that from the workspace. A check that depends on
  the outside world says `cache = false`.
- Editing a check's declaration selects it on the next plan.

## For the project's own tools: `CITRUS_CHECKS`

The runner runs with `CITRUS_CHECKS` set to a JSON file; `citrus targets --json` prints the same fields:

```json
{"toolchain": ["rust-toolchain.toml"], "files": ["citrus.ci"],
 "checks": [{"target": "api.unit", "description": "API unit tests", "cache": true,
             "inputs": ["api/**"], "extra_inputs": ["Cargo.lock"], "resources": ["database"],
             "meta": {"linux": true}, "source": ".citrus/api.ci:3",
             "declaration": {"env": {"SQLX_OFFLINE": "true"}, "extra_inputs": ["Cargo.lock"],
                             "inputs": ["api/**"], "run": [["run", "make", "--no-print-directory", "test-api"]],
                             "target": "api.unit"}}]}
```

`inputs` are `paths`, `extra_inputs` are `reads`, `resources` are the
services in `needs`, `arms` the `match changed` arms with what each runs. `run` is what the check runs: for a `match changed`
check, the arm the plan chose (`CITRUS_CHECKS` of a run), otherwise `_`.

## Input fingerprint

The receipt key of a check is SHA-256 over:

1. its `declaration` as JSON text with keys sorted, `", "` and `": "`
   separators and non-ASCII escaped as `\uXXXX` (Python's
   `json.dumps(declaration, sort_keys=True)`). `run` lists each step as
   `[kind, arguments…]`: `["run", program, args…]` for commands,
   `["wait.tcp", address, seconds]`, `["wait.http", url, seconds]`,
   `["wait.file", path, seconds]`, `["copy", from, to]`;
2. for each repository file matching any glob (tracked and untracked
   non-ignored files, sorted by path), then for each `toolchain` entry in
   order:
   - `missing:<path>\0` if the file does not exist, otherwise
   - `<path>\0x\0` or `<path>\0-\0` (executable for the current user or not)
     followed by the 32-byte SHA-256 of the file content.

The hex digest is the fingerprint.

## Receipts

A PASS of a check is a file `<receipts>/<target>-<fingerprint>.pass`:

```
target=<name>
fingerprint=<hex>
result=PASS
completed_epoch=<unix seconds>
```

A receipt counts only if it is a regular file (not a symlink), owned by the
current user, not group- or world-writable, and younger than seven days.
Other tools of a project may write and read the same receipts by following
this format.
