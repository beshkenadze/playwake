#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 2 || ! -x $1 || $2 != *.flatpak ]]; then
    echo "Usage: $0 /path/to/compiled/playwake /path/to/Playwake.flatpak" >&2
    exit 2
fi

binary=$(realpath -- "$1")
destination=$(realpath -m -- "$2")
packaging=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
source=$(cd -- "$packaging/../.." && pwd)
id=io.github.beshkenadze.Playwake
runtime=26.08
xres_library=/usr/lib/libXRes.so.1
xres_license=/usr/share/licenses/libxres/COPYING
if [[ -e /usr/lib64/libXRes.so.1 ]]; then
    xres_library=/usr/lib64/libXRes.so.1
fi
if [[ -f /usr/share/doc/libXres/COPYING ]]; then
    xres_license=/usr/share/doc/libXres/COPYING
fi
xprop_license=/usr/share/licenses/xorg-xprop/COPYING
if [[ -f /usr/share/licenses/xprop/COPYING ]]; then
    xprop_license=/usr/share/licenses/xprop/COPYING
fi
scratch=$(mktemp -d)
trap 'rm -rf -- "$scratch"' EXIT
build="$scratch/build"

flatpak build-init "$build" "$id" org.freedesktop.Sdk org.freedesktop.Platform "$runtime"
flatpak build --share=network --filesystem="$packaging:ro" "$build" \
    python3 -m pip install --no-compile --target=/app/lib/python3.14/site-packages \
    --requirement "$packaging/requirements.txt"

install -Dm755 "$binary" "$build/files/libexec/playwake/playwake"
strip --strip-debug "$build/files/libexec/playwake/playwake"
media_binary="$(dirname -- "$binary")/playwake-media"
if [[ -x "$media_binary" ]]; then
    install -Dm755 "$media_binary" "$build/files/libexec/playwake/playwake-media"
    strip --strip-debug "$build/files/libexec/playwake/playwake-media"
fi
install -Dm755 "$source/launch-steamos.sh" "$build/files/libexec/playwake/launch-steamos.sh"
install -Dm644 -t "$build/files/libexec/playwake" "$source/media/"{gstreamer_player,stream_overlay}.py
install -Dm755 /usr/bin/xprop "$build/files/bin/xprop"
install -Dm644 "$xres_library" "$build/files/lib/libXRes.so.1"
install -Dm644 "$xres_license" "$build/files/share/licenses/libXres/COPYING"
install -Dm644 "$xprop_license" "$build/files/share/licenses/xprop/COPYING"
install -Dm644 "$source/LICENSE" "$build/files/share/licenses/$id/LICENSE"
python_path=/app/lib/python3.14/site-packages
cat > "$build/files/bin/playwake" <<'LAUNCHER'
#!/bin/sh
exec /app/libexec/playwake/launch-steamos.sh "$@"
LAUNCHER
chmod +x "$build/files/bin/playwake"

install -Dm644 "$packaging/$id.desktop" "$build/files/share/applications/$id.desktop"
install -Dm644 "$packaging/$id.metainfo.xml" "$build/files/share/metainfo/$id.metainfo.xml"
icon="$build/files/share/icons/hicolor/256x256/apps/$id.png"
mkdir -p -- "$(dirname -- "$icon")"
ffmpeg -nostdin -hide_banner -loglevel error -i "$source/assets/branding/icon.png" \
    -vf 'scale=256:256:flags=lanczos' -frames:v 1 -update 1 "$icon"

flatpak build-finish --command=playwake --share=network --share=ipc \
    --socket=x11 --socket=pulseaudio --device=dri --device=input \
    --env="PYTHONPATH=$python_path" \
    "$build"
flatpak build-export "$scratch/repo" "$build" stable
# Record Flathub as the runtime source in the local-install bundle.
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    "$scratch/repo" "$destination" "$id" stable
