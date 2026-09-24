#!/usr/bin/env bash
# Builds target/release/ubergit.app (ad-hoc signed). Usage: scripts/bundle-mac.sh
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release -p ubergit
VERSION=$(cargo pkgid -p ubergit | sed 's/.*[#@]//')
APP=target/release/ubergit.app

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/ubergit "$APP/Contents/MacOS/ubergit"
cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ubergit</string>
  <key>CFBundleDisplayName</key><string>ubergit</string>
  <key>CFBundleIdentifier</key><string>dev.ubergit.app</string>
  <key>CFBundleExecutable</key><string>ubergit</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
codesign --force --sign - "$APP"
echo "$APP"
