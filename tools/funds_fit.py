#!/usr/bin/env python3
"""Fit club funds: the 16 tier words and the game's ~51 group multipliers together, so every club
starts with funds as close as possible to its target (FC 27 career budget / 0.65, else the squad
value estimate), lower leagues and clubs worldwide included (the maintainer, 11 Oct).

  python tools/funds_fit.py <mod dir> <state mem dump> <image.bin> <out patches.toml>

Inputs (nothing from the game goes into the repo):
  <mod dir>/funds_targets.csv  written by `lma-cli squad-import` (id,funds_m,source)
  <mod dir>/database/clubs.csv  its `wealth` column is rewritten with the fitted tiers
  <state mem dump>  `lma-cli state-mem` of any career state of that mod: club records
                    ([0x8273d6c4], 0x3d8 bytes) give each club's group (word +4 >> 21 through the
                    region table [0x82741278]) and division ((word +4 >> 2) & 0x3f)
  <image.bin>       `lma-cli xex-info --dump-image`: the original words the patches check

The game's funds (0x820eaa10): tier word x 10 x the group's byte multiplier (0x820033ac), capped at
999,999,999; English clubs (group 14) in divisions 11 and 12 get x100 fixed, Chelsea x200, Celtic and
Rangers x80 (by uid). The output sets every tier word and group multiplier; the mod.toml's own funds
[[patch]] entries must be replaced by these.
"""
import csv, io, math, struct, sys

TIER_WORDS, GROUP_MULTS, NGROUPS = 0x82703118, 0x820033ac, 51
CHECK = {'Arsenal', 'Liverpool', 'Manchester United', 'Tottenham Hotspur', 'Manchester City', 'Chelsea', 'Brighton', 'West Ham United',
         'Oxford United', 'Paris St Germain', 'Dortmund', 'Bayern', 'Barcelona', 'Juventus', 'Celtic', 'Leeds United', 'Wrexham', 'Ajax'}
SPECIAL = {0x1716: 200, 0x170e: 80, 0x186c: 80}  # Chelsea, Celtic, Rangers (by club uid)


def club_rows(r32, r16, targets):
    """(id, target_m, weight, group or None, fixed multiplier / 100 or None) per funds_targets.csv row,
    from guest memory read with r32/r16 (big-endian words and signed halfwords)."""
    recs, umap, regions = r32(0x8273d6c4), r32(0x8273d6c8), r32(0x82741278)
    idx_of_uid = {}
    for uid in range(0x2056):
        i = r16(umap + uid * 2)
        if i >= 0:
            idx_of_uid[i] = uid
    clubs = []
    for r in targets:
        cid = int(r['id'])
        w1 = r32(recs + cid * 0x3d8 + 4)
        region = w1 >> 21
        rw = r32(regions + region * 32 + 0xc)
        group = ((rw << 17 | rw >> 15) & 0xffffffff) & 0x3f
        division = (w1 >> 2) & 0x3f
        fixed = None
        uid = idx_of_uid.get(cid)
        if uid in SPECIAL:
            fixed = SPECIAL[uid] / 100
        elif group == 14 and division in (11, 12):
            fixed = 1.0
        clubs.append((cid, float(r['funds_m']), 3.0 * math.sqrt(float(r['funds_m'])) if r['source'] == 'budget' else 1.0, group if group < NGROUPS else None, fixed))
    return clubs


def eff(c, mult):
    """A club's multiplier: its fixed one, else its group's."""
    return c[4] if c[4] is not None else mult.get(c[3], 1.0)


def nearest(x, T):
    return min(range(16), key=lambda k: abs(math.log(T[k]) - x))


def fit(clubs, rounds=40):
    """The 16 tiers (GBP m), the group multipliers and each club's tier: weighted k-means on the tiers
    alternating with the best byte multiplier per group."""
    mult = {g: 1.0 for g in range(NGROUPS)}
    T = sorted(math.exp(math.log(0.6) + k / 15 * math.log(400 / 0.6)) for k in range(16))
    for _ in range(rounds):
        # tiers: weighted k-means of log(target / multiplier)
        g = [[] for _ in T]
        for c in clubs:
            x = math.log(c[1] / eff(c, mult))
            g[nearest(x, T)].append((x, c[2]))
        T = sorted(math.exp(sum(x * w for x, w in gg) / sum(w for _, w in gg)) if gg else T[k] for k, gg in enumerate(g))
        # group multipliers: the byte (0.05 .. 2.55) that fits the group's free clubs best
        for gi in range(NGROUPS):
            free = [c for c in clubs if c[3] == gi and c[4] is None]
            if not free:
                continue
            best = min((sum(c[2] * min(abs(math.log(t * b / 100) - math.log(c[1])) for t in T) for c in free), b) for b in range(5, 256))
            mult[gi] = best[1] / 100
    wealth = {c[0]: nearest(math.log(c[1] / eff(c, mult)), T) for c in clubs}
    return T, mult, wealth


def rewrite_wealth(raw, wealth):
    """clubs.csv's text with the `wealth` column set to each listed club's tier."""
    rows = list(csv.reader(raw.splitlines()))
    head = rows[0]; wi, ii = head.index('wealth'), head.index('id')
    for r in rows[1:]:
        cid = int(r[ii])
        if cid in wealth:
            r[wi] = str(wealth[cid])
    out = io.StringIO()
    csv.writer(out, lineterminator='\n').writerows(rows)
    return out.getvalue()


def patch_lines(T, mult, img):
    """The patches.toml lines: every tier word, and each group-multiplier word that changes."""
    word = lambda a: struct.unpack('>I', img[a - 0x82000000:a - 0x82000000 + 4])[0]
    lines = ['# club funds fitted by tools/funds_fit.py: 16 tier words and the group multipliers\n']
    for k, t in enumerate(T):
        a = TIER_WORDS + 4 * k
        lines.append(f'[[patch]]\naddr = "{a:#x}"\nfrom = {word(a)}\nto = {round(t * 1000)}\n')
    for a in range(GROUP_MULTS, GROUP_MULTS + 52, 4):
        old = bytearray(img[a - 0x82000000:a - 0x82000000 + 4]); new = bytearray(old)
        for i in range(4):
            gi = a + i - GROUP_MULTS
            if gi < NGROUPS:
                new[i] = round(100 * mult[gi])
        if new != old:
            lines.append(f'[[patch]]\naddr = "{a:#x}"\nfrom = {struct.unpack(">I", old)[0]}\nto = {struct.unpack(">I", new)[0]}\n')
    return lines


def main(mod, dump, image, out):
    m = open(dump, 'rb').read()
    r32 = lambda a: struct.unpack('>I', m[a:a + 4])[0]
    r16 = lambda a: struct.unpack('>h', m[a:a + 2])[0]
    clubs = club_rows(r32, r16, csv.DictReader(open(f'{mod}/funds_targets.csv', encoding='utf-8')))
    img = open(image, 'rb').read()
    T, mult, wealth = fit(clubs)

    errs = sorted(abs(T[wealth[c[0]]] * eff(c, mult) / c[1] - 1) for c in clubs if c[2] > 1)
    names = {int(r['id']): r['name_E'] for r in csv.DictReader(open(f'{mod}/clubs_fm.csv', encoding='utf-8-sig'))}
    for c in clubs:
        if names.get(c[0]) in CHECK:
            k = wealth[c[0]]
            print(f'  {names[c[0]]:24} target transfer {c[1] * 0.65:7.1f}m  game {T[k] * eff(c, mult) * 0.65:7.1f}m')
    print(f'{len(clubs)} clubs; budget clubs ({len(errs)}): mean error {100 * sum(errs) / len(errs):.1f}%, '
          f'median {100 * errs[len(errs) // 2]:.1f}%, 90th percentile {100 * errs[9 * len(errs) // 10]:.1f}%')
    print('tiers (bank GBP m):', [round(t, 2) for t in T])
    print('group multipliers:', {g: round(100 * v) for g, v in mult.items() if round(100 * v) != 100})

    # the mod's clubs.csv: the fitted tier per club
    p = f'{mod}/database/clubs.csv'
    raw = open(p, encoding='utf-8-sig', newline='').read()
    with open(p, 'w', encoding='utf-8-sig', newline='') as f:
        f.write(rewrite_wealth(raw, wealth))

    lines = patch_lines(T, mult, img)
    open(out, 'w', newline='\n').write('\n'.join(lines))
    print(len(lines) - 1, 'patches ->', out)


if __name__ == '__main__':
    main(*sys.argv[1:5])
