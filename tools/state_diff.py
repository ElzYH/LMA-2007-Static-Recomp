#!/usr/bin/env python3
"""state_diff.py: where did a value land in guest memory, between two save states.

    tools/state_diff.py A.state B.state                 changed 4-byte words, grouped into ranges
    tools/state_diff.py A.state B.state --to 187000000  changed words whose new value is N
    tools/state_diff.py A.state B.state --to 5 --from 3 ... and whose old value was M
    tools/state_diff.py B.state --find 187000000        words equal to N in one state
    options: --width 1|2|4 (default 4), --limit 200, --cli <lma-cli> (default: LMA_CLI or
             ~/lma/play-fc27/lma-cli next to the launcher's lma-app), --xex <default.xex>
             (default ~/lma/LMA_full/default.xex), --keep <dir> (keep the memory dumps)

Memory comes from `lma-cli state-mem <xex> <state> <out.bin>`: a sparse 4 GB file whose offset is
the guest address, so no save-state parsing lives here. Two dumps are compared 1 MB at a time
(identical chunks skipped), then word by word; guest words are big-endian. Feed the addresses it
prints to Ghidra (~/lma/refs/ghidra/export/xrefs.tsv, or pyghidra-mcp) for the readers and writers
(the maintainer, 11 Oct: the player form field, funds between team select and the hub, contract years).
"""
import argparse
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

CHUNK = 1 << 20
MEM = 1 << 32


def default_cli():
    c = os.environ.get("LMA_CLI")
    if c:
        return c
    app = os.path.expanduser("~/lma/play-fc27/lma-app")
    if os.path.islink(app):
        c = os.path.join(os.path.dirname(os.path.realpath(app)), "lma-cli")
        if os.path.exists(c):
            return c
    return "lma-cli"


def dump(state, cli, xex, keep=None):
    """Path of the sparse memory image of `state` (made with lma-cli state-mem, cached by mtime)."""
    # the full path in the name: a/x.state and b/x.state must not share one cached dump
    tag = hashlib.sha1(os.path.abspath(state).encode()).hexdigest()[:8]
    out = os.path.join(keep or tempfile.gettempdir(), f"{os.path.basename(state)}.{tag}.mem")
    if not (os.path.exists(out) and os.path.getmtime(out) >= os.path.getmtime(state)):
        subprocess.run([cli, "state-mem", xex, state, out], check=True, stdout=subprocess.DEVNULL)
    return out


def ranges(addrs, w=4):
    """Sorted addresses of w-byte words grouped into runs (a gap of one unchanged word joins two):
    [(start, end_exclusive)]."""
    out = []
    for a in sorted(addrs):
        if out and a <= out[-1][1] + w:
            out[-1][1] = a + w
        else:
            out.append([a, a + w])
    return [(s, e) for s, e in out]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("states", nargs="+")
    ap.add_argument("--width", type=int, default=4, choices=(1, 2, 4))
    ap.add_argument("--to", type=int, help="new value to look for (B only)")
    ap.add_argument("--find", type=int, help="value to look for in one state")
    ap.add_argument("--from", dest="old", type=int, help="old value (with --to)")
    ap.add_argument("--limit", type=int, default=200)
    ap.add_argument("--cli", default=default_cli())
    ap.add_argument("--xex", default=os.path.expanduser("~/lma/LMA_full/default.xex"))
    ap.add_argument("--keep", help="directory to keep the memory dumps in")
    a = ap.parse_args()
    w = a.width
    import mmap
    fmt = {1: ">B", 2: ">H", 4: ">I"}[w]
    st = struct.Struct(fmt)
    if len(a.states) == 1:
        if a.find is None and a.to is None:
            ap.error("one state needs --find N")
        want = a.find if a.find is not None else a.to
        pat = st.pack(want)
        with open(dump(a.states[0], a.cli, a.xex, a.keep), "rb") as f, mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ) as m:
            hits = []
            for off in range(0, MEM, CHUNK):
                chunk = m[off:off + CHUNK]
                if not chunk.strip(b"\0"):
                    continue
                i = chunk.find(pat)
                while i >= 0:
                    if i % w == 0:
                        hits.append(off + i)
                    i = chunk.find(pat, i + 1)
        print(f"{len(hits)} word(s) == {want} ({w}-byte)")
        for h in hits[: a.limit]:
            print(f"  {h:#010x}")
        return
    da, db = dump(a.states[0], a.cli, a.xex, a.keep), dump(a.states[1], a.cli, a.xex, a.keep)
    changed = []
    with open(da, "rb") as fa, open(db, "rb") as fb, mmap.mmap(fa.fileno(), 0, access=mmap.ACCESS_READ) as ma, mmap.mmap(fb.fileno(), 0, access=mmap.ACCESS_READ) as mb:
        for off in range(0, MEM, CHUNK):
            ca, cb = ma[off:off + CHUNK], mb[off:off + CHUNK]
            if ca == cb:
                continue
            for i in range(0, CHUNK, w):
                x, y = ca[i:i + w], cb[i:i + w]
                if x == y:
                    continue
                xv, yv = st.unpack(x)[0], st.unpack(y)[0]
                if a.to is not None and yv != a.to:
                    continue
                if a.old is not None and xv != a.old:
                    continue
                changed.append((off + i, xv, yv))
    if a.to is not None or a.old is not None:
        print(f"{len(changed)} word(s) changed to {a.to}" + (f" from {a.old}" if a.old is not None else ""))
        for addr, x, y in changed[: a.limit]:
            print(f"  {addr:#010x}: {x} -> {y}")
        return
    rs = ranges((addr for addr, _, _ in changed), w)
    print(f"{len(changed)} changed words in {len(rs)} ranges")
    for s, e in rs[: a.limit]:
        print(f"  {s:#010x}-{e:#010x} ({e - s} bytes)")


if __name__ == "__main__":
    main()
