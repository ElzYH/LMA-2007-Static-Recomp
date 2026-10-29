#!/usr/bin/env python3
"""Tests for tools/regress.py's verdicts with a stub lma-cli (no game files, no real runs):

  python tools/test_regress.py
"""
import json, os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
TOOL = os.path.join(HERE, "regress.py")

# Prints what regress.py parses: registers for a fingerprint (STUB_REGS), a state's clock and frame
# counter, the pre-filter's counts (STUB_MISMATCH), and dumps LMA_FRAMES as files (STUB_PIXELS).
STUB = r'''#!/usr/bin/env python3
import os, sys
e = os.environ
print(" loaded at 1000 instructions")
print("Stats { cycles: 1, frames: 7, x: 0 }")
if e.get("LMA_SEARCH_PREFILTER") == "check":
    print("search filter MISMATCH\n" * int(e.get("STUB_MISMATCH", "0")), end="")
    print("search filter: 5 calls, 2 answered natively")
if e.get("LMA_PEEK"):
    print("  r3 " + e.get("STUB_REGS", "1") + " " + e["LMA_PEEK"])
    open("last_run.log", "w").write("kernel " + e.get("STUB_KERNEL", "k"))
if e.get("LMA_FRAMES"):
    os.makedirs("frames", exist_ok=True)
    for f in e["LMA_FRAMES"].split(","):
        open(os.path.join("frames", f + ".png"), "w").write(e.get("STUB_PIXELS", "p") + f)
'''


def put(path, text, mode=None):
    with open(path, "w") as f:
        f.write(text)
    if mode:
        os.chmod(path, mode)


@unittest.skipIf(os.name == "nt", "the stub lma-cli is a #! script")
class Regress(unittest.TestCase):
    def setUp(self):
        self.d = tempfile.mkdtemp()
        self.cli = os.path.join(self.d, "lma-cli")
        put(self.cli, STUB, 0o755)
        self.states = os.path.join(self.d, "states")
        os.makedirs(self.states)
        self.cfg = {"fingerprints": [{"name": "day", "state": "day.state", "end": 2000, "peek": "0x82000000"}],
                    "frames": [{"name": "match", "state": "m.state", "frames": 3, "every": 10}]}
        self.write_cfg()

    def tearDown(self):
        import shutil
        shutil.rmtree(self.d, ignore_errors=True)

    def write_cfg(self):
        put(os.path.join(self.states, "regress.json"), json.dumps(self.cfg))

    def regress(self, *args, env=None):
        r = subprocess.run([sys.executable, TOOL, "--cli", self.cli, "--states", self.states, "--xex", "x.xex", *args],
                           env={**os.environ, **(env or {})}, capture_output=True, text=True)
        return r.returncode, r.stdout + r.stderr

    def baseline(self):
        code, out = self.regress("--update", "--only", "fingerprints")
        self.assertEqual(code, 0, out)
        with open(os.path.join(self.states, "regress.json")) as f:
            self.cfg = json.load(f)

    def test_update_records_then_the_same_run_passes(self):
        self.baseline()
        self.assertRegex(self.cfg["fingerprints"][0]["hash"], r"^[0-9a-f]{12}$")
        code, out = self.regress("--only", "fingerprints")
        self.assertEqual(code, 0, out)
        self.assertIn("PASS fingerprint day", out)
        self.assertIn("PASS prefilter day: 0 mismatches in 5 search calls checked (2 answerable natively)", out)

    def test_a_register_change_fails(self):
        self.baseline()
        code, out = self.regress("--only", "day", env={"STUB_REGS": "2"})
        self.assertEqual(code, 1)
        self.assertIn("FAIL fingerprint day", out)

    def test_a_kernel_log_change_fails(self):
        self.baseline()
        code, out = self.regress("--only", "fingerprints", env={"STUB_KERNEL": "other"})
        self.assertEqual(code, 1)
        self.assertIn("FAIL fingerprint day", out)

    def test_prefilter_mismatch_fails_and_only_prefilter_skips_the_fingerprint(self):
        self.baseline()
        code, out = self.regress("--only", "prefilter", env={"STUB_MISMATCH": "2"})
        self.assertEqual(code, 1)
        self.assertIn("FAIL prefilter day: 2 mismatches", out)
        self.assertNotIn("fingerprint day", out)

    def test_frames_without_ref_skip_and_with_an_identical_ref_pass(self):
        code, out = self.regress("--only", "frames")
        self.assertEqual(code, 0, out)
        self.assertIn("skip frames match: no --ref", out)
        code, out = self.regress("--only", "match", "--ref", self.cli)
        self.assertEqual(code, 0, out)
        self.assertIn("PASS frames match: 3/3 identical to --ref (3 dumped)", out)
        self.assertNotIn("fingerprint", out)

    def test_frames_differing_from_ref_fail(self):
        ref = os.path.join(self.d, "ref-cli")
        put(ref, STUB.replace('e.get("STUB_PIXELS", "p")', '"q"'), 0o755)
        code, out = self.regress("--only", "frames", "--ref", ref)
        self.assertEqual(code, 1)
        self.assertIn("FAIL frames match: 0/3 identical", out)

    def test_replays_only_with_long(self):
        self.cfg["replays"] = [{"name": "s4", "cli": self.cli, "userdata": self.states, "steps": [], "compare": []}]
        self.write_cfg()
        code, out = self.regress("--only", "replays")
        self.assertEqual(code, 0, out)
        self.assertNotIn("replaying", out)


if __name__ == "__main__":
    unittest.main()
