#!/bin/sh
# Render the app icon with the app's own drawing code and pack it into assets/AppIcon.icns.
set -e
cd "$(dirname "$0")/.."
cargo build --release
TMP=$(mktemp -d)
./target/release/notchi-usage --render-icon "$TMP/icon.png"
SET="$TMP/AppIcon.iconset"
mkdir -p "$SET" assets
for size in 16 32 128 256 512; do
  sips -z $size $size "$TMP/icon.png" --out "$SET/icon_${size}x${size}.png" >/dev/null
  sips -z $((size * 2)) $((size * 2)) "$TMP/icon.png" --out "$SET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$SET" -o assets/AppIcon.icns
sips -z 256 256 "$TMP/icon.png" --out docs/icon.png >/dev/null
rm -rf "$TMP"
echo "wrote assets/AppIcon.icns and docs/icon.png"
