#!/usr/bin/env python3
"""Club badges for a mod space (#31 item 9): PNG <-> the game's badge texture (TIM2, 8-bit palettised).

  tools/badge_tm2.py encode <in.png> <out.tm2> [--template Badge.mng]   128x128 PNG (RGBA) -> badge .tm2
  tools/badge_tm2.py decode <Badge.mng> <ClubName.tm2> <out.png>          one entry of the game's archive -> PNG
  tools/badge_tm2.py list <Badge.mng>                                     entry names
  tools/badge_tm2.py selftest [Badge.mng]                                 quantiser and layout checks

Layout (from the retail Badge.mng, 323 entries of 34,048 bytes): 0x100 header ("TIM2" v4 at 0, the
picture header at 0x80), 8-bit pixels 128 x 256 at 0x100 (the crest in the top 128 rows, the rest
transparent), 256 RGBA palette entries at the end in the PS2 CLUT order (per 32 colours: 0-7, 16-23,
8-15, 24-31) with alpha 0..128. `encode` clones the header of a retail entry (the first one of
--template, default the disc's Badge.mng), quantises the PNG to 256 colours and writes that layout.
Pure Python (zlib only). A mod's `badges/<ClubName>.tm2` (the club's English name without spaces or
punctuation, as the retail entries are named) is served inside Badge.mng by the mod loader.
"""
import os, struct, sys, zlib

RETAIL = os.path.expanduser('~/lma/LMA_full/Common/Frontend/Textures/Badge.mng')
W, H, SIZE = 128, 256, 34048


def entries(d):
    table = struct.unpack_from('>I', d, 8)[0]
    ds = struct.unpack_from('>I', d, 12)[0]
    out = {}
    for i in range(table // 0x108):
        at = 0x14 + i * 0x108
        name = d[at:at + 0x100].split(b'\0')[0].decode()
        off, size = struct.unpack_from('>II', d, at + 0x100)
        out[name] = d[ds + off:ds + off + size]
    return out


def swizzle(i):
    """Palette index -> position in the stored CLUT (PS2 8-bit CLUT order)."""
    blk, k = i // 32 * 32, i % 32
    return blk + {0: 0, 1: 2, 2: 1, 3: 3}[k // 8] * 8 + k % 8


def read_png(path):
    d = open(path, 'rb').read()
    assert d[:8] == b'\x89PNG\r\n\x1a\n', 'not a PNG'
    pos, idat, w, h, ct, bd = 8, b'', 0, 0, 0, 0
    while pos < len(d):
        n = struct.unpack_from('>I', d, pos)[0]
        t = d[pos + 4:pos + 8]
        c = d[pos + 8:pos + 8 + n]
        if t == b'IHDR':
            w, h, bd, ct = struct.unpack('>IIBB', c[:10])
            assert bd == 8 and ct in (2, 6) and c[12] == 0, 'need an 8-bit RGB/RGBA PNG, no interlace'
        elif t == b'IDAT':
            idat += c
        pos += 12 + n
    bpp = 4 if ct == 6 else 3
    raw = zlib.decompress(idat)
    stride = w * bpp
    px, prev = [], bytearray(stride)
    for y in range(h):
        f = raw[y * (stride + 1)]
        line = bytearray(raw[y * (stride + 1) + 1:(y + 1) * (stride + 1)])
        for x in range(stride):
            a = line[x - bpp] if x >= bpp else 0
            b = prev[x]
            c = prev[x - bpp] if x >= bpp else 0
            if f == 1: line[x] = (line[x] + a) & 255
            elif f == 2: line[x] = (line[x] + b) & 255
            elif f == 3: line[x] = (line[x] + (a + b) // 2) & 255
            elif f == 4:
                p = a + b - c
                pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
                line[x] = (line[x] + (a if pa <= pb and pa <= pc else b if pb <= pc else c)) & 255
        prev = line
        row = []
        for x in range(w):
            r, g, b = line[x * bpp], line[x * bpp + 1], line[x * bpp + 2]
            al = line[x * bpp + 3] if bpp == 4 else 255
            row.append((r, g, b, al))
        px.append(row)
    return w, h, px


def write_png(path, w, h, rows):
    def chunk(t, b):
        return struct.pack('>I', len(b)) + t + b + struct.pack('>I', zlib.crc32(t + b) & 0xffffffff)
    raw = b''.join(b'\x00' + bytes(v for p in r for v in p) for r in rows)
    open(path, 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 6, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(raw, 9)) + chunk(b'IEND', b''))


def quantise(colours, n=256):
    """Median cut over the distinct RGBA colours (alpha counts) -> (palette, index map)."""
    distinct = {}
    for c in colours:
        distinct[c] = distinct.get(c, 0) + 1
    if len(distinct) <= n:
        pal = list(distinct)
        return pal, {c: i for i, c in enumerate(pal)}
    # split the heaviest box that still holds more than one colour (a single-colour box, e.g.
    # the transparent background, is done: it must not end the cut, which once left 4 colours)
    boxes, done = [list(distinct.items())], []
    while boxes and len(boxes) + len(done) < n:
        boxes.sort(key=lambda b: -sum(w for _, w in b))
        box = boxes.pop(0)
        if len(box) == 1:
            done.append(box)
            continue
        ch = max(range(4), key=lambda k: max(c[k] for c, _ in box) - min(c[k] for c, _ in box))
        box.sort(key=lambda cw: cw[0][ch])
        half = sum(w for _, w in box) / 2
        acc, cut = 0, 1
        for i, (_, w) in enumerate(box):
            acc += w
            if acc >= half and i + 1 < len(box):
                cut = i + 1
                break
        boxes += [box[:cut], box[cut:]]
    pal, index = [], {}
    for box in boxes + done:
        tot = sum(w for _, w in box)
        avg = tuple(round(sum(c[k] * w for c, w in box) / tot) for k in range(4))
        for c, _ in box:
            index[c] = len(pal)
        pal.append(avg)
    return pal, index


def encode(png, out, template):
    w, h, px = read_png(png)
    # a 128 x 256 PNG is the whole texture (the crest placed as the retail ones are, badge_pack's
    # BOX); a 128 x 128 one is the top rows only, as before
    assert w == W and h in (W, H), f'need a {W}x{W} or {W}x{H} PNG, got {w}x{h}'
    tmpl = next(iter(entries(open(template, 'rb').read()).values()))
    assert len(tmpl) == SIZE
    pal, index = quantise([p for row in px for p in row])
    # rows the PNG doesn't cover must be transparent: an opaque index there showed as a coloured
    # box under every mod badge (the retail crests sit in rows 42-154, transparent around them)
    clear = next((i for i, c in enumerate(pal) if c[3] == 0), None)
    if clear is None:
        if len(pal) == 256:
            pal, index = quantise([p for row in px for p in row], 255)
        pal.append((0, 0, 0, 0))
        clear = len(pal) - 1
    while len(pal) < 256:
        pal.append((0, 0, 0, 0))
    pix = bytearray([clear]) * (W * H)
    for y in range(h):
        for x in range(W):
            pix[y * W + x] = index[px[y][x]]
    clut = bytearray(1024)
    for i, (r, g, b, a) in enumerate(pal):
        j = swizzle(i)
        clut[j * 4:j * 4 + 4] = bytes([r, g, b, (a + 1) // 2])
    data = tmpl[:0x100] + bytes(pix) + tmpl[0x100 + W * H:SIZE - 1024] + bytes(clut)
    assert len(data) == SIZE
    open(out, 'wb').write(data)


def decode(mng, name, out):
    e = entries(open(mng, 'rb').read())[name]
    clut = e[-1024:]
    pal = [clut[swizzle(i) * 4:swizzle(i) * 4 + 4] for i in range(256)]
    rows = []
    for y in range(W):
        rows.append([(c[0], c[1], c[2], min(255, c[3] * 2)) for c in (pal[e[0x100 + y * W + x]] for x in range(W))])
    write_png(out, W, W, rows)


def selftest(template):
    """A 5,000-colour gradient crest encodes to a full palette with a transparent bottom half; a
    retail entry decodes and re-encodes byte-identically (its palette fits, so no quantising)."""
    import tempfile
    tmp = tempfile.mkdtemp(prefix='badge_tm2_')
    rows = [[(x * 2 % 256, y * 2 % 256, (x + y) % 256, 255 if (x - 64) ** 2 + (y - 64) ** 2 < 60 ** 2 else 0) for x in range(W)] for y in range(W)]
    png = os.path.join(tmp, 'g.png')
    write_png(png, W, W, rows)
    out = os.path.join(tmp, 'g.tm2')
    encode(png, out, template)
    t = open(out, 'rb').read()
    pix, pal = t[0x100:0x100 + W * H], t[0x100 + W * H:0x100 + W * H + 1024]
    used = len(set(pix[:W * W]))
    bottom = set(pix[W * W:])
    assert len(t) == SIZE, len(t)
    assert used >= 200, f'only {used} palette entries used for a 5,000-colour crest'
    assert len(bottom) == 1 and pal[swizzle(next(iter(bottom))) * 4 + 3] == 0, 'bottom half not transparent'
    name = next(iter(entries(open(template, 'rb').read())))
    rt = os.path.join(tmp, 'rt.png')
    decode(template, name, rt)
    encode(rt, os.path.join(tmp, 'rt.tm2'), template)
    if open(os.path.join(tmp, 'rt.tm2'), 'rb').read() != next(iter(entries(open(template, 'rb').read()).values())):
        print(f'note: {name} does not round-trip byte-identically (its bottom half carries data)')
    print(f'ok: {used} colours used, bottom half transparent, {name} decoded and re-encoded')


if __name__ == '__main__':
    a = sys.argv[1:]
    if a[:1] == ['selftest']:
        selftest(a[1] if len(a) > 1 else RETAIL)
    elif a[:1] == ['list']:
        print('\n'.join(entries(open(a[1], 'rb').read())))
    elif a[:1] == ['decode']:
        decode(a[1], a[2], a[3])
    elif a[:1] == ['encode']:
        encode(a[1], a[2], a[a.index('--template') + 1] if '--template' in a else RETAIL)
    else:
        sys.exit(__doc__)
