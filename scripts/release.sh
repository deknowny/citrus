#!/usr/bin/env bash
# Tag the current main commit as VERSION once its CI is green, then wait for
# the release workflow to publish binaries and SHA256SUMS.
set -euo pipefail
version="${1:-}"
[[ "$version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: make release VERSION=vX.Y.Z" >&2; exit 2; }
[[ "$(git rev-parse --abbrev-ref HEAD)" == main ]] || { echo "release from main" >&2; exit 2; }
[[ -z "$(git status --porcelain)" ]] || { echo "working tree is not clean" >&2; exit 2; }
git fetch -q origin main
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || { echo "push main first" >&2; exit 2; }
crate="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)"
[[ "v$crate" == "$version" ]] || { echo "Cargo.toml says $crate, not ${version#v}" >&2; exit 2; }
grep -q "^## ${version#v} " CHANGELOG.md || { echo "CHANGELOG.md has no ## ${version#v} entry" >&2; exit 2; }
commit="$(git rev-parse HEAD)"
echo "waiting for CI on $commit…" >&2
run="$(gh run list --commit "$commit" --workflow ci.yml --limit 1 --json databaseId -q '.[0].databaseId')"
[[ -n "$run" ]] || { echo "no CI run for $commit" >&2; exit 1; }
gh run watch "$run" --exit-status --interval 15 >/dev/null
git tag -a "$version" -m "Citrus $version"
git push -q origin "$version"
sleep 5
release="$(gh run list --workflow release.yml --limit 1 --json databaseId,headBranch -q ".[] | select(.headBranch == \"$version\") | .databaseId")"
gh run watch "$release" --exit-status --interval 20 >/dev/null
echo "released $version ($commit)"
gh release view "$version" --json assets -q '.assets[].name'
