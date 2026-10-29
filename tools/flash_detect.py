#!/usr/bin/env python3
"""One-frame flashes in a frame dump (#205): frames where the players turn darker (or brighter)
for exactly one frame.

    tools/flash_detect.py <frames dir>

<frames dir> holds PNGs named by guest frame number, as lma-app's LMA_FRAME_DIR (`0001234.png`) or
lma-cli's LMA_FRAMES (`frame_01234.png`) writes them.
Per frame, in the pitch area (rows 15%-82%, below the score and above the captions), it measures
the share of dark and of bright pixels that aren't grass, and the mean brightness. A frame whose
measure jumps away from both neighbours in the same direction, while the neighbours agree, is a
flash. Only runs of consecutive guest frames are compared. Camera cuts and pans change these
measures smoothly or for good, so they don't count; comparing pixels directly can't tell a pan
from a flash. Needs numpy and Pillow. Tests: tools/test_flash_detect.py
"""
import glob
import os
import re
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


def frame_number(f):
    """The guest frame number at the end of the name: lma-app's `0001234.png`, lma-cli's `frame_01234.png`."""
    m = re.search(r'(\d+)\.png$', os.path.basename(f))
    return int(m.group(1)) if m else None


def run(d):
    fs = sorted((f for f in glob.glob(os.path.join(d, '*.png')) if frame_number(f) is not None and readable(f)), key=frame_number)
    num = [frame_number(f) for f in fs]
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
    if len(sys.argv) != 2 or sys.argv[1].startswith('--'):
        sys.exit(__doc__)
    hits = run(sys.argv[1])
    print(f'{len(hits)} flashes')
    for score, n, name, a, b, c in sorted(hits, reverse=True)[:30]:
        print(f'  frame {n}: {name} {a:.1f} -> {b:.1f} -> {c:.1f} (score {score:.2f})')
    sys.exit(1 if hits else 0)


if __name__ == '__main__':
    main()
