"""Build a stand-in sound bank .mng for one that is missing from an incomplete rip.

  python make_stub_bank.py <donor.mng> <NEWNAME> <out.mng>

Clones the donor's XACT wave bank + sound bank (BG2 archive holding NAME.xwb + NAME.xsb),
renames the banks internally and fixes the sound bank's FCS-16 checksum. The game then finds
a valid bank instead of showing the dirty-disc error; the sounds are the donor's, not the real ones.
"""
import struct
import sys

from lma_db import Archive


def fcs16(data):
    crc = 0xFFFF
    for b in data:
        crc ^= b
        for _ in range(8):
            crc = (crc >> 1) ^ 0x8408 if crc & 1 else crc >> 1
    return ~crc & 0xFFFF


def rename(blob, old, new):
    """Replace NUL-padded bank-name fields (both names fit the same 64-byte field)."""
    o, n = old.encode() + b"\0", new.encode() + b"\0"
    assert len(n) <= 64
    out = bytearray(blob)
    i = out.find(o)
    while i >= 0:
        out[i:i + len(o)] = n.ljust(len(o), b"\0") if len(n) <= len(o) else n
        i = out.find(o, i + 1)
    return out


def main(donor, new, out):
    a = Archive(donor)
    old = a.names[0].rsplit(".", 1)[0]
    files = {}
    for name in a.names:
        ext = name.rsplit(".", 1)[1]
        data = rename(a.files[name], old, new)
        if ext == "xsb":
            struct.pack_into("<H", data, 6, fcs16(data[8:]))
        files[f"{new}.{ext}"] = bytes(data)
    a.names = list(files)
    a.files = files
    open(out, "wb").write(a.build())
    print(f"{out}: {[(n, len(d)) for n, d in files.items()]}")


if __name__ == "__main__":
    main(*sys.argv[1:4])
