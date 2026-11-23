#!/usr/bin/env bash
# Remove the Playwake install, its Steam shortcut and its library artwork.
# Sign-in state in ~/.local/state/ally-remote-play is deliberately kept.
set -euo pipefail

dir="$HOME/.local/share/playwake"
assume_yes=0
restart_steam=1

usage() {
    cat <<'EOF'
Usage: uninstall.sh [options]
  --dir DIR             install directory (default ~/.local/share/playwake)
  --yes, -y             do not ask for confirmation
  --no-restart-steam    leave Steam closed
  --help                show this help
EOF
}

while [[ $# -gt 0 ]]; do
    case $1 in
        --dir) dir=$2; shift 2 ;;
        --dir=*) dir=${1#*=}; shift ;;
        --yes|-y) assume_yes=1; shift ;;
        --no-restart-steam) restart_steam=0; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { echo "error: $*" >&2; exit 1; }

[[ $(uname -s) == Linux ]] || die "this uninstaller targets Linux/SteamOS"
dir=$(cd -- "$dir" 2>/dev/null && pwd) || die "$dir does not exist"
manifest="$dir/install.json"
[[ -f "$manifest" ]] || die "$manifest not found; refusing to remove an unknown directory"
python3 - "$manifest" "$dir" <<'PY' || die "manifest does not describe this directory"
import json
import sys
from pathlib import Path

manifest = json.loads(Path(sys.argv[1]).read_text())
if manifest.get("app") != "playwake":
    raise SystemExit(1)
if Path(manifest.get("dir", "")).resolve() != Path(sys.argv[2]).resolve():
    raise SystemExit(1)
PY
[[ $dir != "$HOME" && $dir != "/" ]] || die "refusing to remove $dir"
if [[ -n ${GAMESCOPE_WAYLAND_DISPLAY:-} ]]; then
    die "Game Mode detected. Switch to Desktop Mode and re-run."
fi

if [[ $assume_yes != 1 ]]; then
    printf 'Remove %s and the Playwake Steam shortcut? Sign-in state is kept. [y/N] ' "$dir"
    read -r answer </dev/tty || die "non-interactive shell; re-run with --yes"
    case $answer in
        y|Y) ;;
        *) echo "Aborted."; exit 0 ;;
    esac
fi

closed_steam=0
if pgrep -x steam >/dev/null 2>&1; then
    echo "Closing Steam (it rewrites shortcuts.vdf on exit)..."
    pkill -15 -x steam 2>/dev/null || true
    closed_steam=1
    for _ in $(seq 1 30); do
        pgrep -x steam >/dev/null 2>&1 || break
        sleep 1
    done
    if pgrep -x steam >/dev/null 2>&1; then
        die "Steam did not exit; close it manually and re-run"
    fi
fi

if [[ -f "$dir/packaging/steam_shortcut.py" ]]; then
    python3 "$dir/packaging/steam_shortcut.py" uninstall --dir "$dir"
else
    echo "warning: steam_shortcut.py missing; remove the Playwake shortcut in Steam manually"
fi

rm -rf -- "$dir"
echo "Removed $dir"
echo "Kept sign-in state: $HOME/.local/state/ally-remote-play"

if [[ $restart_steam == 1 && $closed_steam == 1 ]]; then
    echo "Starting Steam..."
    nohup steam -silent >/dev/null 2>&1 &
    disown 2>/dev/null || true
fi