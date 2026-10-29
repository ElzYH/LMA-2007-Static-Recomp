#!/usr/bin/env python3
"""Tests for tools/seg_time.py with a stub lma-cli (no game files):

  python tools/test_seg_time.py
"""
import os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import seg_time  # noqa: E402

# A state loads at 1000 instructions in 200 ms; a run to N ends "N instructions in STUB_SECS s",
# or stops at 1500 when STUB_STOP is set.
STUB = """#!/usr/bin/env python3
import os, sys
end = int(sys.argv[3])
sys.stderr.write("loaded s at 1000 instructions in 200ms\\n")
if end > 1 and os.environ.get("STUB_STOP"):
    print("1500 instructions in 1.0s")
else:
    print(f"{max(end, 1000)} instructions in {os.environ.get('STUB_SECS', '2.2')}s")
"""


class Seconds(unittest.TestCase):
    def test_the_segment_less_the_load(self):
        text = "loaded x at 1000 instructions in 200ms\n5000 instructions in 2.2s\n"
        self.assertAlmostEqual(seg_time.seconds(text, 5000), 2.0)

    def test_a_run_that_stopped_early_has_no_time(self):
        self.assertIsNone(seg_time.seconds("loaded x at 1000 instructions in 200ms\n3000 instructions in 2.2s\n", 5000))
        self.assertIsNone(seg_time.seconds("panicked\n", 5000))


@unittest.skipIf(os.name == "nt", "the stub lma-cli is a #! script")
class Cli(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.cli = os.path.join(self.t.name, "lma-cli")
        with open(self.cli, "w") as f:
            f.write(STUB)
        os.chmod(self.cli, 0o755)
        self.state = os.path.join(self.t.name, "day.state")
        open(self.state, "w").close()

    def tearDown(self):
        self.t.cleanup()

    def seg(self, a, b, env=None):
        r = subprocess.run([sys.executable, os.path.join(HERE, "seg_time.py"), "--rounds", "2", "--run", "4000", a, self.cli, b, self.cli,
                            self.state], env={**os.environ, **(env or {})}, capture_output=True, text=True)
        return r.returncode, r.stdout + r.stderr

    def test_medians_and_the_difference(self):
        code, out = self.seg("old", "new")
        self.assertEqual(code, 0, out)
        self.assertIn("day: old 2.00 / 2.00 (median 2.00 s) | new 2.00 / 2.00 (median 2.00 s) | +0.0%", out)

    def test_a_stopped_run_gives_no_timing(self):
        code, out = self.seg("old", "new", {"STUB_STOP": "1"})
        self.assertIn("stopped at 1500 of 5000", out)
        self.assertIn("day: a run failed, no timing", out)

    def test_two_sides_with_one_name_stop(self):
        code, out = self.seg("main", "main")
        self.assertNotEqual(code, 0)
        self.assertIn("both sides are named main", out)


if __name__ == "__main__":
    unittest.main()
