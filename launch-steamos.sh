#!/usr/bin/env bash
set -euo pipefail
dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)

# Native video embeds into the GPUI window through X11 on SteamOS.
export DISPLAY=${DISPLAY:-:0}
unset WAYLAND_DISPLAY WAYLAND_SOCKET
if [[ ${ALLY_STEAM_SHORTCUT:-0} == 1 || ${SteamGameId:-0} != 0 || ${SteamAppId:-0} != 0 ]]; then
    export ALLY_GAMESCOPE_FOCUS=0
    if [[ -n ${GAMESCOPE_WAYLAND_DISPLAY:-} ]]; then
        export ALLY_GAMESCOPE_CONTROL_DISPLAY=${ALLY_GAMESCOPE_CONTROL_DISPLAY:-:0}
    fi
elif [[ -n ${GAMESCOPE_WAYLAND_DISPLAY:-} ]]; then
    export ALLY_GAMESCOPE_FOCUS=${ALLY_GAMESCOPE_FOCUS:-1}
fi

umask 077
if [[ -n ${ALLY_LOG_FILE:-} ]]; then
    exec 2>>"$ALLY_LOG_FILE"
fi
exec "$dir/playwake" "$@"
