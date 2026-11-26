#!/bin/bash
# Launch the Mac Catalyst build against a saved Xbox session and stream the
# on-device log to this terminal, so no launch flags need to be remembered.
#
#   ./scripts/live.sh [--height 720|1080|1440] [--input-ms 1..16] [--no-build]
#                     [--metal] [--sync] [--dropold]
#
# Defaults: 1080p input, 4 ms input cadence (250 Hz), fresh build.
# Render-path experiments: --metal (RTCMTLVideoView instead of the sample
# buffer layer), --sync (enqueue on the decode thread), --dropold (flush
# stale queued frames before each enqueue).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/build/DerivedData-catalyst/Build/Products/Debug-maccatalyst/Playwake.app"
BUNDLE="io.github.beshkenadze.playwake.ios"
LOG="$HOME/Documents/lab.log"

HEIGHT=1080
INPUT_MS=4
BUILD=1
EXTRA_ARGS=()

while [ $# -gt 0 ]; do
    case "$1" in
        --height) HEIGHT=$2; shift 2 ;;
        --input-ms) INPUT_MS=$2; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        --metal) EXTRA_ARGS+=(-metal); shift ;;
        --cametal) EXTRA_ARGS+=(-cametal); shift ;;
        --vsync-off) EXTRA_ARGS+=(-vsyncOff); shift ;;
        --sync) EXTRA_ARGS+=(-sync); shift ;;
        --dropold) EXTRA_ARGS+=(-dropold); shift ;;
        --playout-ms) EXTRA_ARGS+=(-playoutMs "$2"); shift 2 ;;
        -h|--help) sed -n '2,11p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ "$BUILD" = 1 ]; then
    (cd "$ROOT" && xcodebuild -workspace PlaywakeIOS.xcworkspace -scheme PlaywakeIOS \
        -configuration Debug -destination 'platform=macOS,variant=Mac Catalyst' \
        -derivedDataPath build/DerivedData-catalyst build -quiet) >&2
fi

pkill -f 'DerivedData-catalyst/Build/Products/Debug-maccatalyst/Playwake.app' 2>/dev/null || true
sleep 1
: > "$LOG"

open -n "$APP" --args -autostream -inputMs "$INPUT_MS" -streamHeight "$HEIGHT" ${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}
# Catalyst focus determines whether GameController exposes the pad at all, and
# the app is not registered with LaunchServices the instant `open` returns.
for _ in 1 2 3; do
    sleep 1
    osascript -e "tell application id \"$BUNDLE\" to activate" >/dev/null 2>&1 || true
done

echo "streaming ${HEIGHT}p, input every ${INPUT_MS} ms ${EXTRA_ARGS[*]+(${EXTRA_ARGS[*]})}; logging to $LOG (Ctrl+C to stop)" >&2
exec tail -n +1 -f "$LOG"
