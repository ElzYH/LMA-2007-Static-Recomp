#!/usr/bin/env python3
"""Check an LMA_XMA_DUMP directory (#199): every decoder sequence the runtime ran.

    tools/audio_check.py <dump dir>

For each `ctxNNN_SSSS_<ch>ch_<rate>.xma` / `.pcm` pair (the packets fed to one context's decoder,
and what it produced), decodes the packets independently with the ffmpeg command line and
compares, and flags output that looks like noise (loud with a very high zero-crossing rate: the
"screech"). Exit status 1 on any mismatch or noise. Needs numpy and ffmpeg.
"""
import glob
import os
import re
import struct
import subprocess
import sys

import numpy as np


def riff(data, channels, rate):
    ext = struct.pack("<HIIIIIIIBBH", 1, 3 if channels == 2 else 4, 0, len(data), 0, 0, 0, 0, 0, 4, 1)
    fmt = struct.pack("<HHIIHHH", 0x166, channels, rate, rate * channels * 2, 2 * channels, 16, len(ext)) + ext
    body = b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt + b"data" + struct.pack("<I", len(data)) + data
    return b"RIFF" + struct.pack("<I", len(body)) + body


def reference(data, channels, rate, tmp):
    with open(tmp, "wb") as f:
        f.write(riff(data, channels, rate))
    out = subprocess.run(["ffmpeg", "-hide_banner", "-v", "quiet", "-i", tmp, "-f", "s16le", "-acodec", "pcm_s16le", "-"], capture_output=True).stdout
    return np.frombuffer(out, dtype="<i2").astype(np.int32)


def noise(pcm, channels, rate):
    """Stretches (100 ms) that are loud and flip sign on most samples."""
    x = pcm.reshape(-1, channels)[:, 0].astype(np.float32)
    w = rate // 10
    out = []
    for i in range(0, len(x) - w, w):
        s = x[i:i + w]
        rms = float(np.sqrt((s ** 2).mean()))
        zc = float(np.mean(np.abs(np.diff(np.sign(s))) > 0))
        if rms > 3000 and zc > 0.5:
            out.append((i / rate, rms, zc))
    return out


def main():
    d = sys.argv[1]
    tmp = os.path.join(d, ".ref.wav")
    bad = 0
    for x in sorted(glob.glob(os.path.join(d, "ctx*.xma"))):
        m = re.search(r"ctx(\d+)_(\d+)_(\d)ch_(\d+)\.xma$", x)
        ctx, seq, ch, rate = int(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4))
        data = open(x, "rb").read()
        ours = np.fromfile(x[:-4] + ".pcm", dtype="<i2").astype(np.int32)
        if len(data) == 0:
            continue
        ref = reference(data, ch, rate, tmp)
        n = min(len(ours), len(ref))
        diff = int(np.abs(ours[:n] - ref[:n]).max()) if n else 0
        first = int(np.argmax(np.abs(ours[:n] - ref[:n]) > 2)) if diff > 2 else None
        nz = noise(ours, ch, rate)
        status = "ok"
        if diff > 2:
            status = f"MISMATCH from sample {first} ({first / ch / rate:.3f} s), max diff {diff}"
        if nz:
            status += f"; NOISE at {', '.join(f'{t:.2f} s (zc {z:.2f})' for t, _, z in nz[:5])}"
        if status != "ok":
            bad += 1
        print(f"ctx {ctx:3} seq {seq:2} {ch}ch {rate} Hz: {len(data) // 2048} packets, ours {len(ours) // ch} vs ffmpeg {len(ref) // ch} frames: {status}")
    if os.path.exists(tmp):
        os.remove(tmp)
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
