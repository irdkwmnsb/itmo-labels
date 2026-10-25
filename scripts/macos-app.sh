#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) "ITMO Labels.app" plus the labelprobe CLI into <out-dir>.
# The bundle is ad-hoc signed only: on first launch macOS asks to confirm opening it.
#
# Usage: scripts/macos-app.sh <out-dir>
set -euo pipefail

out="${1:?usage: $0 <out-dir>}"
targets=(aarch64-apple-darwin x86_64-apple-darwin)
version=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"itmo-labels","version":"\([^"]*\)".*/\1/p')
export MACOSX_DEPLOYMENT_TARGET=11.0

for target in "${targets[@]}"; do
  cargo build --release --locked --target "$target" -p itmo-labels -p itmo-labels-cli
done

app="$out/ITMO Labels.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
lipo -create -output "$app/Contents/MacOS/itmo-labels" target/{aarch64,x86_64}-apple-darwin/release/itmo-labels
lipo -create -output "$out/labelprobe" target/{aarch64,x86_64}-apple-darwin/release/labelprobe

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>ITMO Labels</string>
  <key>CFBundleDisplayName</key><string>ITMO Labels</string>
  <key>CFBundleIdentifier</key><string>com.github.irdkwmnsb.itmo-labels</string>
  <key>CFBundleExecutable</key><string>itmo-labels</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>$MACOSX_DEPLOYMENT_TARGET</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF

codesign --force --sign - "$app"
codesign --force --sign - "$out/labelprobe"
