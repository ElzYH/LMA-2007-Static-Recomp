#!/usr/bin/env python3
"""Tests for tools/audio_glitch.py on synthetic crowd noise (no game data; skipped without numpy):

  python tools/test_audio_glitch.py
"""
import contextlib, io, json, os, sys, tempfile, unittest, wave

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    import numpy as np
    import audio_glitch as ag
except ImportError:  # numpy missing
    np = ag = None

RATE = 48000


@unittest.skipIf(np is None, "needs numpy")
class Glitches(unittest.TestCase):
    def setUp(self):
        self.clean = np.random.default_rng(1).normal(0, 3000, 3 * RATE).astype(np.float32)

    def test_clean_crowd_noise_passes(self):
        self.assertEqual((ag.loops(self.clean, RATE), ag.gaps(self.clean, RATE)), ([], []))
        self.assertEqual(ag.clipping(np.stack([self.clean, self.clean], axis=1), RATE), [])

    def test_a_100_ms_cut_is_a_gap(self):
        cut = self.clean.copy()
        cut[RATE:RATE + RATE // 10] = 0
        g = ag.gaps(cut, RATE)
        self.assertEqual(len(g), 1)
        self.assertAlmostEqual(g[0]["start"], 1.0, delta=0.01)
        self.assertTrue(95 <= g[0]["ms"] <= 105, g)

    def test_silence_between_quiet_sounds_is_no_gap(self):
        quiet = (self.clean / 10).astype(np.float32)  # under the 'loud' level around it
        quiet[RATE:RATE + RATE // 10] = 0
        self.assertEqual(ag.gaps(quiet, RATE), [])

    def test_a_cut_longer_than_500_ms_is_no_gap(self):
        cut = self.clean.copy()
        cut[RATE:RATE + 6 * RATE // 10] = 0
        self.assertEqual(ag.gaps(cut, RATE), [])

    def test_a_60_ms_stretch_looping_for_a_second(self):
        looped = self.clean.copy()
        piece = looped[RATE:RATE + RATE * 60 // 1000]
        looped[RATE:2 * RATE] = np.tile(piece, 1000 // 60 + 1)[:RATE]
        lp = ag.loops(looped, RATE)
        self.assertEqual(len(lp), 1, lp)
        self.assertAlmostEqual(lp[0]["lag_ms"], 60, delta=2)
        self.assertGreaterEqual(lp[0]["end"] - lp[0]["start"], 0.3)

    def test_a_quiet_loop_is_skipped(self):
        looped = (self.clean / 100).astype(np.float32)  # under about -40 dBFS
        piece = looped[RATE:RATE + RATE * 60 // 1000].copy()
        looped[RATE:2 * RATE] = np.tile(piece, 1000 // 60 + 1)[:RATE]
        self.assertEqual(ag.loops(looped, RATE), [])

    def test_clipping_needs_four_samples_in_a_run(self):
        a = np.stack([self.clean, self.clean], axis=1)
        a[RATE:RATE + 100, 0] = 32767
        a[2 * RATE:2 * RATE + 3, 1] = -32768
        c = ag.clipping(a, RATE)
        self.assertEqual(c, [{"start": 1.0, "samples": 100}])


@unittest.skipIf(np is None, "needs numpy")
class Main(unittest.TestCase):
    def wav(self, d, samples):
        p = os.path.join(d, "out.wav")
        with wave.open(p, "wb") as w:
            w.setnchannels(2)
            w.setsampwidth(2)
            w.setframerate(RATE)
            w.writeframes(np.stack([samples, samples], axis=1).astype("<i2").tobytes())
        return p

    def run_main(self, *argv):
        out, old = io.StringIO(), sys.argv
        sys.argv = ["audio_glitch.py", *argv]
        try:
            with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as e:
                ag.main()
        finally:
            sys.argv = old
        return e.exception.code, out.getvalue()

    def test_load_reads_frames_and_rate(self):
        with tempfile.TemporaryDirectory() as d:
            a, rate = ag.load(self.wav(d, np.arange(100, dtype=np.float32)))
        self.assertEqual((a.shape, rate, float(a[99, 1])), ((100, 2), RATE, 99.0))

    def test_json_and_the_exit_status(self):
        clean = np.random.default_rng(2).normal(0, 3000, 2 * RATE)
        cut = clean.copy()
        cut[RATE:RATE + RATE // 10] = 0
        with tempfile.TemporaryDirectory() as d:
            code, out = self.run_main(self.wav(d, clean), "--json")
            self.assertEqual((code, json.loads(out)), (0, {"loops": [], "gaps": [], "clipping": []}))
            code, out = self.run_main(self.wav(d, cut))
        self.assertEqual(code, 1)
        self.assertIn("  gaps: start 1.000, ms 100", out)

    def test_no_arguments_print_the_usage(self):
        code, _ = self.run_main()
        self.assertIn("audio_glitch.py out.wav", str(code))


if __name__ == "__main__":
    unittest.main()
