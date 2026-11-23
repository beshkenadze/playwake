#!/bin/sh
# Builds the portable Rust protocol core for the platform Xcode is targeting
# and drops a universal static library into ios/Rust/libplaywake_core.a.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"          # ios/
CRATE="$ROOT/../ios-core"
OUT="$ROOT/Rust"
mkdir -p "$OUT"

PLATFORM="${PLATFORM_NAME:-iphonesimulator}"
CONFIG="${CONFIGURATION:-Debug}"
if [ "$CONFIG" = "Release" ]; then
    PROFILE=release
    PROFILE_FLAG="--release"
else
    PROFILE=debug
    PROFILE_FLAG=""
fi

if [ "$PLATFORM" = "iphoneos" ]; then
    TARGETS="aarch64-apple-ios"
else
    TARGETS="aarch64-apple-ios-sim x86_64-apple-ios"
fi

LIBS=""
for target in $TARGETS; do
    echo "note: building playwake-ios-core for $target ($PROFILE)" >&2
    ( cd "$CRATE" && cargo build --target "$target" $PROFILE_FLAG )
    lib="$CRATE/target/$target/$PROFILE/libplaywake_core.a"
    if [ ! -f "$lib" ]; then
        echo "error: missing $lib" >&2
        exit 1
    fi
    LIBS="$LIBS $lib"
done

# shellcheck disable=SC2086
xcrun lipo -create $LIBS -output "$OUT/libplaywake_core.a"
echo "note: wrote $OUT/libplaywake_core.a" >&2
