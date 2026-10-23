#!/usr/bin/env bash
# Builds hirc.app and installs it in a directory so Spotlight, the Dock
# and AeroSpace see a normal app.
#
# Usage: scripts/bundle-macos.sh [install-dir]   (default: /Applications)
set -eu

cd "$(dirname "$0")/.."
CARGO="${CARGO:-$(command -v cargo)}"
DEST="${1:-/Applications}"
NAME="hirc"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
APP="target/release/$NAME.app"

"$CARGO" build --release -p hirc

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/hirc "$APP/Contents/MacOS/hirc"
cat >"$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>$NAME</string>
  <key>CFBundleDisplayName</key><string>$NAME</string>
  <key>CFBundleIdentifier</key><string>org.sorichetti.hirc</string>
  <key>CFBundleExecutable</key><string>hirc</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.social-networking</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Sign with no identity so macOS runs it.
codesign --force --sign - "$APP"
mkdir -p "$DEST"
rm -rf "$DEST/$NAME.app"
cp -R "$APP" "$DEST/"
echo "Installed $DEST/$NAME.app"
