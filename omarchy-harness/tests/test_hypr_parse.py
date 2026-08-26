import json
import subprocess
import unittest
from types import SimpleNamespace

from omarchy_harness.cli import doctor
from omarchy_harness.hypr import compact_desktop, find_window
from omarchy_harness.oma import Oma


CLIENTS = [
    {
        "address": "0x1",
        "class": "kitty",
        "title": "perla",
        "workspace": {"id": 1},
        "mapped": True,
        "focusHistoryID": 0,
        "at": [10, 20],
        "size": [800, 600],
    },
    {
        "address": "0x2",
        "class": "spotify",
        "title": "Spotify",
        "workspace": {"id": 2},
        "mapped": True,
        "focusHistoryID": 1,
        "at": [100, 100],
        "size": [400, 300],
    },
]


class HyprParseTests(unittest.TestCase):
    def test_compact_and_find(self):
        desk = compact_desktop(CLIENTS, [{"id": 1}], [{"scale": 1.0}], CLIENTS[0], {"x": 1, "y": 2})
        self.assertEqual(len(desk["windows"]), 2)
        self.assertEqual(find_window(desk, "spotify")["address"], "0x2")
        self.assertEqual(find_window(desk, None)["class"], "kitty")

    def test_desktop_with_fake_run(self):
        payloads = {
            ("hyprctl", "-j", "clients"): json.dumps(CLIENTS),
            ("hyprctl", "-j", "workspaces"): "[]",
            ("hyprctl", "-j", "monitors"): "[]",
            ("hyprctl", "-j", "activewindow"): json.dumps(CLIENTS[0]),
            ("hyprctl", "cursorpos"): "12, 34",
        }

        def run(argv):
            key = tuple(argv)
            stdout = payloads.get(key, "")
            return SimpleNamespace(returncode=0, stdout=stdout, stderr="")

        oma = Oma(run=run)
        desk = oma.desktop()
        self.assertEqual(desk["cursor"], {"x": 12, "y": 34})
        self.assertEqual(desk["windows"][0]["geometry"]["w"], 800)

    def test_doctor_does_not_crash(self):
        code = doctor()
        self.assertIn(code, (0, 1))


if __name__ == "__main__":
    unittest.main()
