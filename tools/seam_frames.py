#!/usr/bin/env python3
"""seam_frames.py <frames dir> [threshold=3]: per frame, the luminance jump across row 384 (of 720) relative to
the neighbouring rows' jumps (ab.py's seam metric); prints frames above the threshold and the top 10."""
import os, sys
import numpy as np
from PIL import Image
d = sys.argv[1]; thr = float(sys.argv[2]) if len(sys.argv) > 2 else 3.0
rows = []
for f in sorted(os.listdir(d)):
    if not f.endswith('.png'): continue
    a = np.asarray(Image.open(os.path.join(d, f)).convert('L'), dtype=float)
    h = a.shape[0] * 384 // 720
    ref = (np.abs(a[h - 1] - a[h - 2]).mean() + np.abs(a[h + 1] - a[h]).mean()) / 2 + 1e-3
    rows.append((np.abs(a[h] - a[h - 1]).mean() / ref, f))
rows.sort(reverse=True)
print(f"{len(rows)} frames, median {np.median([r for r, _ in rows]):.2f}, max {rows[0][0]:.2f} ({rows[0][1]})")
print("above", thr, ":", " ".join(f"{f}:{r:.2f}" for r, f in rows if r > thr)[:600])
