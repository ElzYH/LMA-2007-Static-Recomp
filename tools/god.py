"""Extract a Games-on-Demand (GOD / SVOD) package made by "Copy DVD" into plain files.

  python god.py <path to GOD header file> extract <outdir>
  python god.py <path to GOD header file> list

A GOD package is <TitleID>/00007000/<header> plus <header>.data/Data0000, Data0001, ...
Each DataNNNN part holds 0xA1C4 data blocks of 0x1000 bytes, laid out as one master hash
block, then 203 groups of (1 hash block + 204 data blocks). Concatenating the data blocks
gives the game's XDVDFS partition, which xiso.py then reads.
"""
import io
import os
import sys

import xiso

BLOCK = 0x1000
PER_GROUP = 0xCC
GROUPS = 203
PER_PART = PER_GROUP * GROUPS  # 0xA1C4


class GodStream(io.RawIOBase):
    """Read-only, seekable view of the data blocks across all DataNNNN parts."""

    def __init__(self, data_dir):
        names = sorted(n for n in os.listdir(data_dir) if n.lower().startswith("data"))
        self.parts = [open(os.path.join(data_dir, n), "rb") for n in names]
        self.size = len(self.parts) * PER_PART * BLOCK
        self.pos = 0

    def seekable(self):
        return True

    def readable(self):
        return True

    def seek(self, off, whence=0):
        self.pos = off if whence == 0 else self.pos + off if whence == 1 else self.size + off
        return self.pos

    def tell(self):
        return self.pos

    def _phys(self, block):
        part, i = divmod(block, PER_PART)
        g, j = divmod(i, PER_GROUP)
        return part, BLOCK + g * (BLOCK + PER_GROUP * BLOCK) + BLOCK + j * BLOCK

    def read(self, n=-1):
        if n < 0:
            n = self.size - self.pos
        out = bytearray()
        while n > 0 and self.pos < self.size:
            block, within = divmod(self.pos, BLOCK)
            part, phys = self._phys(block)
            if part >= len(self.parts):
                break
            take = min(n, BLOCK - within)
            f = self.parts[part]
            f.seek(phys + within)
            chunk = f.read(take)
            if not chunk:
                break
            out += chunk
            self.pos += len(chunk)
            n -= len(chunk)
        return bytes(out)


class SectorImage(io.RawIOBase):
    """Disc-sector addressed view of an SVOD package (Xenia's BlockToOffset mapping):
    sector S lives in DataNNNN at (S - 2*start_block [+2 for EGDF]) with hash tables skipped."""

    BLOCK, HASH, L0, L1, PER_FILE = 0x800, 0x1000, 0x198, 0xA1C4, 0x14388

    def __init__(self, data_dir, start_block, egdf):
        names = sorted(n for n in os.listdir(data_dir) if n.lower().startswith("data"))
        self.parts = [open(os.path.join(data_dir, n), "rb") for n in names]
        self.start, self.egdf, self.pos = start_block, egdf, 0

    def _map(self, sector):
        tb = sector - self.start * 2 + (2 if self.egdf else 0)
        fb, fi = tb % self.PER_FILE, tb // self.PER_FILE
        l0 = fb // self.L0 + 1
        off = l0 * self.HASH + (l0 // self.L1 + 1) * self.HASH
        return fi, fb * self.BLOCK + off

    def seek(self, off, whence=0):
        self.pos = off
        return off

    def read(self, n=-1):
        out = bytearray()
        while n > 0:
            sector, within = divmod(self.pos, self.BLOCK)
            fi, phys = self._map(sector)
            take = min(n, self.BLOCK - within)
            f = self.parts[fi]
            f.seek(phys + within)
            chunk = f.read(take)
            if not chunk:
                break
            out += chunk
            self.pos += len(chunk)
            n -= len(chunk)
        return bytes(out)


def svod_params(header_path):
    """(start_data_block, enhanced_gdf) from the LIVE/CON header's SVOD volume descriptor."""
    h = open(header_path, "rb").read(0x400)
    d = h[0x379:0x379 + 0x24]
    features = d[0x18]
    start = d[0x1C] | (d[0x1D] << 8) | (d[0x1E] << 16)
    return start, bool(features & 0x40)


def main():
    header, cmd = sys.argv[1], sys.argv[2]
    data_dir = header + ".data"
    if not os.path.isdir(data_dir):
        raise SystemExit(f"missing {data_dir}")
    start, egdf = svod_params(header)
    img = SectorImage(data_dir, start, egdf)
    # the volume descriptor is the first data block of the package
    raw = GodStream(data_dir)
    raw.seek(0)
    vd = raw.read(28)
    if vd[:20] != xiso.MAGIC:
        raise SystemExit("XDVDFS volume descriptor not found")
    import struct
    root, rsize = struct.unpack("<II", vd[20:28])
    ents = sorted(xiso.walk(img, 0, root, rsize))
    if cmd == "list":
        for path, start_s, length, is_dir in ents:
            print(f"{'D' if is_dir else 'F'} {length:12d} {path}")
        return
    out = sys.argv[3]
    for path, start_s, length, is_dir in ents:
        dst = os.path.join(out, *path.split("/"))
        if is_dir:
            os.makedirs(dst, exist_ok=True)
            continue
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if os.path.exists(dst) and os.path.getsize(dst) == length:
            continue
        img.seek(start_s * xiso.SECTOR)
        with open(dst, "wb") as o:
            left = length
            while left:
                chunk = img.read(min(left, 1 << 22))
                if not chunk:
                    raise SystemExit(f"short read in {path}")
                o.write(chunk)
                left -= len(chunk)
        print(path, length)


if __name__ == "__main__":
    main()
