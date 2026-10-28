#!/usr/bin/env python3
"""Current squads from the live Transfermarkt site, for squad-import's membership (#31, the maintainer 10 Oct:
live Transfermarkt over FC 27; loanees at the club they play for).

  tools/tm_live.py <cache dir> <out.csv> [COMP ...]

For each competition code (default: the leagues the fc27 mod uses) it reads the competition's club
list, then each club's squad page (season 2026, detailed view), with a 2 s pause between requests
and every page cached in <cache dir> (a re-run reads the cache: delete a file to refresh it).
A loanee is listed by the club he plays for ("On loan from X" on that page). Writes one row per
player and club: comp, club_id, club, player_id, name, birth_date (YYYY-MM-DD), number,
loan_from, market_value_eur. The pages and the CSV are derived data: keep them out of the repo.
"""
import csv, html, os, re, sys, time, urllib.request

UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"
BASE = "https://www.transfermarkt.com"
SEASON = 2026
# England 1-5, Scotland 1-2, Italy, Spain, Germany, France, Netherlands, Portugal 1-2, then the
# other top flights FC 27 has (Belgium, Turkey, Saudi Arabia, USA, Austria, Switzerland, Denmark,
# Norway, Sweden, Greece, Poland, Czechia, Brazil, Argentina, Japan, Korea)
COMPS = ["GB1", "GB2", "GB3", "GB4", "CNAT", "SC1", "SC2", "IT1", "IT2", "ES1", "ES2", "L1", "L2", "FR1", "FR2",
         "NL1", "NL2", "PO1", "PO2", "BE1", "TR1", "SA1", "MLS1", "A1", "C1", "DK1", "NO1", "SE1", "GR1", "PL1",
         "TS1", "BRA1", "ARGC", "JAP1", "RSK1"]
last = [0.0]


def fetch(url, path):
    if os.path.exists(path) and os.path.getsize(path) > 1000:
        return open(path, encoding="utf-8", errors="replace").read()
    wait = 2.0 - (time.time() - last[0])
    if wait > 0:
        time.sleep(wait)
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept-Language": "en"})
    try:
        body = urllib.request.urlopen(req, timeout=30).read().decode("utf-8", errors="replace")
    except Exception as e:  # a blocked or missing page is logged and skipped
        print(f"  ! {url}: {e}", flush=True)
        body = ""
    last[0] = time.time()
    if len(body) > 1000:
        open(path, "w", encoding="utf-8").write(body)
    return body


def clubs_of(comp, cache):
    page = fetch(f"{BASE}/x/startseite/wettbewerb/{comp}/saison_id/{SEASON}", os.path.join(cache, f"comp_{comp}.html"))
    seen, out = set(), []
    # club links (attributes in any order); the table's own links carry the season and a title
    # and the season the league is in: calendar-year leagues (MLS, Brazil, Scandinavia, Korea) are
    # in season SEASON - 1 on Transfermarkt; their SEASON pages are next year's, mostly empty, squads
    for tag in re.findall(r'<a [^>]*href="/[^/"]+/startseite/verein/\d+/saison_id/\d+"[^>]*>', page):
        link = re.search(r'href="/([^/"]+)/startseite/verein/(\d+)/saison_id/(\d+)', tag)
        title = re.search(r'title="([^"]+)"', tag)
        if link and title and link.group(2) not in seen:
            seen.add(link.group(2))
            out.append((link.group(2), link.group(1), html.unescape(title.group(1)), int(link.group(3))))
    return out


ROW = re.compile(r'<tr class="(?:odd|even)">(.*?)</tr>\s*(?=<tr class="(?:odd|even)">|</tbody>)', re.S)


def squad(cid, slug, cache, season=SEASON):
    name = f"club_{cid}.html" if season == SEASON else f"club_{cid}_s{season}.html"
    page = fetch(f"{BASE}/{slug}/kader/verein/{cid}/saison_id/{season}/plus/1", os.path.join(cache, name))
    out = []
    for row in ROW.findall(page):
        m = re.search(r'href="/[^"]+/profil/spieler/(\d+)">\s*([^<]+?)\s*<', row)
        if not m:
            continue
        dob = re.search(r'>(\d\d)/(\d\d)/(\d{4}) \(\d+\)<', row)
        num = re.search(r'rn_nummer>(\d+)<', row)
        loan = re.search(r'title="On loan from ([^"]+?) until', row)
        value = re.search(r'marktwertverlauf/spieler/\d+">[^\d<]*([\d.,]+)(k|m)', row)
        eur = ""
        if value:
            eur = str(int(float(value.group(1).replace(",", "")) * (1e6 if value.group(2) == "m" else 1e3)))
        out.append((m.group(1), html.unescape(m.group(2)), f"{dob.group(3)}-{dob.group(2)}-{dob.group(1)}" if dob else "", num.group(1) if num else "", html.unescape(loan.group(1)) if loan else "", eur))
    return out


def main():
    if len(sys.argv) < 3:
        sys.exit(__doc__)
    cache, out_path = sys.argv[1], sys.argv[2]
    comps = sys.argv[3:] or COMPS
    os.makedirs(cache, exist_ok=True)
    rows = []
    for comp in comps:
        clubs = clubs_of(comp, cache)
        print(f"{comp}: {len(clubs)} clubs", flush=True)
        for cid, slug, name, season in clubs:
            if season not in (SEASON, SEASON - 1):
                print(f"  ! {name} ({cid}): the league page links season {season}, skipped (stale league code?)", flush=True)
                continue
            players = squad(cid, slug, cache, season)
            if not players:
                print(f"  ! {name} ({cid}): no squad", flush=True)
            rows += [(comp, cid, name) + p for p in players]
    with open(out_path, "w", newline="", encoding="utf-8") as f:
        w = csv.writer(f)
        w.writerow(["comp", "club_id", "club", "player_id", "name", "birth_date", "number", "loan_from", "market_value_eur"])
        w.writerows(rows)
    print(f"{len(rows)} players -> {out_path}")


if __name__ == "__main__":
    main()
