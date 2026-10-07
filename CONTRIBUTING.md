# Contributing

## Development loop

Citrus checks itself with Citrus:

```sh
cargo build
./target/debug/citrus status     # which of `test` / `lint` your change needs
./target/debug/citrus run        # runs only what is not proven yet
./target/debug/citrus log last   # first error if something failed
```

Plain `make test` and `make lint` work too. CI runs both on Linux and macOS.

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
3. `make release VERSION=vX.Y.Z` — tags the commit after its CI is green and
   waits for the binaries and `SHA256SUMS`.

## Compatibility

`citrus/v1` JSON output, the receipt format (`docs/manifest.md`) and
`citrus.toml` keys are public contracts: add, do not change meaning.
