#!/usr/bin/env bash
# Builds hirc.app, ad-hoc signs it, and installs it so Spotlight, the Dock
# and AeroSpace see a normal app.
#
# Usage: scripts/bundle-macos.sh [--no-install] [install-dir]   (default: /Applications)
#   --no-install  build and sign the bundle under target/release, print its path
# Environment:
#   CARGO  the cargo command (default: cargo on PATH)
#   BIN    a prebuilt binary to bundle; when set, the build is skipped
set -euo pipefail

cd "$(dirname "$0")/.."
CARGO="${CARGO:-$(command -v cargo)}"
INSTALL=1
if [ "${1:-}" = "--no-install" ]; then
    INSTALL=0
    shift
fi
DEST="${1:-/Applications}"
NAME="hirc"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/release/$NAME.app"

if [ -z "${BIN:-}" ]; then
    $CARGO build --release -p hirc
    BIN="target/release/hirc"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/hirc"
cp macos/AppIcon.icns "$APP/Contents/Resources/"
sed "s/@version@/$VERSION/g" macos/Info.plist >"$APP/Contents/Info.plist"

# Sign with no identity so macOS runs it.
codesign --force --sign - "$APP"
if [ "$INSTALL" = 0 ]; then
    echo "Built $PWD/$APP"
    exit 0
fi
mkdir -p "$DEST"
rm -rf "${DEST:?}/$NAME.app"
cp -R "$APP" "$DEST/"
echo "Installed $DEST/$NAME.app"
