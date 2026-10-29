#!/usr/bin/env python3
"""Find audio glitches in a WAV the game wrote (LMA_AUDIO_DUMP / LMA_AUDIO_WAV, #199).

    tools/audio_glitch.py out.wav [--json]

Reports, with timestamps:
- loops: stretches where the sound repeats itself almost exactly at a short lag (20-200 ms), what a
  decoder or ring replaying stale audio sounds like (the "trip-hop loop"); real music and crowd
  noise don't repeat that exactly;
- gaps: sudden silences of 20-500 ms cutting into continuous sound (cuts, underruns);
- clipping: runs of samples at full scale.
Exit status 1 if anything was found. Needs numpy. Tests: tools/test_audio_glitch.py
"""
import json
import sys
import wave

import numpy as np

RATE = 48000


def load(path):
    w = wave.open(path)
    ch, n = w.getnchannels(), w.getnframes()
    a = np.frombuffer(w.readframes(n), dtype="<i2").astype(np.float32).reshape(-1, ch)
    return a, w.getframerate()


def loops(mono, rate):
    """Windows of 200 ms (hop 100 ms) that match themselves shifted by 20-200 ms."""
    win, hop = rate // 5, rate // 10
    lags = range(rate // 50, rate // 5 + 1, rate // 1000)  # 20..200 ms in 1 ms steps
    hits = []
    for start in range(lags[-1], len(mono) - win, hop):
        x = mono[start:start + win]
        e = float(np.dot(x, x))
        if e < win * 300.0 ** 2:  # quieter than about -40 dBFS: skip
            continue
        best = None
        for lag in lags:
            y = mono[start - lag:start - lag + win]
            r = float(np.dot(x - y, x - y)) / e
            if best is None or r < best[0]:
                best = (r, lag)
        if best[0] < 0.02:  # residual under 2% of the energy: an (almost) exact repeat
            hits.append((start / rate, best[1] * 1000 / rate, best[0]))
    # merge consecutive windows into stretches
    out = []
    for t, lag, r in hits:
        if out and t - out[-1]["end"] <= 0.11 and abs(lag - out[-1]["lag_ms"]) < 2:
            out[-1]["end"] = t + win / rate
            out[-1]["residual"] = min(out[-1]["residual"], r)
        else:
            out.append({"start": t, "end": t + win / rate, "lag_ms": lag, "residual": r})
    return [o for o in out if o["end"] - o["start"] >= 0.3]  # at least ~3 windows


def gaps(mono, rate):
    """Silences of 20-500 ms with continuous loud audio in the 300 ms on both sides."""
    blk = rate // 200  # 5 ms
    n = len(mono) // blk
    rms = np.sqrt((mono[:n * blk].reshape(n, blk) ** 2).mean(axis=1))
    loud, silent = rms > 1000, rms < 3
    out, i = [], 0
    while i < n:
        if silent[i]:
            j = i
            while j < n and silent[j]:
                j += 1
            dur = (j - i) * 5
            # a cut into continuous sound: loud for most of the 300 ms on both sides (short
            # separate sounds with silence between, like ball kicks, are not gaps)
            if 20 <= dur <= 500 and i >= 60 and loud[i - 60:i].mean() > 0.8 and loud[j:j + 60].mean() > 0.8:
                out.append({"start": i * 5 / 1000, "ms": dur})
            i = j
        else:
            i += 1
    return out


def clipping(a, rate):
    full = (np.abs(a) >= 32766).any(axis=1)
    out, i, n = [], 0, len(full)
    idx = np.flatnonzero(full)
    while i < len(idx):
        j = i
        while j + 1 < len(idx) and idx[j + 1] - idx[j] < rate // 100:
            j += 1
        if j - i >= 3:
            out.append({"start": idx[i] / rate, "samples": int(j - i + 1)})
        i = j + 1
    return out


def main():
    if len(sys.argv) < 2 or sys.argv[1].startswith("--"):
        sys.exit(__doc__)
    path = sys.argv[1]
    a, rate = load(path)
    mono = a.mean(axis=1)
    found = {"loops": loops(mono, rate), "gaps": gaps(mono, rate), "clipping": clipping(a, rate)}
    if "--json" in sys.argv:
        print(json.dumps(found))
    else:
        print(f"{path}: {len(a) / rate:.1f} s")
        for kind, items in found.items():
            for it in items:
                print(f"  {kind}: " + ", ".join(f"{k} {v:.3f}" if isinstance(v, float) else f"{k} {v}" for k, v in it.items()))
        if not any(found.values()):
            print("  no glitches found")
    sys.exit(1 if any(found.values()) else 0)


if __name__ == "__main__":
    main()
