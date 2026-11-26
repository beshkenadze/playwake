#!/bin/bash
# A/B the Catalyst render path on the loopback latency lab.
# Runs each mode for ~25 s and prints the median barcode e2e (lower is better),
# with decode fps and drops as guards.
#
#   ./scripts/render-ab.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
APP="$ROOT/build/DerivedData-catalyst/Build/Products/Debug-maccatalyst/Playwake.app"
LOG="$HOME/Documents/lab.log"

run_mode() {
    local name=$1; shift
    pkill -f 'DerivedData-catalyst/Build/Products/Debug-maccatalyst/Playwake.app' 2>/dev/null || true
    sleep 1
    : > "$LOG"
    open -n "$APP" --args -autolab -playoutMs 0 "$@"
    sleep 25
    pkill -f 'DerivedData-catalyst/Build/Products/Debug-maccatalyst/Playwake.app' 2>/dev/null || true
    # Skip the first 5 stats lines (warm-up), keep positive e2e samples.
    local values
    values=$(grep -o 'e2e [0-9.]* ms' "$LOG" | awk '{print $2}' | awk 'NR>5 && $1>0')
    local n median fps drops
    n=$(printf '%s\n' "$values" | grep -c . || true)
    if [ "$n" = 0 ]; then
        echo "$name: NO SAMPLES"
        return
    fi
    median=$(printf '%s\n' "$values" | sort -n | awk -v n="$n" 'NR==int((n+1)/2){print $1}')
    fps=$(grep 'stats ' "$LOG" | tail -n +6 | awk '{s+=$4; c++} END{if(c)printf "%.1f", s/c; else print "0"}')
    drops=$(grep -o 'drop [0-9]*' "$LOG" | awk '{if($2>m)m=$2} END{print m+0}')
    printf '%-18s e2e median %6s ms  (n=%s, dec avg %s fps, max drop %s)\n' "$name" "$median" "$n" "$fps" "$drops"
}

run_mode "baseline"
run_mode "sync" -sync
run_mode "sync+dropold" -sync -dropold
run_mode "metal" -metal