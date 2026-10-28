#!/usr/bin/env python3
"""Compare LMA_TEX_LOG files (#166): tools/tex_compare.py ref.log run.log [run.log ...]
A texture (address, size, format) that decodes to exactly one content in the reference run is static
there; a run where it decodes to content the reference never had is a candidate for a read-back race
(a shirt back built from data that hadn't landed, or another team's). Prints those, per run."""
import sys, collections
def load(p):
    d = collections.defaultdict(list)
    for l in open(p):
        g, a, wh, _, fmt, h = l.split()
        d[(a, wh, fmt)].append((int(g), h))
    return d
ref = load(sys.argv[1])
static = {k: v[0][1] for k, v in ref.items() if len({h for _, h in v}) == 1}
bad_any = 0
for p in sys.argv[2:]:
    run = load(p)
    bad = []
    for k, v in run.items():
        if k in static:
            others = sorted({h for _, h in v if h != static[k]})
            if others:
                bad.append((k, len(others), v[0][0]))
    print(f"{p}: {len(run)} textures, {len(bad)} static in the reference but different here")
    for (a, wh, fmt), n, g in sorted(bad)[:30]:
        print(f"   {a} {wh} fmt {fmt}: {n} other content(s)")
    bad_any += len(bad)
sys.exit(1 if bad_any else 0)
