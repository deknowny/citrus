# Citrus's own checks on a pool agent: the Rust toolchain of Cargo.lock's
# era, with Cargo's registry and target directory in the agent's cache
# (/citrus-cache), so a change rebuilds incrementally in seconds.
FROM rust:1-bookworm
RUN rustup component add clippy rustfmt
ENV CARGO_HOME=/citrus-cache/cargo \
    CARGO_TARGET_DIR=/citrus-cache/target
