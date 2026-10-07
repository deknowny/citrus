# Declared checks: `ci/targets.toml`

```toml
[targets.test-api]
description = "API unit tests"   # optional, shown by `citrus why`
cache = true                     # reuse a PASS while inputs are unchanged
inputs = ["api/**"]              # files this check owns: changing one selects it
extra_inputs = ["Cargo.lock"]    # files it only reads: invalidate reuse, do not select
```

- Target names: `[a-z0-9][a-z0-9._-]*`; by default each is a Make target.
- Globs are repository-relative: `*` and `?` stay inside one path segment,
  `**` matches anything, `**/` matches zero or more whole directories.
  Absolute paths and `..` are rejected.
- `inputs` must be non-empty; `cache` must be a boolean.
- Other keys (for example scheduling hints of your own planner) are kept and
  shown by `citrus why`, never rejected. `resources` is read as a list of names.
- Set `cache = true` only when `inputs` + `extra_inputs` list **everything**
  the check reads. An undeclared input makes a reused PASS false.

## Input fingerprint

The receipt key of a declared check is SHA-256 over:

1. the JSON text `{"extra_inputs": [...], "inputs": [...], "target": "<name>"}`
   with keys sorted, `", "` and `": "` separators and non-ASCII escaped as
   `\uXXXX` (Python's `json.dumps(..., sort_keys=True)`);
2. for each repository file matching any glob (tracked and untracked
   non-ignored files, sorted by path), then for each `toolchain_files` entry
   in configured order:
   - `missing:<path>\0` if the file does not exist, otherwise
   - `<path>\0x\0` or `<path>\0-\0` (executable for the current user or not)
     followed by the 32-byte SHA-256 of the file content.

The hex digest is the fingerprint.

## Receipts

A PASS of a declared check is a file `<receipts.dir>/<target>-<fingerprint>.pass`:

```
target=<name>
fingerprint=<hex>
result=PASS
completed_epoch=<unix seconds>
```

A receipt counts only if it is a regular file (not a symlink), owned by the
current user, not group- or world-writable, and younger than
`receipts.max_age_days`. Other tools of a project may write and read the same
receipts by following this format.
