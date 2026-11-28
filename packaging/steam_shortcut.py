#!/usr/bin/env python3
"""Install the Playwake non-Steam shortcut and its Steam library artwork.

Stdlib only, so it runs on the SteamOS system python3. Steam must be closed
while shortcuts.vdf is written: Steam rewrites the file from memory on exit.

App ids follow Steam's own scheme (crc32 of Exe + AppName, high bit set):
the signed 32-bit value goes into shortcuts.vdf, the unsigned one names the
artwork files in userdata/<id>/config/grid.

The Steam Deck gets a Steam Input layout of its own for the shortcut
(packaging/steam_input_neptune.vdf): a gamepad for the games, with the right
trackpad as the mouse and the left one as the wheel for a PC stream. Steam
keys a shortcut's layout by its lowercased name in configset_controller_neptune.vdf.
"""

from __future__ import annotations

import argparse
import base64
import contextlib
import json
import os
import re
import shutil
import socket
import struct
import subprocess
import sys
import time
import urllib.request
import zlib
from pathlib import Path

APP_NAME = "Playwake"
LAUNCHER = "launch-steamos.sh"
STEAMID64_BASE = 76561197960265728
BACKUPS_KEPT = 3
LAYOUT = "packaging/steam_input_neptune.vdf"
LAYOUT_KEY = APP_NAME.lower()
CONFIGSET = "configset_controller_neptune.vdf"
# Steam reads the layout in config/<key>/controller_neptune.vdf for an
# "autosave" entry, and saves the user's edits to it there.
LAYOUT_ENTRY = f'\t"{LAYOUT_KEY}"\n\t{{\n\t\t"autosave"\t\t"1"\n\t}}\n'

# assets/branding source -> grid filename template ({id} = unsigned app id)
ARTWORK = {
    "steam-cover.png": "{id}p.png",
    "steam-hero.png": "{id}_hero.png",
    "icon.png": "{id}_icon.png",
}

BIN_MAP = 0x00
BIN_STRING = 0x01
BIN_INT32 = 0x02
BIN_UINT64 = 0x07
BIN_END = 0x08


def log(message: str) -> None:
    print(message, file=sys.stderr)


def fail(message: str) -> None:
    raise SystemExit(f"error: {message}")


class U64(int):
    """Keeps uint64 fields distinct from int32 so a rewrite stays lossless."""


def _cstring(buf: bytes, pos: int) -> tuple[str, int]:
    end = buf.index(b"\x00", pos)
    return buf[pos:end].decode("utf-8"), end + 1


def parse_vdf(buf: bytes) -> dict:
    root: dict = {}
    stack = [root]
    pos = 0
    while pos < len(buf):
        kind = buf[pos]
        pos += 1
        if kind == BIN_END:
            if len(stack) == 1:
                break
            stack.pop()
            continue
        key, pos = _cstring(buf, pos)
        if kind == BIN_MAP:
            child: dict = {}
            stack[-1][key] = child
            stack.append(child)
        elif kind == BIN_STRING:
            stack[-1][key], pos = _cstring(buf, pos)
        elif kind == BIN_INT32:
            stack[-1][key] = struct.unpack_from("<i", buf, pos)[0]
            pos += 4
        elif kind == BIN_UINT64:
            stack[-1][key] = U64(struct.unpack_from("<Q", buf, pos)[0])
            pos += 8
        else:
            fail(f"unsupported binary VDF type 0x{kind:02x} at offset {pos - 1}")
    return root


def dump_vdf(obj: dict) -> bytes:
    out = bytearray()

    def emit(mapping: dict) -> None:
        for key, value in mapping.items():
            encoded_key = key.encode("utf-8") + b"\x00"
            if isinstance(value, dict):
                out.append(BIN_MAP)
                out.extend(encoded_key)
                emit(value)
                out.append(BIN_END)
            elif isinstance(value, str):
                out.append(BIN_STRING)
                out.extend(encoded_key)
                out.extend(value.encode("utf-8") + b"\x00")
            elif isinstance(value, U64):
                out.append(BIN_UINT64)
                out.extend(encoded_key)
                out.extend(struct.pack("<Q", int(value)))
            elif isinstance(value, int):
                out.append(BIN_INT32)
                out.extend(encoded_key)
                out.extend(struct.pack("<i", value))
            else:
                fail(f"cannot serialize {key!r} of type {type(value).__name__}")

    emit(obj)
    # Steam requires the root map's closing byte; without it the client
    # rejects shortcuts.vdf ("failed to load shortcut file") and drops it.
    out.append(BIN_END)
    return bytes(out)


def parse_text_vdf(text: str) -> dict:
    tokens = [a or b for a, b in re.findall(r'"([^"]*)"|([{}])', text)]

    def walk(i: int) -> tuple[dict, int]:
        obj: dict = {}
        while i < len(tokens):
            if tokens[i] == "}":
                return obj, i + 1
            key = tokens[i]
            i += 1
            if i < len(tokens) and tokens[i] == "{":
                obj[key], i = walk(i + 1)
            elif i < len(tokens):
                obj[key] = tokens[i]
                i += 1
        return obj, i

    return walk(0)[0]


def scan_vdf(text: str) -> tuple[dict, int | None]:
    """The root map's entries that hold a map in a text VDF, by lowercased
    key, each as the span from its key to its closing brace, and where the
    root's closing brace is. Steam writes these files, so editing one entry
    in place keeps the rest of it byte for byte."""
    entries: dict = {}
    depth = 0
    named = None
    current = None
    for match in re.finditer(r'//[^\n]*|"((?:[^"\\]|\\.)*)"|([{}])', text):
        key, brace = match.group(1), match.group(2)
        if key is None and brace is None:
            continue
        if brace == "{":
            depth += 1
            if depth == 2 and named is not None:
                current = named
        elif brace == "}":
            depth -= 1
            if depth == 1 and current is not None:
                entries.setdefault(current[0], (current[1], match.end()))
                current = None
            elif depth == 0:
                return entries, match.start()
        named = (key.lower(), match.start()) if key is not None and depth == 1 else None
    return entries, None


def whole_lines(text: str, start: int, end: int) -> tuple[int, int]:
    """The span widened to whole lines when nothing else shares them."""
    line = text.rfind("\n", 0, start) + 1
    stop = text.find("\n", end)
    stop = len(text) if stop < 0 else stop
    if text[line:start].strip() or text[end:stop].strip():
        return start, end
    return line, min(stop + 1, len(text))


def install_layout(configs: Path, layout: Path, dry_run: bool) -> str:
    """Makes the shortcut's layout ours unless the user picked one; returns
    what was done: "added", "kept" or "missing" (no layout file to add)."""
    configset = configs / CONFIGSET
    text = configset.read_text(encoding="utf-8") if configset.is_file() else '"controller_config"\n{\n}\n'
    entries, close = scan_vdf(text)
    if LAYOUT_KEY in entries:
        return "kept"
    if not layout.is_file():
        log(f"layout missing, skipped: {layout}")
        return "missing"
    if close is None:
        fail(f"cannot read {configset}")
    if dry_run:
        return "added"
    target = configs / LAYOUT_KEY / "controller_neptune.vdf"
    target.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(layout, target)
    backup(configset)
    write_atomic(configset, (text[:close] + LAYOUT_ENTRY + text[close:]).encode("utf-8"))
    return "added"


def remove_layout(configs: Path, layout: Path, dry_run: bool) -> bool:
    """Drops the shortcut's layout entry, and the layout file if it is still
    the one the installer put there."""
    configset = configs / CONFIGSET
    if not configset.is_file():
        return False
    text = configset.read_text(encoding="utf-8")
    span = scan_vdf(text)[0].get(LAYOUT_KEY)
    if span is None:
        return False
    if not dry_run:
        start, end = whole_lines(text, *span)
        backup(configset)
        write_atomic(configset, (text[:start] + text[end:]).encode("utf-8"))
        ours = configs / LAYOUT_KEY / "controller_neptune.vdf"
        if ours.is_file() and layout.is_file() and ours.read_bytes() == layout.read_bytes():
            ours.unlink()
            with contextlib.suppress(OSError):
                ours.parent.rmdir()
    return True


def ids_from_top(top: int) -> dict:
    return {
        "signed": top - 0x100000000,
        "short": top,
        "long": (top << 32) | 0x02000000,
    }


def app_ids(exe: str, name: str = APP_NAME) -> dict:
    """exe must be the exact string stored in the VDF, quotes included."""
    return ids_from_top(zlib.crc32((exe + name).encode("utf-8")) | 0x80000000)


def steam_root(override: str | None) -> Path:
    if override:
        root = Path(override).expanduser().resolve()
    else:
        root = Path.home() / ".local/share/Steam"
        if not root.is_dir():
            legacy = Path.home() / ".steam/steam"
            if legacy.exists():
                root = legacy.resolve()
    if not (root / "userdata").is_dir():
        fail(f"Steam userdata not found under {root}; pass --steam-root")
    return root


def user_id(root: Path, override: str | None) -> str:
    if override:
        return str(override)
    loginusers = root / "config/loginusers.vdf"
    if loginusers.is_file():
        users = parse_text_vdf(loginusers.read_text(encoding="utf-8")).get("users", {})
        recent = [
            key
            for key, value in users.items()
            if isinstance(value, dict) and value.get("MostRecent") == "1"
        ]
        if len(recent) == 1 and recent[0].isdigit():
            candidate = str(int(recent[0]) - STEAMID64_BASE)
            if (root / "userdata" / candidate / "config").is_dir():
                return candidate
    candidates = sorted(
        path.name
        for path in (root / "userdata").iterdir()
        if path.name.isdigit() and (path / "config").is_dir()
    )
    if len(candidates) == 1:
        return candidates[0]
    fail(
        "cannot pick a Steam account "
        f"({', '.join(candidates) or 'none'}); pass --user-id <steamid32>"
    )


def steam_running() -> bool:
    try:
        return subprocess.run(["pgrep", "-x", "steam"], capture_output=True).returncode == 0
    except FileNotFoundError:
        return False


def find_entry(shortcuts: dict, signed_id: int) -> tuple[str | None, dict | None]:
    for key, entry in shortcuts.items():
        if isinstance(entry, dict) and entry.get("appid") == signed_id:
            return key, entry
    for key, entry in shortcuts.items():
        if isinstance(entry, dict) and entry.get("AppName") == APP_NAME:
            return key, entry
    return None, None


def build_entry(install_dir: Path, ids: dict, existing: dict | None) -> dict:
    fields = {
        "appid": ids["signed"],
        "AppName": APP_NAME,
        "Exe": f'"{install_dir / LAUNCHER}"',
        "StartDir": f'"{install_dir}"',
        "icon": str(install_dir / "assets/branding/icon.png"),
        "ShortcutPath": "",
        "LaunchOptions": "",
        "IsHidden": 0,
        "AllowDesktopConfig": 1,
        "AllowOverlay": 1,
        "OpenVR": 0,
        "Devkit": 0,
        "DevkitGameID": "",
        "DevkitOverrideAppID": 0,
        "LastPlayTime": 0,
        "FlatpakAppID": "",
        "tags": {},
    }
    if existing is None:
        return fields
    merged = dict(existing)
    for key, value in fields.items():
        if key in ("LastPlayTime", "tags") and key in existing:
            continue
        merged[key] = value
    return merged


def renumber(shortcuts: dict) -> dict:
    return {str(index): entry for index, entry in enumerate(shortcuts.values())}


def backup(path: Path) -> Path | None:
    if not path.is_file():
        return None
    stamp = time.strftime("%Y%m%d-%H%M%S")
    target = path.with_name(f"{path.name}.bak-{stamp}")
    suffix = 1
    while target.exists():
        target = path.with_name(f"{path.name}.bak-{stamp}-{suffix}")
        suffix += 1
    shutil.copy2(path, target)
    older = sorted(path.parent.glob(f"{path.name}.bak-*"))
    for stale in older[:-BACKUPS_KEPT]:
        stale.unlink()
    return target


def write_atomic(path: Path, payload: bytes) -> None:
    tmp = path.with_name(f"{path.name}.tmp-{os.getpid()}")
    tmp.write_bytes(payload)
    os.replace(tmp, path)


def install_artwork(grid_dir: Path, branding: Path, short_id: int, dry_run: bool) -> dict:
    written = {}
    for source, template in ARTWORK.items():
        src = branding / source
        if not src.is_file():
            log(f"artwork missing, skipped: {src}")
            continue
        dst = grid_dir / template.format(id=short_id)
        written[source] = str(dst)
        if dry_run:
            continue
        grid_dir.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dst)
    return written


def remove_artwork(grid_dir: Path, short_id: int, dry_run: bool) -> list[str]:
    removed = []
    for template in ARTWORK.values():
        path = grid_dir / template.format(id=short_id)
        if path.is_file():
            removed.append(str(path))
            if not dry_run:
                path.unlink()
    return removed


def load_shortcuts(vdf_path: Path) -> dict:
    if not vdf_path.is_file():
        return {"shortcuts": {}}
    data = parse_vdf(vdf_path.read_bytes())
    data.setdefault("shortcuts", {})
    return data


def save_shortcuts(vdf_path: Path, data: dict, dry_run: bool) -> str | None:
    if dry_run:
        return None
    saved = backup(vdf_path)
    vdf_path.parent.mkdir(parents=True, exist_ok=True)
    write_atomic(vdf_path, dump_vdf(data))
    return str(saved) if saved else None


def run(args: argparse.Namespace) -> dict:
    install_dir = Path(args.dir).expanduser().resolve()
    launcher = install_dir / LAUNCHER
    if args.action == "install" and not launcher.is_file():
        fail(f"{launcher} not found; run install.sh first")

    root = steam_root(args.steam_root)
    account = user_id(root, args.user_id)
    config = root / "userdata" / account / "config"
    vdf_path = config / "shortcuts.vdf"
    grid_dir = config / "grid"
    configs = root / "steamapps/common/Steam Controller Configs" / account / "config"

    exe = f'"{launcher}"'
    ids = app_ids(exe)
    data = load_shortcuts(vdf_path)
    shortcuts = data["shortcuts"]
    key, existing = find_entry(shortcuts, ids["signed"])
    if existing is not None and isinstance(existing.get("appid"), int):
        # Adopt a hand-added Playwake shortcut and keep the id Steam already knows.
        ids = ids_from_top(existing["appid"] & 0xFFFFFFFF)

    result = {
        "action": args.action,
        "steam_root": str(root),
        "user_id": account,
        "shortcuts_vdf": str(vdf_path),
        "grid_dir": str(grid_dir),
        "install_dir": str(install_dir),
        "exe": exe,
        "appid_signed": ids["signed"],
        "appid_short": ids["short"],
        "appid_long": ids["long"],
        "dry_run": args.dry_run,
    }

    if args.action == "install":
        if not args.dry_run and not args.force and steam_running():
            fail("Steam is running; close it first (install.sh does this) or pass --force")
        entry = build_entry(install_dir, ids, existing)
        shortcuts[key if key is not None else str(len(shortcuts))] = entry
        data["shortcuts"] = renumber(shortcuts)
        result["adopted"] = existing is not None
        result["backup"] = save_shortcuts(vdf_path, data, args.dry_run)
        result["artwork"] = install_artwork(
            grid_dir, install_dir / "assets/branding", ids["short"], args.dry_run
        )
        result["layout"] = install_layout(configs, install_dir / LAYOUT, args.dry_run)
    else:
        if key is not None:
            del shortcuts[key]
            data["shortcuts"] = renumber(shortcuts)
        result["removed_entry"] = key is not None
        result["backup"] = save_shortcuts(vdf_path, data, args.dry_run)
        result["artwork_removed"] = remove_artwork(grid_dir, ids["short"], args.dry_run)
        result["layout_removed"] = remove_layout(configs, install_dir / LAYOUT, args.dry_run)

    return result


def report(result: dict) -> None:
    if result["action"] == "install":
        verb = "would update" if result["dry_run"] else "updated"
        log(f"{verb} shortcut '{APP_NAME}' for Steam account {result['user_id']}")
        log(f"  appid: {result['appid_signed']} (grid {result['appid_short']})")
        log(f"  exe:   {result['exe']}")
        for source, target in result.get("artwork", {}).items():
            log(f"  art:   {source} -> {target}")
        log(f"  Steam Deck layout: {result.get('layout')}")
        if result.get("backup"):
            log(f"  backup: {result['backup']}")
    else:
        log(f"removed shortcut: {result['removed_entry']}")
        for path in result.get("artwork_removed", []):
            log(f"  art removed: {path}")
        log(f"  Steam Deck layout removed: {result.get('layout_removed')}")


CDP_PORT = 8080
REFRESH_ASSETS = (("steam-cover.png", 0), ("steam-hero.png", 1))


def _cdp_eval(expression: str, timeout: float = 30.0) -> str:
    """Evaluate JS in Steam's shared library context over the CEF debug port."""
    targets = json.load(urllib.request.urlopen(
        f"http://127.0.0.1:{CDP_PORT}/json/list", timeout=5))
    page = next(t for t in targets if t.get("title") == "SharedJSContext")
    host, rest = page["webSocketDebuggerUrl"].split("://", 1)[1].split("/", 1)
    hostname, port = host.split(":")
    sock = socket.create_connection((hostname, int(port)), timeout=timeout)
    key = base64.b64encode(os.urandom(16)).decode()
    sock.sendall((f"GET /{rest} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\n"
                  f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
                  f"Sec-WebSocket-Version: 13\r\n\r\n").encode())
    response = b""
    while b"\r\n\r\n" not in response:
        response += sock.recv(4096)
    if b"101" not in response.split(b"\r\n")[0]:
        fail("CEF debug handshake refused")

    def send_frame(payload: bytes) -> None:
        mask = os.urandom(4)
        header = bytearray([0x81])
        size = len(payload)
        if size < 126:
            header.append(0x80 | size)
        elif size < 65536:
            header.append(0x80 | 126)
            header += struct.pack(">H", size)
        else:
            header.append(0x80 | 127)
            header += struct.pack(">Q", size)
        sock.sendall(bytes(header + mask) +
                     bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def read_exact(count: int) -> bytes:
        buf = b""
        while len(buf) < count:
            chunk = sock.recv(count - len(buf))
            if not chunk:
                fail("CEF debug connection closed")
            buf += chunk
        return buf

    send_frame(json.dumps({"id": 1, "method": "Runtime.evaluate", "params": {
        "expression": expression, "awaitPromise": True, "returnByValue": True}}).encode())
    while True:
        first, second = read_exact(2)
        opcode, size = first & 0x0F, second & 0x7F
        if size == 126:
            size = struct.unpack(">H", read_exact(2))[0]
        elif size == 127:
            size = struct.unpack(">Q", read_exact(8))[0]
        payload = read_exact(size)
        if opcode == 1:
            message = json.loads(payload)
            if message.get("id") == 1:
                return json.dumps(message.get("result", {}))


def refresh_artwork(install_dir: Path, args: argparse.Namespace) -> dict:
    """Push cover/hero through Steam's own artwork API.

    The client caches its grid/ scan and does not rescan after an install
    replaces the files, leaving stale library placeholders. The API call is
    the same one Steam's custom-artwork UI uses; it rescans and notifies.
    """
    root = steam_root(args.steam_root)
    account = user_id(root, args.user_id)
    shortcuts = load_shortcuts(root / "userdata" / account / "config" / "shortcuts.vdf")["shortcuts"]
    _, entry = find_entry(shortcuts, 0)
    if entry is None or not isinstance(entry.get("appid"), int):
        fail("no Playwake shortcut to refresh artwork for")
    appid = entry["appid"] & 0xFFFFFFFF
    result = {"appid": appid, "assets": {}}
    for source, asset_type in REFRESH_ASSETS:
        src = install_dir / "assets/branding" / source
        if not src.is_file():
            continue
        encoded = base64.b64encode(src.read_bytes()).decode()
        expression = (f'SteamClient.Apps.SetCustomArtworkForApp({appid}, '
                      f'"{encoded}", "png", {asset_type})')
        last_error = None
        for attempt in range(6):
            try:
                result["assets"][source] = _cdp_eval(expression)
                break
            except (OSError, StopIteration, SystemExit) as error:
                last_error = error
                time.sleep(5)
        else:
            raise SystemExit(f"error: artwork refresh failed: {last_error}")
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["install", "uninstall", "refresh-artwork"])
    parser.add_argument("--dir", required=True, help="Playwake install directory")
    parser.add_argument("--steam-root", help="Steam root (default ~/.local/share/Steam)")
    parser.add_argument("--user-id", help="Steam userdata id32 (default: most recent login)")
    parser.add_argument("--force", action="store_true", help="write even if Steam is running")
    parser.add_argument("--dry-run", action="store_true", help="report changes, write nothing")
    parser.add_argument("--json", action="store_true", help="print the result as JSON on stdout")
    args = parser.parse_args(argv)

    if args.action == "refresh-artwork":
        result = refresh_artwork(Path(args.dir).expanduser().resolve(), args)
        if args.json:
            print(json.dumps(result, indent=2))
        else:
            names = ", ".join(result["assets"]) or "nothing to refresh"
            log(f"refreshed library artwork for appid {result['appid']}: {names}")
        return 0

    result = run(args)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        report(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())