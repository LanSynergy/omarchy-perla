import unittest

from omarchy_harness.allowlist import ScriptError, looks_like_polkit, plan
from omarchy_harness.oma import combo_to_wtype


class AllowlistTests(unittest.TestCase):
    def test_theme(self):
        program, args = plan('omarchy-theme-set "Tokyo Night"')
        self.assertEqual(program, "omarchy-theme-set")
        self.assertEqual(args, ["Tokyo Night"])

    def test_workspace(self):
        program, args = plan("hyprctl dispatch workspace 3")
        self.assertEqual(program, "hyprctl")
        self.assertEqual(args[0], "dispatch")

    def test_rejects_exec(self):
        with self.assertRaises(ScriptError):
            plan("hyprctl dispatch exec kitty")

    def test_rejects_shell(self):
        with self.assertRaises(ScriptError):
            plan("bash -c 'reboot'")

    def test_shutdown_needs_flag(self):
        with self.assertRaises(ScriptError):
            plan("omarchy-system-shutdown")
        program, _ = plan("omarchy-system-shutdown", allow_destructive=True)
        self.assertEqual(program, "omarchy-system-shutdown")

    def test_omarchy_shell(self):
        plan("omarchy-shell shell summon omarchy.menu '{}'")
        with self.assertRaises(ScriptError):
            plan("omarchy-shell shell rescanPlugins")

    def test_polkit(self):
        self.assertTrue(looks_like_polkit("hyprpolkitagent"))
        self.assertFalse(looks_like_polkit("kitty"))

    def test_combo(self):
        self.assertEqual(
            combo_to_wtype("ctrl+k"),
            ["-M", "ctrl", "-k", "k", "-m", "ctrl"],
        )
        self.assertIn("Escape", combo_to_wtype("esc"))


if __name__ == "__main__":
    unittest.main()
