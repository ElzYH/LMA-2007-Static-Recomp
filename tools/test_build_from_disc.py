#!/usr/bin/env python3
"""Tests for tools/build_from_disc.py with cargo, the recompiler and git stubbed out (no disc, no
build):

  python tools/test_build_from_disc.py
"""
import contextlib, io, os, sys, tempfile, unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import build_from_disc as bfd  # noqa: E402


class Flow(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.addCleanup(self.t.cleanup)
        self.d = self.t.name
        self.runs, self.git = [], []
        self.cfg = os.path.join(self.d, "cfg", "lma-rs", "config.toml")
        for p in (mock.patch.object(bfd, "run", lambda cmd, cwd=bfd.RS, env=None: self.runs.append((cmd, env))),
                  mock.patch.object(bfd.subprocess, "run", lambda cmd, cwd=None: self.git.append(cmd)),
                  mock.patch.object(bfd.shutil, "which", lambda name: "/bin/" + name),
                  mock.patch.object(bfd, "config_path", lambda: self.cfg)):
            p.start()
            self.addCleanup(p.stop)

    def main(self, *argv):
        out = io.StringIO()
        with mock.patch.object(sys, "argv", ["build_from_disc.py", *argv]), contextlib.redirect_stdout(out):
            bfd.main()
        return out.getvalue()

    def folder(self, xex="default.xex"):
        game = os.path.join(self.d, "game")
        os.makedirs(game, exist_ok=True)
        open(os.path.join(game, xex), "wb").close()
        return game

    def test_an_extracted_folder_is_recompiled_built_and_configured(self):
        game = self.folder("DEFAULT.XEX")
        out = self.main(game, "--jobs", "3")
        cmds = [c for c, _ in self.runs]
        self.assertEqual(cmds[0], ["cargo", "build", "--release", "-p", "recomp"])
        self.assertEqual(cmds[1][1:], [os.path.join(game, "DEFAULT.XEX"), "all", "crates/game-blocks", "999999", "32"])
        self.assertEqual(cmds[2], ["cargo", "build", "--release", "-p", "lma-cli", "-p", "lma-app"])
        self.assertTrue(all(env["CARGO_BUILD_JOBS"] == "3" for _, env in self.runs))
        with open(self.cfg, encoding="utf-8") as f:
            self.assertEqual(f.read(), 'game_dir = "' + game.replace("\\", "\\\\") + '"\n')
        self.assertIn("wrote", out)

    def test_an_existing_config_is_kept(self):
        game = self.folder()
        os.makedirs(os.path.dirname(self.cfg))
        with open(self.cfg, "w") as f:
            f.write('game_dir = "/elsewhere"\n')
        self.assertIn("kept your", self.main(game))
        with open(self.cfg) as f:
            self.assertEqual(f.read(), 'game_dir = "/elsewhere"\n')

    def test_an_iso_is_extracted_into_the_game_dir_first(self):
        iso = os.path.join(self.d, "lma.iso")
        open(iso, "wb").close()
        game = os.path.join(self.d, "out")
        with self.assertRaises(SystemExit) as e:  # the stub extracts nothing, so no default.xex
            self.main(iso, "--game-dir", game)
        self.assertEqual(self.runs[0][0][1:], [os.path.join(bfd.ROOT, "tools", "xiso.py"), iso, "extract", game])
        self.assertIn("no default.xex", str(e.exception.code))

    def test_an_iso_with_the_files_already_extracted_skips_extraction(self):
        game = self.folder()
        iso = os.path.join(self.d, "lma.iso")
        open(iso, "wb").close()
        self.assertIn("using the files already extracted", self.main(iso, "--game-dir", game))
        self.assertEqual(self.runs[0][0][:2], ["cargo", "build"])

    def test_missing_source_and_missing_cargo(self):
        with self.assertRaises(SystemExit) as e:
            self.main(os.path.join(self.d, "nope.iso"))
        self.assertIn("doesn't exist", str(e.exception.code))
        with mock.patch.object(bfd.shutil, "which", lambda name: None), self.assertRaises(SystemExit) as e:
            self.main(self.folder())
        self.assertIn("rustup.rs", str(e.exception.code))
        self.assertEqual(self.runs, [])

    def test_placeholders_hidden_from_git_in_a_clone_and_a_worktree(self):
        game = self.folder()
        for kind in ("dir", "file", None):
            self.git.clear()
            with mock.patch.object(bfd, "ROOT", self.d):
                git = os.path.join(self.d, ".git")
                if kind == "dir":
                    os.makedirs(git)
                elif kind == "file":
                    os.rmdir(git)
                    with open(git, "w") as f:
                        f.write("gitdir: /x/.git/worktrees/w\n")
                else:
                    os.remove(git)
                self.main(game)
            want = [["git", "update-index", "--skip-worktree", "crates/game-blocks/Cargo.toml", "crates/game-blocks/src/lib.rs"]] if kind else []
            self.assertEqual(self.git, want, kind)


class Helpers(unittest.TestCase):
    def test_find_xex_ignores_case(self):
        with tempfile.TemporaryDirectory() as d:
            self.assertIsNone(bfd.find_xex(d))
            open(os.path.join(d, "Default.Xex"), "wb").close()
            self.assertEqual(bfd.find_xex(d), os.path.join(d, "Default.Xex"))

    def test_config_path_per_platform(self):
        with mock.patch.object(bfd.sys, "platform", "darwin"):
            self.assertTrue(bfd.config_path().endswith(os.path.join("Library", "Application Support", "lma-rs", "config.toml")))
        if os.name != "nt":
            with mock.patch.object(bfd.sys, "platform", "linux"), mock.patch.dict(os.environ, {"XDG_CONFIG_HOME": "/xdg"}):
                self.assertEqual(bfd.config_path(), "/xdg/lma-rs/config.toml")


if __name__ == "__main__":
    unittest.main()
