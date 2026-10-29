#!/usr/bin/env python3
"""Tests for tools/funds_fit.py on synthetic club records (no game data):

  python tools/test_funds_fit.py
"""
import csv, math, os, struct, sys, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import funds_fit as ff  # noqa: E402

RECS, UMAP, REG = 0x1000_0000, 0x1080_0000, 0x1090_0000


def memory(clubs, uids=()):
    """r32/r16 over club records: clubs = {id: (region, division)}, regions r in group r; uids = (uid, id)."""
    w = {0x8273d6c4: RECS, 0x8273d6c8: UMAP, 0x82741278: REG}
    h = {UMAP + uid * 2: cid for uid, cid in uids}
    for cid, (region, div) in clubs.items():
        w[RECS + cid * 0x3d8 + 4] = region << 21 | div << 2
        w[REG + region * 32 + 0xc] = (region << 15 | region >> 17) & 0xffffffff  # rotated left 17: the group
    return (lambda a: w.get(a, 0)), (lambda a: h.get(a, -1))


def targets(rows):
    return [{'id': str(c), 'funds_m': str(m), 'source': s} for c, m, s in rows]


class ClubRows(unittest.TestCase):
    def test_group_division_weight_and_fixed_multipliers(self):
        r32, r16 = memory({1: (5, 3), 2: (14, 11), 3: (14, 2), 4: (9, 1), 5: (63, 1)}, uids=[(0x1716, 4)])
        got = ff.club_rows(r32, r16, targets([(1, 16, 'budget'), (2, 4, 'value'), (3, 9, 'budget'), (4, 100, 'value'), (5, 1, 'value')]))
        self.assertEqual(got, [(1, 16.0, 12.0, 5, None),   # weight 3 x sqrt(target) for a budget
                               (2, 4.0, 1.0, 14, 1.0),     # England, division 11: x100 fixed
                               (3, 9.0, 9.0, 14, None),
                               (4, 100.0, 1.0, 9, 2.0),    # Chelsea by uid: x200
                               (5, 1.0, 1.0, None, None)])  # group 63: none of the 51


class Fit(unittest.TestCase):
    def game(self, c, T, mult, wealth):
        return T[wealth[c[0]]] * ff.eff(c, mult)

    def test_one_club_per_tier_fits_exactly(self):
        want = [0.6 * (400 / 0.6) ** (k / 15) for k in range(16)]
        clubs = [(k + 1, t, 3 * math.sqrt(t), None, 1.0) for k, t in enumerate(want)]
        T, mult, wealth = ff.fit(clubs)
        self.assertEqual(wealth, {k + 1: k for k in range(16)})
        for c in clubs:
            self.assertAlmostEqual(self.game(c, T, mult, wealth) / c[1], 1.0, places=9)

    def test_a_richer_group_is_fitted_through_its_multiplier(self):
        # group 7's clubs at twice the x1 clubs' targets: tiers alone can't hold both, the byte can
        # (x2 on the same tier or x1.3 one tier up; the fit is not unique, the error is what counts)
        base = [0.6 * (400 / 0.6) ** (k / 15) for k in range(16)]
        clubs = [(k + 1, t, 3 * math.sqrt(t), None, 1.0) for k, t in enumerate(base)]
        clubs += [(100 + k, 2 * t, 3 * math.sqrt(2 * t), 7, None) for k, t in enumerate(base[:10])]
        T, mult, wealth = ff.fit(clubs)
        self.assertNotEqual(mult[7], 1.0)
        self.assertTrue(0.05 <= mult[7] <= 2.55 and round(100 * mult[7]) == 100 * mult[7])
        for c in clubs:
            self.assertLess(abs(self.game(c, T, mult, wealth) / c[1] - 1), 0.03, c)
        self.assertEqual([m for g, m in mult.items() if g != 7], [1.0] * (ff.NGROUPS - 1))

    def test_eff_prefers_the_fixed_multiplier(self):
        self.assertEqual(ff.eff((1, 1.0, 1.0, 3, 0.8), {3: 2.0}), 0.8)
        self.assertEqual(ff.eff((1, 1.0, 1.0, 3, None), {3: 2.0}), 2.0)
        self.assertEqual(ff.eff((1, 1.0, 1.0, None, None), {3: 2.0}), 1.0)


class Outputs(unittest.TestCase):
    def test_wealth_column_rewritten_for_listed_clubs_only(self):
        raw = 'id,name,wealth\r\n1,"A, B",3\r\n2,C,5\r\n'
        out = ff.rewrite_wealth(raw, {1: 12})
        self.assertEqual(out, 'id,name,wealth\n1,"A, B",12\n2,C,5\n')
        self.assertEqual(list(csv.reader(out.splitlines()))[1][1], 'A, B')

    def test_patches_every_tier_and_only_changed_group_words(self):
        img = bytearray(0x71_0000)
        for k in range(16):
            a = ff.TIER_WORDS - 0x8200_0000 + 4 * k
            img[a:a + 4] = struct.pack('>I', 1000 + k)
        img[ff.GROUP_MULTS - 0x8200_0000:ff.GROUP_MULTS - 0x8200_0000 + 52] = bytes([100] * 52)
        T = [0.5 * (k + 1) for k in range(16)]
        mult = {g: 1.0 for g in range(ff.NGROUPS)}
        mult[5] = 2.5
        lines = ff.patch_lines(T, mult, bytes(img))
        self.assertEqual(len(lines), 1 + 16 + 1)
        self.assertEqual(lines[1], f'[[patch]]\naddr = "{ff.TIER_WORDS:#x}"\nfrom = 1000\nto = 500\n')
        self.assertEqual(lines[-1], f'[[patch]]\naddr = "{ff.GROUP_MULTS + 4:#x}"\nfrom = {0x64646464}\nto = {0x64fa6464}\n')

    def test_bytes_past_the_51_groups_are_kept(self):
        img = bytearray(0x71_0000)
        img[ff.GROUP_MULTS - 0x8200_0000:ff.GROUP_MULTS - 0x8200_0000 + 52] = bytes([100] * 51 + [7])
        mult = {g: 1.0 for g in range(ff.NGROUPS)}
        self.assertEqual(len(ff.patch_lines([1.0] * 16, mult, bytes(img))), 17)


if __name__ == '__main__':
    unittest.main()
