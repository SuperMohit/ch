#!/usr/bin/env bash
# Prints one section of CHANGELOG.md without its `## ` heading and without `### `
# headings that have nothing under them. The release workflow uses it for the GitHub
# release notes, and scripts/release.sh for the notes it moves under the new version.
#
#   scripts/changelog-notes.sh 0.2.0
#   scripts/changelog-notes.sh Unreleased
#
# Exits 1 if the section doesn't exist.
set -euo pipefail

section=${1:?usage: scripts/changelog-notes.sh <version|Unreleased> [CHANGELOG.md]}
file=${2:-CHANGELOG.md}

awk -v want="$section" '
    $1 == "##" {
        if (found) exit
        # `## 0.2.0 (2026-10-13)` or `## Unreleased`
        if ($2 == want) { found = 1; next }
    }
    found { line[++n] = $0 }
    END {
        if (!found) exit 1
        m = 0
        for (i = 1; i <= n; i++) {
            if (line[i] ~ /^### /) {
                for (j = i + 1; j <= n && line[j] !~ /^### /; j++)
                    if (line[j] ~ /[^[:space:]]/) break
                # Nothing before the next heading or the end: drop the heading and its
                # blank lines.
                if (j > n || line[j] ~ /^### /) { i = j - 1; continue }
            }
            out[++m] = line[i]
        }
        first = 1; last = m
        while (first <= m && out[first] !~ /[^[:space:]]/) first++
        while (last >= first && out[last] !~ /[^[:space:]]/) last--
        for (i = first; i <= last; i++) print out[i]
    }
' "$file"
