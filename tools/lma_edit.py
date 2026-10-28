"""LMA Manager 2007 database editor (CSV round trip).

  python lma_edit.py export  <Export.mng> <csv_dir>
  python lma_edit.py import  <Export.mng> <csv_dir> <out.mng>
  python lma_edit.py install <out.mng> <game_dir> [--as ExportPR.mng]

export writes players.csv, clubs.csv and nations.csv (open them in Excel / LibreOffice, keep UTF-8).
import starts from the original archive and applies the CSVs on top, so every byte the editor does
not understand is kept as it was.

players.csv
  id            row key, do not change. Ids 0..N-1 are fixed slots in Players.bin.
  club_id       club the player belongs to (blank = free agent). Max 32 per club.
  squad_slot    order inside the club's squad list (lower first).
  forename, surname   free text (cp1252). New names are appended to the name pools.
  age, nation_id, ratings (0-100)   see FORMATS.md for which labels are verified.
clubs.csv
  id, name_E..name_S (max 25 chars), short_E..short_S (max 7 chars)
"""
import csv
import os
import shutil
import sys

from lma_db import LANGS, PLAYER_FIELDS, Database

RATING_COLS = [c for c, *_ in PLAYER_FIELDS if c not in ("surname_id", "forename_id", "nation_id", "age", "position", "position2", "squad_number", "height_cm")]
POS = ["GK", "DEF", "MID", "ATT"]
POS2 = ["", "DEF", "MID", "ATT"]
PLAYER_COLS = ["id", "club_id", "club", "squad_slot", "forename", "surname", "squad_number", "position", "position2", "age", "height_cm", "nation_id", "nation"] + RATING_COLS

# nation ids. 0-50 are the major nations A-Z, 51+ the rest A-Z. Names were identified from each
# nation's best-known players (see nations.csv); fill blanks in nations.csv, it overrides this table.
KNOWN_NATIONS = dict(enumerate(
    "Algeria Argentina Australia Austria Belgium Brazil Bulgaria Chile Colombia Croatia Cyprus "
    "Czech_Rep. Denmark Eire England Finland France Germany Greece Hungary Iceland Israel Italy "
    "Jamaica Japan Malta Mexico Holland N._Ireland Norway Paraguay Poland Portugal Romania Russia "
    "Scotland Serbia Singapore Slovakia Slovenia South_Africa South_Korea Spain Sweden Switzerland "
    "Tunisia Turkey Ukraine Uruguay USA Wales Albania Andorra Angola Armenia".split()))
KNOWN_NATIONS = {k: v.replace("_", " ") for k, v in KNOWN_NATIONS.items()}
KNOWN_NATIONS.update({56: "Azerbaijan", 58: "Belarus", 59: "Benin", 61: "Bolivia", 62: "Bosnia",
                      64: "Burkina Faso", 65: "Burundi", 66: "Cameroon", 67: "Canada", 68: "Cape Verde",
                      70: "Chad", 71: "China", 72: "DR Congo", 73: "Costa Rica", 74: "Ivory Coast",
                      79: "Ecuador", 80: "Egypt", 82: "Estonia", 84: "Gabon", 85: "Gambia", 86: "Georgia",
                      87: "Ghana", 90: "Guatemala", 92: "Guinea", 94: "Honduras", 97: "Iran", 98: "Iraq",
                      101: "Kenya", 103: "Latvia", 106: "Liberia", 108: "Liechtenstein", 109: "Lithuania",
                      110: "Luxembourg", 111: "Macedonia", 113: "Malawi", 114: "Mali", 115: "Mauritius",
                      116: "Moldova", 117: "Morocco", 118: "Mozambique", 119: "Namibia", 120: "New Zealand",
                      121: "Nigeria", 122: "North Korea", 123: "Oman", 124: "Panama", 126: "Peru",
                      128: "Rwanda", 130: "Senegal", 131: "Sierra Leone", 137: "Suriname", 142: "Thailand",
                      143: "Togo", 144: "Trinidad & Tobago", 146: "Uganda", 148: "Uzbekistan",
                      149: "Venezuela", 150: "Vietnam", 152: "Zambia", 153: "Zimbabwe"})


def nation_names(csv_dir):
    path = os.path.join(csv_dir, "nations.csv")
    names = dict(KNOWN_NATIONS)
    if os.path.exists(path):
        for row in csv.DictReader(open(path, encoding="utf-8-sig")):
            if row["name"]:
                names[int(row["nation_id"])] = row["name"]
    return names


def export(mng, out):
    os.makedirs(out, exist_ok=True)
    db = Database(mng)
    owners = db.owners()
    names = nation_names(out)
    by_nation = {}
    with open(os.path.join(out, "players.csv"), "w", newline="", encoding="utf-8-sig") as f:
        w = csv.DictWriter(f, PLAYER_COLS)
        w.writeheader()
        for pid in range(len(db.players)):
            p = db.player(pid)
            ci, slot = owners.get(pid, ("", ""))
            row = {"id": pid, "club_id": ci, "squad_slot": slot,
                   "club": db.club_names["E"][ci] if ci != "" else "",
                   "nation": names.get(p["nation_id"], "")}
            row.update({c: p[c] for c in PLAYER_COLS if c in p})
            row["position"], row["position2"] = POS[p["position"]], POS2[p["position2"]]
            w.writerow(row)
            by_nation.setdefault(p["nation_id"], []).append((sum(p[c] for c in RATING_COLS), p["surname"]))
    with open(os.path.join(out, "clubs.csv"), "w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f)
        w.writerow(["id"] + [f"name_{l}" for l in LANGS] + [f"short_{l}" for l in LANGS] + ["wealth", "manager", "stadium", "cap_north", "cap_east",
                                                                "cap_south", "cap_west", "squad_size"])
        for ci in range(len(db.clubs)):
            w.writerow([ci] + [db.club_names[l][ci] for l in LANGS] + [db.short_names[l][ci] for l in LANGS]
                       + [db.club_wealth(ci), db.manager_surname(ci)]
                       + ([db.stadium_names[ci]] + db.stands(ci) if ci < len(db.stadiums) else [""] * 5)
                       + [len(db.squad(ci))])
    with open(os.path.join(out, "nations.csv"), "w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f)
        w.writerow(["nation_id", "name", "players", "best_players"])
        for nid in sorted(by_nation):
            best = ", ".join(s.strip() for _, s in sorted(by_nation[nid], reverse=True)[:6])
            w.writerow([nid, names.get(nid, ""), len(by_nation[nid]), best])
    print(f"exported {len(db.players)} players, {len(db.clubs)} clubs to {out}")


def apply(mng, csv_dir, out_mng):
    db = Database(mng)
    squads = {}
    errors = []
    for row in csv.DictReader(open(os.path.join(csv_dir, "players.csv"), encoding="utf-8-sig")):
        pid = int(row["id"])
        if not 0 <= pid < len(db.players):
            errors.append(f"player id {pid} out of range (0..{len(db.players) - 1})")
            continue
        vals = {c: row[c] for c in RATING_COLS + ["age", "nation_id", "squad_number", "height_cm"] if row.get(c, "") != ""}
        try:
            vals["position"] = POS.index(row["position"].strip().upper())
            vals["position2"] = POS2.index(row["position2"].strip().upper())
            vals["forename_id"] = db.forenames.index(row["forename"])
            vals["surname_id"] = db.surnames.index(row["surname"])
            for c in RATING_COLS:
                if c in vals and not 0 <= int(vals[c]) <= 100:
                    raise ValueError(f"{c}={vals[c]} must be 0-100")
            if "age" in vals and not 16 <= int(vals["age"]) <= 47:
                raise ValueError(f"age={vals['age']} must be 16-47")
            db.set_player(pid, vals)
        except (ValueError, UnicodeEncodeError) as e:
            errors.append(f"player {pid} ({row['forename']} {row['surname']}): {e}")
        if row["club_id"] != "":
            slot = float(row["squad_slot"]) if row["squad_slot"] != "" else 99.0
            squads.setdefault(int(row["club_id"]), []).append((slot, pid))
    for ci in range(len(db.clubs)):
        if db.is_placeholder(ci) and ci not in squads:
            continue
        ids = [pid for _, pid in sorted(squads.get(ci, []))]
        if len(ids) > 32:
            errors.append(f"club {ci} ({db.club_names['E'][ci]}) has {len(ids)} players, max 32")
            continue
        db.set_squad(ci, ids)
    cpath = os.path.join(csv_dir, "clubs.csv")
    if os.path.exists(cpath):
        for row in csv.DictReader(open(cpath, encoding="utf-8-sig")):
            ci = int(row["id"])
            for l in LANGS:
                db.club_names[l][ci] = row[f"name_{l}"]
                db.short_names[l][ci] = row[f"short_{l}"]
            if row.get("manager", "") != "" and row["manager"] != db.manager_surname(ci):
                db.set_manager_surname(ci, row["manager"])
            if row.get("wealth", "") != "":
                db.set_club_wealth(ci, int(row["wealth"]))
            if ci < len(db.stadiums) and row.get("cap_north", "") != "":
                db.stadium_names[ci] = row["stadium"]
                caps = [int(row[k]) for k in ("cap_north", "cap_east", "cap_south", "cap_west")]
                if not all(0 <= c <= 65535 for c in caps):
                    sys.exit(f"club {ci}: stand capacity must be 0-65535")
                db.set_stands(ci, caps)
    if errors:
        print("\n".join(errors[:50]))
        sys.exit(f"{len(errors)} error(s), nothing written")
    try:
        data = db.build()
    except ValueError as e:
        sys.exit(f"error: {e}")
    open(out_mng, "wb").write(data)
    for label, pool in (("forename", db.forenames), ("surname", db.surnames)):
        if pool.dirty:
            print(f"note: new {label}s added, pool now {len(pool.names)} entries")
    print(f"wrote {out_mng} ({len(data)} bytes)")


def install(mng, game_dir, name="Export.mng"):
    dst = os.path.join(game_dir, "Common", "GameData", "Misc", name)
    bak = dst + ".orig"
    if not os.path.exists(bak):
        shutil.copy2(dst, bak)
    shutil.copy2(mng, dst)
    print(f"installed {mng} -> {dst} (original kept as {bak})")


if __name__ == "__main__":
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    if a[0] == "export":
        export(a[1], a[2])
    elif a[0] == "import":
        apply(a[1], a[2], a[3])
    elif a[0] == "install":
        install(a[1], a[2], a[4] if len(a) > 4 and a[3] == "--as" else "Export.mng")
    else:
        sys.exit(__doc__)
