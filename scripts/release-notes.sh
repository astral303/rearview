#!/usr/bin/env bash
# Write a tag's GitHub release notes from its CHANGELOG.md section.
# Usage: scripts/release-notes.sh <tag> [output-file]
set -euo pipefail

tag="${1:?usage: scripts/release-notes.sh <tag> [output-file]}"
output="${2:-}"
version="${tag#v}"

changelog="$(git show "$tag:CHANGELOG.md" | tr -d '\r')"

# The lines after the first heading that starts with $1, up to the next
# second-level heading outside a fenced code block. The heading itself is
# skipped: the release title repeats it.
section_after() {
  awk -v prefix="$1" '
    !found && index($0, prefix) == 1 { found = 1; next }
    !found { next }
    /^[[:space:]]*(```|~~~)/ { fenced = !fenced }
    !fenced && /^## / { exit }
    { print }
  ' <<< "$changelog"
}

# Raise each heading outside a fenced code block a level, beside GitHub's
# generated `## What's Changed`.
raise_headings() {
  awk '
    /^[[:space:]]*(```|~~~)/ { fenced = !fenced }
    !fenced && /^###+ / { sub(/^#/, "") }
    { print }
  '
}

# A release candidate takes the Unreleased section at its tag; only a final
# release's `cargo release` retitles that section.
notes="$(section_after "## v$version (")"
if [ -z "$notes" ] && [[ "$tag" == *-* ]]; then
  notes="$(section_after "## Unreleased")"
fi
if ! grep -qE '^[[:space:]]*([-*+]|[0-9]+\.) ' <<< "$notes"; then
  if [[ "$tag" == *-* ]]; then
    echo "CHANGELOG.md at $tag has no entries under '## v$version (' or '## Unreleased'." >&2
  else
    echo "CHANGELOG.md at $tag has no entries under '## v$version ('; a final release needs its own section." >&2
  fi
  exit 1
fi

# Join wrapped lines: GitHub renders a single newline in a release body as a
# line break.
body="$(
  raise_headings <<< "$notes" |
    panache --flavor gfm --isolated -q format -o wrap=reflow -o line-width=9999 -
)"

if [ -n "$output" ]; then
  printf '%s\n' "$body" > "$output"
else
  printf '%s\n' "$body"
fi
