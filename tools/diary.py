#!/usr/bin/env python3
"""The game's fixture diary, read from a save state (#735): every day of the season with its
competition rounds and their fixtures (club uids and scores).

  tools/diary.py <state> [--cli PATH] [--xex PATH] [what]
  tools/diary.py --mem <mem.bin> [what]          (a dump from `lma-cli state-mem`)

  what (default: summary):
    --date DD/MM            the rounds of that day, one line each (fixtures, with teams, played)
    --comp N [--round R]    every fixture of competition N (and round R)
    --uid U[,U...]          every fixture of these club uids
    --diff <state|mem.bin>  per competition: fixtures, with teams, played, here vs there

The layout (Ghidra, #735):
  [0x82740400] -> the first day record. A day: +0 u16 packed date (day<<11 | (month-1)<<7 | ...),
  +2 u8 entry count, +3 u8 kind (0 competition rounds, 1 and 2 fixed-size entries of 4 / 2 bytes,
  3 the diary's end), +4 u16 bytes to the next day, entries from +6.
  A round (kind 0): +0 s8 competition, +1 u8 round, +2 u8 fixture count, fixtures from +4, 6 bytes
  each: u16 home uid, u16 away uid, u16 status (high nibble home goals, next nibble away goals;
  0xF = not played, 0xE = no result either).
  Functions: 0x820ef568 day_is_end, 0x820efee0 next_day, 0x820ef480 entry, 0x821302d8 find_round,
  0x82130458 has_next, 0x820ef0d8 round byte, 0x820ef108 a fixture's team, 0x82130de8 "has a result".
Nothing from the game is written anywhere: the dump goes to a temp file that is deleted.
"""
import argparse, collections, mmap, os, struct, subprocess, sys, tempfile

DIARY_PTR = 0x82740400
MAX_DAYS = 4000


class Mem:
    """Guest memory from a flat dump (guest address = file offset)."""

    def __init__(self, path):
        self.f = open(path, "rb")
        self.m = mmap.mmap(self.f.fileno(), 0, access=mmap.ACCESS_READ)

    def read(self, a, n):
        return self.m[a:a + n]


def u8(read, a):
    return read(a, 1)[0]


def u16(read, a):
    return struct.unpack(">H", read(a, 2))[0]


def u32(read, a):
    return struct.unpack(">I", read(a, 4))[0]


def date_of(packed):
    """(day, month) of a day record's packed date."""
    return packed >> 11, ((packed >> 7) & 0xF) + 1


def played(status):
    return status >> 12 < 14


def days(read):
    """Yield (index, address, packed date, kind, rounds) for every day record up to the end marker;
    rounds = [(competition, round, [(home, away, status), ...])] for kind 0, else []. Raises on a
    zero-length day or a diary that runs past MAX_DAYS (the loops of #735 would spin there)."""
    day = u32(read, DIARY_PTR)
    for i in range(MAX_DAYS):
        if day == 0:
            return
        count, kind, length = u8(read, day + 2), u8(read, day + 3), u16(read, day + 4)
        rounds = []
        if kind == 0:
            e = day + 6
            for _ in range(count):
                comp = struct.unpack("b", read(e, 1))[0]
                rnd, n = u8(read, e + 1), u8(read, e + 2)
                fx = [(u16(read, e + 4 + 6 * j), u16(read, e + 6 + 6 * j), u16(read, e + 8 + 6 * j)) for j in range(n)]
                rounds.append((comp, rnd, fx))
                e += 4 + 6 * n
        yield i, day, u16(read, day), kind, rounds
        if kind == 3:
            return
        if length == 0:
            raise ValueError(f"day {i} at {day:#x} has length 0: next_day would return it forever")
        day += length
    raise ValueError(f"no end marker within {MAX_DAYS} days")


def fill(read):
    """Per competition: [fixtures, with teams, played]."""
    out = collections.defaultdict(lambda: [0, 0, 0])
    for _, _, _, _, rounds in days(read):
        for comp, _, fx in rounds:
            for h, a, s in fx:
                c = out[comp]
                c[0] += 1
                c[1] += bool(h or a)
                c[2] += played(s)
    return dict(out)


def fixture_line(i, packed, comp, rnd, j, n, h, a, s):
    d, m = date_of(packed)
    score = "-" if not played(s) else f"{s >> 12}-{(s >> 8) & 0xF}"
    return f"day {i:3} {d:2}/{m:<2} comp {comp:2} round {rnd:2} fx {j:2}/{n:<3} {h:5} v {a:5}  {score:>5}  status {s:04x}"


def summary(read):
    n, end, comps = 0, None, collections.Counter()
    for i, addr, _, kind, rounds in days(read):
        n = i + 1
        if kind == 3:
            end = addr
        for comp, _, _ in rounds:
            comps[comp] += 1
    print(f"{n} day records from {u32(read, DIARY_PTR):#x}, " + (f"end marker at {end:#x}" if end else "no end marker"))
    print("rounds per competition: " + ", ".join(f"{c}:{k}" for c, k in sorted(comps.items())))


def dump_state(state, cli, xex, out):
    r = subprocess.run([cli, "state-mem", xex, state, out], capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"lma-cli state-mem failed: {r.stderr.strip()[-400:]}")


def open_mem(path, a, tmp):
    if path.endswith(".state"):
        out = os.path.join(tmp, f"mem{len(os.listdir(tmp))}.bin")
        dump_state(path, a.cli, a.xex, out)
        path = out
    return Mem(path).read


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("state", nargs="?", help="a save state (dumped through lma-cli state-mem)")
    ap.add_argument("--mem", help="a flat guest memory dump instead of a state")
    ap.add_argument("--cli", default=os.path.join("lma-rs", "target", "release", "lma-cli"))
    ap.add_argument("--xex", default=os.path.expanduser(os.path.join("~", "lma", "LMA_full", "default.xex")))
    ap.add_argument("--date")
    ap.add_argument("--comp", type=int)
    ap.add_argument("--round", type=int)
    ap.add_argument("--uid")
    ap.add_argument("--diff")
    a = ap.parse_args(argv)
    if not (a.state or a.mem):
        ap.error("a state or --mem")
    with tempfile.TemporaryDirectory() as tmp:
        read = open_mem(a.mem or a.state, a, tmp)
        if a.date:
            want = tuple(int(x) for x in a.date.split("/"))
            for i, _, packed, _, rounds in days(read):
                if date_of(packed) == want:
                    for comp, rnd, fx in rounds:
                        print(f"day {i:3} {a.date} comp {comp:2} round {rnd:2} fixtures {len(fx):3} with teams {sum(bool(h or w) for h, w, _ in fx):3} played {sum(played(s) for _, _, s in fx):3}")
        elif a.comp is not None or a.uid:
            uids = {int(x) for x in a.uid.split(",")} if a.uid else None
            for i, _, packed, _, rounds in days(read):
                for comp, rnd, fx in rounds:
                    if a.comp is not None and (comp != a.comp or a.round is not None and rnd != a.round):
                        continue
                    for j, (h, w, s) in enumerate(fx):
                        if uids is None or h in uids or w in uids:
                            print(fixture_line(i, packed, comp, rnd, j, len(fx), h, w, s))
        elif a.diff:
            here, there = fill(read), fill(open_mem(a.diff, a, tmp))
            print("comp  here: fixtures/with teams/played   there: fixtures/with teams/played")
            for c in sorted(set(here) | set(there)):
                x, y = here.get(c, [0, 0, 0]), there.get(c, [0, 0, 0])
                print(f"{c:4}  {x[0]:5} {x[1]:5} {x[2]:5}            {y[0]:5} {y[1]:5} {y[2]:5}{'' if x == y else '   <-'}")
        else:
            summary(read)


if __name__ == "__main__":
    main()
