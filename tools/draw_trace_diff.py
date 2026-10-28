#!/usr/bin/env python3
"""Draws that differ for one frame only (#205), from an LMA_DRAW_TRACE file.

    tools/draw_trace_diff.py <trace> <frame> [<frame> ...]
    tools/draw_trace_diff.py --selftest

<frame> is a presented frame in LMA_FRAME_DIR's numbering (the command processor's count). The
trace's render-thread frames are matched to it through the offset seen most often between the two
counters. For each frame, prints the draws (by index in the frame) whose shader, float-constant or
texture hashes differ from both neighbouring frames while the neighbours agree: the draw that went
wrong for one frame.
"""
import collections, struct, sys

FIELDS = ('vs', 'ps', 'vfloat', 'pfloat', 'fetch')


def load(data):
    """{frame: {draw index: hashes}} and the frame offset (CP - render thread) from a trace's bytes."""
    recs = [struct.unpack_from('<8I', data, k) for k in range(0, len(data) - 31, 32)]
    by_frame = collections.defaultdict(dict)
    offsets = collections.Counter()
    for rt, cp, idx, *h in recs:
        by_frame[rt][idx] = h
        offsets[cp - rt] += 1
    if len(by_frame) == 1:
        # no presents on the render thread (lma-cli headless): number by the CP's frames instead
        by_frame = collections.defaultdict(dict)
        for rt, cp, idx, *h in recs:
            by_frame[cp][len(by_frame[cp])] = h
        offsets = collections.Counter({0: 1})
    return by_frame, (offsets.most_common(1)[0][0] if offsets else None), len(recs)


def one_frame_draws(by_frame, r):
    """The draws of render frame r whose hashes differ from both neighbours while those agree, or
    None without both neighbours."""
    a, b, c = by_frame.get(r - 1), by_frame.get(r), by_frame.get(r + 1)
    if not (a and b and c):
        return None
    return [(i, [n for n, x, y in zip(FIELDS, a[i], b[i]) if x != y]) for i in sorted(b) if i in a and i in c and a[i] == c[i] and b[i] != a[i]]


def selftest():
    # 5 render frames of 4 draws (CP frames 100 ahead); draw 2's pixel constants change in frame 3
    # only, draw 1's vertex shader from frame 3 on (a lasting change, not a glitch)
    data = b''
    for f in range(1, 6):
        for i in range(4):
            h = [10 + i, 20 + i, 30 + i, 40 + i, 50 + i]
            if f == 3 and i == 2:
                h[3] = 99
            if f >= 3 and i == 1:
                h[0] = 77
            data += struct.pack('<8I', f, f + 100, i, *h)
    by_frame, off, n = load(data)
    assert (off, n, len(by_frame)) == (100, 20, 5), (off, n)
    assert one_frame_draws(by_frame, 3) == [(2, ['pfloat'])], one_frame_draws(by_frame, 3)
    assert one_frame_draws(by_frame, 2) == [] and one_frame_draws(by_frame, 4) == []
    assert one_frame_draws(by_frame, 5) is None
    print('selftest ok: only the one-frame change is named')


def main():
    if sys.argv[1:] == ['--selftest']:
        return selftest()
    path, frames = sys.argv[1], [int(x) for x in sys.argv[2:]]
    by_frame, off, n = load(open(path, 'rb').read())
    if off is None:
        sys.exit('empty trace')
    print(f'{n} draws in {len(by_frame)} frames; frame offset {off} (CP - render thread)')
    for f in frames:
        for r in (f - off - 1, f - off, f - off + 1):
            odd = one_frame_draws(by_frame, r)
            if odd is None:
                continue
            a, b, c = (by_frame[k] for k in (r - 1, r, r + 1))
            shift = '' if len(a) == len(b) == len(c) else ' (draw counts differ: later indices may be shifted, so a one-frame draw can hide among them)'
            print(f'frame {f} (render frame {r}, draws {len(a)}/{len(b)}/{len(c)}): {len(odd)} draws differ only here{shift}', odd[:12])


if __name__ == '__main__':
    main()
