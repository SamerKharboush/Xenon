#!/usr/bin/env bash
# scripts/release.sh <version>
#
# Bump tauri.conf.json + Cargo.toml (+ Cargo.lock), commit "release: vX.Y.Z",
# and create the git tag — but NEVER push. Prints the push command for you to
# run by hand.
#
# Usage:
#   scripts/release.sh 0.3.0
#
# After this prints, run:
#   git push origin main && git push origin v0.3.0
set -euo pipefail

usage() {
    echo "usage: $0 <version>   e.g. $0 0.3.0" >&2
    exit 1
}

[ $# -eq 1 ] || usage

VERSION="$1"
# Strip a leading v if the caller passed "v0.3.0".
VERSION="${VERSION#v}"
TAG="v${VERSION}"

# Validate: MAJOR.MINOR.PATCH with optional -prerelease / +build.
if ! echo "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
    echo "error: '$VERSION' is not a valid semver (MAJOR.MINOR.PATCH)" >&2
    exit 1
fi

# Always operate from the repo root (this script lives in scripts/).
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

CONF="src-tauri/tauri.conf.json"
CARGO="src-tauri/Cargo.toml"

# --- 1. tauri.conf.json ------------------------------------------------------
if ! grep -q "\"version\"[[:space:]]*:[[:space:]]*\"$VERSION\"" "$CONF"; then
    # sed -i '' is the macOS form; this repo targets macOS.
    sed -i '' -E "s/(\"version\"[[:space:]]*:[[:space:]]*\")[^\"]*(\")/\1${VERSION}\2/" "$CONF"
    echo "bumped $CONF -> $VERSION"
else
    echo "$CONF already at $VERSION"
fi

# --- 2. Cargo.toml (first version = package version) -------------------------
if ! grep -qE "^version[[:space:]]*=[[:space:]]*\"$VERSION\"" "$CARGO"; then
    # Replace only the FIRST `version = "..."` line (the [package] version).
    # BSD sed lacks GNU's `0,/re/` range, so use awk for the first-match bump.
    tmp=$(mktemp)
    awk -v v="$VERSION" '
        !done && /^version[[:space:]]*=[[:space:]]*"/ {
            print "version = \"" v "\""
            done = 1
            next
        }
        { print }
    ' "$CARGO" > "$tmp" && mv "$tmp" "$CARGO"
    echo "bumped $CARGO -> $VERSION"
else
    echo "$CARGO already at $VERSION"
fi

# --- 3. Cargo.lock: sync the xenon package version ---------------------------
# Prefer the precise update; fall back to a (debug) build that rewrites the lock.
( cd src-tauri && cargo update -p xenon --precise "$VERSION" >/dev/null 2>&1 ) \
    || ( cd src-tauri && cargo build >/dev/null 2>&1 )

# --- 4. Commit + tag ---------------------------------------------------------
git add "$CONF" "$CARGO" src-tauri/Cargo.lock
git commit -m "release: v$VERSION"
git tag -a "$TAG" -m "Xenon $TAG"

echo
echo "Committed and tagged $TAG. Nothing was pushed."
echo
echo "Push when ready:"
echo "  git push origin main && git push origin $TAG"