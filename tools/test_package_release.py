#!/usr/bin/env python3
"""Tests for tools/package_release.py (fake binaries, no game files):

  python tools/test_package_release.py
"""
import os, stat, subprocess, sys, tempfile, unittest, zipfile

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

    def test_ffmpeg_dlls_follow_the_imports(self):
        # lma-app.exe imports avcodec; avcodec imports avutil and swresample; avformat isn't wanted
        for n in ("lma-app", "lma-cli"):
            os.rename(os.path.join(self.bin, n), os.path.join(self.bin, n + ".exe"))
        with open(os.path.join(self.bin, "lma-app.exe"), "ab") as f:
            f.write(b"\0KERNEL32.dll\0avcodec-62.dll\0")
        ff = os.path.join(self.d, "ffmpeg-lgpl-shared")
        os.makedirs(os.path.join(ff, "bin"))
        dlls = {"avcodec-62.dll": b"MZ avutil-60.dll swresample-6.dll", "avutil-60.dll": b"MZ",
                "swresample-6.dll": b"MZ avutil-60.dll", "avformat-62.dll": b"MZ", "ffmpeg.exe": b"MZ"}
        for n, data in dlls.items():
            open(os.path.join(ff, "bin", n), "wb").write(data)
        open(os.path.join(ff, "LICENSE.txt"), "w").write("GNU LESSER GENERAL PUBLIC LICENSE\n")
        out = os.path.join(self.d, "win-ff.zip")
        self.assertEqual(package_release.main(["--out", out, "--bin", self.bin, "--ffmpeg", ff]), 0)
        z = zipfile.ZipFile(out)
        names = set(z.namelist())
        self.assertTrue({"avcodec-62.dll", "avutil-60.dll", "swresample-6.dll", "LICENSE-FFmpeg.txt"} <= names, names)
        self.assertFalse({"avformat-62.dll", "ffmpeg.exe"} & names, "only what the binaries need")
        licence = z.read("LICENSE-FFmpeg.txt").decode()
        self.assertIn("DLLs: avcodec-62.dll, avutil-60.dll, swresample-6.dll", licence)
        self.assertIn("GNU LESSER GENERAL PUBLIC LICENSE", licence)

    def test_ffmpeg_without_its_licence_is_refused(self):
        with open(os.path.join(self.bin, "lma-app"), "ab") as f:
            f.write(b"avutil-60.dll")
        ff = os.path.join(self.d, "ff-nolicence")
        os.makedirs(ff)
        open(os.path.join(ff, "avutil-60.dll"), "wb").write(b"MZ")
        out = os.path.join(self.d, "x.zip")
        with self.assertRaises(SystemExit):
            package_release.main(["--out", out, "--bin", self.bin, "--ffmpeg", ff])
        self.assertFalse(os.path.exists(out), "no zip without the licence")

    def test_ffmpeg_static_ships_the_licence_and_recipe_not_dlls(self):
        # Route 1 (#827): binaries with FFmpeg linked in; ffmpeg_min.sh's output holds the licence
        lib = os.path.join(self.d, "ffmpeg-min")
        os.makedirs(lib)
        open(os.path.join(lib, "COPYING.LGPLv2.1"), "w").write("GNU LESSER GENERAL PUBLIC LICENSE Version 2.1\n")
        open(os.path.join(lib, "VERSION"), "w").write("8.1\n")
        recipe = os.path.join(self.d, "ffmpeg_min.sh")
        open(recipe, "w").write("#!/usr/bin/env bash\n")
        old, package_release.RECIPE = package_release.RECIPE, recipe
        try:
            out = os.path.join(self.d, "win-static.zip")
            self.assertEqual(package_release.main(["--out", out, "--bin", self.bin, "--ffmpeg-static", lib]), 0)
            z = zipfile.ZipFile(out)
            names = set(z.namelist())
            self.assertTrue({"LICENSE-FFmpeg.txt", "FFmpeg-relink/ffmpeg_min.sh"} <= names, names)
            self.assertFalse([n for n in names if n.lower().endswith(".dll")], "no DLLs")
            licence = z.read("LICENSE-FFmpeg.txt").decode()
            self.assertIn("linked statically", licence)
            self.assertIn("Version: FFmpeg 8.1", licence)
            self.assertIn("GNU LESSER GENERAL PUBLIC LICENSE", licence)
            # a binary that still imports an FFmpeg DLL is refused, before any zip is written
            with open(os.path.join(self.bin, "lma-app"), "ab") as f:
                f.write(b"avcodec-62.dll")
            out2 = os.path.join(self.d, "win-static2.zip")
            with self.assertRaises(SystemExit):
                package_release.main(["--out", out2, "--bin", self.bin, "--ffmpeg-static", lib])
            self.assertFalse(os.path.exists(out2))
            # and so is a library folder without its licence
            os.remove(os.path.join(lib, "COPYING.LGPLv2.1"))
            with self.assertRaises(SystemExit):
                package_release.ffmpeg_static_files([], lib)
        finally:
            package_release.RECIPE = old

    def test_source_is_the_tracked_files_at_the_ref_with_ffmpeg_min(self):
        # the first-run build's input (#232): git archive, so ignored and untracked files stay out
        repo = os.path.join(self.d, "repo")
        os.makedirs(os.path.join(repo, "lma-rs", "tools"))
        git = lambda *a: subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *a], cwd=repo, check=True, capture_output=True)
        git("init", "-q")
        open(os.path.join(repo, "lma-rs", "Cargo.toml"), "w").write("[workspace]\n")
        open(os.path.join(repo, "lma-rs", "tools", "run.sh"), "w").write("#!/bin/sh\n")
        os.chmod(os.path.join(repo, "lma-rs", "tools", "run.sh"), 0o755)
        open(os.path.join(repo, ".gitignore"), "w").write("parts/\n")
        git("add", ".")
        git("commit", "-qm", "x")
        os.makedirs(os.path.join(repo, "parts"))
        open(os.path.join(repo, "parts", "gb00.rs"), "w").write("ignored\n")
        open(os.path.join(repo, "untracked.txt"), "w").write("local\n")
        lib = os.path.join(self.d, "ffmpeg-min")
        for sub, f in (("include/libavcodec", "avcodec.h"), ("lib", "libavcodec.a")):
            os.makedirs(os.path.join(lib, sub))
            open(os.path.join(lib, sub, f), "w").write("x\n")
        open(os.path.join(lib, "COPYING.LGPLv2.1"), "w").write("LGPL\n")
        recipe = os.path.join(self.d, "ffmpeg_min.sh")
        open(recipe, "w").write("#!/usr/bin/env bash\n")
        old = package_release.REPO, package_release.RECIPE
        package_release.REPO, package_release.RECIPE = repo, recipe
        try:
            out = os.path.join(self.d, "src.zip")
            self.assertEqual(package_release.main(["--out", out, "--root", "r", "--bin", self.bin, "--ffmpeg-static", lib, "--source", "HEAD"]), 0)
            z = zipfile.ZipFile(out)
            names = set(z.namelist())
            for n in ("r/source/lma-rs/Cargo.toml", "r/source/.gitignore", "r/source/ffmpeg-min/include/libavcodec/avcodec.h", "r/source/ffmpeg-min/lib/libavcodec.a"):
                self.assertIn(n, names)
            self.assertFalse([n for n in names if "parts/" in n or "untracked" in n], names)
            self.assertTrue((z.getinfo("r/source/lma-rs/tools/run.sh").external_attr >> 16) & stat.S_IXUSR, "scripts stay executable")
            with self.assertRaises(SystemExit):  # an unknown ref, before any zip is written
                package_release.main(["--out", os.path.join(self.d, "bad.zip"), "--bin", self.bin, "--source", "no-such-ref"])
            self.assertFalse(os.path.exists(os.path.join(self.d, "bad.zip")))
        finally:
            package_release.REPO, package_release.RECIPE = old


if __name__ == "__main__":
    unittest.main()
