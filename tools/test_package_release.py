#!/usr/bin/env python3
"""Tests for tools/package_release.py (fake binaries, no game files):

  python tools/test_package_release.py
"""
import os, stat, sys, tempfile, unittest, zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import package_release  # noqa: E402


class Package(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.bin = os.path.join(self.d, "bin")
        os.makedirs(self.bin)
        for n in ("lma-app", "lma-cli"):
            open(os.path.join(self.bin, n), "wb").write(b"\xcf\xfa\xed\xfe" + b"\0" * 32)
        mods = os.path.join(self.d, "mods.zip")
        with zipfile.ZipFile(mods, "w") as z:
            z.writestr("mods\\menu.txt", "fc27|FC 27|x\n")  # rc1's Windows zip had backslashes
            z.writestr("mods\\fc27\\mod.toml", "base_year = 2026\n")
        self.mods = mods
        self.readme = os.path.join(self.d, "README-mac.txt")
        open(self.readme, "w").write("read me\n")

    def test_mac_layout_with_exec_bits_and_glyphs(self):
        out = os.path.join(self.d, "mac.zip")
        self.assertEqual(package_release.main(["--out", out, "--root", "lma-x-mac", "--bin", self.bin, "--mods", self.mods, "--file", self.readme]), 0)
        z = zipfile.ZipFile(out)
        names = set(z.namelist())
        for n in ("lma-x-mac/lma-app", "lma-x-mac/lma-cli", "lma-x-mac/README-mac.txt", "lma-x-mac/mods/menu.txt", "lma-x-mac/mods/fc27/mod.toml"):
            self.assertIn(n, names)
        self.assertTrue(any(n.startswith("lma-x-mac/assets/glyphs/ps5/") and n.endswith(".png") for n in names))
        self.assertFalse(any("\\" in n for n in names), "forward slashes only")
        mode = z.getinfo("lma-x-mac/lma-app").external_attr >> 16
        self.assertTrue(mode & stat.S_IXUSR, "lma-app stays executable")

    def test_flat_windows_layout(self):
        for n in ("lma-app", "lma-cli"):
            os.rename(os.path.join(self.bin, n), os.path.join(self.bin, n + ".exe"))
        out = os.path.join(self.d, "win.zip")
        self.assertEqual(package_release.main(["--out", out, "--bin", self.bin, "--mods", self.mods]), 0)
        names = set(zipfile.ZipFile(out).namelist())
        self.assertIn("lma-app.exe", names)
        self.assertIn("mods/menu.txt", names)

    def test_game_content_is_refused(self):
        state = os.path.join(self.d, "quick.state")
        open(state, "wb").write(b"s")
        out = os.path.join(self.d, "bad.zip")
        self.assertEqual(package_release.main(["--out", out, "--bin", self.bin, "--file", state]), 1)
        self.assertFalse(os.path.exists(out), "a zip with game content is deleted")


if __name__ == "__main__":
    unittest.main()
