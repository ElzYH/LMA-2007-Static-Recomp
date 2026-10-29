#!/usr/bin/env python3
"""Tests for tools/keep_endings.py on a throwaway git repo:

  python tools/test_keep_endings.py
"""
import os, subprocess, sys, tempfile, unittest

TOOL = os.path.join(os.path.dirname(os.path.abspath(__file__)), "keep_endings.py")


class KeepEndings(unittest.TestCase):
    def run_on(self, base, edited):
        """Commit `base`, overwrite it with `edited` (what an editor wrote), run the tool; the file's bytes."""
        with tempfile.TemporaryDirectory() as d:
            git = lambda *a: subprocess.run(["git", "-C", d, *a], check=True, capture_output=True)
            git("init", "-q")
            git("config", "core.autocrlf", "false")
            p = os.path.join(d, "f.txt")
            open(p, "wb").write(base)
            git("add", "f.txt")
            git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "base")
            open(p, "wb").write(edited)
            subprocess.run([sys.executable, TOOL, d, "HEAD", "f.txt"], check=True, capture_output=True)
            return open(p, "rb").read()

    def test_unchanged_lines_keep_their_own_endings(self):
        base = b"a\r\nb\nc\r\n"
        self.assertEqual(self.run_on(base, b"a\nb\nc\n"), base)

    def test_a_changed_line_takes_the_ending_it_replaces(self):
        self.assertEqual(self.run_on(b"a\r\nb\nc\r\n", b"a\nB\nC\n"), b"a\r\nB\nC\r\n")

    def test_an_inserted_line_takes_the_ending_above_it(self):
        self.assertEqual(self.run_on(b"a\r\nb\n", b"a\nnew\nb\n"), b"a\r\nnew\r\nb\n")

    def test_an_insert_at_the_top_takes_the_first_lines_ending(self):
        self.assertEqual(self.run_on(b"a\r\nb\n", b"top\na\nb\n"), b"top\r\na\r\nb\n")

    def test_deleted_lines_go(self):
        self.assertEqual(self.run_on(b"a\r\nb\nc\r\n", b"a\nc\n"), b"a\r\nc\r\n")

    def test_no_final_newline_stays_none(self):
        self.assertEqual(self.run_on(b"a\r\nb", b"a\nb"), b"a\r\nb")

    def test_lines_added_after_a_last_line_without_newline(self):
        # base ends without a newline; the edit appends lines: the old last line needs an ending now
        self.assertEqual(self.run_on(b"a\r\nb", b"a\nb\nc\n"), b"a\r\nb\r\nc\r\n")


if __name__ == "__main__":
    unittest.main()
