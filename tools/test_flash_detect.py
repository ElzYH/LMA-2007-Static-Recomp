#!/usr/bin/env python3
"""Tests for tools/flash_detect.py on synthetic pitch frames (no game data; skipped without numpy
and Pillow):

  python tools/test_flash_detect.py
"""
import contextlib, io, os, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
try:
    import numpy as np
    from PIL import Image
    import flash_detect as fd
except ImportError:
    np = fd = None


def pitch(n, dark_at=None, cut_at=25):
    """A striped pitch panning 3 px a frame (a cut at `cut_at`), 12 players walking across it;
    at frame `dark_at` the players are shadowed."""
    x = np.arange(320)[None, :] + 3 * n + (100 if n >= cut_at else 0)
    stripes = ((x // 20) % 2).astype(np.uint8) * 25
    img = np.zeros((180, 320, 3), np.uint8)
    img[..., 0], img[..., 1], img[..., 2] = 40 + stripes, 120 + stripes, 40 + stripes
    kit = (60, 50, 20) if n == dark_at else (230, 200, 40)
    for i in range(12):
        px, py = (25 * i + 2 * n) % 300, 40 + (37 * i) % 90
        img[py:py + 10, px:px + 4] = kit
    return Image.fromarray(img)


@unittest.skipIf(fd is None, "needs numpy and Pillow")
class Flashes(unittest.TestCase):
    def setUp(self):
        self.t = tempfile.TemporaryDirectory()
        self.d = self.t.name
        self.addCleanup(self.t.cleanup)

    def frames(self, numbers, name="{:07}.png", **kw):
        for k, n in enumerate(numbers):
            pitch(k, **kw).save(os.path.join(self.d, name.format(n)))

    def flagged(self):
        return sorted({n for _, n, *_ in fd.run(self.d)})

    def test_only_the_darkened_frame_through_pans_and_a_cut(self):
        self.frames(range(1000, 1040), dark_at=17)
        self.assertEqual(self.flagged(), [1017])

    def test_steady_play_has_none(self):
        self.frames(range(1000, 1040))
        self.assertEqual(self.flagged(), [])

    def test_a_gap_in_the_numbers_isnt_compared_across(self):
        # frame 17's neighbours aren't consecutive guest frames: nothing to compare it with
        self.frames([*range(1000, 1017), 1030, *range(1050, 1072)], dark_at=17)
        self.assertEqual(self.flagged(), [])

    def test_lma_cli_names_sorted_by_number_past_five_digits(self):
        # frame_99999 .. frame_100038: a text sort would put 100000.. first
        self.frames(range(99990, 100030), name="frame_{:05}.png", dark_at=12)
        self.assertEqual(self.flagged(), [100002])

    def test_a_cut_short_frame_and_other_files_are_skipped(self):
        self.frames(range(1000, 1040), dark_at=17)
        with open(os.path.join(self.d, "1040.png"), "wb") as f:
            f.write(b"\x89PNG\r\n\x1a\n")  # a killed run's last frame
        with open(os.path.join(self.d, "screen.png"), "wb") as f:
            f.write(b"x")
        self.assertEqual(self.flagged(), [1017])

    def test_frame_numbers(self):
        self.assertEqual([fd.frame_number(f) for f in ("a/0001234.png", "frame_00012.png", "screen.png", "12.jpg")], [1234, 12, None, None])

    def test_main_exit_status_and_usage(self):
        self.frames(range(1000, 1040), dark_at=17)
        out, old = io.StringIO(), sys.argv
        try:
            sys.argv = ["flash_detect.py", self.d]
            with contextlib.redirect_stdout(out), self.assertRaises(SystemExit) as e:
                fd.main()
            self.assertEqual(e.exception.code, 1)
            self.assertIn("frame 1017:", out.getvalue())
            sys.argv = ["flash_detect.py"]
            with self.assertRaises(SystemExit) as e:
                fd.main()
            self.assertIn("flash_detect.py <frames dir>", str(e.exception.code))
        finally:
            sys.argv = old


if __name__ == "__main__":
    unittest.main()
