#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) "ITMO Labels.app" plus the labelprobe CLI into <out-dir>,
# next to "Open ITMO Labels.command", which clears the download quarantine and opens the app.
# The bundle is ad-hoc signed only, so a quarantined copy is blocked by Gatekeeper; the
# .command is blocked too, but needs "Open Anyway" only once and then fixes the whole folder.
# scripts/install-macos.sh avoids the quarantine altogether.
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

launcher="$out/Open ITMO Labels.command"
cat > "$launcher" <<'EOF'
#!/bin/bash
# Clears the download quarantine from this folder, then opens the app.
cd "$(dirname "$0")" || exit 1
xattr -dr com.apple.quarantine . 2>/dev/null
open "ITMO Labels.app"
EOF
chmod +x "$launcher"
