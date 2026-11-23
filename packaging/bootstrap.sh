#!/usr/bin/env bash
# One-line installer entry point for Steam Deck / SteamOS (run in Desktop Mode):
#
#   curl -fsSL https://raw.githubusercontent.com/beshkenadze/playwake/main/packaging/bootstrap.sh | bash
#
# Downloads the latest release bundle, verifies its checksum, unpacks it and
# hands over to packaging/install.sh. All arguments are passed through, e.g.
#   curl -fsSL .../bootstrap.sh | bash -s -- --yes --dir "$HOME/games/playwake"
set -euo pipefail

repo=${PLAYWAKE_REPO:-beshkenadze/playwake}

for tool in curl python3 tar sha256sum; do
    command -v "$tool" >/dev/null || { echo "error: $tool is required" >&2; exit 1; }
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Fetching the latest Playwake release from $repo..."
release_json="$tmp/release.json"
curl -fsSL -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$repo/releases/latest" -o "$release_json"

{
    read -r tag
    read -r tarball_url
    read -r checksum_url
} < <(python3 - "$release_json" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    release = json.load(handle)
assets = {asset["name"]: asset["browser_download_url"] for asset in release.get("assets", [])}
tarballs = [name for name in assets if name.endswith("-steamos-x86_64.tar.gz")]
if len(tarballs) != 1:
    raise SystemExit(f"expected one SteamOS tarball in release {release.get('tag_name')}, got {tarballs}")
name = tarballs[0]
if name + ".sha256" not in assets:
    raise SystemExit(f"release is missing {name}.sha256")
print(release["tag_name"])
print(assets[name])
print(assets[name + ".sha256"])
PY
)

name=$(basename "$tarball_url")
echo "Downloading $name ($tag)..."
curl -fsSL -o "$tmp/$name" "$tarball_url"
curl -fsSL -o "$tmp/$name.sha256" "$checksum_url"
(cd "$tmp" && sha256sum -c "$name.sha256")

tar -xzf "$tmp/$name" -C "$tmp"
bundle=$(echo "$tmp"/playwake-*-steamos-x86_64)
[[ -x "$bundle/packaging/install.sh" ]] || { echo "error: unpacked bundle has no installer" >&2; exit 1; }

exec bash "$bundle/packaging/install.sh" "$@"