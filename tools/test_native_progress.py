#!/usr/bin/env python3
"""Tests for tools/native_progress.py on a synthetic Ghidra export (no game data):

  python tools/test_native_progress.py
"""
import collections, os, re, shutil, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import native_progress as np  # noqa: E402

GUEST = os.path.join(HERE, "..", "lma-rs", "crates", "guest", "src")


def counted(calls):
    callers = collections.Counter()
    for bs in calls.values():
        for b in bs:
            callers[b] += 1
    return callers


class Export(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, self.d)

    def write(self, name, text):
        p = os.path.join(self.d, name)
        with open(p, "w") as f:
            f.write(text)
        return p


class Reach(unittest.TestCase):
    CALLS = {1: {2, 3}, 2: {4}, 3: {4, 9}, 4: set(), 9: set(), 5: {9}, 6: {9}}

    def test_the_walk_stops_at_shared_helpers(self):
        got, stopped = np.reach({1}, self.CALLS, counted(self.CALLS), 3)
        self.assertEqual((got, stopped), ({1, 2, 3, 4}, {9}))

    def test_an_entry_point_is_never_a_stop(self):
        self.assertEqual(np.reach({9}, self.CALLS, counted(self.CALLS), 3), ({9}, set()))

    def test_cycles_end(self):
        calls = {1: {2}, 2: {1}}
        self.assertEqual(np.reach({1}, calls, counted(calls), 25), ({1, 2}, set()))


class Owner(unittest.TestCase):
    def test_the_body_holds_the_address_the_end_is_exclusive(self):
        starts, sizes = [0x100, 0x200], {0x100: 0x40, 0x200: 0x10}
        self.assertEqual([np.owner(starts, sizes, a) for a in (0xff, 0x100, 0x13c, 0x140, 0x204)], [None, 0x100, 0x100, None, 0x200])


class Hooks(Export):
    def test_addresses_table(self):
        p = self.write("hooks.rs", "pub const ADDRESSES: &[u32] = &[\n    0x8233_98A0, // a\n    0x820E_39D8,\n];\n")
        self.assertEqual(np.hook_sites(p), {0x823398A0, 0x820E39D8})

    def test_no_table_exits(self):
        with self.assertRaises(SystemExit):
            np.hook_sites(self.write("hooks.rs", "pub const OTHER: &[u32] = &[1];\n"))

    def test_the_real_guest_table_parses(self):
        sites = np.hook_sites(os.path.join(GUEST, "hooks.rs"))
        self.assertGreater(len(sites), 0)
        self.assertTrue(all(0x8200_0000 <= a < 0x8300_0000 for a in sites))


class Pointers(Export):
    def test_address_taken_and_vtables(self):
        # 0x8100 takes 0x8200's address and a vtable's (0x8900: 0x8300, 0x8400); a call isn't an edge here
        self.write("xrefs.tsv", "from\tto\ttype\n8100\t8200\tPARAM\n8104\t8900\tDATA\n8900\t8300\tDATA\n8904\t8400\tDATA\n8108\t8500\tUNCONDITIONAL_CALL\n")
        sizes = {0x8100: 0x10, 0x8200: 4, 0x8300: 4, 0x8400: 4, 0x8500: 4}
        self.assertEqual(sorted(np.pointer_edges(self.d, set(sizes), sizes)), [(0x8100, 0x8200), (0x8100, 0x8300), (0x8100, 0x8400)])

    def test_load_export_adds_pointer_edges_once(self):
        self.write("functions.tsv", "address\tname\tsize\n8100\ta\t16\n8200\tb\t4\n")
        self.write("calls.tsv", "8100 8200\n8100 8200\n")
        self.write("xrefs.tsv", "from\tto\ttype\n8104\t8200\tPARAM\n")
        funcs, calls, callers, sizes = np.load_export(self.d)
        self.assertEqual((funcs, dict(calls), callers[0x8200]), ({0x8100, 0x8200}, {0x8100: {0x8200}}, 1))
        _, calls, _, _ = np.load_export(self.d, pointers=False)
        self.assertEqual(dict(calls), {0x8100: {0x8200}})


class Files(Export):
    def test_functions_referencing_a_source_path(self):
        self.write("strings.tsv", "8a00\t20\tD:\\LMA\\Transfer\\Bid.cpp\n8a40\t10\tother\n")
        self.write("xrefs.tsv", "from\tto\ttype\n8104\t8a00\tDATA\n8204\t8a40\tDATA\n9000\t8a00\tDATA\n")
        sizes = {0x8100: 0x10, 0x8200: 0x10}
        self.assertEqual(np.file_entries(self.d, sizes, ["\\transfer\\"]), {0x8100})  # case-blind; 0x9000 is in no function
        self.assertEqual(np.file_entries(self.d, sizes, []), set())


class Tune(Export):
    def test_instruction_and_data_entries(self):
        self.write("xrefs.tsv", "from\tto\ttype\n8104\t8900\tDATA\n8204\t8904\tREAD\n")
        p = self.write("tune.rs", 'name: "a", kind: Kind::Imm(0x3860_0001), site: 0x8104\n'
                                  'name: "b", kind: Kind::Table(1), site: 0x8900\n'
                                  'name: "c", kind: Kind::F32, site: 0x7000\n'
                                  'name: "d", kind: Kind::Bytes(8), site: 0x8900\n')
        sizes = {0x8100: 0x10, 0x8200: 0x10}
        self.assertEqual(np.tune_sites(p, self.d, sizes), [("a", {0x8100}), ("b", {0x8100}), ("c", set()), ("d", {0x8100, 0x8200})])

    def test_every_tunable_of_the_real_table_is_found(self):
        # a TUNABLES line the regex misses would drop out of the counts without a word
        self.write("xrefs.tsv", "from\tto\ttype\n")
        src = os.path.join(GUEST, "tune.rs")
        want = re.findall(r'Tunable \{ name: "(\w+)"', open(src).read())
        self.assertGreater(len(want), 0)
        self.assertEqual([n for n, _ in np.tune_sites(src, self.d, {})], want)


class Report(unittest.TestCase):
    def test_rows_count_reached_native_shared_and_tune(self):
        calls = {0x82187DE0: {0x10, 0x20}, 0x10: {0x99}, 0x20: {0x99}, 0x30: {0x99}}
        callers = counted(calls)
        rows, total, native, helpers = np.report({0x82187DE0, 0x10, 0x20, 0x30, 0x99}, calls, callers, {0x10, 0x30}, 3,
                                                 tune=[("k", {0x20}), ("other", {0x30})])
        row = dict((r[0], r[1:]) for r in rows)["contracts and wages"]
        entries = len(np.SYSTEMS["contracts and wages"][0])  # every entry point counts, called or not
        self.assertEqual(row, (entries + 2, 1, 1, 1))
        self.assertEqual(dict((r[0], r[1:]) for r in rows)["training"], (None, None, None, 0))
        self.assertEqual((total, native, helpers), (5, 2, 1))


if __name__ == "__main__":
    unittest.main()
