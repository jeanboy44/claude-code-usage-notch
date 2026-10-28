#!/bin/sh
# Build a menu-bar-less .app (LSUIElement) into ./dist and optionally install to /Applications.
set -e
cd "$(dirname "$0")"
cargo build --release
APP=dist/NotchiUsage.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
# The executable name is what Keychain prompts show ("NotchiUsage wants to use ...").
cp target/release/notchi-usage "$APP/Contents/MacOS/NotchiUsage"
cp assets/AppIcon.icns "$APP/Contents/Resources/"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>NotchiUsage</string>
  <key>CFBundleIdentifier</key><string>dev.local.notchi-usage</string>
  <key>CFBundleExecutable</key><string>NotchiUsage</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key><string>0.2.6</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
</dict>
</plist>
PLIST
codesign --force --sign - --identifier dev.local.notchi-usage "$APP" >/dev/null 2>&1 || echo "warning: ad-hoc codesign failed" >&2
echo "built $APP"
if [ "$1" = "--install" ]; then
  rm -rf /Applications/NotchiUsage.app
  cp -R "$APP" /Applications/
  echo "installed to /Applications/NotchiUsage.app"
fi
