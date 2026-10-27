#!/bin/sh
# Print the CHANGELOG.md section of one version, for the release page.
# Usage: scripts/release-notes.sh 0.1.0
set -eu
version=${1:?usage: scripts/release-notes.sh VERSION}
notes=$(awk -v version="$version" '
    /^## / { if (found) exit; if (index($0, "## " version " ") == 1) { found = 1; next } }
    found { print }
' CHANGELOG.md)
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    echo "CHANGELOG.md has no section for $version (expected a heading like \"## $version (2026-11-02)\")" >&2
    exit 1
fi
printf '%s\n' "$notes"
