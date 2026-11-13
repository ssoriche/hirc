#!/usr/bin/env bash
# Checks scripts/set-version.sh on a copy of the workspace manifests.
set -euo pipefail
cd "$(dirname "$0")/.."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/scripts"
cp Cargo.toml Cargo.lock "$tmp/"
cp scripts/set-version.sh "$tmp/scripts/"
"$tmp/scripts/set-version.sh" 9.8.7
# The workspace version, and nothing else, moved.
grep -q '^version = "9.8.7"$' "$tmp/Cargo.toml"
[ "$(grep -c '9\.8\.7' "$tmp/Cargo.toml")" = 1 ]
# Both crates in the lock file, and no other crate.
for c in hirc hirc-core; do
    grep -A1 "^name = \"$c\"\$" "$tmp/Cargo.lock" | grep -q '^version = "9.8.7"$'
done
[ "$(grep -c '^version = "9\.8\.7"$' "$tmp/Cargo.lock")" = 2 ]
# A bad version is refused.
if "$tmp/scripts/set-version.sh" 2>/dev/null; then
    echo "no version: should fail" >&2
    exit 1
fi
echo "set-version: ok"
