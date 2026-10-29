#!/usr/bin/env python3
"""How much of each game system is readable native code (docs/NATIVE_GAME.md's tables, #870).

  tools/native_progress.py [--export DIR] [--hooks FILE] [--shared N] [--markdown]

Reads the Ghidra export (`functions.tsv`, `calls.tsv` from scripts/ExportAll.java; local, never in
the repo) and the native hook sites (`guest::hooks::ADDRESSES`). For each system in SYSTEMS it walks
the call graph from the system's entry points and counts the functions reached and how many of them
have a native hook. A function with N or more callers (`--shared`, default 25) is a shared helper
(memory, strings, the record getters): it counts once under "shared" and the walk doesn't go through
it, so every system doesn't reach the whole game. A hook counts its function only (a wrapper or a
replacement alike), so the % is a lower bound on what is native and an upper bound on what is
rewritten; it is the first number to watch, not the last.

Tests: tools/test_native_progress.py
"""
import argparse
import bisect
import collections
import os
import re
import sys

# Per system: entry points (addresses from the #704 audit, PORT_PLAN and the hook constants) and the
# game's own source files (their paths are in the game's strings, from assertions: every function that
# references one is an entry point too). A system with neither is listed as "to find".
SYSTEMS = {
    "contracts and wages": ([0x82187DE0, 0x821884C0, 0x82188750, 0x82123DC8, 0x82120D38, 0x82118E88, 0x82187738, 0x8211C658], []),
    "transfers": ([0x821794C8, 0x8218C430, 0x82189088, 0x82188F38, 0x821889C0, 0x8217C410, 0x821208E0, 0x8211DCB0],
                  ["\\Transfer\\", "\\LoanAI.cpp"]),
    "finances and the AI economy": ([0x820E4018, 0x82104CD8, 0x822BD6B8, 0x820EAA10, 0x821071C8, 0x82107200, 0x821075A0, 0x82107608, 0x820E6BE0, 0x820EC3D8, 0x820ECAE0, 0x820EC798],
                                    ["\\Sponsors\\", "\\ScrSponsorship.cpp"]),
    "competitions and the calendar": ([0x82138E68, 0x82135930, 0x821354D0], ["\\Diary\\", "\\CupDraws\\", "\\ScrCalendar.cpp", "\\ScrLeagueDiary.cpp", "\\ScrCupDiary.cpp"]),
    "squads and AI squad management": ([0x82118FC0, 0x8218CD50, 0x8225BF10, 0x8225EDF0, 0x8225EDC0, 0x8218F068],
                                       ["\\Squad\\SelectTeam.cpp", "\\PickBestTeam\\", "\\CPUManager.cpp"]),
    "formations and tactics": ([0x8210F330, 0x8210F3F8, 0x8210F4A8, 0x8210F570, 0x82191B40], ["\\KeepTactics.cpp"]),
    "training": ([], ["\\Training\\"]),
    "player ratings and development": ([0x8211F4C8, 0x8211BE88], ["\\Players\\Player.cpp"]),
    "renderer": ([0x823398A0, 0x82339CA8, 0x823393D0, 0x823402F8, 0x82336958], ["\\Core_Graphics\\", "\\Core_Texture\\", "\\Core_Blitter\\"]),
    "audio": ([], ["\\Audio\\Audio_Xbox2.cpp", "\\MatchSounds\\", "\\MatchReportAudio.cpp"]),
    "match simulation loop": ([], ["\\Match\\Match.cpp", "\\Match\\MatchControl.cpp", "\\MatchAI\\"]),
    "day processing": ([], ["\\NextDay\\", "\\MidSeason\\", "\\EndOfSeason\\"]),
    "input": ([], ["\\Core_Controller\\", "\\Input\\"]),
    "saves": ([], ["\\MemCard\\", "\\SaveGame\\"]),
    "asset loading": ([], ["\\Core_IO\\", "\\File\\LMAFile.cpp"]),
}


def load_export(d, pointers=True):
    funcs, callers, sizes = set(), collections.Counter(), {}
    with open(os.path.join(d, "functions.tsv")) as f:
        next(f)
        for line in f:
            c = line.split("\t")
            funcs.add(int(c[0], 16))
            sizes[int(c[0], 16)] = int(c[2])
    calls = collections.defaultdict(set)
    with open(os.path.join(d, "calls.tsv")) as f:
        for line in f:
            a, b = line.split()[:2]
            a, b = int(a, 16), int(b, 16)
            if b not in calls[a]:
                calls[a].add(b)
                callers[b] += 1
    if pointers:
        for a, b in pointer_edges(d, funcs, sizes):
            if b not in calls[a]:
                calls[a].add(b)
                callers[b] += 1
    return funcs, calls, callers, sizes


def pointer_edges(d, funcs, sizes):
    """Calls the call graph doesn't have: the game's engine calls through function pointers and
    vtables. A function that takes another's address (lis/addi of it) counts as calling it, and one
    that takes a pointer table's address (a vtable: consecutive words pointing at functions) as
    calling every function in the table."""
    starts = sorted(sizes)
    table, code = {}, []
    with open(os.path.join(d, "xrefs.tsv")) as f:
        next(f)
        for line in f:
            a, b, t = line.rstrip("\n").split("\t")[:3]
            if not b.startswith("8") or t not in ("DATA", "PARAM", "READ"):
                continue
            a, b = int(a, 16), int(b, 16)
            f0 = owner(starts, sizes, a)
            if f0 is None:
                if b in funcs:
                    table[a] = b  # a data word pointing at a function
            else:
                code.append((f0, b))
    for f0, b in code:
        if b in funcs and b != f0:
            yield f0, b
        k = b
        while k in table:
            yield f0, table[k]
            k += 4


def file_entries(d, sizes, patterns):
    """The functions that reference a string containing any of `patterns` (the game's source paths)."""
    if not patterns:
        return set()
    strs = set()
    with open(os.path.join(d, "strings.tsv")) as f:
        for line in f:
            c = line.rstrip("\n").split("\t")
            if len(c) >= 3 and any(p.lower() in c[2].lower() for p in patterns):
                strs.add(int(c[0], 16))
    starts = sorted(sizes)
    out = set()
    with open(os.path.join(d, "xrefs.tsv")) as f:
        next(f)
        for line in f:
            a, b = line.split("\t")[:2]
            if b.startswith("8") and int(b, 16) in strs:  # skips stack and register references
                f0 = owner(starts, sizes, int(a, 16))
                if f0 is not None:
                    out.add(f0)
    return out


def owner(starts, sizes, addr):
    """The function whose body holds `addr`."""
    k = bisect.bisect_right(starts, addr) - 1
    if k >= 0 and addr < starts[k] + sizes[starts[k]]:
        return starts[k]
    return None


def hook_sites(path):
    """The addresses in `pub const ADDRESSES: &[u32] = &[...]`."""
    src = open(path).read()
    m = re.search(r"pub const ADDRESSES: &\[u32\] = &\[(.*?)\];", src, re.S)
    if not m:
        sys.exit(f"{path}: no ADDRESSES table")
    return {int(x.replace("_", ""), 16) for x in re.findall(r"0x[0-9A-Fa-f_]+", m.group(1))}


def reach(entries, calls, callers, shared):
    """(the functions reached from `entries`, the shared helpers it stopped at)."""
    seen, stopped, todo = set(), set(), list(entries)
    while todo:
        f = todo.pop()
        if f in seen:
            continue
        if callers[f] >= shared and f not in entries:
            stopped.add(f)
            continue
        seen.add(f)
        todo.extend(calls.get(f, ()))
    return seen, stopped


def tune_sites(path, export, sizes):
    """(name, the functions its code is in) for each entry of guest/src/tune.rs: an instruction's
    function, or the functions that read a data entry's span."""
    src = open(path).read()
    starts = sorted(sizes)
    out, data = [], []
    for m in re.finditer(r'name: "(\w+)", kind: Kind::(\w+)(?:\((0x[0-9A-Fa-f_]+|\d+)\))?, site: (0x[0-9A-Fa-f_]+)', src):
        name, kind, arg, site = m.groups()
        site = int(site.replace("_", ""), 16)
        if kind == "Imm":
            f0 = owner(starts, sizes, site)
            out.append((name, {f0} if f0 is not None else set()))
        else:
            n = {"Table": 4 * int(arg or 0), "Bytes": int(arg or 0)}.get(kind, 4)
            data.append((name, site, site + n, set()))
            out.append((name, data[-1][3]))
    with open(os.path.join(export, "xrefs.tsv")) as f:
        next(f)
        for line in f:
            a, b = line.split("\t")[:2]
            if not b.startswith("8"):
                continue
            b = int(b, 16)
            for _, lo, hi, who in data:
                if lo <= b < hi:
                    k = bisect.bisect_right(starts, int(a, 16)) - 1
                    if k >= 0:
                        who.add(starts[k])
    return out


def report(funcs, calls, callers, native, shared, files=lambda pats: set(), tune=()):
    rows = []
    for name, (addrs, pats) in SYSTEMS.items():
        entries = set(addrs) | files(pats)
        if not entries:
            rows.append((name, None, None, None, 0))
            continue
        got, stopped = reach(set(entries), calls, callers, shared)
        rows.append((name, len(got), len(got & native), len(stopped), sum(1 for _, fs in tune if fs & got)))
    helpers = {f for f in funcs if callers[f] >= shared}
    return rows, len(funcs), len(native & funcs), len(helpers)


def main():
    here = os.path.dirname(os.path.abspath(__file__))
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--export", default=os.path.expanduser("~/lma/refs/ghidra/export"), help="the Ghidra export (functions.tsv, calls.tsv)")
    ap.add_argument("--hooks", default=os.path.join(here, "..", "lma-rs", "crates", "guest", "src", "hooks.rs"))
    ap.add_argument("--shared", type=int, default=25, help="a function with this many callers or more is a shared helper")
    ap.add_argument("--markdown", action="store_true", help="a table for docs/NATIVE_GAME.md")
    ap.add_argument("--tune", default=os.path.join(here, "..", "lma-rs", "crates", "guest", "src", "tune.rs"), help="guest/src/tune.rs: count each system's [tune] names")
    ap.add_argument("--no-pointers", action="store_true", help="the call graph only (no function-pointer and vtable edges)")
    a = ap.parse_args()
    funcs, calls, callers, sizes = load_export(a.export, not a.no_pointers)
    tune = tune_sites(a.tune, a.export, sizes) if os.path.exists(a.tune) else []
    rows, total, native_total, helpers = report(funcs, calls, callers, hook_sites(a.hooks), a.shared, lambda pats: file_entries(a.export, sizes, pats), tune)
    if a.markdown:
        print("| system | functions reached | with a native hook | % | shared helpers it calls | `[tune]` names |")
        print("|---|---|---|---|---|---|")
        for name, n, h, s, t in rows:
            print(f"| {name} | to find | | | | |" if n is None else f"| {name} | {n} | {h} | {100 * h / n:.1f}% | {s} | {t} |")
    else:
        for name, n, h, s, t in rows:
            print(f"{name:32} to find (no entry points yet)" if n is None else f"{name:32} {n:5} reached, {h:3} native ({100 * h / n:4.1f}%), {s} shared helpers, {t} [tune] names")
    if tune:
        print(f"[tune]: {len(tune)} names in {a.tune}")
    print(f"\nwhole game: {total} functions in the export, {native_total} with a native hook ({100 * native_total / total:.2f}%); {helpers} shared helpers (>= {a.shared} callers)")


if __name__ == "__main__":
    main()
