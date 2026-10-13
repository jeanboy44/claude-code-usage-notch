#!/bin/sh
# Build, sign, notarize (when a Developer ID is configured), zip and publish a GitHub release.
#
#   scripts/release.sh 0.1.3 "What changed"
#
# Notarized builds (no Gatekeeper warning for users) need, once:
#   1. An Apple Developer Program membership and a "Developer ID Application" certificate
#      in your login keychain (Xcode → Settings → Accounts → Manage Certificates).
#   2. A notarytool profile:
#        xcrun notarytool store-credentials notchi --apple-id you@example.com --team-id TEAMID
#   3. export DEVELOPER_ID="Developer ID Application: Your Name (TEAMID)"
# Without DEVELOPER_ID the app is ad-hoc signed, as before.
set -e
VERSION="$1"
NOTES="${2:-}"
[ -n "$VERSION" ] || { echo "usage: $0 <version> [notes]" >&2; exit 1; }
cd "$(dirname "$0")/.."

sed -i '' "s/^version = \".*\"/version = \"$VERSION\"/" Cargo.toml
sed -i '' "s#<key>CFBundleVersion</key><string>[^<]*</string>#<key>CFBundleVersion</key><string>$VERSION</string>#" bundle.sh
./bundle.sh
APP=dist/NotchiUsage.app

if [ -n "$DEVELOPER_ID" ]; then
  codesign --force --options runtime --timestamp --sign "$DEVELOPER_ID" "$APP"
  ditto -c -k --keepParent "$APP" dist/notarize.zip
  xcrun notarytool submit dist/notarize.zip --keychain-profile "${NOTARY_PROFILE:-notchi}" --wait
  xcrun stapler staple "$APP"
  rm -f dist/notarize.zip
  SIGNED="Signed with Developer ID and notarized by Apple."
else
  SIGNED="Ad-hoc signed: the first time, right-click → Open (or \`xattr -dr com.apple.quarantine /Applications/NotchiUsage.app\`)."
fi

rm -f dist/NotchiUsage.zip
ditto -c -k --keepParent "$APP" dist/NotchiUsage.zip

git add -A
git commit -q -m "Release $VERSION" || true
git tag -a "v$VERSION" -m "Notchi Usage $VERSION"
git push -q
git push -q origin "v$VERSION"
gh release create "v$VERSION" dist/NotchiUsage.zip --title "Notchi Usage $VERSION" --notes "$NOTES

Install: download **NotchiUsage.zip**, move the app to /Applications and open it. $SIGNED At the Keychain prompt choose **Always Allow**. Turn on **Launch at Login** from the right-click menu. Apple Silicon, macOS 14+."
