#!/usr/bin/env python3
"""Tests for tools/macos/bundle.sh (fake binaries, a stub otool; no game files):

  python tools/test_bundle.py
"""
import os, plistlib, shutil, stat, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
BUNDLE = os.path.join(HERE, "macos", "bundle.sh")


@unittest.skipUnless(sys.platform == "darwin" and os.path.isfile(BUNDLE), "macOS only (bundle.sh, codesign)")
class Bundle(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.rel = os.path.join(self.d, "rel")
        for sub in ("mods/fc27", "assets/glyphs/ps5", "source/lma-rs", "FFmpeg-relink"):
            os.makedirs(os.path.join(self.rel, sub))
        for name in ("lma-app", "lma-cli"):
            p = os.path.join(self.rel, name)
            open(p, "w").write("#!/bin/sh\n")
            os.chmod(p, 0o755)
        files = {"mods/menu.txt": "fc27|FC 27|x\n", "mods/fc27/mod.toml": "base_year = 2026\n",
                 "assets/glyphs/ps5/a.png": "png", "source/lma-rs/Cargo.toml": "[workspace]\n",
                 "FFmpeg-relink/ffmpeg_min.sh": "#!/bin/sh\n", "LICENSE-FFmpeg.txt": "LGPL\n", "README-mac.txt": "read me\n"}
        for rel, text in files.items():
            open(os.path.join(self.rel, rel), "w").write(text)
        # otool -L: the stub prints what LMA_TEST_OTOOL holds (a Homebrew library or nothing)
        self.bin = os.path.join(self.d, "bin")
        os.makedirs(self.bin)
        stub = os.path.join(self.bin, "otool")
        open(stub, "w").write('#!/bin/sh\necho "$2:"\n[ -n "$LMA_TEST_OTOOL" ] && echo "\t$LMA_TEST_OTOOL (compatibility version 1.0.0)"\nexit 0\n')
        os.chmod(stub, 0o755)

    def tearDown(self):
        shutil.rmtree(self.d, ignore_errors=True)

    def run_bundle(self, **env):
        e = dict(os.environ, PATH=self.bin + os.pathsep + os.environ["PATH"], **env)
        e.pop("LMA_BUNDLE_ICON", None)
        out = os.path.join(self.d, "out")
        os.makedirs(out, exist_ok=True)
        r = subprocess.run(["bash", BUNDLE, self.rel, out, "1.0-test"], env=e, capture_output=True, text=True)
        return r, os.path.join(out, "LMA.app")

    def test_layout_plist_source_and_no_icon(self):
        r, app = self.run_bundle()
        self.assertEqual(r.returncode, 0, r.stderr)
        c = os.path.join(app, "Contents")
        for rel in ("MacOS/lma-app", "MacOS/lma-cli", "Resources/mods/menu.txt", "Resources/mods/fc27/mod.toml",
                    "Resources/assets/glyphs/ps5/a.png", "Resources/source/lma-rs/Cargo.toml",
                    "Resources/FFmpeg-relink/ffmpeg_min.sh", "Resources/LICENSE-FFmpeg.txt", "Resources/README-mac.txt"):
            self.assertTrue(os.path.isfile(os.path.join(c, rel)), rel)
        self.assertTrue(os.stat(os.path.join(c, "MacOS/lma-app")).st_mode & stat.S_IXUSR, "lma-app stays executable")
        plist = plistlib.load(open(os.path.join(c, "Info.plist"), "rb"))
        self.assertEqual(plist["CFBundleExecutable"], "lma-app")
        self.assertEqual(plist["CFBundleShortVersionString"], "1.0-test")
        # no drawn icon (the maintainer, 28 Oct): only with LMA_BUNDLE_ICON
        self.assertNotIn("CFBundleIconFile", plist)
        self.assertFalse(os.path.exists(os.path.join(c, "Resources/LMA.icns")))

    def test_a_release_folder_without_source_still_bundles(self):
        shutil.rmtree(os.path.join(self.rel, "source"))
        r, app = self.run_bundle()
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(os.path.exists(os.path.join(app, "Contents/Resources/source")))
        self.assertTrue(os.path.isdir(os.path.join(app, "Contents/Resources/mods")))

    def test_homebrew_ffmpeg_is_refused_unless_local(self):
        lib = "/opt/homebrew/opt/ffmpeg/lib/libavcodec.62.dylib"
        r, app = self.run_bundle(LMA_TEST_OTOOL=lib)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Homebrew", r.stderr)
        self.assertFalse(os.path.exists(app), "no bundle for other Macs from a Homebrew-linked build")
        r, app = self.run_bundle(LMA_TEST_OTOOL=lib, LMA_BUNDLE_LOCAL="1")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("runs only where they are installed", r.stderr)

    def test_no_lma_app_is_refused(self):
        os.remove(os.path.join(self.rel, "lma-app"))
        r, _ = self.run_bundle()
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("no lma-app", r.stderr)


if __name__ == "__main__":
    unittest.main()
