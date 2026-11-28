#!/usr/bin/env python3
"""Unit tests for packaging/steam_shortcut.py (stdlib unittest, no device needed)."""

from __future__ import annotations

import contextlib
import io
import json
import re
import struct
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import steam_shortcut as sc

# Known-answer vector: exe (quoted) + "Playwake", crc32 | 0x80000000.
EXE = '"/home/deck/.local/share/playwake/launch-steamos.sh"'
IDS = {"signed": -1360613940, "short": 2934353356, "long": 12602951698961399808}

FIXTURE = (
    b"\x00shortcuts\x00"
    b"\x000\x00"
    b"\x02appid\x00" + struct.pack("<i", -598031679) +
    b"\x01AppName\x00Other Game\x00"
    b"\x00tags\x00"
    b"\x010\x00favorite\x00"
    b"\x08"
    b"\x07SomeU64\x00" + struct.pack("<Q", 2**40) +
    b"\x08"
    b"\x08"
    b"\x08"
)


def run_cli(argv: list[str]) -> dict:
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        code = sc.main(argv + ["--json"])
    if code != 0:
        raise AssertionError(f"exit {code}: {err.getvalue()}")
    return json.loads(out.getvalue())


class Tree:
    """A fake Steam root plus a Playwake install directory."""

    def __init__(self, base: Path, accounts: list[str] = ("123",)):
        self.root = base / "Steam"
        (self.root / "config").mkdir(parents=True)
        for account in accounts:
            (self.root / "userdata" / account / "config").mkdir(parents=True)
        self.app = base / "app"
        (self.app / "assets/branding").mkdir(parents=True)
        (self.app / sc.LAUNCHER).write_text("#!/bin/sh\n")
        for name in sc.ARTWORK:
            (self.app / "assets/branding" / name).write_bytes(b"png-" + name.encode())
        (self.app / "packaging").mkdir()
        (self.app / sc.LAYOUT).write_text('"controller_mappings"\n{\n}\n')
        self.root = self.root.resolve()
        self.app = self.app.resolve()

    @property
    def config(self) -> Path:
        return self.root / "userdata/123/config"

    @property
    def vdf(self) -> Path:
        return self.config / "shortcuts.vdf"

    def cli(self, action: str, *extra: str) -> dict:
        return run_cli(
            [action, "--dir", str(self.app), "--steam-root", str(self.root), *extra]
        )

    def entries(self) -> dict:
        return sc.parse_vdf(self.vdf.read_bytes())["shortcuts"]

    def write_shortcuts(self, entries: dict) -> None:
        self.vdf.write_bytes(sc.dump_vdf({"shortcuts": entries}))

    @property
    def configs(self) -> Path:
        return self.root / "steamapps/common/Steam Controller Configs/123/config"

    @property
    def configset(self) -> Path:
        return self.configs / sc.CONFIGSET


# What Steam writes: tabs, the layouts other apps use, our key's name inside another entry.
CONFIGSET = (
    '"controller_config"\n{\n'
    '\t"261640"\n\t{\n\t\t"template"\t\t"gamepad+mouse.vdf"\n\t}\n'
    '\t"moonlight"\n\t{\n\t\t"template"\t\t"controller_neptune_gamepad_fps.vdf"\n\t\t"playwake"\t\t"x"\n\t}\n'
    '}\n'
)
OUR_ENTRY = '\t"playwake"\n\t{\n\t\t"autosave"\t\t"1"\n\t}\n'


def vdf_pairs(text: str) -> list:
    """A text VDF as nested (key, value) lists, keeping repeated keys."""
    tokens = [m.group(1) if m.group(1) is not None else m.group(2)
              for m in re.finditer(r'"((?:[^"\\]|\\.)*)"|([{}])', text)]

    def walk(i: int) -> tuple[list, int]:
        pairs = []
        while i < len(tokens) and tokens[i] != "}":
            key, i = tokens[i], i + 1
            if tokens[i] == "{":
                value, i = walk(i + 1)
            else:
                value, i = tokens[i], i + 1
            pairs.append((key, value))
        return pairs, i + 1

    return walk(0)[0]


def values(pairs: list, key: str) -> list:
    return [value for name, value in pairs if name == key]


class AppIdTests(unittest.TestCase):
    def test_known_answer(self):
        self.assertEqual(sc.app_ids(EXE), IDS)

    def test_quotes_are_part_of_the_key(self):
        self.assertNotEqual(sc.app_ids(EXE), sc.app_ids(EXE.strip('"')))

    def test_signed_round_trip(self):
        self.assertEqual(sc.ids_from_top(IDS["signed"] & 0xFFFFFFFF), IDS)


class VdfTests(unittest.TestCase):
    def test_round_trip_is_byte_exact(self):
        parsed = sc.parse_vdf(FIXTURE)
        self.assertEqual(parsed["shortcuts"]["0"]["appid"], -598031679)
        self.assertEqual(parsed["shortcuts"]["0"]["tags"], {"0": "favorite"})
        self.assertIsInstance(parsed["shortcuts"]["0"]["SomeU64"], sc.U64)
        self.assertEqual(sc.dump_vdf(parsed), FIXTURE)

    def test_empty_file_is_empty_map(self):
        self.assertEqual(sc.dump_vdf({}), b"\x08")
        self.assertEqual(sc.parse_vdf(b""), {})
        self.assertEqual(sc.parse_vdf(b"\x08"), {})


class InstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tree = Tree(Path(self.tmp.name))

    def test_creates_shortcut_and_artwork(self):
        result = self.tree.cli("install")
        self.assertEqual(result["appid_signed"], sc.app_ids(f'"{self.tree.app / sc.LAUNCHER}"')["signed"])
        entry = self.tree.entries()["0"]
        self.assertEqual(entry["AppName"], "Playwake")
        self.assertEqual(entry["Exe"], f'"{self.tree.app / sc.LAUNCHER}"')
        self.assertEqual(entry["StartDir"], f'"{self.tree.app}"')
        self.assertEqual(entry["AllowOverlay"], 1)
        self.assertEqual(entry["icon"], str(self.tree.app / "assets/branding/icon.png"))
        short = result["appid_short"]
        grid = self.tree.config / "grid"
        self.assertEqual((grid / f"{short}p.png").read_bytes(), b"png-steam-cover.png")
        self.assertEqual((grid / f"{short}_hero.png").read_bytes(), b"png-steam-hero.png")
        self.assertEqual((grid / f"{short}_icon.png").read_bytes(), b"png-icon.png")

    def test_second_run_is_byte_identical(self):
        self.tree.cli("install")
        first = self.vdf_bytes()
        result = self.tree.cli("install")
        self.assertEqual(self.vdf_bytes(), first)
        self.assertTrue(result["adopted"])
        self.assertEqual(len(self.tree.entries()), 1)

    def vdf_bytes(self) -> bytes:
        return self.tree.vdf.read_bytes()

    def test_adopts_manual_shortcut_keeping_its_id(self):
        self.tree.write_shortcuts(
            {
                "0": {
                    "appid": -598031679,
                    "AppName": "Playwake",
                    "Exe": '"/old/path/playwake"',
                    "LastPlayTime": 1700000000,
                    "tags": {"0": "favorite"},
                    "FutureField": 42,
                }
            }
        )
        result = self.tree.cli("install")
        self.assertEqual(result["appid_signed"], -598031679)
        entries = self.tree.entries()
        self.assertEqual(len(entries), 1)
        entry = entries["0"]
        self.assertEqual(entry["LastPlayTime"], 1700000000)
        self.assertEqual(entry["tags"], {"0": "favorite"})
        self.assertEqual(entry["FutureField"], 42)
        self.assertEqual(entry["Exe"], f'"{self.tree.app / sc.LAUNCHER}"')
        self.assertTrue((self.tree.config / "grid" / f"{result['appid_short']}p.png").is_file())

    def test_keeps_other_shortcuts_and_renumbers(self):
        self.tree.write_shortcuts(
            {"0": {"appid": -1, "AppName": "Other"}, "1": {"appid": -2, "AppName": "Another"}}
        )
        self.tree.cli("install")
        entries = self.tree.entries()
        self.assertEqual([e["AppName"] for e in entries.values()], ["Other", "Another", "Playwake"])
        self.assertEqual(list(entries), ["0", "1", "2"])

    def test_backup_is_written_and_pruned(self):
        self.tree.write_shortcuts({"0": {"appid": -1, "AppName": "Other"}})
        for _ in range(5):
            self.tree.cli("install", "--force")
        backups = sorted(self.tree.config.glob("shortcuts.vdf.bak-*"))
        self.assertEqual(len(backups), sc.BACKUPS_KEPT)

    def test_dry_run_writes_nothing(self):
        result = self.tree.cli("install", "--dry-run")
        self.assertTrue(result["dry_run"])
        self.assertFalse(self.tree.vdf.exists())
        self.assertFalse((self.tree.config / "grid").exists())

    def test_refuses_while_steam_runs(self):
        with mock.patch.object(sc, "steam_running", return_value=True):
            with self.assertRaises(SystemExit):
                self.tree.cli("install")
            self.tree.cli("install", "--force")
        self.assertTrue(self.tree.vdf.is_file())

    def test_missing_launcher_fails(self):
        (self.tree.app / sc.LAUNCHER).unlink()
        with self.assertRaises(SystemExit):
            self.tree.cli("install")


class UninstallTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tree = Tree(Path(self.tmp.name))

    def test_removes_only_our_entry_and_artwork(self):
        self.tree.write_shortcuts({"0": {"appid": -1, "AppName": "Other"}})
        result = self.tree.cli("install")
        grid = self.tree.config / "grid"
        (grid / "999p.png").write_bytes(b"someone else")
        removed = self.tree.cli("uninstall")
        self.assertTrue(removed["removed_entry"])
        entries = self.tree.entries()
        self.assertEqual([e["AppName"] for e in entries.values()], ["Other"])
        self.assertEqual(list(entries), ["0"])
        self.assertFalse((grid / f"{result['appid_short']}p.png").exists())
        self.assertFalse((grid / f"{result['appid_short']}_hero.png").exists())
        self.assertFalse((grid / f"{result['appid_short']}_icon.png").exists())
        self.assertTrue((grid / "999p.png").is_file())

    def test_uninstall_without_shortcut_is_safe(self):
        result = self.tree.cli("uninstall")
        self.assertFalse(result["removed_entry"])
        self.assertEqual(result["artwork_removed"], [])


class LayoutTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.tree = Tree(Path(self.tmp.name))
        self.tree.configs.mkdir(parents=True)

    def test_install_adds_our_layout_after_the_others(self):
        self.tree.configset.write_text(CONFIGSET)
        self.assertEqual(self.tree.cli("install")["layout"], "added")
        self.assertEqual(self.tree.configset.read_text(), CONFIGSET[:-2] + OUR_ENTRY + "}\n")
        layout = self.tree.configs / "playwake/controller_neptune.vdf"
        self.assertEqual(layout.read_text(), (self.tree.app / sc.LAYOUT).read_text())
        self.assertEqual(len(list(self.tree.configs.glob(sc.CONFIGSET + ".bak-*"))), 1)

    def test_install_makes_the_configset_when_steam_has_none(self):
        self.tree.cli("install")
        self.assertEqual(self.tree.configset.read_text(), '"controller_config"\n{\n' + OUR_ENTRY + "}\n")

    def test_a_layout_the_user_picked_stays(self):
        picked = CONFIGSET[:-2] + '\t"Playwake"\n\t{\n\t\t"template"\t\t"controller_neptune_wasd.vdf"\n\t}\n}\n'
        self.tree.configset.write_text(picked)
        self.assertEqual(self.tree.cli("install")["layout"], "kept")
        self.assertEqual(self.tree.configset.read_text(), picked)
        self.assertFalse((self.tree.configs / "playwake").exists())

    def test_a_second_install_keeps_the_layout_steam_saved(self):
        self.tree.configset.write_text(CONFIGSET)
        self.tree.cli("install")
        layout = self.tree.configs / "playwake/controller_neptune.vdf"
        layout.write_text("edited in Steam")
        self.assertEqual(self.tree.cli("install")["layout"], "kept")
        self.assertEqual(self.tree.configset.read_text(), CONFIGSET[:-2] + OUR_ENTRY + "}\n")
        self.assertEqual(layout.read_text(), "edited in Steam")

    def test_uninstall_removes_only_our_entry_and_layout(self):
        self.tree.configset.write_text(CONFIGSET)
        self.tree.cli("install")
        self.assertTrue(self.tree.cli("uninstall")["layout_removed"])
        self.assertEqual(self.tree.configset.read_text(), CONFIGSET)
        self.assertFalse((self.tree.configs / "playwake").exists())
        self.assertFalse(self.tree.cli("uninstall")["layout_removed"])

    def test_dry_run_writes_no_layout(self):
        self.tree.cli("install", "--dry-run")
        self.assertFalse(self.tree.configset.exists())

    def test_shipped_layout_keeps_the_gamepad_and_makes_the_trackpads_a_mouse(self):
        layout = Path(__file__).resolve().parent / "steam_input_neptune.vdf"
        mappings = dict(vdf_pairs(layout.read_text(encoding="utf-8")))["controller_mappings"]
        self.assertEqual(values(mappings, "controller_type"), ["controller_neptune"])
        groups = {values(group, "id")[0]: group for group in values(mappings, "group")}
        sources = dict(values(values(mappings, "preset")[0], "group_source_bindings")[0])
        active = {source.split()[0]: groups[gid] for gid, source in sources.items() if source.endswith(" active")}

        def bindings(group: list) -> list:
            return [binding for _, inp in values(group, "inputs")[0]
                    for _, activator in values(inp, "activators")[0]
                    for bound in values(activator, "bindings") for binding in values(bound, "binding")]

        self.assertEqual(values(active["right_trackpad"], "mode"), ["absolute_mouse"])
        self.assertEqual(bindings(active["right_trackpad"]), ["mouse_button LEFT"])
        self.assertEqual(values(active["left_trackpad"], "mode"), ["scrollwheel"])
        self.assertIn("mouse_button RIGHT", bindings(active["left_trackpad"]))
        self.assertIn("mouse_wheel SCROLL_UP", bindings(active["left_trackpad"]))
        for source in ("left_trigger", "right_trigger"):
            self.assertEqual(values(active[source], "mode"), ["trigger"])
        self.assertEqual(bindings(active["button_diamond"]), [f"xinput_button {b}" for b in "ABXY"])
        self.assertEqual(values(active["joystick"], "mode"), ["joystick_move"])
        self.assertEqual(values(active["right_joystick"], "mode"), ["joystick_move"])

    def test_finds_only_a_root_entry_that_holds_a_map(self):
        text = '"c"\n{\n\t"a"\t\t"playwake"\n\t"playwake"\t\t"1"\n\t"b"\n\t{\n\t\t"playwake"\n\t\t{\n\t\t}\n\t}\n}\n'
        entries, close = sc.scan_vdf(text)
        self.assertEqual(list(entries), ["b"])
        self.assertEqual(close, len(text) - 2)
        start, end = sc.scan_vdf(CONFIGSET)[0]["moonlight"]
        self.assertTrue(CONFIGSET[start:end].startswith('"moonlight"') and CONFIGSET[start:end].endswith("}"))

    def test_uninstall_from_a_one_line_configset_keeps_the_others(self):
        self.tree.configset.write_text('"c" { "other" { "template" "x.vdf" } "Playwake" { "autosave" "1" } }')
        self.assertTrue(self.tree.cli("uninstall")["layout_removed"])
        self.assertEqual(self.tree.configset.read_text(), '"c" { "other" { "template" "x.vdf" }  }')

    def test_a_brace_in_a_comment_is_no_brace(self):
        commented = CONFIGSET[:-2] + "\t// a { in a comment\n}\n"
        self.tree.configset.write_text(commented)
        self.tree.cli("install")
        self.assertEqual(self.tree.configset.read_text(), commented[:-2] + OUR_ENTRY + "}\n")

    def test_uninstall_keeps_a_layout_the_user_edited(self):
        self.tree.cli("install")
        layout = self.tree.configs / "playwake/controller_neptune.vdf"
        layout.write_text("edited in Steam")
        self.tree.cli("uninstall")
        self.assertNotIn("playwake", self.tree.configset.read_text().lower())
        self.assertEqual(layout.read_text(), "edited in Steam")


class AccountTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)

    def test_most_recent_login_wins(self):
        tree = Tree(self.base, accounts=("123", "456"))
        (tree.root / "config/loginusers.vdf").write_text(
            '"users"\n{\n'
            '  "76561197960265851"\n  {\n    "MostRecent"\t\t"0"\n  }\n'
            '  "76561197960266184"\n  {\n    "MostRecent"\t\t"1"\n  }\n}\n'
        )
        self.assertEqual(sc.user_id(tree.root, None), "456")

    def test_single_account_needs_no_loginusers(self):
        tree = Tree(self.base)
        self.assertEqual(sc.user_id(tree.root, None), "123")

    def test_ambiguous_accounts_fail(self):
        tree = Tree(self.base, accounts=("123", "456"))
        with self.assertRaises(SystemExit):
            sc.user_id(tree.root, None)
        self.assertEqual(sc.user_id(tree.root, "456"), "456")

    def test_override_is_used_verbatim(self):
        tree = Tree(self.base)
        self.assertEqual(sc.user_id(tree.root, "777"), "777")


if __name__ == "__main__":
    unittest.main()