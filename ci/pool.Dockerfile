# Citrus's own checks on a pool agent: the Rust toolchain, with Cargo's
# registry and target directory in the agent's cache (/citrus-cache), so a
# change rebuilds incrementally in seconds. The target directory is named
# `cargo-target` so the agent gives each slot its own (`slots::target_dirs`):
# a shared one let a slot's build replace another's binary mid-check (the
# `test` and `pool` checks build inside it). The tests commit to throwaway
# repositories: Git needs an identity. Their Postgres is a throwaway
# container: the Docker CLI talks to the agent's daemon (its socket is mounted,
# the network is the host's).
FROM docker:29-cli AS docker
FROM rust:1-bookworm
COPY --from=docker /usr/local/bin/docker /usr/local/bin/docker
RUN rustup component add clippy rustfmt \
 && git config --system user.name ci \
 && git config --system user.email ci@example.invalid \
 && git config --system init.defaultBranch main
ENV CARGO_HOME=/citrus-cache/cargo \
    CARGO_TARGET_DIR=/citrus-cache/cargo-target
