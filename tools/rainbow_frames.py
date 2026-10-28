#!/usr/bin/env python3
"""rainbow_frames.py <frames dir> [top=15]: frames where the pitch area (rows 15%-82%) suddenly holds many
saturated non-green hues (a rainbow gradient over the terrain, #540): per frame the share of pixels with
saturation > 0.5 whose hue is outside the green band, and the jump against the previous frame."""
import os, sys
import numpy as np
from PIL import Image
d = sys.argv[1]; top = int(sys.argv[2]) if len(sys.argv) > 2 else 15
prev = None; rows = []
for f in sorted(x for x in os.listdir(d) if x.endswith('.png')):
    im = Image.open(os.path.join(d, f)).convert('HSV')
    a = np.asarray(im, dtype=float)
    h, w, _ = a.shape
    pitch = a[int(h * 0.15):int(h * 0.82)]
    hue, sat = pitch[..., 0] / 255.0 * 360.0, pitch[..., 1] / 255.0
    odd = ((sat > 0.5) & ((hue < 60) | (hue > 170))).mean()
    rows.append((odd, f, 0.0 if prev is None else odd - prev)); prev = odd
rows.sort(key=lambda r: -r[2])
print(f"{len(rows)} frames; median odd-hue share {np.median([r[0] for r in rows]):.3f}")
print("biggest jumps:", " ".join(f"{f}:+{j:.2f}({o:.2f})" for o, f, j in rows[:top]))
