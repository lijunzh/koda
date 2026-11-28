#!/bin/sh
# Rebuild everything drawn from the app's icon, assets/icon.svg:
#   assets/icon.icns                 the app's icon, which `npm run package` sets
#   docs/src/assets/koda-logo.png    the logo in the README and the manual
#   docs/theme/favicon.{svg,png}     the manual's tab icon
# Needs rsvg-convert (`brew install librsvg`) and macOS's iconutil. The
# results are committed, so a release build needs neither.
set -eu
cd "$(dirname "$0")/.."
svg=assets/icon.svg
set_dir=$(mktemp -d)/icon.iconset
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
  rsvg-convert "$svg" -w "$size" -h "$size" -o "$set_dir/icon_${size}x${size}.png"
  double=$((size * 2))
  rsvg-convert "$svg" -w "$double" -h "$double" -o "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o assets/icon.icns
rm -rf "$(dirname "$set_dir")"

docs=../../docs
rsvg-convert "$svg" -w 512 -h 512 -o "$docs/src/assets/koda-logo.png"
cp "$svg" "$docs/theme/favicon.svg"
rsvg-convert "$svg" -w 64 -h 64 -o "$docs/theme/favicon.png"
echo "Built assets/icon.icns, the docs' logo and the manual's favicon."
