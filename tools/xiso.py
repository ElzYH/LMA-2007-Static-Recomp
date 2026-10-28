"""Minimal XDVDFS (Xbox / Xbox 360 XISO) lister and extractor.

usage: python xiso.py <image.iso> list
       python xiso.py <image.iso> extract <outdir>
"""
import os
import struct
import sys

SECTOR = 2048
MAGIC = b"MICROSOFT*XBOX*MEDIA"


def find_base(f):
    for base in (0, 0xFD90000, 0x2080000, 0x18300000):
        f.seek(base + 32 * SECTOR)
        if f.read(20) == MAGIC:
            return base
    raise SystemExit("no XDVDFS volume found")


def walk(f, base, sector, size, prefix=""):
    """Yield (path, start_sector, size, is_dir) for a directory table."""
    if size == 0:
        return
    f.seek(base + sector * SECTOR)
    table = f.read(size)
    seen = set()
    stack = [0]
    while stack:
        off = stack.pop()
        if off in seen or off + 14 > len(table):
            continue
        seen.add(off)
        left, right, start, length, attr, nlen = struct.unpack_from("<HHIIBB", table, off)
        if left == 0xFFFF:  # padding
            continue
        name = table[off + 14:off + 14 + nlen].decode("latin1")
        path = prefix + name
        if attr & 0x10:
            yield path, start, length, True
            yield from walk(f, base, start, length, path + "/")
        else:
            yield path, start, length, False
        if left:
            stack.append(left * 4)
        if right:
            stack.append(right * 4)


def entries(iso):
    f = open(iso, "rb")
    base = find_base(f)
    f.seek(base + 32 * SECTOR + 20)
    root, rsize = struct.unpack("<II", f.read(8))
    return f, base, sorted(walk(f, base, root, rsize))


def main():
    iso, cmd = sys.argv[1], sys.argv[2]
    f, base, ents = entries(iso)
    if cmd == "list":
        for path, start, length, is_dir in ents:
            print(f"{'D' if is_dir else 'F'} {length:12d} {path}")
    elif cmd == "extract":
        out = sys.argv[3]
        for path, start, length, is_dir in ents:
            dst = os.path.join(out, *path.split("/"))
            if is_dir:
                os.makedirs(dst, exist_ok=True)
                continue
            os.makedirs(os.path.dirname(dst), exist_ok=True)
            if os.path.exists(dst) and os.path.getsize(dst) == length:
                continue
            f.seek(base + start * SECTOR)
            with open(dst, "wb") as o:
                left = length
                while left:
                    chunk = f.read(min(left, 1 << 22))
                    o.write(chunk)
                    left -= len(chunk)
            print(path, length)


if __name__ == "__main__":
    main()
