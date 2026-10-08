# Citrus's own checks on a pool agent: the Rust toolchain, with Cargo's
# registry and target directory in the agent's cache (/citrus-cache), so a
# change rebuilds incrementally in seconds. The tests commit to throwaway
# repositories: Git needs an identity.
FROM rust:1-bookworm
RUN rustup component add clippy rustfmt \
 && git config --system user.name ci \
 && git config --system user.email ci@example.invalid \
 && git config --system init.defaultBranch main
ENV CARGO_HOME=/citrus-cache/cargo \
    CARGO_TARGET_DIR=/citrus-cache/target
