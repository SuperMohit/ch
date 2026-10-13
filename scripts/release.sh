#!/usr/bin/env bash
# Cuts a release locally: moves the notes under `## Unreleased` in CHANGELOG.md to a new
# `## <version> (<date>)` section, sets the workspace version in Cargo.toml, then commits
# and tags. Nothing is pushed; pushing the tag starts .github/workflows/release.yml.
#
#   scripts/release.sh 0.2.0        (or `just release 0.2.0`)
#
# CARGO_CMD overrides the cargo command used to refresh Cargo.lock (the justfile sets it).
set -euo pipefail

version=${1:?usage: scripts/release.sh <version>, for example 0.2.0}
tag="v$version"
cargo_cmd=${CARGO_CMD:-cargo}

die() {
    echo "release: $*" >&2
    exit 1
}

cd "$(git rev-parse --show-toplevel)"

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
    die "'$version' is not a version like 1.2.3 or 1.2.3-rc.1"
[[ $(git branch --show-current) == main ]] || die "releases are cut from main"
[[ -z $(git status --porcelain) ]] || die "commit or stash your changes first"
! git rev-parse -q --verify "refs/tags/$tag" >/dev/null || die "tag $tag already exists"
! scripts/changelog-notes.sh "$version" >/dev/null 2>&1 ||
    die "CHANGELOG.md already has a $version section"

notes=$(scripts/changelog-notes.sh Unreleased) ||
    die "CHANGELOG.md has no '## Unreleased' section"
grep -qE '^[-*] ' <<<"$notes" || die "there is nothing under '## Unreleased' in CHANGELOG.md"

# From here on files are changed: put them back if anything fails.
trap 'git checkout -- CHANGELOG.md Cargo.toml Cargo.lock; die "failed, CHANGELOG.md, Cargo.toml and Cargo.lock were restored"' ERR

tmp=$(mktemp)
NOTES="$notes" awk -v heading="## $version ($(date -u +%F))" '
    state == 0 && /^## Unreleased[[:space:]]*$/ {
        print "## Unreleased"
        print ""
        n = split("Breaking New Improved Fixed Security", kinds, " ")
        for (i = 1; i <= n; i++) { print "### " kinds[i]; print "" }
        print heading
        print ""
        print ENVIRON["NOTES"]
        print ""
        state = 1
        next
    }
    state == 1 && /^## / { state = 2 }
    state == 1 { next }
    { print }
' CHANGELOG.md >"$tmp"
mv "$tmp" CHANGELOG.md

tmp=$(mktemp)
awk -v version="$version" '
    /^\[/ { in_package = ($0 == "[workspace.package]") }
    in_package && /^version = / { $0 = "version = \"" version "\""; done = 1 }
    { print }
    END { if (!done) exit 1 }
' Cargo.toml >"$tmp"
mv "$tmp" Cargo.toml

# Updates the workspace crates' versions in Cargo.lock, and checks that it still builds.
# shellcheck disable=SC2086 # CARGO_CMD may be several words
$cargo_cmd check --workspace --quiet

git add CHANGELOG.md Cargo.toml Cargo.lock
git commit --quiet -m "Release $tag"
git tag -a "$tag" -m "$tag"
trap - ERR

echo "Committed and tagged $tag. Review the commit, then publish with:"
echo "  git push origin main $tag"
