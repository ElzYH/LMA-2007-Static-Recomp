#!/usr/bin/env python3
"""Dump the season calendar inside Export.mng (#703): the transfer windows (TWNNNN.bin) and the
rounds of a season's diary (diaryNNNN.bin), with every round's date decoded.

    python tools/diary_dump.py <Export.mng> [season 0-19] [--rounds N] [--records]

Export.mng is a BG2 archive (lma-data archive.rs) holding Diary20.mng, itself a BG2 archive of 20
seasons (2006/07 .. 2025/26): TWNNNN.bin (108 bytes, 27 big-endian u32) and diaryNNNN.bin (~83 KB).

TW: two windows, each `open_day, open_dom, open_month0, close_day, close_dom, close_month0, 11, 0,
flag`, days counted from 1 July of the season (0 = 1 Jul, 61 = 31 Aug, 184 = 1 Jan, 214 = 31 Jan);
identical in all 20 files on the disc.

Diary: runs of 6-byte fixture records (`home u16 BE, away u16 BE, ff 00`, 1-based club ids as
clubs.csv numbers them plus one: a Ligue 2 round is ten records in 263..282; `00 00 00 00 ff 00`
is a blank slot)
separated by round headers of 0, 4, 10, 20 or 24 bytes (58 for the first). A header's 16-bit
little-endian fields with the season in bits 10+ are date codes: day = code & 0x3ff from 1 July
(round 1 of 2006/07, 2007/08 and 2008/09 fall on 14 Sep 2006, 6 Sep 2007 and 16 Oct 2008, all
Thursdays; round 2 on Sundays; the fixed code 11 is 12 July; bit 15 is a flag on some). The other pairs (`31 04`, `32 1f`,
`30 25`, `02 03`, ...) are the same in every season: competition, round and leg codes, still to be
read off against the game's calendar screen. Nothing is written; the game's files stay the user's.
"""
import datetime as dt
import struct
import sys


def parse_bg2(d):
    if d[:4] != b"BG2\0":
        sys.exit("not a BG2 archive")
    table_bytes, data_start = struct.unpack(">II", d[8:16])
    entries = []
    off = 0x14
    while off + 0x108 <= 0x14 + table_bytes:
        name = d[off:off + 0x100].split(b"\0", 1)[0].decode("latin-1")
        o, s = struct.unpack(">II", d[off + 0x100:off + 0x108])
        entries.append((name, d[data_start + o:data_start + o + s]))
        off += 0x108
    return dict(entries)


def season_date(season_year, code):
    """A diary date code to a date: the season in bits 10+, the day from 1 July in the low ten."""
    return dt.date(season_year, 7, 1) + dt.timedelta(days=code & 0x3FF)


def rounds_of(b):
    """(header bytes, records) per round: records are the 6-byte `.. ff 00` runs."""
    ff = [i for i in range(len(b) - 1) if b[i] == 0xFF and b[i + 1] == 0x00]
    out, k = [], 0
    while k < len(ff):
        j = k
        while j + 1 < len(ff) and ff[j + 1] - ff[j] == 6:
            j += 1
        hdr_start = ff[k - 1] + 2 if k > 0 else 0
        recs = [struct.unpack(">HH", b[ff[i] - 4:ff[i]]) for i in range(k, j + 1)]
        out.append((b[hdr_start:ff[k] - 4], recs))
        k = j + 1
    return out


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    flags = [a for a in sys.argv[1:] if a.startswith("--")]
    if not args:
        sys.exit(__doc__)
    season = int(args[1]) if len(args) > 1 else 0
    max_rounds = next((int(f.split("=", 1)[1]) for f in flags if f.startswith("--rounds=")), 40)
    show_records = "--records" in flags
    outer = parse_bg2(open(args[0], "rb").read())
    inner = parse_bg2(outer["Diary20.mng"])
    year = 2006 + season
    tw = struct.unpack(">27I", inner[f"TW{season:04}.bin"])
    print(f"season {season} = {year}/{str(year + 1)[2:]}")
    for w in range(2):
        f = tw[w * 9:w * 9 + 9]
        o, c = season_date(year, f[0]), season_date(year, f[3])
        print(f"  transfer window {w + 1}: day {f[0]} ({o}, {f[1]}/{f[2] + 1}) to day {f[3]} ({c}, {f[4]}/{f[5] + 1}); other fields {f[6:]}")
    print(f"  third group: {tw[18:]}")
    diary = rounds_of(inner[f"diary{season:04}.bin"])
    # a date field is one whose value in the neighbouring season's file sits one season (0x400)
    # further on, give or take the weekday shift: season 0's season bits are zero, so small
    # fields would otherwise look like dates
    other = season + 1 if season < 19 else season - 1
    step = 0x400 * (other - season)
    diary_other = rounds_of(inner[f"diary{other:04}.bin"])
    print(f"  {len(diary)} rounds, {sum(len(r) for _, r in diary)} records (dates checked against season {other})")
    for i, (hdr, recs) in enumerate(diary[:max_rounds]):
        dates = []
        hdr_other = diary_other[i][0] if i < len(diary_other) and len(diary_other[i][0]) == len(hdr) else b""
        for k in range(0, len(hdr) - 1, 2):
            v = struct.unpack("<H", hdr[k:k + 2])[0] & 0x7FFF  # bit 15 is a flag on some rounds
            if v >> 10 != season or (v & 0x3FF) >= 366:
                continue
            if hdr_other:
                w = struct.unpack("<H", hdr_other[k:k + 2])[0] & 0x7FFF
                if abs((w - v) - step) > 40:
                    continue
            dates.append(f"+{k}:{season_date(year, v)}")
        real = [r for r in recs if r != (0, 0)]
        print(f"  round {i:4}: header {len(hdr):2} bytes [{hdr.hex(' ')}] dates {' '.join(dates) or '-'}; {len(recs)} records, {len(real)} filled")
        if show_records and real:
            print("      " + " ".join(f"{a - 1}-{b - 1}" for a, b in real[:24]) + (" ..." if len(real) > 24 else "") + "  (clubs.csv ids)")


if __name__ == "__main__":
    main()
