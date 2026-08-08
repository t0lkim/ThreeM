#!/usr/bin/env bash
#
# Print one version's section of CHANGELOG.md, for use as a GitHub Release body.
#
#   .github/scripts/changelog-section.sh 0.2.0 [CHANGELOG.md]
#
# Exits non-zero if the version has no section, or has one with nothing in it.
# That is the point: the release workflow calls this before it builds anything,
# so a tag pushed against a changelog that was never updated fails in the first
# job rather than publishing binaries with an empty body.
#
# This lives in a script rather than inline in the workflow so it can be run —
# and read — locally, against the real file, before a tag exists.

set -euo pipefail

version="${1:?usage: changelog-section.sh <version> [changelog]}"
changelog="${2:-CHANGELOG.md}"

if [ ! -f "$changelog" ]; then
    echo "changelog-section: no such file: $changelog" >&2
    exit 1
fi

# A section runs from `## [<version>]` to the next `## [` heading. The version
# is compared as the exact text between the brackets, not as a regex match on
# the line, so `## [0.2.0] — 2026-08-09` and a bare `## [0.2.0]` both hit and
# `0.2` does not match `0.2.0`.
#
# Two things are dropped on the way out. Link-reference definitions
# (`[0.2.0]: https://…`) sit at the foot of the file and belong to the document,
# not to any one release — and because they come after the last section, they
# would otherwise be swept into it. Blank lines are buffered and only emitted
# once a non-blank line follows, which trims the leading and trailing padding
# around the section without a second pass.
body=$(
    awk -v want="$version" '
        /^## \[/ {
            token = $0
            sub(/^## \[/, "", token)
            sub(/\].*$/, "", token)
            if (token == want) { inside = 1; next }
            if (inside) { exit }
            next
        }
        !inside { next }
        /^\[[^]]+\]: / { next }
        /^[[:space:]]*$/ { if (started) pending++; next }
        {
            started = 1
            while (pending-- > 0) print ""
            pending = 0
            print
        }
    ' "$changelog"
)

if [ -z "$body" ]; then
    echo "changelog-section: $changelog has no non-empty '## [$version]' section" >&2
    echo "changelog-section: sections present:" >&2
    grep -n '^## \[' "$changelog" >&2 || true
    exit 1
fi

printf '%s\n' "$body"
