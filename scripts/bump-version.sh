#!/bin/sh
# Sets the version for the next release: scripts/bump-version.sh 0.8.0
#
# Updates Cargo.toml and Cargo.lock (plain text edits, no cargo needed) and creates
# release-notes/<version>.md for the "What's new" text. Commit all three in a pull
# request; merging it into master builds and publishes the release.
set -eu

version="${1:-}"
case "$version" in
    [0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "usage: $0 X.Y.Z" >&2; exit 1 ;;
esac

cd "$(dirname "$0")/.."
old=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

# The first `version = ` in Cargo.toml is the package's own.
sed -i.bak "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
# In Cargo.lock: the version line right after `name = "magic-pdf"`.
sed -i.bak "/^name = \"magic-pdf\"$/{n;s/^version = \".*\"/version = \"$version\"/;}" Cargo.lock
rm -f Cargo.toml.bak Cargo.lock.bak

grep -q "^version = \"$version\"" Cargo.toml || { echo "could not update Cargo.toml" >&2; exit 1; }
grep -A1 '^name = "magic-pdf"$' Cargo.lock | grep -q "^version = \"$version\"" \
    || { echo "could not update Cargo.lock" >&2; exit 1; }

notes="release-notes/$version.md"
if [ ! -f "$notes" ]; then
    mkdir -p release-notes
    printf '## What'"'"'s new in %s\n\n' "$version" > "$notes"
fi

echo "Version $old -> $version. Now write the release text in $notes."
