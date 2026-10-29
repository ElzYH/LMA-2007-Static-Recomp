#!/usr/bin/env python3
"""Tests for tools/state_diff.py with a stub `lma-cli state-mem` (no game files):

  python tools/test_state_diff.py
"""
import os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import state_diff  # noqa: E402

# `state-mem <xex> <state> <out>`: the "memory" is the state file's bytes at guest address 0x10000
STUB = """#!/usr/bin/env python3
import sys
_, _, _, state, out = sys.argv
with open(state, "rb") as f, open(out, "wb") as o:
    o.seek(0x10000)
    o.write(f.read())
"""


def put(path, data, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
    if mode:
        os.chmod(path, mode)


class Ranges(unittest.TestCase):
    def test_words_join_across_one_unchanged_word(self):
        self.assertEqual(state_diff.ranges([0x10, 0x14, 0x1C, 0x40]), [(0x10, 0x20), (0x40, 0x44)])

    def test_byte_width_ranges_are_bytes(self):
        self.assertEqual(state_diff.ranges([0x10], 1), [(0x10, 0x11)])
        self.assertEqual(state_diff.ranges([0x10, 0x12, 0x20], 1), [(0x10, 0x13), (0x20, 0x21)])


@unittest.skipIf(os.name == "nt", "the stub lma-cli is a #! script")
class Cli(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.d = self.t.name
        self.cli = os.path.join(self.d, "lma-cli")
        put(self.cli, STUB.encode(), 0o755)

    def tearDown(self):
        self.t.cleanup()

    def diff(self, *args):
        r = subprocess.run([sys.executable, os.path.join(HERE, "state_diff.py"), *args, "--cli", self.cli,
                            "--xex", "x.xex", "--keep", os.path.join(self.d, "keep")], capture_output=True, text=True, check=True)
        return r.stdout

    def test_changed_words_and_to_from(self):
        os.makedirs(os.path.join(self.d, "keep"))
        a, b = os.path.join(self.d, "a.state"), os.path.join(self.d, "b.state")
        put(a, (3).to_bytes(4, "big") + bytes(4) + (7).to_bytes(4, "big"))
        put(b, (5).to_bytes(4, "big") + bytes(4) + (9).to_bytes(4, "big"))
        out = self.diff(a, b)
        self.assertIn("2 changed words in 1 ranges", out)
        self.assertIn("0x00010000-0x0001000c (12 bytes)", out)
        out = self.diff(a, b, "--to", "5", "--from", "3")
        self.assertIn("1 word(s) changed to 5 from 3", out)
        self.assertIn("0x00010000: 3 -> 5", out)
        self.assertIn("1 word(s) == 9 (4-byte)", self.diff(b, "--find", "9"))

    def test_same_name_in_two_folders_is_two_dumps(self):
        os.makedirs(os.path.join(self.d, "keep"))
        a, b = os.path.join(self.d, "old", "x.state"), os.path.join(self.d, "new", "x.state")
        put(a, (1).to_bytes(4, "big"))
        put(b, (2).to_bytes(4, "big"))
        self.assertIn("1 changed words in 1 ranges", self.diff(a, b))


if __name__ == "__main__":
    unittest.main()
