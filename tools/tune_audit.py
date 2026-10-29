#!/usr/bin/env python3
"""Who reads each data entry of the [tune] table (#870): every function that references a word of a
Table, Bytes or F32 entry's span, from the Ghidra export (local, never in the repo).

  tools/tune_audit.py [--export DIR] [--tune FILE]

A data entry changes what every reader sees, so it is safe only if its readers all belong to the rule
it names (a float literal the compiler shares, like 5.0 with 28 readers, is not). Prints one line per
entry; entries with readers in more than one function are marked "check".
Tests: tools/test_tune_audit.py
"""
import argparse
import bisect
import os
import re
import sys


def entries(src):
    """(name, kind, site, bytes) of the data entries in guest/src/tune.rs."""
    out = []
    for m in re.finditer(r'name: "(\w+)", kind: Kind::(Table|Bytes|F32)(?:\((\d+)\))?, site: (0x[0-9a-fA-F_]+)', src):
        name, kind, n, site = m.groups()
        size = {"Table": 4 * int(n or 0), "Bytes": int(n or 0), "F32": 4}[kind]
        out.append((name, kind, int(site.replace("_", ""), 16), size))
    return out


def readers(export, spans):
    """For each (lo, hi) span: the functions holding a reference into it."""
    fs = []
    with open(os.path.join(export, "functions.tsv")) as f:
        next(f)
        for line in f:
            c = line.split("\t")
            fs.append((int(c[0], 16), int(c[2])))
    fs.sort()
    starts = [a for a, _ in fs]
    los = sorted((lo, hi, i) for i, (lo, hi) in enumerate(spans))
    keys = [lo for lo, _, _ in los]
    out = [set() for _ in spans]
    with open(os.path.join(export, "xrefs.tsv")) as f:
        next(f)
        for line in f:
            a, b = line.split("\t")[:2]
            if not b.startswith("8"):
                continue
            a, b = int(a, 16), int(b, 16)
            k = bisect.bisect_right(keys, b) - 1
            while k >= 0 and los[k][0] <= b:
                lo, hi, i = los[k]
                if b < hi:
                    j = bisect.bisect_right(starts, a) - 1
                    # code Ghidra left outside a function (a switch's cases): the function before it
                    owner = starts[j] if j >= 0 else a
                    out[i].add(owner)
                k -= 1
    return out


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--export", default=os.path.expanduser("~/lma/refs/ghidra/export"))
    ap.add_argument("--tune", default=os.path.join(here, "..", "lma-rs", "crates", "guest", "src", "tune.rs"))
    a = ap.parse_args()
    es = entries(open(a.tune).read())
    if not es:
        sys.exit(f"{a.tune}: no data entries")
    rs = readers(a.export, [(s, s + n) for _, _, s, n in es])
    for (name, kind, site, n), r in zip(es, rs):
        who = ", ".join(f"{x:#x}" for x in sorted(r)) or "none"
        print(f"{'check' if len(r) != 1 else 'ok':5} {name:28} {kind:5} {site:#x}+{n:<4} read by {who}")


if __name__ == "__main__":
    main()
