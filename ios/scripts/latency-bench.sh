#!/bin/sh
# Builds the iOS app, runs the in-app loopback lab, and prints the median
# end-to-end latency (ms, capture -> decoded pixels rendered) as a single
# number on stdout. Diagnostics go to stderr. Lower is better.
set -eu

IOS_ROOT=$(cd "$(dirname "$0")/.." && pwd)
BUNDLE=io.github.beshkenadze.playwake.ios
WARMUP=${WARMUP:-44}
SAMPLES=${SAMPLES:-20}

UDID=${SIM_UDID:-}
if [ -z "$UDID" ]; then
    UDID=$(xcrun simctl list devices booted | sed -n 's/.*(\([0-9A-F-]\{36\}\)) (Booted).*/\1/p' | head -n 1)
fi
[ -n "$UDID" ] || { echo "no booted simulator" >&2; exit 1; }

xcodebuild -project "$IOS_ROOT/PlaywakeIOS.xcodeproj" -scheme PlaywakeIOS \
    -configuration Debug -sdk iphonesimulator -destination "id=$UDID" \
    -derivedDataPath "$IOS_ROOT/build/DerivedData" build -quiet >&2

APP="$IOS_ROOT/build/DerivedData/Build/Products/Debug-iphonesimulator/Playwake.app"
xcrun simctl terminate "$UDID" "$BUNDLE" 2>/dev/null || true
xcrun simctl uninstall "$UDID" "$BUNDLE" >/dev/null 2>&1 || true
xcrun simctl install "$UDID" "$APP"
# shellcheck disable=SC2086
xcrun simctl launch "$UDID" "$BUNDLE" -autolab ${LAUNCH_ARGS:-} >/dev/null
sleep "$WARMUP"

DATA=$(xcrun simctl get_app_container "$UDID" "$BUNDLE" data)
LOG="$DATA/Documents/lab.log"
LINES=$(grep '^stats ' "$LOG" 2>/dev/null | tail -n "$SAMPLES")
[ -n "$LINES" ] || { echo "no stats in $LOG" >&2; tail -n 20 "$LOG" >&2 2>/dev/null || true; exit 1; }

echo "--- samples (last $SAMPLES) ---" >&2
printf '%s\n' "$LINES" >&2

printf '%s\n' "$LINES" |
    awk -F'e2e ' '{split($2, a, " ms"); print a[1]}' |
    sort -n |
    awk '{v[NR] = $1} END {print (NR % 2 ? v[(NR + 1) / 2] : (v[NR / 2] + v[NR / 2 + 1]) / 2)}'
