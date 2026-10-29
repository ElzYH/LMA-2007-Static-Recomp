#!/usr/bin/env python3
"""Tests for tools/value_ladder.py on synthetic mod.toml files (no game data):

  python tools/test_value_ladder.py
"""
import contextlib, io, os, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import value_ladder as vl  # noqa: E402

try:
    import tomllib
except ImportError:  # Python < 3.11
    tomllib = None

BASE = 'name = "x"\n\n[[patch]]\naddr = "0x82703118"\nfrom = 300\nto = 2000\n'


class Scaled(unittest.TestCase):
    def test_22_ladder_patches_divide_the_words(self):
        out = vl.scaled(BASE, 3)
        self.assertEqual(out.count("[[patch]]"), 23)
        self.assertIn('addr = "0x82005508"\nfrom = 287500\nto = 95833\n', out)
        self.assertIn('addr = "0x8200555c"\nfrom = 40000\nto = 13333\n', out)
        self.assertTrue(out.startswith(BASE.rstrip("\n")))

    def test_rescaling_replaces_the_old_ladder(self):
        twice = vl.scaled(vl.scaled(BASE, 3), 2)
        self.assertEqual(twice.count("[[patch]]"), 23)
        self.assertIn("from = 287500\nto = 143750", twice)
        self.assertNotIn("95833", twice)
        self.assertEqual(twice.count("Player values x"), 2)  # the comment of each run stays (it's text)

    def test_factor_below_one_lowers_values(self):
        self.assertIn("from = 287500\nto = 575000", vl.scaled(BASE, 0.5))

    @unittest.skipIf(tomllib is None, "tomllib needs Python 3.11")
    def test_output_parses_with_other_tables_kept(self):
        src = BASE + '\n[tune]\nwage_refuse_margin = 50\n'
        out = vl.scaled(src, 2.5)
        doc = tomllib.loads(out)
        self.assertEqual(doc["tune"], {"wage_refuse_margin": 50})
        got = {int(p["addr"], 16): (p["from"], p["to"]) for p in doc["patch"]}
        self.assertEqual(got[0x82703118], (300, 2000))
        self.assertEqual([got[vl.LADDER_BASE + 4 * i] for i in range(22)], [(w, round(w / 2.5)) for w in vl.LADDER])

    def test_crlf_ladder_patches_are_replaced_too(self):
        once = vl.scaled(BASE, 3).replace("\n", "\r\n")
        twice = vl.scaled(once, 2)
        self.assertNotIn("95833", twice)
        self.assertIn("0x82703118", twice)

    def test_ladder_is_decreasing_and_22_words(self):
        self.assertEqual(len(vl.LADDER), 22)
        self.assertEqual(vl.LADDER, sorted(vl.LADDER, reverse=True))
        self.assertEqual(vl.LADDER_BASE + 4 * 21, 0x8200555C)


class Main(unittest.TestCase):
    def test_rewrites_the_file_in_place(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "mod.toml")
            with open(p, "w", newline="") as f:
                f.write(BASE)
            argv, sys.argv = sys.argv, ["value_ladder.py", p, "4"]
            try:
                with contextlib.redirect_stdout(io.StringIO()) as out:
                    self.assertEqual(vl.main(), 0)
            finally:
                sys.argv = argv
            self.assertIn("value ladder x4 (22 patches)", out.getvalue())
            with open(p, newline="") as f:
                self.assertIn("from = 287500\nto = 71875", f.read())

    def test_wrong_arguments_print_usage(self):
        argv, sys.argv = sys.argv, ["value_ladder.py"]
        try:
            with open(os.devnull, "w") as null:
                out, sys.stdout = sys.stdout, null
                try:
                    self.assertEqual(vl.main(), 2)
                finally:
                    sys.stdout = out
        finally:
            sys.argv = argv


if __name__ == "__main__":
    unittest.main()
