#!/usr/bin/env bash
# Fail when a Markdown file links to a repository path that does not exist.
set -euo pipefail
status=0
while IFS= read -r file; do
  dir="$(dirname "$file")"
  while IFS= read -r link; do
    target="${link%%#*}"
    [[ -z "$target" || "$target" == http* || "$target" == mailto:* ]] && continue
    if [[ ! -e "$dir/$target" ]]; then
      echo "$file: broken link $link" >&2
      status=1
    fi
  done < <(grep -oE '\]\([^)]+\)' "$file" | sed -E 's/^\]\(//; s/\)$//')
done < <(git ls-files '*.md')
[[ $status == 0 ]] && echo "links ok"
exit $status
