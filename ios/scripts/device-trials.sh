#!/bin/sh
# Live latency trials on a physical iPhone.
#
# Builds the Debug configuration with Swift optimization on (lab.log stays
# enabled, timings are realistic), installs it, streams the first console with
# `-remote`, runs the latency trials and prints the client pipeline metrics.
#
# Needs: the phone unlocked and a game on the console in a still scene whose
# camera turns with the right stick.
#
# Usage: DEVICE=<udid> [EXTRA_ARGS="-cametal"] ./scripts/device-trials.sh [stream height, default 720]
# Last output line: client pipeline ms = jb + dec + hold + sub + pres + actuate→send.
set -eu

IOS_ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEVICE=${DEVICE:?set DEVICE to the iPhone UDID}
HEIGHT=${1:-720}
BUNDLE=io.github.beshkenadze.playwake.ios
OUT="$IOS_ROOT/build/device-trials"
LOG="$OUT/console.log"
APP="$IOS_ROOT/build/DerivedData-device/Build/Products/Debug-iphoneos/Playwake.app"
mkdir -p "$OUT"

(cd "$IOS_ROOT" && tuist generate --no-open) >&2
xcodebuild -workspace "$IOS_ROOT/PlaywakeIOS.xcworkspace" -scheme PlaywakeIOS \
    -configuration Debug -destination "id=$DEVICE" \
    -derivedDataPath "$IOS_ROOT/build/DerivedData-device" -allowProvisioningUpdates \
    SWIFT_OPTIMIZATION_LEVEL=-O SWIFT_COMPILATION_MODE=wholemodule build -quiet >&2
xcrun devicectl device install app --device "$DEVICE" "$APP" >/dev/null

: > "$LOG"
xcrun devicectl device process launch --console --terminate-existing --device "$DEVICE" "$BUNDLE" \
    -- -autostream -remote -streamHeight "$HEIGHT" ${EXTRA_ARGS:-} > "$LOG" 2>&1 &

for _ in $(seq 1 60); do
    grep -q 'remote control on' "$LOG" && break
    if grep -q 'Locked' "$LOG"; then echo "device is locked" >&2; exit 1; fi
    sleep 1
done
grep -q 'remote control on' "$LOG" || { echo "stream did not start" >&2; exit 1; }

# Let the "remote play started" toast fade; the trials add their own 4 s warmup.
sleep 8
# Remote command ids must grow; milliseconds keep them above any earlier one.
printf '{"id":%s,"trials":true}' "$(perl -MTime::HiRes=time -e 'printf "%d", time * 1000')" > "$OUT/command.json"
xcrun devicectl device copy to --device "$DEVICE" --domain-type appDataContainer \
    --domain-identifier "$BUNDLE" --source "$OUT/command.json" \
    --destination Documents/remote/command.json >/dev/null

for _ in $(seq 1 90); do
    grep -q 'trials done' "$LOG" && break
    sleep 1
done
grep -q 'trials done' "$LOG" || { echo "trials did not finish" >&2; exit 1; }

grep -E '\[lab\] trials done' "$LOG" | tail -1 >&2
# Medians of the per-second stats printed while the trials ran.
awk '
    /\[lab\] trial self-check/ { on = 1; next }
    /\[lab\] trials done/ {
        on = 0
        s = $0; sub(/.*poll→turn median /, "", s); turn = s + 0
        s = $0; sub(/.*actuate→send median /, "", s); wait = s + 0
        s = $0; sub(/.*n=/, "", s); n = s + 0
        next
    }
    on && /\[lab\] stats / {
        k++
        for (i = 1; i <= NF; i++) {
            if ($i == "jb") jb[k] = $(i + 1)
            if ($i == "sub") sub_[k] = $(i + 1)
            if ($i == "hold") hold[k] = $(i + 1)
            if ($i == "pres") pres[k] = $(i + 1)
            if ($i == "ren") ren[k] = $(i + 1)
            if ($i == "drop") { if (k == 1) drop0 = $(i + 1); drop1 = $(i + 1) }
            if ($i == "dec" && $(i + 2) == "ms") dec[k] = $(i + 1)
        }
    }
    function median(a, n,   i, j, t, b) {
        for (i = 1; i <= n; i++) b[i] = a[i]
        for (i = 1; i <= n; i++) for (j = i + 1; j <= n; j++) if (b[j] < b[i]) { t = b[i]; b[i] = b[j]; b[j] = t }
        return n % 2 ? b[(n + 1) / 2] : (b[n / 2] + b[n / 2 + 1]) / 2
    }
    END {
        if (k == 0) { print "no stats during trials" > "/dev/stderr"; exit 1 }
        if (n == 0) { print "no trial samples" > "/dev/stderr"; exit 1 }
        m_jb = median(jb, k); m_dec = median(dec, k); m_sub = median(sub_, k)
        m_pres = median(pres, k); m_ren = median(ren, k); m_hold = median(hold, k)
        printf "jb %.2f dec %.2f hold %.2f sub %.2f pres %.2f actuate→send %.2f | turn %.1f n %d ren %.1f drop %d\n",
            m_jb, m_dec, m_hold, m_sub, m_pres, wait, turn, n, m_ren, drop1 - drop0 > "/dev/stderr"
        printf "%.2f\n", m_jb + m_dec + m_hold + m_sub + m_pres + wait
    }
' "$LOG"
