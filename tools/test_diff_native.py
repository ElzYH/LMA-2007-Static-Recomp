#!/usr/bin/env python3
"""Tests for tools/diff_native.py's range reporting (no game files needed)."""
import os, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diff_native  # noqa: E402


class Ranges(unittest.TestCase):
    def test_identical_and_single_change(self):
        self.assertEqual(diff_native.ranges(b'abcdef', b'abcdef'), [])
        self.assertEqual(diff_native.ranges(b'abcdef', b'abXdef'), [(2, 3)])

    def test_adjacent_runs_merge_and_separate_ones_dont(self):
        self.assertEqual(diff_native.ranges(b'aaaaaaaa', b'abbaabba'), [(1, 3), (5, 7)])

    def test_length_mismatch_is_a_trailing_range(self):
        self.assertEqual(diff_native.ranges(b'abc', b'abcd'), [(3, 4)])

    def test_first_difference(self):
        self.assertIsNone(diff_native.first_difference(['a', 'b'], ['a', 'b']))
        self.assertEqual(diff_native.first_difference(['a', 'b'], ['a', 'c']), (1, 'b', 'c'))
        self.assertEqual(diff_native.first_difference(['a'], ['a', 'c']), (1, '<end>', 'c'))

    def test_limit(self):
        self.assertEqual(len(diff_native.ranges(b'a' * 100, b'ab' * 50, limit=5)), 5)

    def test_files_compare_in_chunks_with_offsets(self):
        import tempfile
        d = tempfile.mkdtemp()
        a, b = os.path.join(d, 'a'), os.path.join(d, 'b')
        big = bytearray(b'\0' * (diff_native.CHUNK + 100))
        open(a, 'wb').write(big)
        big[5] = 1
        big[diff_native.CHUNK + 3] = 7
        open(b, 'wb').write(big)
        size, diffs, peek = diff_native.diff_files(a, b)
        self.assertEqual((size, diffs), (diff_native.CHUNK + 100, [(5, 6), (diff_native.CHUNK + 3, diff_native.CHUNK + 4)]))
        self.assertEqual(peek(b, diff_native.CHUNK + 3, diff_native.CHUNK + 4), b'\x07')


if __name__ == '__main__':
    unittest.main()
