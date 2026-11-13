#!/usr/bin/env bash
# Stamps a release version into the workspace manifest and Cargo.lock.
# Usage: scripts/set-version.sh <version>
set -euo pipefail
v="${1:?usage: set-version.sh <version>}"
cd "$(dirname "$0")/.."

# The first line that starts `version = "…"` is [workspace.package]'s.
perl -0pi -e 's/^version = "[^"]*"/version = "'"$v"'"/m' Cargo.toml
# Our two crates in Cargo.lock.
for c in hirc hirc-core; do
    perl -0pi -e 's/(\nname = "'"$c"'"\nversion = ")[^"]*(")/${1}'"$v"'${2}/' Cargo.lock
done

grep -q "^version = \"$v\"" Cargo.toml
for c in hirc hirc-core; do
    grep -A1 "^name = \"$c\"\$" Cargo.lock | grep -q "^version = \"$v\"\$"
done
