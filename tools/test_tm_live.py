#!/usr/bin/env python3
"""Tests for tools/tm_live.py's page parsing, on made-up cached pages (no network, no real data):

  python tools/test_tm_live.py
"""
import os, sys, tempfile, unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import tm_live  # noqa: E402

PAD = "<!-- " + "x" * 1200 + " -->"  # fetch() only trusts cached pages over 1000 bytes

COMP = PAD + """
<a title="Alpha FC" href="/alpha-fc/startseite/verein/11/saison_id/2026">Alpha</a>
<a href="/beta-united/startseite/verein/22/saison_id/2025" class="x" title="Beta &amp; Co">Beta</a>
<a title="Alpha FC" href="/alpha-fc/startseite/verein/11/saison_id/2026">again</a>
<a href="/gamma/startseite/verein/33/saison_id/2026">no title: not a table link</a>
"""

SQUAD = PAD + """<table><tbody>
<tr class="odd"><td><div class=rn_nummer>7</div></td>
<td><a href="/a-player/profil/spieler/101"> Anna Player </a></td>
<td>31/12/2001 (24)</td>
<td><a title="On loan from Beta &amp; Co until 30/06/2027" href="/beta"></a></td>
<td><a href="/a-player/marktwertverlauf/spieler/101">&euro;12.50m</a></td></tr>
<tr class="even"><td><div class=rn_nummer>-</div></td>
<td><a href="/b-player/profil/spieler/102">Bo Player</a></td>
<td>01/02/2005 (21)</td>
<td><a href="/b-player/marktwertverlauf/spieler/102">&euro;800k</a></td></tr>
</tbody></table>"""


class Pages(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.mkdtemp(prefix="tm_live_")

    def write(self, name, body):
        with open(os.path.join(self.dir, name), "w", encoding="utf-8") as f:
            f.write(body)

    def test_clubs_with_their_league_season(self):
        self.write("comp_XX1.html", COMP)
        self.assertEqual(tm_live.clubs_of("XX1", self.dir), [("11", "alpha-fc", "Alpha FC", 2026), ("22", "beta-united", "Beta & Co", 2025)])

    def test_squad_rows(self):
        self.write("club_11.html", SQUAD)
        self.assertEqual(
            tm_live.squad("11", "alpha-fc", self.dir),
            [("101", "Anna Player", "2001-12-31", "7", "Beta & Co", "12500000"), ("102", "Bo Player", "2005-02-01", "", "", "800000")],
        )

    def test_last_season_squads_have_their_own_cache_file(self):
        self.write("club_22_s2025.html", SQUAD)
        self.assertEqual(len(tm_live.squad("22", "beta-united", self.dir, 2025)), 2)


if __name__ == "__main__":
    unittest.main()
