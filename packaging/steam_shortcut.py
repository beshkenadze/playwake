#!/usr/bin/env python3
"""Install the Playwake non-Steam shortcut and its Steam library artwork.

Stdlib only, so it runs on the SteamOS system python3. Steam must be closed
while shortcuts.vdf is written: Steam rewrites the file from memory on exit.

App ids follow Steam's own scheme (crc32 of Exe + AppName, high bit set):
the signed 32-bit value goes into shortcuts.vdf, the unsigned one names the
artwork files in userdata/<id>/config/grid.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

APP_NAME = "Playwake"
LAUNCHER = "launch-steamos.sh"
STEAMID64_BASE = 76561197960265728
BACKUPS_KEPT = 3

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
    else:
        if key is not None:
            del shortcuts[key]
            data["shortcuts"] = renumber(shortcuts)
        result["removed_entry"] = key is not None
        result["backup"] = save_shortcuts(vdf_path, data, args.dry_run)
        result["artwork_removed"] = remove_artwork(grid_dir, ids["short"], args.dry_run)

    return result


def report(result: dict) -> None:
    if result["action"] == "install":
        verb = "would update" if result["dry_run"] else "updated"
        log(f"{verb} shortcut '{APP_NAME}' for Steam account {result['user_id']}")
        log(f"  appid: {result['appid_signed']} (grid {result['appid_short']})")
        log(f"  exe:   {result['exe']}")
        for source, target in result.get("artwork", {}).items():
            log(f"  art:   {source} -> {target}")
        if result.get("backup"):
            log(f"  backup: {result['backup']}")
    else:
        log(f"removed shortcut: {result['removed_entry']}")
        for path in result.get("artwork_removed", []):
            log(f"  art removed: {path}")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=["install", "uninstall"])
    parser.add_argument("--dir", required=True, help="Playwake install directory")
    parser.add_argument("--steam-root", help="Steam root (default ~/.local/share/Steam)")
    parser.add_argument("--user-id", help="Steam userdata id32 (default: most recent login)")
    parser.add_argument("--force", action="store_true", help="write even if Steam is running")
    parser.add_argument("--dry-run", action="store_true", help="report changes, write nothing")
    parser.add_argument("--json", action="store_true", help="print the result as JSON on stdout")
    args = parser.parse_args(argv)

    result = run(args)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        report(result)
    return 0


if __name__ == "__main__":
    sys.exit(main())