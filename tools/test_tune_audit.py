#!/usr/bin/env python3
"""Tests for tools/tune_audit.py on a synthetic Ghidra export (no game data):

  python tools/test_tune_audit.py
"""
import os, re, shutil, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import tune_audit  # noqa: E402

TUNE_RS = os.path.join(HERE, "..", "lma-rs", "crates", "guest", "src", "tune.rs")


class Entries(unittest.TestCase):
    def test_data_kinds_and_their_sizes(self):
        src = ('Tunable { name: "a", kind: Kind::Table(3), site: 0x8200_0010, also: &[] }\n'
               'Tunable { name: "b", kind: Kind::F32, site: 0x82000040 }\n'
               'Tunable { name: "c", kind: Kind::Bytes(5), site: 0x8200_0050 }\n'
               'Tunable { name: "d", kind: Kind::Imm(0x3860_0001), site: 0x8210_0000 }\n')
        self.assertEqual(tune_audit.entries(src),
                         [("a", "Table", 0x82000010, 12), ("b", "F32", 0x82000040, 4), ("c", "Bytes", 0x82000050, 5)])

    def test_every_data_entry_of_the_real_table_is_read(self):
        # a TUNABLES line the regex misses would drop out of the audit without a word
        src = open(TUNE_RS).read()
        want = len(re.findall(r'kind: Kind::(?:Table|Bytes|F32)\b', src))
        got = tune_audit.entries(src)
        self.assertGreater(want, 0)
        self.assertEqual(len(got), want)
        self.assertEqual(len({n for n, *_ in got}), len(got))
        self.assertTrue(all(n > 0 and 0x8200_0000 <= s < 0x8300_0000 for _, _, s, n in got), got)


class Readers(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.d)

    def export(self, functions, xrefs):
        with open(os.path.join(self.d, "functions.tsv"), "w") as f:
            f.write("address\tname\tsize\n" + "".join(f"{a:08x}\tf\t{n}\n" for a, n in functions))
        with open(os.path.join(self.d, "xrefs.tsv"), "w") as f:
            f.write("from\tto\ttype\n" + "".join(f"{a:08x}\t{b}\tDATA\n" for a, b in xrefs))

    def test_each_reference_goes_to_its_function(self):
        self.export([(0x82100000, 16), (0x82100100, 16)],
                    [(0x82100004, "82000018"), (0x82100104, "82000040"), (0x82100108, "8200001c"), (0x82100008, "82000054")])
        r = tune_audit.readers(self.d, [(0x82000010, 0x8200001c), (0x82000040, 0x82000044), (0x82000050, 0x82000055)])
        self.assertEqual(r, [{0x82100000}, {0x82100100}, {0x82100000}])

    def test_span_end_is_exclusive(self):
        self.export([(0x82100000, 16)], [(0x82100004, "8200001c")])
        self.assertEqual(tune_audit.readers(self.d, [(0x82000010, 0x8200001c)]), [set()])

    def test_overlapping_spans_both_see_the_reader(self):
        self.export([(0x82100000, 16), (0x82100100, 16)], [(0x82100004, "82000014"), (0x82100104, "82000020")])
        r = tune_audit.readers(self.d, [(0x82000010, 0x82000030), (0x82000014, 0x82000018)])
        self.assertEqual(r, [{0x82100000, 0x82100100}, {0x82100000}])

    def test_code_outside_a_function_belongs_to_the_one_before(self):
        # a switch's cases Ghidra left outside the function, and a reference before every function
        self.export([(0x82100000, 16)], [(0x82100400, "82000010"), (0x82000000, "82000010")])
        self.assertEqual(tune_audit.readers(self.d, [(0x82000010, 0x82000014)]), [{0x82100000, 0x82000000}])

    def test_targets_outside_the_image_are_skipped(self):
        self.export([(0x82100000, 16)], [(0x82100004, "EXTERNAL:0001"), (0x82100008, "00000010")])
        self.assertEqual(tune_audit.readers(self.d, [(0x10, 0x20)]), [set()])


if __name__ == "__main__":
    unittest.main()
