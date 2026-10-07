# Contributing

## Development loop

Citrus checks itself with Citrus:

```sh
cargo build
./target/debug/citrus status     # which of `test` / `lint` your change needs
./target/debug/citrus run        # runs only what is not proven yet
./target/debug/citrus log last   # first error if something failed
```

The checks are declared in [`citrus.ci`](citrus.ci); `./target/debug/citrus do fmt`
formats the code. CI runs the same cargo commands on Linux and macOS.

## Trying a change in a real repository

Build Citrus and run it from the other repository, before pushing anything:

```sh
cargo build --manifest-path /path/to/citrus/Cargo.toml
cd /path/to/your-repo && /path/to/citrus/target/debug/citrus status
```

Launchers that pin Citrus (see the consumer section of the README) usually
accept a local source tree for this, so only your own session uses it.

## Releases

Releases are for people installing binaries. A repository can also pin any
green `main` commit and build it with
`cargo install --git https://github.com/deknowny/citrus --rev <sha> --locked`.

1. Update `version` in `Cargo.toml` and add a `## X.Y.Z` entry to `CHANGELOG.md`.
2. Push to `main` and let CI pass.
3. `citrus run`, then `citrus release start github --version X.Y.Z --approve`
   (the `release "github"` block in `citrus.ci`): checks the commit is on
   `origin/main` with that version and notes, tags it and waits for the
   binaries and `SHA256SUMS`.

## Compatibility

Before 1.0 the `.ci` language and the CLI may change incompatibly: no
compatibility shims for old formats. Record every breaking change in
`CHANGELOG.md`. The receipt and `CITRUS_CHECKS` formats are documented in
`docs/manifest.md`; change them together with that page.
