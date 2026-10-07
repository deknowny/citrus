# Checks and tasks live in citrus.ci (`citrus run`, `citrus do`).
.PHONY: release

# make release VERSION=v0.3.0 — tag a green main commit and publish binaries.
release:
	./scripts/release.sh "$(VERSION)"
