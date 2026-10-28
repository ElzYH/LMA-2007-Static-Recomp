#!/usr/bin/env python3
"""tear_frames.py <frames dir> [k=4]: frames whose two EDRAM halves come from different frames.
For frame N (split at row 384/720): d_top = MAD(top N, top N-1), d_bot = MAD(bottom N, bottom N-1).
A torn frame has one half still equal to N-1 (small d) while the other half moved on (large d) and the
scene is moving (N-1 -> N+1 differs in that half too). Score = max(d_top, d_bot) / (min(d_top, d_bot) + 0.5),
reported when the small side < 1.0 and the large side > k, with fades/cuts excluded (both halves jump)."""
import os, sys
import numpy as np
from PIL import Image
d = sys.argv[1]; k = float(sys.argv[2]) if len(sys.argv) > 2 else 4.0
names = sorted(x for x in os.listdir(d) if x.endswith('.png'))
imgs = [np.asarray(Image.open(os.path.join(d, f)).convert('L'), dtype=float) for f in names]
hits = []
for i in range(1, len(imgs) - 1):
    p, a, n = imgs[i - 1], imgs[i], imgs[i + 1]
    h = a.shape[0] * 384 // 720
    dt = np.abs(a[:h] - p[:h]).mean(); db = np.abs(a[h:] - p[h:]).mean()
    # the half that stayed must be one that moves between N-1 and N+1 (else it is just a static half)
    mt = np.abs(n[:h] - p[:h]).mean(); mb = np.abs(n[h:] - p[h:]).mean()
    if dt < 1.0 and db > k and mt > k / 2:
        hits.append((db / (dt + 0.5), names[i], dt, db))
    if db < 1.0 and dt > k and mb > k / 2:
        hits.append((dt / (db + 0.5), names[i], dt, db))
hits.sort(reverse=True)
print(f"{len(names)} frames, {len(hits)} torn-looking (one half still the previous frame, the other moved by > {k})")
print(" ".join(f"{f}(top {dt:.1f}/bot {db:.1f})" for s, f, dt, db in hits[:15]))
