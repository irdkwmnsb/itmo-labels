#!/usr/bin/env bash
# Installs "ITMO Labels.app" into ~/Applications and opens it. Unlike a browser, curl does not
# set com.apple.quarantine, so Gatekeeper never blocks the ad-hoc signed app.
#
# Usage: curl -fsSL https://raw.githubusercontent.com/irdkwmnsb/itmo-labels/main/scripts/install-macos.sh | bash
#        ... | bash -s -- <zip-url>    (a given macos-universal archive instead of the latest release)
set -euo pipefail

repo=irdkwmnsb/itmo-labels
url="${1:-}"
if [[ -z "$url" ]]; then
  url=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest" |
    sed -n 's/.*"browser_download_url": *"\([^"]*-macos-universal\.zip\)".*/\1/p' | head -n 1)
  [[ -n "$url" ]] || { echo "no macos-universal archive in the latest release of $repo" >&2; exit 1; }
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $url"
curl -fL --progress-bar -o "$tmp/pkg.zip" "$url"
ditto -x -k "$tmp/pkg.zip" "$tmp/pkg"
app=$(find "$tmp/pkg" -maxdepth 2 -name "ITMO Labels.app" -print -quit)
[[ -n "$app" ]] || { echo "ITMO Labels.app not found in the archive" >&2; exit 1; }

dest="$HOME/Applications/ITMO Labels.app"
mkdir -p "$HOME/Applications"
rm -rf "$dest"
ditto "$app" "$dest"
echo "Installed $dest"
open "$dest"
