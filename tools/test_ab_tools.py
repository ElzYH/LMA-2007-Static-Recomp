#!/usr/bin/env python3
"""Tests for tools/ab_live.py and tools/seg_time.py's parsing (no game files needed):

  python tools/test_ab_tools.py
"""
import os, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import ab_live, seg_time  # noqa: E402


class Live(unittest.TestCase):
    def test_medians_after_ten_seconds(self):
        text = ('5s: 20 fps (1% low 5) | game 30 fps\n10s: 50 fps (1% low 20) | game 50 fps\n'
                '15s: 60 fps (1% low 30) | game 60 fps\n20s: 56 fps (1% low 28) | game 57 fps\n')
        self.assertEqual(ab_live.summarise(text), (56, 28))

    def test_panic_and_silence(self):
        self.assertIsNone(ab_live.summarise("10s: 50 fps (1% low 20)\nthread 'main' panicked at ..."))
        self.assertIsNone(ab_live.summarise('5s: 50 fps (1% low 20)\n'))


class Segment(unittest.TestCase):
    def test_run_time_excludes_the_load(self):
        # lma-cli's timer starts before the load: the end line's 10.35 s include the load's 1.78 s
        text = 'loaded x.state at 309181970650 instructions in 1.78s\n...\n329181989981 instructions in 10.35s (31815.4 M/s)\n'
        self.assertAlmostEqual(seg_time.seconds(text, 329181970650), 8.57)

    def test_a_load_under_a_second_is_in_milliseconds(self):
        text = 'loaded x.state at 1557802345114 instructions in 875.67ms\n1577802345114 instructions in 5.48s (3649.6 M/s)\n'
        self.assertAlmostEqual(seg_time.seconds(text, 1577802345114), 5.48 - 0.87567)

    def test_the_end_line_may_come_before_the_load_line(self):
        # stdout (the end line) and stderr (the load line) are joined in either order (#370)
        text = '1577802345114 instructions in 5.48s (3649.6 M/s)\nloaded x.state at 1557802345114 instructions in 875.67ms\n'
        self.assertAlmostEqual(seg_time.seconds(text, 1577802345114), 5.48 - 0.87567)

    def test_a_cold_boot_has_no_load_line(self):
        self.assertEqual(seg_time.seconds('20000000000 instructions in 42.00s (476.2 M/s)\n', 20000000000), 42.0)

    def test_a_run_that_stopped_early_gives_no_time(self):
        text = 'loaded x.state at 309181970650 instructions in 1.78s\nexit: Stop\n309200000000 instructions in 1.30s\n'
        self.assertIsNone(seg_time.seconds(text, 329181970650))
        self.assertIsNone(seg_time.seconds('panicked', 1))


if __name__ == '__main__':
    unittest.main()
