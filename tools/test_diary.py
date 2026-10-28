#!/usr/bin/env python3
"""Tests for tools/diary.py on a synthetic diary (no game data):

  python tools/test_diary.py
"""
import os, struct, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diary  # noqa: E402

BASE = 0x1000


def day_record(packed, kind, entries, length=None):
    body = b"".join(entries)
    n = len(entries)
    length = 6 + len(body) if length is None else length
    return struct.pack(">HBBH", packed, n, kind, length) + body


def round_entry(comp, rnd, fixtures):
    return struct.pack(">bBBB", comp, rnd, len(fixtures), 0) + b"".join(struct.pack(">HHH", *f) for f in fixtures)


def memory(records):
    """A read(addr, n) over the records laid out from BASE, with the diary pointer set."""
    blob = b"".join(records)
    mem = {diary.DIARY_PTR: struct.pack(">I", BASE)}

    def read(a, n):
        if a == diary.DIARY_PTR:
            return mem[a][:n]
        return blob[a - BASE:a - BASE + n]
    return read


# 25 Oct: day 25, month 10
OCT25 = 25 << 11 | 9 << 7


class Diary(unittest.TestCase):
    def test_walks_rounds_and_fixtures_to_the_end(self):
        read = memory([
            day_record(OCT25, 0, [round_entry(35, 12, [(6240, 6328, 0xFF00), (5800, 5816, 0x3000)]), round_entry(47, 25, [(0, 0, 0xFF00)])]),
            day_record(OCT25 + (1 << 11), 1, [b"\0" * 4, b"\0" * 4]),
            day_record(0, 3, []),
        ])
        got = list(diary.days(read))
        self.assertEqual(len(got), 3)
        i, _, packed, kind, rounds = got[0]
        self.assertEqual((i, diary.date_of(packed), kind), (0, (25, 10), 0))
        self.assertEqual(rounds[0], (35, 12, [(6240, 6328, 0xFF00), (5800, 5816, 0x3000)]))
        self.assertEqual(got[2][3], 3, "the end marker is the last record")
        self.assertEqual(diary.fill(read), {35: [2, 2, 1], 47: [1, 0, 0]})

    def test_scores_and_unplayed(self):
        self.assertFalse(diary.played(0xFF00))
        self.assertFalse(diary.played(0xE000))
        self.assertTrue(diary.played(0x0000), "0-0 is a result")
        self.assertIn("3-4", diary.fixture_line(0, OCT25, 35, 12, 4, 16, 5853, 6142, 0x3400))
        self.assertIn("    -", diary.fixture_line(0, OCT25, 35, 12, 9, 16, 6240, 6328, 0xFF00))

    def test_a_zero_length_day_is_reported_not_looped(self):
        read = memory([day_record(OCT25, 1, [b"\0" * 4], length=0)])
        with self.assertRaisesRegex(ValueError, "length 0"):
            list(diary.days(read))


if __name__ == "__main__":
    unittest.main()
