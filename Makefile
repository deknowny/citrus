# Development commands. `citrus run` picks the ones a change needs.
.PHONY: test lint fmt release

test:
	cargo test --locked

lint:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings

fmt:
	cargo fmt

# make release VERSION=v0.3.0 — tag a green main commit and publish binaries.
release:
	./scripts/release.sh "$(VERSION)"

.PHONY: docs doctor
# Relative links in Markdown files point at existing files.
docs:
	./scripts/check-links.sh

# This repository's own Citrus configuration is valid.
doctor:
	cargo run --quiet --locked -- doctor --text
