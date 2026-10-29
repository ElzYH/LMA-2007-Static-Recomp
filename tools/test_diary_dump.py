#!/usr/bin/env python3
"""Tests for tools/diary_dump.py on a synthetic Export.mng (no game data):

  python tools/test_diary_dump.py
"""
import contextlib, datetime as dt, io, os, struct, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diary_dump  # noqa: E402


def bg2(files):
    """A BG2 archive: the table from 0x14 (0x100-byte name, offset, size per entry), then the data."""
    table, data = b"", b""
    for name, body in files.items():
        table += name.encode().ljust(0x100, b"\0") + struct.pack(">II", len(data), len(body))
        data += body
    start = 0x14 + len(table)
    return b"BG2\0" + bytes(4) + struct.pack(">II", len(table), start) + bytes(4) + table + data


def rec(home, away):
    return struct.pack(">HH", home, away) + b"\xff\x00"


def code(season, day, flag=0):
    return struct.pack("<H", flag | season << 10 | day)


def diary(season):
    # round 0: a 4-byte header (a date and a fixed pair) then two fixtures and a blank slot;
    # round 1: a 2-byte header (the date with the bit-15 flag) then one fixture
    return (code(season, 75) + b"\x31\x04" + rec(264, 265) + rec(266, 267) + rec(0, 0)
            + code(season, 78, 0x8000) + rec(270, 271))


def tw():
    return struct.pack(">27I", 0, 1, 6, 61, 31, 7, 11, 0, 1, 184, 1, 0, 214, 31, 0, 11, 0, 1, *range(9))


def export():
    inner = {}
    for s in range(20):
        inner[f"TW{s:04}.bin"] = tw()
        inner[f"diary{s:04}.bin"] = diary(s)
    return bg2({"Other.bin": b"x", "Diary20.mng": bg2(inner)})


class Archive(unittest.TestCase):
    def test_bg2_entries_come_back_by_name(self):
        got = diary_dump.parse_bg2(bg2({"a.bin": b"abc", "b.bin": b"", "c.bin": b"\x00\x01"}))
        self.assertEqual(got, {"a.bin": b"abc", "b.bin": b"", "c.bin": b"\x00\x01"})

    def test_not_bg2_exits(self):
        with self.assertRaises(SystemExit):
            diary_dump.parse_bg2(b"PK\x03\x04" + bytes(32))


class Dates(unittest.TestCase):
    def test_day_counts_from_1_july_and_ignores_the_season_bits(self):
        self.assertEqual(diary_dump.season_date(2006, 0), dt.date(2006, 7, 1))
        self.assertEqual(diary_dump.season_date(2006, 75), dt.date(2006, 9, 14))  # round 1 of 2006/07
        self.assertEqual(diary_dump.season_date(2007, 1 << 10 | 61), dt.date(2007, 8, 31))
        self.assertEqual(diary_dump.season_date(2006, 184), dt.date(2007, 1, 1))


class Rounds(unittest.TestCase):
    def test_runs_of_6_byte_records_split_at_headers(self):
        got = diary_dump.rounds_of(diary(3))
        self.assertEqual([(len(h), r) for h, r in got], [(4, [(264, 265), (266, 267), (0, 0)]), (2, [(270, 271)])])
        self.assertEqual(got[0][0], code(3, 75) + b"\x31\x04")

    def test_no_records_no_rounds(self):
        self.assertEqual(diary_dump.rounds_of(b"\x01\x02\x03"), [])


class Main(unittest.TestCase):
    def run_main(self, *args):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "Export.mng")
            with open(p, "wb") as f:
                f.write(export())
            out, argv = io.StringIO(), sys.argv
            sys.argv = ["diary_dump.py", p, *args]
            try:
                with contextlib.redirect_stdout(out):
                    diary_dump.main()
            finally:
                sys.argv = argv
        return out.getvalue().splitlines()

    def test_windows_and_dated_rounds(self):
        lines = self.run_main("1", "--records")
        self.assertEqual(lines[0], "season 1 = 2007/08")
        self.assertIn("transfer window 1: day 0 (2007-07-01, 1/7) to day 61 (2007-08-31, 31/8)", lines[1])
        self.assertIn("transfer window 2: day 184 (2008-01-01, 1/1) to day 214 (2008-01-31, 31/1)", lines[2])
        self.assertIn("2 rounds, 4 records (dates checked against season 2)", lines[4])
        self.assertIn("dates +0:2007-09-14; 3 records, 2 filled", lines[5])
        self.assertEqual(lines[6].split(), ["263-264", "265-266", "(clubs.csv", "ids)"])
        self.assertIn("dates +0:2007-09-17; 1 records, 1 filled", lines[7])  # bit 15 masked off

    def test_last_season_checks_against_the_one_before(self):
        lines = self.run_main("19", "--rounds=1")
        self.assertIn("dates checked against season 18", lines[4])
        self.assertEqual(sum(l.startswith("  round") for l in lines), 1)


if __name__ == "__main__":
    unittest.main()
