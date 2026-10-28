#!/usr/bin/env python3
"""video_bands.py <phone video of the game> [--top 0.35 --bottom 0.75]: counts the one-frame bands
over the pitch (#540): frames where a share of the pitch area turns purple/blue (blue above green)
while the frame is mostly pitch. Consecutive hits (gap <= 3 frames) count as one event.

the maintainer's video of 13 Oct (build c34e4ae, night match, 29 s): 12 events, e.g. frames 175 and 739.
The pitch rows default to 35-75% of the video's height (a phone held at the screen); adjust them
when the game window sits elsewhere. Needs ffmpeg and numpy; the video itself stays local.
"""
import argparse, subprocess, sys
import numpy as np

W, H = 320, 180


def frame_scores(path, top, bottom):
    p = subprocess.Popen(['ffmpeg', '-v', 'error', '-i', path, '-vf', f'scale={W}:{H}', '-f', 'rawvideo',
                          '-pix_fmt', 'rgb24', '-'], stdout=subprocess.PIPE)
    n = 0
    while True:
        b = p.stdout.read(W * H * 3)
        if len(b) < W * H * 3:
            break
        a = np.frombuffer(b, np.uint8).reshape(H, W, 3).astype(int)
        yield n, scores(a[int(H * top):int(H * bottom)])
        n += 1


def scores(pitch):
    """(share of green pixels, share of purple/blue ones) in an RGB array."""
    r, g, b = pitch[..., 0], pitch[..., 1], pitch[..., 2]
    return float((g > b + 30).mean()), float(((b > g + 5) & (b > 60)).mean())


def events(hits, gap=3):
    """Groups of frame numbers no more than `gap` apart."""
    out = []
    for f in hits:
        if out and f - out[-1][-1] <= gap:
            out[-1].append(f)
        else:
            out.append([f])
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('video')
    ap.add_argument('--top', type=float, default=0.35)
    ap.add_argument('--bottom', type=float, default=0.75)
    ap.add_argument('--fps', type=float, default=59.94)
    a = ap.parse_args()
    hits, frames = [], 0
    for n, (green, purple) in frame_scores(a.video, a.top, a.bottom):
        frames = n + 1
        if green > 0.5 and purple > 0.01:  # a pitch frame (not a cutscene) with a band
            hits.append(n)
    ev = events(hits)
    print(f'{frames} frames ({frames / a.fps:.1f} s): {len(ev)} band events')
    for e in ev:
        print(f'  t={e[0] / a.fps:6.2f}s frames {e[0]}-{e[-1]}')


if __name__ == '__main__':
    if len(sys.argv) > 1 and sys.argv[1] == '--selftest':
        pitch = np.zeros((10, 10, 3), int)
        pitch[..., 1] = 160
        pitch[..., 2] = 40
        assert scores(pitch) == (1.0, 0.0)
        pitch[8:, :, 2] = 200  # a band over the bottom fifth
        assert scores(pitch) == (0.8, 0.2)
        assert events([1, 2, 3, 10, 30, 32]) == [[1, 2, 3], [10], [30, 32]]
        print('selftest OK')
    else:
        main()
