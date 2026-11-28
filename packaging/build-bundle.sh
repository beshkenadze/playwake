#!/usr/bin/env bash
# Build the SteamOS release bundle: playwake binary + Python worker + launcher +
# branding + installer, packed as dist/playwake-<version>-steamos-x86_64.tar.gz.
#
# Default: build inside an ubuntu:22.04 Docker container (glibc 2.35, so the
# binary runs on any newer SteamOS). On a Linux x86_64 host with cargo and the
# build dependencies installed, pass --native to build directly.
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out="$repo_root/dist"
image=ubuntu:22.04
native=0

usage() {
    cat <<'EOF'
Usage: build-bundle.sh [--native] [--image IMAGE]
  --native        build on this host instead of Docker (Linux x86_64 + cargo)
  --image IMAGE   Docker image to build in (default ubuntu:22.04)
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --native) native=1; shift ;;
        --image) image=$2; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

mkdir -p "$out"

# Shared build/package steps. SRC = repo, OUT = artifact directory.
build_steps=$(cat <<'EOS'
set -euo pipefail
cd "$SRC"
cargo test --release --locked
cargo build --release --locked
cargo test --release --locked -p playwake-media
cargo build --release --locked -p playwake-media
python3 -m unittest discover -s packaging -p 'test_*.py'

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
name="playwake-${version}-steamos-x86_64"
bundle="$OUT/$name"
rm -rf "$bundle"
mkdir -p "$bundle/assets/branding" "$bundle/packaging"
cp "${CARGO_TARGET_DIR:-target}/release/playwake" "$bundle/playwake"
cp "${CARGO_TARGET_DIR:-target}/release/playwake-media" "$bundle/playwake-media"
cp launch-steamos.sh "$bundle/"
cp assets/branding/*.png "$bundle/assets/branding/"
cp packaging/install.sh packaging/uninstall.sh packaging/steam_shortcut.py packaging/steam_input_neptune.vdf "$bundle/packaging/"
mkdir -p "$bundle/runtime"
pkgs=$(mktemp -d)
grep -vE '^[[:space:]]*(#|$)' packaging/runtime-packages.txt | while read -r url; do
    file="$pkgs/$(basename "$url")"
    curl -fsSL -o "$file" "$url"
    tar --zstd -xf "$file" -C "$bundle/runtime"
done
rm -rf "$pkgs"
# Keep only the plugins SteamOS lacks. The other gst-plugins-bad plugins need
# codec libraries this bundle does not carry, and GStreamer warns about each.
find "$bundle/runtime/usr/lib/gstreamer-1.0" -name '*.so' \
    ! -name libgstwebrtc.so ! -name libgstdtls.so ! -name libgstsrtp.so \
    ! -name libgstsctp.so ! -name libgstva.so ! -name libgstlibav.so -delete
rm -rf "$bundle/runtime/usr/include" "$bundle/runtime/usr/lib/pkgconfig"
printf '%s\n' "$version" > "$bundle/VERSION"
(cd "$bundle" && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS)
tar -czf "$OUT/$name.tar.gz" -C "$OUT" "$name"
(cd "$OUT" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
rm -rf "$bundle"
echo "Built $OUT/$name.tar.gz"
EOS
)

apt_setup=$(cat <<'EOS'
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
# shellcheck disable=SC2046
apt-get install -y -qq --no-install-recommends $(cat "$SRC/packaging/build-deps.txt")
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
. "$HOME/.cargo/env"
EOS
)

if [[ $native == 1 ]]; then
    [[ $(uname -s) == Linux && $(uname -m) == x86_64 ]] || {
        echo "--native requires Linux x86_64" >&2; exit 1; }
    command -v cargo >/dev/null || { echo "cargo not found; install Rust >= 1.85" >&2; exit 1; }
    command -v python3 >/dev/null || { echo "python3 not found" >&2; exit 1; }
    SRC="$repo_root" OUT="$out" bash -c "$build_steps"
else
    command -v docker >/dev/null || { echo "docker not found; use --native on a Linux x86_64 host" >&2; exit 1; }
    docker run --rm --platform linux/amd64 \
        -v "$repo_root":/src -v "$out":/out \
        -e SRC=/src -e OUT=/out -e CARGO_TARGET_DIR=/tmp/cargo-target \
        "$image" bash -c "$apt_setup
$build_steps"
fi