#!/usr/bin/env python3
"""Tests for tools/xiso.py on a tiny synthetic XDVDFS image (no disc):

  python tools/test_xiso.py
"""
import os, struct, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import xiso  # noqa: E402

S = xiso.SECTOR


def entry(left, right, start, length, attr, name):
    """One directory entry, padded to 4 bytes (the tree's offsets are in 4-byte units)."""
    e = struct.pack("<HHIIBB", left, right, start, length, attr, len(name)) + name
    return e + b"\xff" * (-len(e) % 4)


def image(base=0, truncate=None):
    """Root table: media/ (sector 34, holding a.bin at sector 41) whose left child is default.xex
    (sector 40): a two-entry binary tree, as the disc's directory tables are."""
    xex, abin = b"XEX2" + b"x" * 1000, b"abc"
    sub = entry(0, 0, 41, len(abin), 0x80, b"a.bin")
    media = entry(0, 0, 34, len(sub), 0x10, b"media")
    root = entry(len(media) // 4, 0, 34, len(sub), 0x10, b"media") + entry(0, 0, 40, len(xex), 0x80, b"default.xex")
    img = bytearray(base + 42 * S)
    for sector, data in ((32, xiso.MAGIC + struct.pack("<II", 33, len(root))), (33, root), (34, sub), (40, xex), (41, abin)):
        img[base + sector * S:base + sector * S + len(data)] = data
    return bytes(img[:truncate] if truncate else img), xex, abin


class Xiso(unittest.TestCase):
    def iso(self, d, data):
        p = os.path.join(d, "g.iso")
        with open(p, "wb") as f:
            f.write(data)
        return p

    def test_lists_files_and_dirs_from_the_tree(self):
        with tempfile.TemporaryDirectory() as d:
            data, xex, abin = image()
            f, base, ents = xiso.entries(self.iso(d, data))
            f.close()
            self.assertEqual(base, 0)
            self.assertEqual([(p, n, isdir) for p, _, n, isdir in ents],
                             [("default.xex", len(xex), False), ("media", len(entry(0, 0, 41, 3, 0x80, b"a.bin")), True), ("media/a.bin", 3, False)])

    def test_finds_the_volume_at_a_360_offset(self):
        with tempfile.TemporaryDirectory() as d:
            data, _, _ = image(base=0x2080000)
            f, base, ents = xiso.entries(self.iso(d, data))
            f.close()
            self.assertEqual(base, 0x2080000)
            self.assertEqual(len(ents), 3)

    def test_no_volume_stops(self):
        with tempfile.TemporaryDirectory() as d:
            with self.assertRaises(SystemExit):
                xiso.entries(self.iso(d, b"\0" * 40 * S))

    def test_extract_writes_the_files(self):
        with tempfile.TemporaryDirectory() as d:
            data, xex, _ = image()
            out = os.path.join(d, "out")
            subprocess.run([sys.executable, os.path.join(HERE, "xiso.py"), self.iso(d, data), "extract", out], check=True, capture_output=True)
            with open(os.path.join(out, "default.xex"), "rb") as f:
                self.assertEqual(f.read(), xex)
            with open(os.path.join(out, "media", "a.bin"), "rb") as f:
                self.assertEqual(f.read(), b"abc")

    def test_a_truncated_image_stops_instead_of_looping(self):
        with tempfile.TemporaryDirectory() as d:
            data, _, _ = image(truncate=40 * S + 100)  # default.xex's data cut short
            r = subprocess.run([sys.executable, os.path.join(HERE, "xiso.py"), self.iso(d, data), "extract", os.path.join(d, "out")],
                               capture_output=True, text=True, timeout=20)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("truncated", r.stderr)


if __name__ == "__main__":
    unittest.main()
