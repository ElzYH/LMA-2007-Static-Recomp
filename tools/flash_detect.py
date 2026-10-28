#!/usr/bin/env python3
"""One-frame flashes in a frame dump (#205): frames where the players turn darker (or brighter)
for exactly one frame.

    tools/flash_detect.py <frames dir>
    tools/flash_detect.py --selftest

<frames dir> holds PNGs named by guest frame number, as lma-app's LMA_FRAME_DIR writes them.
Per frame, in the pitch area (rows 15%-82%, below the score and above the captions), it measures
the share of dark and of bright pixels that aren't grass, and the mean brightness. A frame whose
measure jumps away from both neighbours in the same direction, while the neighbours agree, is a
flash. Only runs of consecutive guest frames are compared. Camera cuts and pans change these
measures smoothly or for good, so they don't count; comparing pixels directly can't tell a pan
from a flash. Needs numpy and Pillow.
"""
import glob
import os
import sys

import numpy as np
from PIL import Image


def readable(f):
    try:
        Image.open(f).load()
        return True
    except Exception:
        return False  # the last frame of a killed run may be cut short


def stats(f):
    a = np.asarray(Image.open(f).convert('RGB'), dtype=np.int16)
    h = a.shape[0]
    a = a[int(h * .15):int(h * .82)]
    r, g, b = a[..., 0], a[..., 1], a[..., 2]
    lum = (r * 3 + g * 6 + b) / 10
    grass = (g > r + 8) & (g > b + 8)
    n = lum.size
    return np.array([((lum < 50) & ~grass).sum() / n * 1000, ((lum > 150) & ~grass).sum() / n * 1000, lum.mean(), grass.sum() / n * 100])


NAMES = ['dark', 'bright', 'lum', 'grass']
# below these the measure is noise (per mille of pixels, brightness, percent)
FLOOR = {'dark': 2, 'bright': 2, 'lum': 5, 'grass': 3}


def selftest():
    """Synthetic frames: a striped pitch panning sideways with players walking across it, and
    a camera cut; in one frame the players are darker. Only that frame may be flagged."""
    import tempfile
    with tempfile.TemporaryDirectory() as d:
        for n in range(40):
            x = np.arange(320)[None, :] + 3 * n  # the camera pans 3 px a frame
            if n >= 25:
                x = x + 100  # a cut
            stripes = ((x // 20) % 2).astype(np.uint8) * 25
            img = np.zeros((180, 320, 3), np.uint8)
            img[..., 0], img[..., 1], img[..., 2] = 40 + stripes, 120 + stripes, 40 + stripes
            kit = (60, 50, 20) if n == 17 else (230, 200, 40)  # frame 17: shadowed players
            for i in range(12):
                px, py = (25 * i + 2 * n) % 300, 40 + (37 * i) % 90
                img[py:py + 10, px:px + 4] = kit
            Image.fromarray(img).save(os.path.join(d, f'{1000 + n:07}.png'))
        found = run(d)
    flagged = sorted({n for _, n, *_ in found})
    assert flagged == [1017], flagged
    print('selftest ok: only the darkened frame is flagged')


def run(d):
    fs = [f for f in sorted(glob.glob(os.path.join(d, '*.png'))) if readable(f)]
    num = [int(os.path.basename(f)[:-4]) for f in fs]
    s = np.array([stats(f) for f in fs])
    hits = []
    for k in range(1, len(fs) - 1):
        if num[k] - num[k - 1] != 1 or num[k + 1] - num[k] != 1:
            continue
        for j, name in enumerate(NAMES):
            a, b, c = s[k - 1, j], s[k, j], s[k + 1, j]
            scale = max(abs(a), abs(c), FLOOR[name])
            d1, d2 = b - a, b - c
            if np.sign(d1) == np.sign(d2) and min(abs(d1), abs(d2)) > 0.25 * scale and abs(a - c) < 0.08 * scale:
                hits.append((min(abs(d1), abs(d2)) / scale, num[k], name, a, b, c))
    return hits


def main():
    if sys.argv[1] == '--selftest':
        return selftest()
    hits = run(sys.argv[1])
    print(f'{len(hits)} flashes')
    for score, n, name, a, b, c in sorted(hits, reverse=True)[:30]:
        print(f'  frame {n}: {name} {a:.1f} -> {b:.1f} -> {c:.1f} (score {score:.2f})')
    sys.exit(1 if hits else 0)


if __name__ == '__main__':
    main()
