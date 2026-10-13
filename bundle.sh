#!/bin/sh
# Build a menu-bar-less .app (LSUIElement) into ./dist and optionally install to /Applications.
set -e
cd "$(dirname "$0")"
cargo build --release
APP=dist/NotchiUsage.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/notchi-usage "$APP/Contents/MacOS/"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>NotchiUsage</string>
  <key>CFBundleIdentifier</key><string>dev.local.notchi-usage</string>
  <key>CFBundleExecutable</key><string>notchi-usage</string>
  <key>CFBundleVersion</key><string>0.1.0</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$APP" >/dev/null 2>&1 || echo "warning: ad-hoc codesign failed" >&2
echo "built $APP"
if [ "$1" = "--install" ]; then
  rm -rf /Applications/NotchiUsage.app
  cp -R "$APP" /Applications/
  echo "installed to /Applications/NotchiUsage.app"
fi
