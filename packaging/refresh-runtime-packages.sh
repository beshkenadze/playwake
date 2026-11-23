#!/usr/bin/env bash
# Print the pinned runtime package URLs for the SteamOS release this device runs.
# Run it on a SteamOS device after an OS update, then replace the URLs in
# packaging/runtime-packages.txt with this output.
set -euo pipefail

packages=(gst-plugins-bad gst-plugin-va gst-libav libsrtp)
pattern="/(gst-plugins-bad|gst-plugin-va|gst-libav|libsrtp)-[0-9]"

command -v pacman >/dev/null || { echo "error: run this on a SteamOS device" >&2; exit 1; }
pacman -Sp --noconfirm "${packages[@]}" | grep -E "$pattern"