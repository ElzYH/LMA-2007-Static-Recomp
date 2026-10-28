"""Tiny PowerPC disassembler for reading hot loops in the decrypted LMA image.

usage: python ppcdis.py <default.pe> <start_hex> <end_hex>
Covers the common integer/branch/load-store forms; anything else prints as raw.
"""
import struct
import sys

BASE = 0x82000000


def sx16(v):
    return v - 0x10000 if v & 0x8000 else v


def dis(w, a):
    op, rd, ra, rb = w >> 26, (w >> 21) & 31, (w >> 16) & 31, (w >> 11) & 31
    si, ui = sx16(w & 0xFFFF), w & 0xFFFF
    loads = {32: "lwz", 33: "lwzu", 34: "lbz", 40: "lhz", 42: "lha", 48: "lfs", 50: "lfd"}
    stores = {36: "stw", 37: "stwu", 38: "stb", 44: "sth", 52: "stfs", 54: "stfd"}
    if op in loads:
        return f"{loads[op]} r{rd},{si:#x}(r{ra})"
    if op in stores:
        return f"{stores[op]} r{rd},{si:#x}(r{ra})"
    if op == 58:
        return f"{['ld','ldu','lwa','?'][w & 3]} r{rd},{sx16(w & 0xFFFC):#x}(r{ra})"
    if op == 62:
        return f"{['std','stdu','?','?'][w & 3]} r{rd},{sx16(w & 0xFFFC):#x}(r{ra})"
    if op == 14:
        return f"li r{rd},{si}" if ra == 0 else f"addi r{rd},r{ra},{si}"
    if op == 15:
        return f"lis r{rd},{ui:#x}" if ra == 0 else f"addis r{rd},r{ra},{ui:#x}"
    if op == 24:
        return f"ori r{ra},r{rd},{ui:#x}"
    if op == 28:
        return f"andi. r{ra},r{rd},{ui:#x}"
    if op == 10:
        return f"cmpl{'d' if (w >> 21) & 1 else 'w'}i cr{rd >> 2},r{ra},{ui:#x}"
    if op == 11:
        return f"cmp{'d' if (w >> 21) & 1 else 'w'}i cr{rd >> 2},r{ra},{si}"
    if op == 16:
        return f"bc{'l' if w & 1 else ''} {rd},{ra},{a + sx16(w & 0xFFFC):#x}"
    if op == 18:
        li = w & 0x3FFFFFC
        li = li - 0x4000000 if li & 0x2000000 else li
        return f"b{'l' if w & 1 else ''} {a + li:#x}"
    if op == 19:
        xo = (w >> 1) & 0x3FF
        return {16: "blr" if rd == 20 else f"bclr {rd},{ra}", 528: f"bcctr{'l' if w & 1 else ''} {rd},{ra}"}.get(xo, f"cr-op {xo}")
    if op == 21:
        return f"rlwinm r{ra},r{rd},{rb},{(w >> 6) & 31},{(w >> 1) & 31}"
    if op == 31:
        xo = (w >> 1) & 0x3FF
        if xo in (339, 467):
            spr = ((w >> 11) & 31) << 5 | ((w >> 16) & 31)
            n = {8: "lr", 9: "ctr", 1: "xer"}.get(spr, str(spr))
            return f"{'mf' if xo == 339 else 'mt'}{n} r{rd}"
        names = {444: "or", 28: "and", 0: "cmpw", 32: "cmplw", 266: "add", 40: "subf", 23: "lwzx", 151: "stwx",
                 235: "mullw", 8: "subfc", 136: "subfe", 316: "xor", 24: "slw", 536: "srw", 824: "srawi", 87: "lbzx"}
        return f"{names.get(xo, 'x31/' + str(xo))} r{rd},r{ra},r{rb}"
    return f".long {w:#010x}"


if __name__ == "__main__":
    d = open(sys.argv[1], "rb").read()
    lo, hi = int(sys.argv[2], 16), int(sys.argv[3], 16)
    for a in range(lo, hi, 4):
        w = struct.unpack_from(">I", d, a - BASE)[0]
        print(f"{a:#x}  {w:08x}  {dis(w, a)}")
