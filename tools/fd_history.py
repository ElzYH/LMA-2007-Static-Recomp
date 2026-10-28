#!/usr/bin/env python3
"""League champions for the seasons no Transfermarkt dataset covers, for squad-import's
--history-extra (the maintainer, 22 Oct: previous winners missing): every season's final table computed from
football-data.co.uk's match results, the champion's row written as
`country,tier,season,club,won,drawn,lost,for,against,points` (season = its starting year).

    python tools/fd_history.py <cache dir> <out.csv> [first season] [last season] [--names <clubs.csv> ...]

--names: clubs.csv files (the base export's, then the mod's) whose `name_E` the importer matches:
football-data's short names ("Leicester", "Inter", "Raith Rvs") become the club's full name there
when one club of the same country fits (a name word in common, abbreviations expanded); the
importer matches champions by name, and the short ones went unmatched.

Divisions (football-data code -> LMA country, tier as squad-import's history uses them: England 0,
Scotland 1, Italy 2, Spain 3, Germany 4, France 5, Netherlands 6, Portugal 7): E0-E3 + EC (Premier
League .. National League), SC0-SC3, I1-I2, SP1-SP2, D1-D2, F1-F2, N1, P1. Not covered there (they
keep their empty rows): the Eerste Divisie, Liga Portugal 2, France's National, Germany's third
tier. The importer only fills rows no other source has, so overlapping seasons cost nothing.

Points: 3 for a win, 1 for a draw; a points deduction isn't in the results, so a deducted champion
reads a few points high (rare at the top). Ties: points, then head-to-head (points, then goal
difference among the tied clubs) in Spain, Italy and Portugal (H2H), then goal difference, goals. One request
per division and season, cached in <cache dir> (derived data: keep it out of the repo)."""
import csv
import io
import os
import sys
import time
import urllib.request

BASE = "https://www.football-data.co.uk/mmz4281"
DIVISIONS = {
    "E0": (0, 0), "E1": (0, 1), "E2": (0, 2), "E3": (0, 3), "EC": (0, 4),
    "SC0": (1, 0), "SC1": (1, 1), "SC2": (1, 2), "SC3": (1, 3),
    "I1": (2, 0), "I2": (2, 1),
    "SP1": (3, 0), "SP2": (3, 1),
    "D1": (4, 0), "D2": (4, 1),
    "F1": (5, 0), "F2": (5, 1),
    "N1": (6, 0),
    "P1": (7, 0),
}


def fetch(div, season, cache):
    code = f"{season % 100:02}{(season + 1) % 100:02}"
    path = os.path.join(cache, f"{div}_{code}.csv")
    if not (os.path.exists(path) and os.path.getsize(path) > 200):
        url = f"{BASE}/{code}/{div}.csv"
        try:
            body = urllib.request.urlopen(urllib.request.Request(url, headers={"User-Agent": "lma-rs history"}), timeout=30).read()
        except Exception as e:
            print(f"  ! {url}: {e}", flush=True)
            return None
        open(path, "wb").write(body)
        time.sleep(0.5)
    return open(path, "rb").read().decode("utf-8-sig", errors="replace")


# leagues that break a points tie on head-to-head first (La Liga 2006/07: Real Madrid over Barcelona)
H2H = {"SP1", "SP2", "I1", "I2", "P1"}


def table(text, h2h=False):
    clubs = {}
    games = []
    for r in csv.DictReader(io.StringIO(text)):
        h, a = (r.get("HomeTeam") or r.get("HT") or "").strip(), (r.get("AwayTeam") or r.get("AT") or "").strip()
        try:
            hg, ag = int(r.get("FTHG") or r.get("HG")), int(r.get("FTAG") or r.get("AG"))
        except (TypeError, ValueError):
            continue  # an unplayed or abandoned game
        if not h or not a:
            continue
        games.append((h, a, hg, ag))
        for club, f, ag_ in ((h, hg, ag), (a, ag, hg)):
            w = clubs.setdefault(club, [0, 0, 0, 0, 0])
            w[0 if f > ag_ else 1 if f == ag_ else 2] += 1
            w[3] += f
            w[4] += ag_
    rows = [(c, *v, 3 * v[0] + v[1]) for c, v in clubs.items()]
    def mini(group):
        # points and goal difference in the games among `group`
        m = {c: [0, 0] for c in group}
        for h, a, hg, ag in games:
            if h in m and a in m:
                m[h][0] += 3 if hg > ag else 1 if hg == ag else 0
                m[a][0] += 3 if ag > hg else 1 if hg == ag else 0
                m[h][1] += hg - ag
                m[a][1] += ag - hg
        return m
    by_points = {}
    for t in rows:
        by_points.setdefault(t[6], []).append(t[0])
    hh = {}
    if h2h:
        for pts, group in by_points.items():
            if len(group) > 1:
                hh.update(mini(group))
    rows.sort(key=lambda t: (-t[6], -hh.get(t[0], [0, 0])[0], -hh.get(t[0], [0, 0])[1], -(t[4] - t[5]), -t[4], t[0]))
    return rows


ABBREV = {"rvs": "rovers", "utd": "united", "weds": "wednesday", "c": "caledonian", "sth": "south", "dag": "dagenham", "red": "redbridge",
          "man": "manchester", "nott'm": "nottingham", "sp": "sporting", "ath": "atletico", "sociedad": "real sociedad", "vallecano": "rayo vallecano"}
# LMA's country codes in clubs.csv for each division's country (0 England .. 7 Portugal), base export numbering
CLUB_COUNTRY = {0: "0", 1: "1", 2: "2", 3: "3", 4: "4", 5: "5", 6: "6", 7: "7"}


def words(n):
    import re, unicodedata
    n = unicodedata.normalize("NFKD", n).encode("ascii", "ignore").decode().lower()
    out = []
    for w in re.split(r"[^a-z0-9']+", n):
        if w:
            out.extend(ABBREV.get(w, w).split())
    return [w for w in out if w not in ("fc", "afc", "cf", "ac", "sc", "as", "the", "club", "de", "calcio", "1")]


def name_map(paths):
    """club name_E -> (country, words) from clubs.csv files, later files overriding earlier ids."""
    by_id = {}
    for p in paths:
        for r in csv.DictReader(open(p, encoding="utf-8-sig")):
            prev = by_id.get(r["id"], {})
            name = (r.get("name_E") or "").strip() or prev.get("name", "")
            country = (r.get("country") or "").strip() or prev.get("country", "")
            by_id[r["id"]] = {"name": name, "country": country}
    return [(v["name"], v["country"], set(words(v["name"]))) for v in by_id.values() if v["name"]]


def full_name(short, clubs, country):
    w = words(short)
    if not w:
        return short
    exact = [n for n, c, ws in clubs if ws == set(w)]
    if len(exact) == 1:
        return exact[0]
    hits = [n for n, c, ws in clubs if set(w) <= ws and (c == country or not c)]
    if len(hits) == 1:
        return hits[0]
    hits = [n for n, c, ws in clubs if c == country and w[0] in ws]
    return hits[0] if len(hits) == 1 else short


def main():
    args = sys.argv[1:]
    names = []
    if "--names" in args:
        k = args.index("--names")
        names, args = args[k + 1:], args[:k]
    if len(args) < 2:
        sys.exit(__doc__)
    cache, out = args[0], args[1]
    first = int(args[2]) if len(args) > 2 else 2006
    last = int(args[3]) if len(args) > 3 else 2025
    clubs = name_map(names) if names else []
    os.makedirs(cache, exist_ok=True)
    lines = [("country", "tier", "season", "club", "won", "drawn", "lost", "for", "against", "points")]
    for div, (country, tier) in DIVISIONS.items():
        got = 0
        for season in range(first, last + 1):
            text = fetch(div, season, cache)
            rows = table(text, div in H2H) if text else []
            if len(rows) < 8:
                continue  # missing, or a season still in progress at the source
            c, w, d, l, f, a, p = rows[0]
            if clubs:
                c = full_name(c, clubs, CLUB_COUNTRY[country])
            lines.append((country, tier, season, c, w, d, l, f, a, p))
            got += 1
        print(f"{div} -> {country}/{tier}: {got} seasons", flush=True)
    with open(out, "w", newline="", encoding="utf-8") as fh:
        csv.writer(fh).writerows(lines)
    print(f"wrote {out}: {len(lines) - 1} champions")


if __name__ == "__main__":
    main()
