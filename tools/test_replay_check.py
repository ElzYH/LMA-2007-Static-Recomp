#!/usr/bin/env python3
"""Tests for tools/replay_check.py with a stub lma-cli (no game files, no real runs):

  python tools/test_replay_check.py
"""
import os, subprocess, sys, tempfile, unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import replay_check  # noqa: E402

# Loading a state prints its clock; a peek prints one memory row: the saved state's own value, or
# for a replay "same" only in the stretch STUB_MATCH_AFTER names (LMA_REPLAY_AFTER_LOADS), else
# "diff". Every call's LMA_AUDIO goes to calls.log.
STUB = """#!/usr/bin/env python3
import os
e = os.environ
with open(os.path.join(e["STUB_DIR"], "calls.log"), "a") as f:
    f.write(e.get("LMA_AUDIO", "-") + "\\n")
if e.get("LMA_NORENDER"):
    print("loaded state at 500 instructions")
elif e.get("LMA_PEEK"):
    if e.get("LMA_REPLAY"):
        v = "11111111" if e.get("LMA_REPLAY_AFTER_LOADS") == e.get("STUB_MATCH_AFTER", "0") else "22222222"
    else:
        v = "11111111"
    print("  0x40000000: " + v + " 00000000 00000000 00000000")
    print("  r0  00 01 02 03")
"""


def put(path, text, mode=None):
    with open(path, "w") as f:
        f.write(text)
    if mode:
        os.chmod(path, mode)


class DiffRanges(unittest.TestCase):
    def test_the_modules_own_case(self):
        replay_check.test_diff_ranges()  # defined beside the code; nothing ran it before


@unittest.skipIf(os.name == "nt", "the stub lma-cli is a #! script")
class Cli(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.d = self.t.name
        self.cli = os.path.join(self.d, "lma-cli")
        put(self.cli, STUB, 0o755)
        self.rec = os.path.join(self.d, "s.rec")
        put(self.rec + ".journal", "i 1\n")
        self.state = os.path.join(self.d, "s1.state")
        put(self.state, "")

    def tearDown(self):
        self.t.cleanup()

    def check(self, *args, env=None):
        r = subprocess.run([sys.executable, os.path.join(HERE, "replay_check.py"), self.rec, self.state, "--cli", self.cli,
                            "--xex", "x.xex", *args], env={**os.environ, "STUB_DIR": self.d, **(env or {})},
                           capture_output=True, text=True)
        return r.returncode, r.stdout + r.stderr

    def calls(self):
        with open(os.path.join(self.d, "calls.log")) as f:
            return f.read().split()

    def test_a_matching_replay_is_ok(self):
        code, out = self.check()
        self.assertEqual(code, 0, out)
        self.assertIn("s1.state @ 500: ok", out)

    def test_a_different_replay_fails_and_diff_says_where(self):
        code, out = self.check("--diff", "5", env={"STUB_MATCH_AFTER": "9"})
        self.assertEqual(code, 1)
        self.assertIn("DIFFERENT", out)
        self.assertIn("0x40000000..0x40000010 (16 bytes)", out)

    def test_with_loads_each_stretch_is_tried_until_one_matches(self):
        put(self.rec + ".journal", "i 1\nl 2\nl 3\n")
        code, out = self.check(env={"STUB_MATCH_AFTER": "2"})
        self.assertEqual(code, 0, out)
        self.assertIn("after 2 load(s): ok", out)

    def test_no_journal_stops(self):
        os.remove(self.rec + ".journal")
        code, out = self.check()
        self.assertEqual(code, 1)
        self.assertIn("journal missing", out)

    def test_a_sound_on_recording_replays_with_audio(self):
        put(self.rec + ".audio", "1\n")
        env = {k: v for k, v in os.environ.items() if k != "LMA_AUDIO"}
        r = subprocess.run([sys.executable, os.path.join(HERE, "replay_check.py"), self.rec, self.state, "--cli", self.cli,
                            "--xex", "x.xex"], env={**env, "STUB_DIR": self.d}, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertIn("replaying with LMA_AUDIO=1", r.stdout)
        self.assertEqual(self.calls()[1:], ["1", "1"])  # the load-info run is silent; saved and replay runs have sound


if __name__ == "__main__":
    unittest.main()
