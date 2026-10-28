#!/usr/bin/env python3
"""Club badges for a mod space from an FM logo pack (#31 item 9).
  tools/badge_pack.py <pack.zip> <clubs_fm.csv> <mods/NAME/badges> [--folder Clubs/Normal/Normal] [--template Badge.mng]

  tools/badge_pack.py --refit <mods/NAME/badges> [--template Badge.mng]   (re-place existing badges)
`clubs_fm.csv` is what `lma-cli squad-import` writes (id, name_E, fm_uid). For every club whose FM uid
has `<folder>/<uid>.png` in the pack (FMG-style packs name logos by FM uid), the logo is fitted into the
box the retail crests use (BOX, in the 128 x 256 texture) and written as `<badges>/<ClubName>.tm2` with
`badge_tm2.encode`, the name as the mod loader expects it (letters and digits of the English name).
Scaling uses Pillow when installed, else macOS `sips`. Clubs without a logo are listed at the end
(FMG 2025 lacks Everton, Walsall and eight more English clubs); the mod loader gives them the
country crest. Rerun after a pack or roster change; existing .tm2 files are overwritten.
"""
import csv, os, re, shutil, subprocess, sys, tempfile, zipfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import badge_tm2


# Where the retail crests sit in the 128 x 256 badge texture (Badge.mng's 315 entries, median
# opaque box): x 20-109, y 42-154. A logo filling the top 128 x 128 instead floated above the
# original badges in game (the maintainer, 22 Oct), so logos are fitted into this box.
BOX = (20, 42, 109, 154)


def place(im):
    """An RGBA logo fitted into BOX (aspect kept, centred) on a transparent 128 x 256 canvas."""
    from PIL import Image  # type: ignore
    x0, y0, x1, y1 = BOX
    bw, bh = x1 - x0, y1 - y0
    im = im.copy()
    bb = im.split()[3].getbbox()
    if bb:
        im = im.crop(bb)  # the logo itself, not the pack's own margins
    s = min(bw / im.width, bh / im.height)
    im = im.resize((max(1, round(im.width * s)), max(1, round(im.height * s))), Image.LANCZOS)
    canvas = Image.new('RGBA', (badge_tm2.W, badge_tm2.H), (0, 0, 0, 0))
    canvas.paste(im, (x0 + (bw - im.width) // 2, y0 + (bh - im.height) // 2))
    return canvas


def scale(src, dst, size=128):
    """src PNG -> dst PNG: the whole 128 x 256 texture with the logo in BOX (Pillow), else the old
    size x size image centred (sips only)."""
    try:
        from PIL import Image  # type: ignore
        place(Image.open(src).convert('RGBA')).save(dst)
        return
    except ImportError:
        pass
    r = subprocess.run(['sips', '-Z', str(size), '-s', 'format', 'png', src, '--out', dst], capture_output=True)
    if r.returncode != 0:
        raise RuntimeError('no Pillow and sips failed: ' + r.stderr.decode(errors='replace').strip())
    w, h, px = badge_tm2.read_png(dst)
    if (w, h) != (size, size):
        canvas = [[(0, 0, 0, 0)] * size for _ in range(size)]
        ox, oy = (size - w) // 2, (size - h) // 2
        for y in range(h):
            for x in range(w):
                canvas[oy + y][ox + x] = px[y][x]
        badge_tm2.write_png(dst, size, size, canvas)


def refit(folder, template):
    """--refit <badges dir>: re-place every existing .tm2 badge into BOX (no logo pack needed)."""
    from PIL import Image  # type: ignore
    n = 0
    for f in sorted(os.listdir(folder)):
        if not f.endswith('.tm2'):
            continue
        p = os.path.join(folder, f)
        d = open(p, 'rb').read()
        if len(d) != badge_tm2.SIZE:
            continue
        clut = d[-1024:]
        pal = [clut[badge_tm2.swizzle(i) * 4:badge_tm2.swizzle(i) * 4 + 4] for i in range(256)]
        im = Image.new('RGBA', (badge_tm2.W, badge_tm2.H))
        im.putdata([(c[0], c[1], c[2], min(255, c[3] * 2)) for c in (pal[d[0x100 + i]] for i in range(badge_tm2.W * badge_tm2.H))])
        with tempfile.TemporaryDirectory(prefix='badge_refit_') as tmp:
            png = os.path.join(tmp, 'b.png')
            place(im).save(png)
            badge_tm2.encode(png, p, template)
        n += 1
    print(f'{n} badges re-placed in {folder}')


def main(a):
    if a[:1] == ['--refit'] and len(a) >= 2:
        template = a[a.index('--template') + 1] if '--template' in a else badge_tm2.RETAIL
        return refit(a[1], template)
    if len(a) < 3:
        sys.exit(__doc__)
    pack, clubs_csv, out = a[0], a[1], a[2]
    folder = a[a.index('--folder') + 1] if '--folder' in a else 'Clubs/Normal/Normal'
    template = a[a.index('--template') + 1] if '--template' in a else badge_tm2.RETAIL
    os.makedirs(out, exist_ok=True)
    rows = list(csv.DictReader(open(clubs_csv, encoding='utf-8-sig')))
    z = zipfile.ZipFile(pack)
    by_uid = {}
    for n in z.namelist():
        m = re.search(r'(?:^|/)' + re.escape(folder) + r'/(\d+)\.png$', n)
        if m:
            by_uid[m.group(1)] = n
    done, missing = 0, []
    with tempfile.TemporaryDirectory(prefix='badge_pack_') as tmp:
        for r in rows:
            name, uid = r['name_E'], r.get('fm_uid', '')
            if not uid or uid not in by_uid:
                missing.append(name)
                continue
            key = re.sub(r'[^A-Za-z0-9]', '', name)
            src = os.path.join(tmp, uid + '.png')
            with z.open(by_uid[uid]) as f, open(src, 'wb') as g:
                shutil.copyfileobj(f, g)
            png = os.path.join(tmp, key + '.png')
            scale(src, png)
            badge_tm2.encode(png, os.path.join(out, key + '.tm2'), template)
            done += 1
    print(f'{done} badges written to {out}; {len(missing)} clubs without a logo in the pack')
    if missing:
        print('  ' + ', '.join(missing[:40]) + (' ...' if len(missing) > 40 else ''))


if __name__ == '__main__':
    main(sys.argv[1:])
