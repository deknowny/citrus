# Declared checks

```
check "test-api" {
  about = "API unit tests"          # optional, shown by `citrus why`
  owns = ["api/**"]                 # files this check owns: changing one selects it
  reads = ["Cargo.lock"]            # files it only reads: invalidate reuse, do not select
  run = make("test-api")            # one action or a list of them
  cache = true                      # reuse a PASS while all of the above is unchanged
  resources = ["contracts"]         # resource classes for the project's scheduler
  env = { SQLX_OFFLINE: "true" }    # extra environment of its steps
  meta = { linux: true }            # data for the project's own tools, kept as written
}
```

- Names: `[a-z0-9][a-z0-9._-]*`.
- Globs are repository-relative: `*` and `?` stay inside one path segment,
  `**` matches anything, `**/` matches zero or more whole directories.
  Absolute paths and `..` are rejected; a glob matching no file is reported.
- `owns` must be non-empty and `run` must name something to run.
- Set `cache = true` only when `owns` + `reads` list **everything** the check
  reads. An undeclared input makes a reused PASS false.
- Editing a check's declaration selects it on the next plan.

## For the project's own tools: `CITRUS_CHECKS`

A planner or pool declared in `citrus.ci` runs with `CITRUS_CHECKS` set to a
JSON file; `citrus targets --json` prints the same fields:

```json
{"toolchain": ["rust-toolchain.toml"], "files": ["citrus.ci"],
 "checks": [{"target": "test-api", "description": "API unit tests", "cache": true,
             "inputs": ["api/**"], "extra_inputs": ["Cargo.lock"], "resources": ["contracts"],
             "meta": {"linux": true}, "source": "citrus.ci:3",
             "declaration": {"env": {"SQLX_OFFLINE": "true"}, "extra_inputs": ["Cargo.lock"],
                             "inputs": ["api/**"], "run": [["run", "make", "--no-print-directory", "test-api"]],
                             "target": "test-api"}}]}
```

## Input fingerprint

The receipt key of a declared check is SHA-256 over:

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

A PASS of a declared check is a file `<receipts>/<target>-<fingerprint>.pass`:

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
