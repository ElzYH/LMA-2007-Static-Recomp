//! Spreadsheet editing of the database: the Rust port of `tools/lma_edit.py`.
//!
//! `export` writes `players.csv`, `clubs.csv`, `nations.csv` and, on the 360, `history.csv`
//! (UTF-8 with BOM, opens in Excel / LibreOffice). `apply` reads `players.csv` and/or `clubs.csv` from a folder and
//! changes the database in place; every byte it does not understand is kept as it was.
//!
//! Files written by `export` (or by `lma_edit.py export`) can be applied unchanged and give a
//! byte-identical database. `apply` is also *sparse*, so a mod can ship only what it changes:
//! rows not listed, columns left out and blank cells keep the original value. Only `club_id`
//! needs care: when the column is present, a blank cell makes the player a free agent.
//!
//! players.csv: `id` (required), `club_id`, `squad_slot` (order in the club's squad, lower
//! first; blank = last), `forename`, `surname` (new names are appended to the name pools;
//! forename `-` = none, stored as the game does for single-name players: one space),
//! `position`, `position2` (GK/DEF/MID/ATT; `-` clears position2), `age` (16-47),
//! `height_cm` (150-213), `nation_id`, `squad_number`, and the 0-100 ratings. `club` and
//! `nation` are informational and ignored.
//! clubs.csv: `id` (required), `name_E`..`name_S` (max 25 characters), `short_E`..`short_S`
//! (max 7), `wealth` (0-15), `country` (0-15, the league's country) and `division` (0-63, within
//! the country; see `Database::club_division`), `manager` (surname), `manager_rep` (0-99, the
//! manager's reputation; the 360's record only), `stadium` (max 39),
//! `cap_north`, `cap_east`, `cap_south`, `cap_west` (0-65535, all four together), `record_of` (a
//! club id: this id takes that whole club, record, squad and names; leagues are id ranges, so this
//! is how a club changes league; applied before everything else, which then names the new ids).
//! `squad_size` is ignored.
//! free_managers.csv (#795): the unattached managers, after the clubs' own records: `slot`
//! (0.., required), `manager` (surname), `manager_rep` (0-99). Rows not listed keep theirs.

use crate::csv::{write_row, Sheet};
use crate::db::{Field, Position, AGE, FACE, FOOT, HAIR, HAIR_COLOUR, HEIGHT, LANGS, NATION, RATINGS, SIDES, SKIN, SQUAD_MAX, SQUAD_NUMBER, VALUE};
use crate::{Database, Error, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

const STAND_COLS: [&str; 4] = ["cap_north", "cap_east", "cap_south", "cap_west"];

/// Ratings in record order, the column order `lma_edit.py` uses.
fn rating_cols() -> Vec<Field> {
    let mut r = RATINGS.to_vec();
    r.sort_by_key(|f| f.pos);
    r
}

/// Player fields found on the 360's record only (#31), exported after the ratings, with their
/// ranges.
const FIELDS_360: [(Field, u32, u32); 7] = [(SKIN, 0, 3), (FACE, 0, 15), (VALUE, 0, 65535), (SIDES, 0, 7), (FOOT, 0, 3), (HAIR, 0, 31), (HAIR_COLOUR, 0, 13)];

/// Numeric player columns: field and allowed range of the stored value. `FIELDS_360` only on the
/// 360, the one layout they were found in.
fn numeric_cols(e: crate::Endian) -> Vec<(Field, u32, u32)> {
    let mut v = vec![(AGE, 16, 47), (HEIGHT, 150, 213), (NATION, 0, 255), (SQUAD_NUMBER, 0, 127)];
    if e == crate::Endian::Big {
        v.extend(FIELDS_360);
    }
    v.extend(rating_cols().into_iter().map(|f| (f, 0, 100)));
    v
}

// ---- export ------------------------------------------------------------------------------

pub fn export(db: &Database, dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let nations = nation_names(dir, db.endian())?;
    let squads: Vec<Vec<usize>> = (0..db.club_count()).map(|ci| if db.is_placeholder(ci) { vec![] } else { db.squad(ci) }).collect();
    let mut owner: HashMap<usize, (usize, usize)> = HashMap::new();
    for (ci, squad) in squads.iter().enumerate() {
        for (slot, &pid) in squad.iter().enumerate() {
            owner.entry(pid).or_insert((ci, slot));
        }
    }
    let ratings = rating_cols();
    let mut out = String::from("\u{feff}");
    let mut header = ["id", "club_id", "club", "squad_slot", "forename", "surname", "squad_number", "position", "position2", "age", "height_cm", "nation_id", "nation"].map(String::from).to_vec();
    header.extend(ratings.iter().map(|f| f.name.to_string()));
    let fields_360 = db.endian() == crate::Endian::Big;
    if fields_360 {
        header.extend(FIELDS_360.iter().map(|(f, _, _)| f.name.to_string()));
    }
    write_row(&mut out, &header);
    let mut by_nation: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for pid in 0..db.player_count() {
        let p = db.player(pid);
        let (club_id, club, slot) = match owner.get(&pid) {
            Some(&(ci, slot)) => (ci.to_string(), db.club_names[0][ci].clone(), slot.to_string()),
            None => Default::default(),
        };
        let mut row = vec![pid.to_string(), club_id, club, slot, p.forename.clone(), p.surname.clone(), p.squad_number.to_string()];
        row.push(p.position.label().into());
        row.push(p.position2.map_or("", Position::label).into());
        row.extend([p.age.to_string(), p.height_cm.to_string(), p.nation_id.to_string(), nations.get(&p.nation_id).cloned().unwrap_or_default()]);
        let values: Vec<u32> = ratings.iter().map(|&f| db.field(pid, f)).collect();
        row.extend(values.iter().map(u32::to_string));
        if fields_360 {
            row.extend(FIELDS_360.iter().map(|(f, _, _)| db.field(pid, *f).to_string()));
        }
        write_row(&mut out, &row);
        by_nation.entry(p.nation_id).or_default().push((values.iter().sum(), p.surname));
    }
    std::fs::write(dir.join("players.csv"), out)?;

    let mut out = String::from("\u{feff}");
    let mut header: Vec<String> = vec!["id".into()];
    header.extend(LANGS.iter().map(|l| format!("name_{l}")));
    header.extend(LANGS.iter().map(|l| format!("short_{l}")));
    header.extend(["wealth", "country", "division", "manager", "manager_rep", "stadium"].map(String::from));
    header.extend(STAND_COLS.map(String::from));
    header.push("squad_size".into());
    write_row(&mut out, &header);
    for ci in 0..db.club_count() {
        let mut row = vec![ci.to_string()];
        row.extend((0..LANGS.len()).map(|l| db.club_names[l][ci].clone()));
        row.extend((0..LANGS.len()).map(|l| db.short_names[l][ci].clone()));
        row.extend([db.club_wealth(ci).to_string(), db.club_country(ci).to_string(), db.club_division(ci).to_string(), db.manager_surname(ci).to_string()]);
        row.push(db.manager_reputation(ci).map(|r| r.to_string()).unwrap_or_default());
        match db.stands(ci) {
            Some(caps) => {
                row.push(db.stadium_names[ci].clone());
                row.extend(caps.map(|c| c.to_string()));
            }
            None => row.extend(std::iter::repeat_n(String::new(), 5)),
        }
        row.push(squads[ci].len().to_string());
        write_row(&mut out, &row);
    }
    std::fs::write(dir.join("clubs.csv"), out)?;

    let mut out = String::from("\u{feff}");
    write_row(&mut out, &["slot", "manager", "manager_rep"]);
    for (slot, mi) in (db.club_count()..db.manager_count()).enumerate() {
        write_row(&mut out, &[slot.to_string(), db.manager_surname(mi).to_string(), db.manager_reputation(mi).map(|r| r.to_string()).unwrap_or_default()]);
    }
    std::fs::write(dir.join("free_managers.csv"), out)?;

    let mut out = String::from("\u{feff}");
    write_row(&mut out, &["nation_id", "name", "players", "best_players"]);
    let mut ids: Vec<_> = by_nation.keys().copied().collect();
    ids.sort();
    for nid in ids {
        let mut players = by_nation.remove(&nid).unwrap();
        let count = players.len();
        players.sort_by(|a, b| b.cmp(a));
        let best: Vec<&str> = players.iter().take(6).map(|(_, s)| s.trim()).collect();
        write_row(&mut out, &[nid.to_string(), nations.get(&nid).cloned().unwrap_or_default(), count.to_string(), best.join(", ")]);
    }
    std::fs::write(dir.join("nations.csv"), out)?;
    export_history(db, dir)?;
    export_cups(db, dir)?;
    export_last_tables(db, dir)
}

const LAST_TABLE_COLS: [&str; 5] = ["country", "tier", "position", "club_id", "club"];

/// last_tables.csv: last season's final tables (`Database::last_table`), from which a new career
/// draws its European entrants: `country` (`db::LAST_TABLE_NATIONS`: 0 England, 1 Scotland, 2
/// Spain, 3 Italy, 4 France, 5 Germany, 6 Netherlands, 7 Portugal; not history.csv's order),
/// `tier`, `position` (1 = champions), `club_id` (blank for a uid whose club the database no
/// longer has), `club` (informational), `uid`.
fn export_last_tables(db: &Database, dir: &Path) -> Result<()> {
    let mut out = String::from("\u{feff}");
    let mut cols = LAST_TABLE_COLS.to_vec();
    cols.push("uid");
    write_row(&mut out, &cols);
    let mut any = false;
    for (c, tiers) in crate::db::LAST_TABLE_SIZES.iter().enumerate() {
        for t in 0..tiers.len() {
            let table = db.last_table(c, t).unwrap_or_default();
            if table.iter().all(|&u| u == 0) {
                continue;
            }
            any = true;
            for (k, &uid) in table.iter().enumerate() {
                let club = (uid != 0).then(|| db.club_of_uid(uid)).flatten();
                write_row(&mut out, &[c.to_string(), t.to_string(), (k + 1).to_string(), club.map(|c| c.to_string()).unwrap_or_default(), club.map(|c| db.club_names[0][c].clone()).unwrap_or_default(), uid.to_string()]);
            }
        }
    }
    if any {
        std::fs::write(dir.join("last_tables.csv"), out)?;
    }
    Ok(())
}

/// history.csv: the league champions (`Database::league_history`), one row per season with a
/// champion: `country`, `tier` (0 = top), `row` (0 = the newest season), `club_id` (blank for a
/// club the database no longer has), `club` (informational), `uid`, `won`, `drawn`, `lost`,
/// `for`, `against`, `points`.
fn export_history(db: &Database, dir: &Path) -> Result<()> {
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &HISTORY_COLS);
    let mut any = false;
    for c in 0..crate::db::HISTORY_COUNTRIES {
        for t in 0..crate::db::HISTORY_TIERS {
            for (i, r) in db.league_history(c, t).unwrap_or_default().iter().enumerate().filter(|(_, r)| r.uid != 0) {
                any = true;
                let club = db.club_of_uid(r.uid);
                let mut row = vec![c.to_string(), t.to_string(), i.to_string(), club.map(|c| c.to_string()).unwrap_or_default(), club.map(|c| db.club_names[0][c].clone()).unwrap_or_default()];
                row.extend([r.uid as u32, r.won as u32, r.drawn as u32, r.lost as u32, r.goals_for as u32, r.goals_against as u32, r.points as u32].map(|v| v.to_string()));
                write_row(&mut out, &row);
            }
        }
    }
    if any {
        std::fs::write(dir.join("history.csv"), out)?;
    }
    Ok(())
}

/// cups.csv: the cup finals (`Database::cup_history`), one row per final: the cup's name
/// (`db::CUPS`), the row (0 = the season ending in the base year), winner and runner-up by club id
/// and name (a uid alone for a club the database no longer has), the score.
fn export_cups(db: &Database, dir: &Path) -> Result<()> {
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &CUP_COLS);
    let mut any = false;
    for (c, (name, _)) in crate::db::CUPS.iter().enumerate() {
        for (i, r) in db.cup_history(c).unwrap_or_default().iter().enumerate().filter(|(_, r)| r.winner != 0) {
            any = true;
            let mut row = vec![name.to_string(), i.to_string()];
            for uid in [r.winner, r.runner_up] {
                let club = db.club_of_uid(uid);
                row.extend([club.map(|c| c.to_string()).unwrap_or_default(), club.map(|c| db.club_names[0][c].clone()).unwrap_or_default(), uid.to_string()]);
            }
            row.extend([r.winner_goals.to_string(), r.runner_up_goals.to_string()]);
            write_row(&mut out, &row);
        }
    }
    if any {
        std::fs::write(dir.join("cups.csv"), out)?;
    }
    Ok(())
}

const CUP_COLS: [&str; 10] = ["cup", "row", "winner_id", "winner", "winner_uid", "runner_up_id", "runner_up", "runner_up_uid", "winner_goals", "runner_up_goals"];

/// cups.csv (see `export_cups`): every cup it names gets exactly its rows; a club is its `*_id`
/// when given (its uid looked up), else its `*_uid`.
fn apply_cups(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let mut cups: std::collections::BTreeMap<usize, Vec<(usize, crate::db::CupRow)>> = std::collections::BTreeMap::new();
    for (n, r) in sheet.rows.iter().enumerate() {
        let num = |c: &str, max: u32| -> std::result::Result<Option<u32>, String> {
            match sheet.get(r, sheet.col(c)) {
                None => Ok(None),
                Some(v) => v.parse::<u32>().ok().filter(|&x| x <= max).map(Some).ok_or_else(|| format!("cups.csv line {}: {c} {v:?} must be 0-{max}", n + 2)),
            }
        };
        let row = (|| -> std::result::Result<(usize, usize, crate::db::CupRow), String> {
            let need = |c: &str, max: u32| num(c, max).and_then(|v| v.ok_or_else(|| format!("cups.csv line {}: no {c}", n + 2)));
            let name = sheet.get(r, sheet.col("cup")).unwrap_or_default();
            let cup = crate::db::CUPS.iter().position(|c| c.0 == name).ok_or_else(|| format!("cups.csv line {}: cup {name:?} is not one of {:?}", n + 2, crate::db::CUPS.map(|c| c.0)))?;
            let row = need("row", (crate::db::CUP_ROWS - 1) as u32)? as usize;
            let uid = |side: &str| -> std::result::Result<u16, String> {
                match num(&format!("{side}_id"), 65535)? {
                    Some(ci) => db.uid_of_club(ci as usize).ok_or_else(|| format!("cups.csv line {}: club {ci} has no uid", n + 2)),
                    None => need(&format!("{side}_uid"), 65535).map(|u| u as u16),
                }
            };
            let b = |c: &str| need(c, 255).map(|v| v as u8);
            Ok((cup, row, crate::db::CupRow { winner: uid("winner")?, runner_up: uid("runner_up")?, winner_goals: b("winner_goals")?, runner_up_goals: b("runner_up_goals")? }))
        })();
        match row {
            Ok((cup, i, f)) => cups.entry(cup).or_default().push((i, f)),
            Err(e) => errors.push(e),
        }
    }
    for (c, mut rows) in cups {
        rows.sort_by_key(|r| r.0);
        if rows.windows(2).any(|w| w[0].0 == w[1].0) {
            errors.push(format!("cups.csv: {} names a row twice", crate::db::CUPS[c].0));
            continue;
        }
        let len = rows.last().map_or(0, |r| r.0 + 1);
        let mut all = vec![crate::db::CupRow::default(); len];
        for (i, f) in rows {
            all[i] = f;
        }
        if let Err(e) = db.set_cup_history(c, &all) {
            errors.push(e.to_string());
        }
    }
}

const HISTORY_COLS: [&str; 12] = ["country", "tier", "row", "club_id", "club", "uid", "won", "drawn", "lost", "for", "against", "points"];

/// history.csv (see `export_history`): every (country, tier) it names gets exactly its rows;
/// a row's club is `club_id` when given (its uid looked up), else `uid`.
fn apply_history(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let mut leagues: std::collections::BTreeMap<(usize, usize), Vec<(usize, crate::db::HistoryRow)>> = std::collections::BTreeMap::new();
    for (n, r) in sheet.rows.iter().enumerate() {
        let num = |c: &str, max: u32| -> std::result::Result<Option<u32>, String> {
            match sheet.get(r, sheet.col(c)) {
                None => Ok(None),
                Some(v) => v.parse::<u32>().ok().filter(|&x| x <= max).map(Some).ok_or_else(|| format!("history.csv line {}: {c} {v:?} must be 0-{max}", n + 2)),
            }
        };
        let row = (|| -> std::result::Result<((usize, usize), usize, crate::db::HistoryRow), String> {
            let need = |c: &str, max: u32| num(c, max).and_then(|v| v.ok_or_else(|| format!("history.csv line {}: no {c}", n + 2)));
            let (country, tier, row) = (need("country", 255)? as usize, need("tier", 255)? as usize, need("row", 65535)? as usize);
            let uid = match num("club_id", 65535)? {
                Some(ci) => db.uid_of_club(ci as usize).ok_or_else(|| format!("history.csv line {}: club {ci} has no uid", n + 2))?,
                None => need("uid", 65535)? as u16,
            };
            let b = |c: &str| need(c, 255).map(|v| v as u8);
            Ok(((country, tier), row, crate::db::HistoryRow { uid, won: b("won")?, drawn: b("drawn")?, lost: b("lost")?, goals_for: b("for")?, goals_against: b("against")?, points: b("points")? }))
        })();
        match row {
            Ok((league, i, h)) => leagues.entry(league).or_default().push((i, h)),
            Err(e) => errors.push(e),
        }
    }
    for ((c, t), mut rows) in leagues {
        rows.sort_by_key(|r| r.0);
        if rows.windows(2).any(|w| w[0].0 == w[1].0) {
            errors.push(format!("history.csv: country {c} tier {t} names a row twice"));
            continue;
        }
        let len = rows.last().map_or(0, |r| r.0 + 1);
        let mut all = vec![crate::db::HistoryRow::default(); len];
        for (i, h) in rows {
            all[i] = h;
        }
        if let Err(e) = db.set_league_history(c, t, &all) {
            errors.push(e.to_string());
        }
    }
}

/// last_tables.csv (see `export_last_tables`): every (country, tier) it names gets that table,
/// which must name each position of the league once; a row's club is its `uid` when given, else
/// `club_id`'s uid (club 0's is known only from its league history).
fn apply_last_tables(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let mut leagues: std::collections::BTreeMap<(usize, usize), Vec<(usize, u16)>> = std::collections::BTreeMap::new();
    for (n, r) in sheet.rows.iter().enumerate() {
        let num = |c: &str| -> std::result::Result<Option<u32>, String> {
            match sheet.get(r, sheet.col(c)) {
                None => Ok(None),
                Some(v) => v.parse::<u32>().ok().filter(|&x| x <= 65535).map(Some).ok_or_else(|| format!("last_tables.csv line {}: {c} {v:?} must be a number", n + 2)),
            }
        };
        let row = (|| -> std::result::Result<((usize, usize), usize, u16), String> {
            let need = |c: &str| num(c).and_then(|v| v.ok_or_else(|| format!("last_tables.csv line {}: no {c}", n + 2)));
            let (country, tier, pos) = (need("country")? as usize, need("tier")? as usize, need("position")? as usize);
            let uid = match (num("uid")?, num("club_id")?) {
                (Some(u), _) => u as u16,
                (None, Some(ci)) => db.uid_of_club(ci as usize).ok_or_else(|| format!("last_tables.csv line {}: club {ci} has no uid", n + 2))?,
                (None, None) => return Err(format!("last_tables.csv line {}: no club_id or uid", n + 2)),
            };
            Ok(((country, tier), pos, uid))
        })();
        match row {
            Ok((league, pos, uid)) => leagues.entry(league).or_default().push((pos, uid)),
            Err(e) => errors.push(e),
        }
    }
    for ((c, t), mut rows) in leagues {
        rows.sort_by_key(|r| r.0);
        let n = crate::db::LAST_TABLE_SIZES.get(c).and_then(|l| l.get(t)).copied().unwrap_or(0);
        if rows.iter().map(|r| r.0).ne(1..=n) {
            errors.push(format!("last_tables.csv: country {c} tier {t} must name positions 1-{n} once each"));
            continue;
        }
        let uids: Vec<u16> = rows.iter().map(|r| r.1).collect();
        if let Err(e) = db.set_last_table(c, t, &uids) {
            errors.push(e.to_string());
        }
    }
}

/// Built-in nation names (the PS2's own ids for its database, #3), overridden by a
/// `nations.csv` already in `dir`.
fn nation_names(dir: &Path, e: crate::Endian) -> Result<HashMap<u32, String>> {
    let known = if e == crate::Endian::Little { KNOWN_NATIONS_PS2 } else { KNOWN_NATIONS };
    let mut names: HashMap<u32, String> = known.iter().map(|&(id, n)| (id, n.to_string())).collect();
    let path = dir.join("nations.csv");
    if path.exists() {
        let sheet = Sheet::read(&path)?;
        let (id, name) = (sheet.col("nation_id"), sheet.col("name"));
        for row in &sheet.rows {
            if let (Some(i), Some(n)) = (sheet.get(row, id).and_then(|v| v.parse().ok()), sheet.get(row, name)) {
                names.insert(i, n.to_string());
            }
        }
    }
    Ok(names)
}

// ---- apply -------------------------------------------------------------------------------

/// The fewest players a club with a squad keeps through an apply (11 on the pitch).
pub const MIN_PLAYERS: usize = 11;

/// Apply `players.csv` and/or `clubs.csv` from `dir`. Returns notes (e.g. name pools grown).
/// On error the database is left part-edited: discard it.
pub fn apply(db: &mut Database, dir: &Path) -> Result<Vec<String>> {
    let mut errors = Vec::new();
    let (players, clubs, history, cups) = (dir.join("players.csv"), dir.join("clubs.csv"), dir.join("history.csv"), dir.join("cups.csv"));
    let last_tables = dir.join("last_tables.csv");
    let free_managers = dir.join("free_managers.csv");
    if !players.exists() && !clubs.exists() && !history.exists() && !cups.exists() && !last_tables.exists() && !free_managers.exists() {
        return Err(Error::Value(format!("{}: no players.csv, clubs.csv, history.csv, cups.csv, last_tables.csv or free_managers.csv", dir.display())));
    }
    let clubs = if clubs.exists() { Some(Sheet::read(&clubs)?) } else { None };
    let squads_before: Vec<usize> = (0..db.club_count()).map(|ci| db.squad(ci).len()).collect();
    // whole-club moves first (`record_of`): players.csv and the other club columns name the
    // clubs' new ids
    if let Some(sheet) = &clubs {
        apply_club_moves(db, sheet, &mut errors);
    }
    let mut named = HashSet::new();
    if players.exists() {
        named = apply_players(db, &Sheet::read(&players)?, &mut errors);
    }
    if let Some(sheet) = &clubs {
        apply_clubs(db, sheet, &mut errors);
    }
    if history.exists() {
        apply_history(db, &Sheet::read(&history)?, &mut errors);
    }
    if cups.exists() {
        apply_cups(db, &Sheet::read(&cups)?, &mut errors);
    }
    if last_tables.exists() {
        apply_last_tables(db, &Sheet::read(&last_tables)?, &mut errors);
    }
    if free_managers.exists() {
        apply_free_managers(db, &Sheet::read(&free_managers)?, &mut errors);
    }
    // a club that fielded a team must still field one: the match engine's event picker loops for
    // ever on a side with no eligible player (#31: fc27final3's 1-player squads hung careers)
    for (ci, &before) in squads_before.iter().enumerate() {
        let now = db.squad(ci).len();
        if before >= MIN_PLAYERS && now < MIN_PLAYERS && !db.is_placeholder(ci) {
            errors.push(format!("club {ci} ({}): {now} players left, a match needs {MIN_PLAYERS}", db.club_name(ci)));
        }
    }
    if !errors.is_empty() {
        let n = errors.len();
        errors.truncate(50);
        return Err(Error::Value(format!("{n} error(s) in {}:\n{}", dir.display(), errors.join("\n"))));
    }
    let mut notes = Vec::new();
    // a record given another player (a surname) drops its 2006 loan: Havertz on Muamba's record
    // was out on loan (#31)
    let cleared = db.clear_loans(&named)?;
    if cleared > 0 {
        notes.push(format!("{cleared} loan(s) of the records' 2006 players cleared"));
    }
    for (label, pool) in [("forename", &db.forenames), ("surname", &db.surnames)] {
        if pool.grew() {
            notes.push(format!("new {label}s added, pool now {} entries", pool.names.len()));
        }
    }
    Ok(notes)
}

/// Returns the records the sheet gives another player (a new surname).
fn apply_players(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) -> HashSet<usize> {
    let col = |n: &str| sheet.col(n);
    let (id_col, club_col, slot_col) = (col("id"), col("club_id"), col("squad_slot"));
    let mut named = HashSet::new();
    if id_col.is_none() {
        errors.push("players.csv: no id column".into());
        return named;
    }
    let numeric: Vec<(Field, u32, u32, Option<usize>)> = numeric_cols(db.endian()).into_iter().map(|(f, lo, hi)| (f, lo, hi, col(f.name))).collect();
    // squads as (slot, player); untouched members keep their position as the slot
    let mut squads: Vec<Vec<(f64, usize)>> = (0..db.club_count())
        .map(|ci| if db.is_placeholder(ci) { vec![] } else { db.squad(ci).into_iter().enumerate().map(|(k, p)| (k as f64, p)).collect() })
        .collect();
    let mut member_of: HashMap<usize, Vec<usize>> = HashMap::new();
    for (ci, squad) in squads.iter().enumerate() {
        for &(_, pid) in squad {
            member_of.entry(pid).or_default().push(ci);
        }
    }
    let mut touched = BTreeSet::new();
    for row in &sheet.rows {
        let Some(pid) = sheet.get(row, id_col).and_then(|v| v.parse::<usize>().ok()).filter(|&p| p < db.player_count()) else {
            errors.push(format!("players.csv: player id {:?} out of range (0..{})", sheet.get(row, id_col).unwrap_or(""), db.player_count() - 1));
            continue;
        };
        // a record now holding someone else: its surname changed (an unchanged export re-applied
        // keeps every loan, so it still rebuilds byte for byte)
        let surname = db.player(pid).surname;
        if let Err(e) = apply_player(db, sheet, row, pid, &numeric) {
            errors.push(format!("player {pid}: {e}"));
        }
        if db.player(pid).surname != surname {
            named.insert(pid);
        }
        if club_col.is_none() {
            continue;
        }
        let club = match sheet.get(row, club_col).map(|v| v.parse::<usize>()) {
            None => None,
            Some(Ok(ci)) if ci < db.club_count() => Some(ci),
            Some(_) => {
                errors.push(format!("player {pid}: club_id {:?} out of range", sheet.get(row, club_col).unwrap_or("")));
                continue;
            }
        };
        let slot = match sheet.get(row, slot_col).map(str::parse::<f64>) {
            None => 99.0,
            Some(Ok(s)) => s,
            Some(Err(_)) => {
                errors.push(format!("player {pid}: squad_slot is not a number"));
                continue;
            }
        };
        for ci in member_of.remove(&pid).unwrap_or_default() {
            squads[ci].retain(|e| e.1 != pid);
            touched.insert(ci);
        }
        if let Some(ci) = club {
            squads[ci].push((slot, pid));
            member_of.entry(pid).or_default().push(ci);
            touched.insert(ci);
        }
    }
    for ci in touched {
        let squad = &mut squads[ci];
        squad.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
        if squad.len() > SQUAD_MAX {
            errors.push(format!("club {ci} ({}) has {} players, max {SQUAD_MAX}", db.club_names[0][ci], squad.len()));
            continue;
        }
        let ids: Vec<usize> = squad.iter().map(|e| e.1).collect();
        if let Err(e) = db.set_squad(ci, &ids) {
            errors.push(e.to_string());
        }
    }
    named
}

fn apply_player(db: &mut Database, sheet: &Sheet, row: &[String], pid: usize, numeric: &[(Field, u32, u32, Option<usize>)]) -> Result<()> {
    // names are matched exactly (untrimmed) against the pools, like lma_edit.py
    let raw = |n: &str| sheet.col(n).and_then(|c| row.get(c)).filter(|v| !v.trim().is_empty());
    // `-` clears the forename; the game stores single-name players (Gilberto, Lauren) as " "
    let (fore, sur) = (raw("forename").map(|f| if f.trim() == "-" { " " } else { f }), raw("surname"));
    if fore.is_some() || sur.is_some() {
        let p = db.player(pid);
        db.set_name(pid, fore.unwrap_or(&p.forename), sur.unwrap_or(&p.surname))?;
    }
    let pos = |v: &str| ["GK", "DEF", "MID", "ATT"].iter().position(|p| p.eq_ignore_ascii_case(v)).map(|p| p as u32);
    if let Some(v) = sheet.get(row, sheet.col("position")) {
        db.set_field(pid, crate::db::POSITION, pos(v).ok_or_else(|| Error::Value(format!("position {v:?} must be GK/DEF/MID/ATT")))?)?;
    }
    if let Some(v) = sheet.get(row, sheet.col("position2")) {
        let code = if v == "-" { Some(0) } else { pos(v).filter(|&p| p > 0) };
        db.set_field(pid, crate::db::POSITION2, code.ok_or_else(|| Error::Value(format!("position2 {v:?} must be DEF/MID/ATT or -")))?)?;
    }
    for &(f, lo, hi, c) in numeric {
        if let Some(v) = sheet.get(row, c) {
            let n: u32 = v.parse().ok().filter(|n| (lo..=hi).contains(n)).ok_or_else(|| Error::Value(format!("{}={v} must be {lo}-{hi}", f.name)))?;
            db.set_field(pid, f, n)?;
        }
    }
    Ok(())
}

/// clubs.csv `record_of`: the row's id takes the whole club that was at `record_of`
/// (`Database::move_clubs`).
fn apply_club_moves(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let (id_col, of_col) = (sheet.col("id"), sheet.col("record_of"));
    if of_col.is_none() {
        return;
    }
    let mut moves = Vec::new();
    for row in &sheet.rows {
        let Some(of) = sheet.get(row, of_col) else { continue };
        match (sheet.get(row, id_col).and_then(|v| v.parse::<usize>().ok()), of.parse::<usize>()) {
            (Some(to), Ok(from)) => moves.push((to, from)),
            _ => errors.push(format!("clubs.csv: record_of {of:?} for id {:?} must be club ids", sheet.get(row, id_col).unwrap_or(""))),
        }
    }
    if let Err(e) = db.move_clubs(&moves) {
        errors.push(format!("clubs.csv: {e}"));
    }
}

fn apply_clubs(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let id_col = sheet.col("id");
    if id_col.is_none() {
        return errors.push("clubs.csv: no id column".into());
    }
    for row in &sheet.rows {
        let Some(ci) = sheet.get(row, id_col).and_then(|v| v.parse::<usize>().ok()).filter(|&c| c < db.club_count()) else {
            errors.push(format!("clubs.csv: club id {:?} out of range (0..{})", sheet.get(row, id_col).unwrap_or(""), db.club_count() - 1));
            continue;
        };
        if let Err(e) = apply_club(db, sheet, row, ci) {
            errors.push(format!("club {ci}: {e}"));
        }
    }
}

/// The unattached managers (#795): `slot` k is manager record club_count() + k.
fn apply_free_managers(db: &mut Database, sheet: &Sheet, errors: &mut Vec<String>) {
    let slots = db.manager_count() - db.club_count();
    let Some(slot_col) = sheet.col("slot") else { return errors.push("free_managers.csv: no slot column".into()) };
    for row in &sheet.rows {
        let Some(slot) = sheet.get(row, Some(slot_col)).and_then(|v| v.parse::<usize>().ok()).filter(|&s| s < slots) else {
            errors.push(format!("free_managers.csv: slot {:?} out of range (0..{})", sheet.get(row, Some(slot_col)).unwrap_or(""), slots.saturating_sub(1)));
            continue;
        };
        if let Err(e) = apply_free_manager(db, sheet, row, db.club_count() + slot) {
            errors.push(format!("free manager {slot}: {e}"));
        }
    }
}

fn apply_free_manager(db: &mut Database, sheet: &Sheet, row: &[String], mi: usize) -> Result<()> {
    let cell = |n: &str| sheet.get(row, sheet.col(n));
    if let Some(v) = cell("manager") {
        if v != db.manager_surname(mi) {
            db.set_manager_surname(mi, v)?;
        }
    }
    if let Some(v) = cell("manager_rep") {
        db.set_manager_reputation(mi, v.parse().map_err(|_| Error::Value(format!("manager_rep {v:?} must be 0-99")))?)?;
    }
    Ok(())
}

fn apply_club(db: &mut Database, sheet: &Sheet, row: &[String], ci: usize) -> Result<()> {
    let cell = |n: &str| sheet.get(row, sheet.col(n));
    let check = |what: &str, v: &str, max: usize| match crate::text::encode(v) {
        None => Err(Error::Value(format!("{what} {v:?} has characters the game can't show"))),
        Some(b) if b.len() > max => Err(Error::Value(format!("{what} {v:?} is longer than {max} characters"))),
        Some(_) => Ok(v.to_string()),
    };
    for (li, l) in LANGS.iter().enumerate() {
        if let Some(v) = cell(&format!("name_{l}")) {
            db.club_names[li][ci] = check("name", v, 25)?;
        }
        if let Some(v) = cell(&format!("short_{l}")) {
            db.short_names[li][ci] = check("short name", v, 7)?;
        }
    }
    if let Some(v) = cell("wealth") {
        db.set_club_wealth(ci, v.parse().map_err(|_| Error::Value(format!("wealth {v:?} must be 0-15")))?)?;
    }
    if let Some(v) = cell("country") {
        db.set_club_country(ci, v.parse().map_err(|_| Error::Value(format!("country {v:?} must be 0-15")))?)?;
    }
    if let Some(v) = cell("division") {
        db.set_club_division(ci, v.parse().map_err(|_| Error::Value(format!("division {v:?} must be 0-63")))?)?;
    }
    if let Some(v) = cell("manager") {
        if v != db.manager_surname(ci) {
            db.set_manager_surname(ci, v)?;
        }
    }
    if let Some(v) = cell("manager_rep") {
        db.set_manager_reputation(ci, v.parse().map_err(|_| Error::Value(format!("manager_rep {v:?} must be 0-99")))?)?;
    }
    let has_stadium = ci < db.stadium_count();
    if let Some(v) = cell("stadium") {
        if !has_stadium {
            return Err(Error::Value("has no stadium record".into()));
        }
        db.stadium_names[ci] = check("stadium", v, 39)?;
    }
    let caps: Vec<Option<&str>> = STAND_COLS.iter().map(|c| cell(c)).collect();
    if caps.iter().any(Option::is_some) {
        if !has_stadium {
            return Err(Error::Value("has no stadium record".into()));
        }
        let parsed: Option<Vec<u16>> = caps.iter().map(|c| c.and_then(|v| v.parse().ok())).collect();
        let caps = parsed.ok_or_else(|| Error::Value("stand capacities need all four of cap_north/east/south/west, each 0-65535".into()))?;
        db.set_stands(ci, [caps[0], caps[1], caps[2], caps[3]]);
    }
    Ok(())
}

/// Nation ids: 0-50 are the major nations A-Z, 51+ the rest A-Z (identified from each nation's
/// best-known players; same table as lma_edit.py).
const KNOWN_NATIONS: &[(u32, &str)] = &[
    (0, "Algeria"), (1, "Argentina"), (2, "Australia"), (3, "Austria"), (4, "Belgium"), (5, "Brazil"),
    (6, "Bulgaria"), (7, "Chile"), (8, "Colombia"), (9, "Croatia"), (10, "Cyprus"), (11, "Czech Rep."),
    (12, "Denmark"), (13, "Eire"), (14, "England"), (15, "Finland"), (16, "France"), (17, "Germany"),
    (18, "Greece"), (19, "Hungary"), (20, "Iceland"), (21, "Israel"), (22, "Italy"), (23, "Jamaica"),
    (24, "Japan"), (25, "Malta"), (26, "Mexico"), (27, "Holland"), (28, "N. Ireland"), (29, "Norway"),
    (30, "Paraguay"), (31, "Poland"), (32, "Portugal"), (33, "Romania"), (34, "Russia"), (35, "Scotland"),
    (36, "Serbia"), (37, "Singapore"), (38, "Slovakia"), (39, "Slovenia"), (40, "South Africa"),
    (41, "South Korea"), (42, "Spain"), (43, "Sweden"), (44, "Switzerland"), (45, "Tunisia"), (46, "Turkey"),
    (47, "Ukraine"), (48, "Uruguay"), (49, "USA"), (50, "Wales"), (51, "Albania"), (52, "Andorra"),
    (53, "Angola"), (54, "Armenia"), (56, "Azerbaijan"), (58, "Belarus"), (59, "Benin"), (61, "Bolivia"),
    (62, "Bosnia"), (64, "Burkina Faso"), (65, "Burundi"), (66, "Cameroon"), (67, "Canada"), (68, "Cape Verde"),
    (70, "Chad"), (71, "China"), (72, "DR Congo"), (73, "Costa Rica"), (74, "Ivory Coast"), (79, "Ecuador"),
    (80, "Egypt"), (82, "Estonia"), (84, "Gabon"), (85, "Gambia"), (86, "Georgia"), (87, "Ghana"),
    (90, "Guatemala"), (92, "Guinea"), (94, "Honduras"), (97, "Iran"), (98, "Iraq"), (101, "Kenya"),
    (103, "Latvia"), (106, "Liberia"), (108, "Liechtenstein"), (109, "Lithuania"), (110, "Luxembourg"),
    (111, "Macedonia"), (113, "Malawi"), (114, "Mali"), (115, "Mauritius"), (116, "Moldova"), (117, "Morocco"),
    (118, "Mozambique"), (119, "Namibia"), (120, "New Zealand"), (121, "Nigeria"), (122, "North Korea"),
    (123, "Oman"), (124, "Panama"), (126, "Peru"), (128, "Rwanda"), (130, "Senegal"), (131, "Sierra Leone"),
    (137, "Suriname"), (142, "Thailand"), (143, "Togo"), (144, "Trinidad & Tobago"), (146, "Uganda"),
    (148, "Uzbekistan"), (149, "Venezuela"), (150, "Vietnam"), (152, "Zambia"), (153, "Zimbabwe"),
];

/// The PS2's nation ids (#3): its 32 major nations A-Z, then the rest A-Z. Named by matching
/// ~19,500 players with the 360's database (each PS2 id holds one 360 nation's players); the
/// 15 ids no matched player carries are left unnamed.
const KNOWN_NATIONS_PS2: &[(u32, &str)] = &[
    (0, "Argentina"), (1, "Austria"), (2, "Belgium"), (3, "Brazil"), (4, "Bulgaria"), (5, "Chile"),
    (6, "Croatia"), (7, "Cyprus"), (8, "Czech Rep."), (9, "Denmark"), (10, "Eire"), (11, "England"),
    (12, "France"), (13, "Germany"), (14, "Greece"), (15, "Italy"), (16, "Malta"), (17, "Holland"),
    (18, "N. Ireland"), (19, "Norway"), (20, "Poland"), (21, "Portugal"), (22, "Romania"),
    (23, "Russia"), (24, "Scotland"), (25, "Serbia"), (26, "Slovakia"), (27, "Spain"), (28, "Sweden"),
    (29, "Switzerland"), (30, "Turkey"), (31, "Ukraine"), (32, "Albania"), (33, "Algeria"),
    (34, "Andorra"), (35, "Angola"), (36, "Armenia"), (38, "Australia"), (39, "Azerbaijan"),
    (41, "Belarus"), (42, "Benin"), (44, "Bolivia"), (45, "Bosnia"), (47, "Burkina Faso"),
    (48, "Burundi"), (49, "Cameroon"), (50, "Canada"), (51, "Cape Verde"), (53, "Chad"), (54, "China"),
    (55, "Colombia"), (56, "DR Congo"), (57, "Costa Rica"), (58, "Ivory Coast"), (63, "Ecuador"),
    (64, "Egypt"), (66, "Estonia"), (68, "Finland"), (69, "Gabon"), (70, "Gambia"), (71, "Georgia"),
    (72, "Ghana"), (75, "Guatemala"), (77, "Guinea"), (79, "Honduras"), (80, "Hungary"),
    (81, "Iceland"), (84, "Iran"), (85, "Iraq"), (86, "Israel"), (87, "Jamaica"), (88, "Japan"),
    (90, "Kenya"), (92, "Latvia"), (94, "Liberia"), (96, "Liechtenstein"), (97, "Lithuania"),
    (98, "Luxembourg"), (99, "Macedonia"), (101, "Malawi"), (102, "Mali"), (103, "Mauritius"),
    (104, "Mexico"), (105, "Moldova"), (106, "Morocco"), (107, "Mozambique"), (108, "Namibia"),
    (109, "New Zealand"), (110, "Nigeria"), (112, "Oman"), (113, "Panama"), (115, "Paraguay"),
    (116, "Peru"), (118, "Rwanda"), (120, "Senegal"), (121, "Sierra Leone"), (123, "Slovenia"),
    (124, "South Africa"), (125, "South Korea"), (130, "Suriname"), (135, "Thailand"), (136, "Togo"),
    (137, "Trinidad & Tobago"), (138, "Tunisia"), (140, "Uganda"), (142, "Uruguay"), (143, "USA"),
    (144, "Uzbekistan"), (145, "Venezuela"), (146, "Vietnam"), (147, "Wales"), (149, "Zambia"),
    (150, "Zimbabwe"),
];

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::fixtures::{fake_db, fake_db_with};

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("lma-edit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn apply_csv(db: &mut Database, name: &str, files: &[(&str, &str)]) -> Result<Vec<String>> {
        let dir = tmp(name);
        for (f, text) in files {
            std::fs::write(dir.join(f), text).unwrap();
        }
        let r = apply(db, &dir);
        std::fs::remove_dir_all(&dir).unwrap();
        r
    }

    /// The unattached managers: exported after the clubs' own, written by slot, the clubs' own
    /// records untouched; a slot past the end or a bad reputation is an error.
    #[test]
    fn free_managers_by_slot() {
        let mut db = fake_db();
        assert_eq!((db.club_count(), db.manager_count()), (3, 5));
        let clubs: Vec<String> = (0..3).map(|i| db.manager_surname(i).to_string()).collect();
        let before = db.manager_surname(3).to_string();
        apply_csv(&mut db, "free_mgr", &[("free_managers.csv", "slot,manager,manager_rep\n1,Moyes,80\n")]).unwrap();
        assert_eq!((db.manager_surname(4), db.manager_reputation(4)), ("Moyes", Some(80)));
        assert_eq!(db.manager_surname(3), before, "slot 0 not listed: kept");
        assert_eq!((0..3).map(|i| db.manager_surname(i).to_string()).collect::<Vec<_>>(), clubs, "the clubs' managers keep theirs");
        let dir = tmp("free_mgr_export");
        export(&db, &dir).unwrap();
        let csv = std::fs::read_to_string(dir.join("free_managers.csv")).unwrap();
        assert!(csv.lines().nth(2).unwrap().starts_with("1,Moyes,80"), "{csv}");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(apply_csv(&mut db, "free_mgr_far", &[("free_managers.csv", "slot,manager\n2,Moyes\n")]).unwrap_err().to_string().contains("out of range"));
        assert!(apply_csv(&mut db, "free_mgr_rep", &[("free_managers.csv", "slot,manager_rep\n0,100\n")]).is_err());
    }

    #[test]
    fn export_then_apply_is_byte_identical() {
        let mut db = fake_db();
        let orig = db.build().unwrap();
        let dir = tmp("roundtrip");
        export(&db, &dir).unwrap();
        let players = std::fs::read_to_string(dir.join("players.csv")).unwrap();
        assert!(players.starts_with("\u{feff}id,club_id,club,squad_slot,forename,surname,"));
        assert!(players.contains("\r\n1,0,Arsenal,1,Ashley,Cole,0,DEF,,25,180,0,Algeria,"));
        let mut again = Database::parse(&orig).unwrap();
        assert_eq!(apply(&mut again, &dir).unwrap(), Vec::<String>::new());
        assert_eq!(again.build().unwrap(), orig);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// League histories (#31): exported by club id (uids through ClubRmap; a historic club with
    /// no record keeps its bare uid), applied back byte for byte, and a league's rows replaced by
    /// what history.csv names for it; club 0's uid is the one its history uses.
    #[test]
    fn league_history_round_trips_and_applies_by_club() {
        use crate::db::HistoryRow;
        let mut db = fake_db();
        assert_eq!(db.league_history(0, 0), None, "no World.bin");
        // ClubRmap: uid 5800 -> club 0 (and every unused uid too), 5910 -> club 1, 6144 -> club 2,
        // 8003 -> 1200 (a club the database no longer has)
        let mut rmap = vec![0u8; 2 * 8300];
        for (uid, ci) in [(5910usize, 1u16), (6144, 2), (8003, 1200)] {
            rmap[2 * uid..2 * uid + 2].copy_from_slice(&ci.to_be_bytes());
        }
        db.put_file("ClubRmap.bin", rmap);
        let row = |uid: u16, w: u8, pts: u8| HistoryRow { uid, won: w, drawn: 4, lost: 5, goals_for: 72, goals_against: 22, points: pts };
        let mut world = vec![0u8; 0x21b0 + 8 * 0x2aa0];
        for (i, r) in [row(5910, 29, 91), row(5800, 26, 90), HistoryRow::default(), row(8003, 20, 44)].iter().enumerate() {
            let at = 0x21b0 + 8 * i;
            world[at..at + 2].copy_from_slice(&r.uid.to_be_bytes());
            world[at + 2..at + 8].copy_from_slice(&[r.won, r.drawn, r.lost, r.goals_for, r.goals_against, r.points]);
        }
        // and the loan table World.bin ends with (empty), as on the disc
        for _ in 0..700 {
            world.extend([0xFF, 0xFF, 0, 0, 0, 0, 0, 0]);
        }
        world.extend([0u8; 32]);
        db.put_file("World.bin", world);
        assert_eq!(db.uid_of_club(0), Some(5800), "club 0: the uid its history uses");
        assert_eq!((db.club_of_uid(5910), db.club_of_uid(8003)), (Some(1), None));
        let dir = tmp("history_export");
        export(&db, &dir).unwrap();
        let csv = std::fs::read_to_string(dir.join("history.csv")).unwrap();
        assert!(csv.contains("0,0,0,1,Everton,5910,29,4,5,72,22,91\r\n") && csv.contains("0,0,3,,,8003,20,4,5,72,22,44\r\n"), "{csv}");
        let before = db.build().unwrap();
        let mut again = Database::parse(&before).unwrap();
        apply(&mut again, &dir).unwrap();
        assert_eq!(again.build().unwrap(), before, "export then apply: byte-identical");
        std::fs::remove_dir_all(&dir).unwrap();
        // a new season on top by club id; the league keeps only the rows the file names
        apply_csv(&mut db, "history", &[("history.csv", "country,tier,row,club_id,uid,won,drawn,lost,for,against,points\n0,0,0,2,,28,7,3,97,45,91\n0,0,1,1,,29,4,5,72,22,91\n")]).unwrap();
        let h = db.league_history(0, 0).unwrap();
        assert_eq!((h[0].uid, h[0].won, h[1].uid, h[2].uid, h[3].uid), (6144, 28, 5910, 0, 0));
        assert!(apply_csv(&mut db, "history_bad", &[("history.csv", "country,tier,row,club_id,uid,won,drawn,lost,for,against,points\n0,0,0,,,1,1,1,1,1,1\n")]).is_err(), "no club");
    }

    /// Last season's final tables (#607): exported by club id, applied back byte for byte, and a
    /// league's table replaced by one naming each position once; a short table is refused.
    #[test]
    fn last_tables_round_trip_and_apply_by_club() {
        use crate::db::{LAST_TABLE_AT, LAST_TABLE_SIZES};
        let mut db = fake_db();
        assert_eq!(db.last_table(0, 0), None, "no World.bin");
        let mut rmap = vec![0u8; 2 * 8300];
        for (uid, ci) in [(5910usize, 1u16), (6144, 2), (8003, 1200)] {
            rmap[2 * uid..2 * uid + 2].copy_from_slice(&ci.to_be_bytes());
        }
        db.put_file("ClubRmap.bin", rmap);
        let total: usize = LAST_TABLE_SIZES.iter().flat_map(|t| t.iter()).sum();
        assert_eq!(total, 445);
        let mut world = vec![0u8; LAST_TABLE_AT + 2 * total + 0x100];
        // the Premiership: Everton (5910) champions, Arsenal (club 0, 5800) second, a gone club third
        for (k, uid) in [5910u16, 5800, 8003].into_iter().enumerate() {
            world[LAST_TABLE_AT + 2 * k..LAST_TABLE_AT + 2 * k + 2].copy_from_slice(&uid.to_be_bytes());
        }
        // Scotland's top tier starts after England's 116
        world[LAST_TABLE_AT + 2 * 116..LAST_TABLE_AT + 2 * 116 + 2].copy_from_slice(&6144u16.to_be_bytes());
        db.put_file("World.bin", world);
        assert_eq!(db.last_table(1, 0).unwrap()[0], 6144);
        let dir = tmp("last_tables_export");
        export(&db, &dir).unwrap();
        let csv = std::fs::read_to_string(dir.join("last_tables.csv")).unwrap();
        assert!(csv.contains("0,0,1,1,Everton,5910\r\n") && csv.contains("0,0,3,,,8003\r\n") && csv.contains("1,0,1,2,"), "{csv}");
        let before = db.build().unwrap();
        let mut again = Database::parse(&before).unwrap();
        apply(&mut again, &dir).unwrap();
        assert_eq!(again.build().unwrap(), before, "export then apply: byte-identical");
        std::fs::remove_dir_all(&dir).unwrap();
        // a new Premiership table: Arsenal champions, Everton second, the rest by uid
        let mut sheet = String::from("country,tier,position,club_id,uid\n0,0,1,0,5800\n0,0,2,1,\n");
        for k in 3..=20 {
            sheet.push_str(&format!("0,0,{k},,{}\n", 7000 + k));
        }
        apply_csv(&mut db, "last_tables", &[("last_tables.csv", &sheet)]).unwrap();
        let t = db.last_table(0, 0).unwrap();
        assert_eq!((t[0], t[1], t[2], t[19]), (5800, 5910, 7003, 7020));
        assert!(apply_csv(&mut db, "last_tables_short", &[("last_tables.csv", "country,tier,position,club_id,uid\n0,0,1,0,\n")]).is_err(), "19 positions missing");
    }

    /// Cup histories (the maintainer, 12 Oct): the finals export by club id, apply back byte for byte, and a
    /// cup's rows are replaced by what cups.csv names for it; an unknown cup is refused.
    #[test]
    fn cup_history_round_trips_and_applies_by_club() {
        use crate::db::{CupRow, CUPS};
        let mut db = fake_db();
        assert_eq!(db.cup_history(0), None, "no World.bin");
        let mut rmap = vec![0u8; 2 * 8300];
        for (uid, ci) in [(5910usize, 1u16), (6144, 2), (8003, 1200)] {
            rmap[2 * uid..2 * uid + 2].copy_from_slice(&ci.to_be_bytes());
        }
        db.put_file("ClubRmap.bin", rmap);
        let mut world = vec![0u8; 0x21b0 + 8 * 0x2aa0];
        // the European Cup's newest final (Everton 2-1 Arsenal), the FA Cup's (a club the database lost)
        world[0x70..0x78].copy_from_slice(&[0x17, 0x16, 0x16, 0xa8, 2, 1, 0, 0]);
        world[0xef0..0xef8].copy_from_slice(&[0x1f, 0x43, 0x17, 0x16, 3, 3, 0, 0]);
        for _ in 0..700 {
            world.extend([0xFF, 0xFF, 0, 0, 0, 0, 0, 0]);
        }
        world.extend([0u8; 32]);
        db.put_file("World.bin", world);
        assert_eq!(db.cup_history(0).unwrap()[0], CupRow { winner: 5910, runner_up: 5800, winner_goals: 2, runner_up_goals: 1 });
        let dir = tmp("cups_export");
        export(&db, &dir).unwrap();
        let csv = std::fs::read_to_string(dir.join("cups.csv")).unwrap();
        assert!(csv.contains("european_cup,0,1,Everton,5910,0,Arsenal,5800,2,1\r\n") && csv.contains("fa_cup,0,,,8003,1,Everton,5910,3,3\r\n"), "{csv}");
        let before = db.build().unwrap();
        let mut again = Database::parse(&before).unwrap();
        apply(&mut again, &dir).unwrap();
        assert_eq!(again.build().unwrap(), before, "export then apply: byte-identical");
        std::fs::remove_dir_all(&dir).unwrap();
        // a new final on top by club id, the old one a season down; the FA Cup untouched
        let cols = CUP_COLS.join(",");
        apply_csv(&mut db, "cups", &[("cups.csv", &format!("{cols}\neuropean_cup,0,2,,,1,,,1,0\neuropean_cup,1,1,,,0,,,2,1\n"))]).unwrap();
        let h = db.cup_history(0).unwrap();
        assert_eq!((h[0].winner, h[0].runner_up, h[1].winner, h[2].winner), (6144, 5910, 5910, 0));
        assert_eq!(db.cup_history(3).unwrap()[0].winner, 8003);
        assert_eq!(CUPS[3].0, "fa_cup");
        assert!(apply_csv(&mut db, "cups_bad", &[("cups.csv", &format!("{cols}\nworld_cup,0,1,,,0,,,1,0\n"))]).is_err(), "unknown cup");
    }

    /// World.bin's loan table (#31): a record given another player (a surname) loses its loan,
    /// the rest stay in order with the free slots after them; a record only re-rated keeps it.
    #[test]
    fn a_renamed_record_loses_its_loan() {
        let mut db = fake_db();
        assert_eq!(db.loans().unwrap(), None, "no World.bin: nothing to do");
        let slot = |pid: u16, x: u16| [pid.to_be_bytes(), x.to_be_bytes(), 0xB680u16.to_be_bytes(), [0, 0]].concat();
        let mut world = vec![7u8; 16]; // whatever comes before the table
        for (pid, x) in [(1, 5895), (2, 5851), (3, 6240)] {
            world.extend(slot(pid, x));
        }
        while world.len() < 16 + 700 * 8 {
            world.extend([0xFF, 0xFF, 0, 0, 0, 0, 0, 0]);
        }
        world.extend([9u8; 32]); // and the 32 bytes after the table, left alone
        db.put_file("World.bin", world);
        assert_eq!(db.loans().unwrap(), Some(vec![1, 2, 3]));
        let notes = apply_csv(&mut db, "loans", &[("players.csv", "id,surname,tackling\n2,Havertz,50\n3,,60\n1,Cole,40\n")]).unwrap();
        assert_eq!(notes.iter().filter(|n| n.contains("loan")).count(), 1, "{notes:?}");
        assert_eq!(db.loans().unwrap(), Some(vec![1, 3]), "2 renamed; 3 only re-rated; 1 named as it already was");
        assert!(db.build().is_ok());
        // a table out of order is no table: nothing is read or rewritten
        let mut world = vec![0u8; 700 * 8 + 32];
        world[..8].copy_from_slice(&slot(3, 1));
        world[8..16].copy_from_slice(&slot(1, 1));
        db.put_file("World.bin", world.clone());
        assert_eq!(db.loans().unwrap(), None);
        assert_eq!(db.clear_loans(&[1usize, 3].into_iter().collect()).unwrap(), 0);
        // the table at the very end (ExportPR.mng's World.bin, 32 bytes shorter)
        let mut world = vec![7u8; 16];
        world.extend(slot(2, 9));
        while world.len() < 16 + 700 * 8 {
            world.extend([0xFF, 0xFF, 0, 0, 0, 0, 0, 0]);
        }
        db.put_file("World.bin", world);
        assert_eq!(db.loans().unwrap(), Some(vec![2]));
    }

    /// The face (#697) is bits 275-278 with `skin` its middle two: a `face` cell sets all four and
    /// wins over a `skin` cell in the same row; `skin` alone keeps the outer bits.
    #[test]
    fn face_holds_skin_as_its_middle_bits() {
        let mut db = fake_db();
        apply_csv(&mut db, "face_skin", &[("players.csv", "id,face
1,9
")]).unwrap();
        assert_eq!((db.field(1, crate::db::FACE), db.field(1, SKIN)), (9, 0), "ori1 = 1001: skin bits 00");
        apply_csv(&mut db, "face_skin2", &[("players.csv", "id,skin
1,2
")]).unwrap();
        assert_eq!(db.field(1, crate::db::FACE), 13, "skin alone keeps bits 275 and 278: 1 10 1 = med2");
        apply_csv(&mut db, "face_skin3", &[("players.csv", "id,skin,face
1,0,4
")]).unwrap();
        assert_eq!((db.field(1, crate::db::FACE), db.field(1, SKIN)), (4, 2), "face wins: blk2 = 0 10 0");
        assert!(apply_csv(&mut db, "face_bad", &[("players.csv", "id,face
1,16
")]).is_err());
        for set in crate::db::FACE_SETS {
            for id in set {
                assert!(id < 16);
            }
        }
    }

    /// skin, value, sides and foot (#31 items 5, 1 and 3): exported as columns on the 360, set by
    /// a sparse edit, range checked.
    #[test]
    fn skin_is_exported_and_applied() {
        let mut db = fake_db();
        assert_eq!(apply_csv(&mut db, "skin", &[("players.csv", "id,skin,value,sides,foot
1,2,45000,6,2
")]).unwrap(), Vec::<String>::new());
        assert_eq!((db.field(1, SKIN), db.field(1, VALUE), db.field(1, SIDES), db.field(1, FOOT)), (2, 45000, 6, 2));
        let dir = tmp("skin_export");
        export(&db, &dir).unwrap();
        let players = std::fs::read_to_string(dir.join("players.csv")).unwrap();
        assert!(players.lines().next().unwrap().ends_with(",skin,face,value,sides,foot,hair,hair_colour"), "{players}");
        // face 4 = skin 2 with the outer bits 0 (blk2)
        assert!(players.contains(",2,4,45000,6,2,0,0\r\n"), "{players}");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(apply_csv(&mut db, "skin_bad", &[("players.csv", "id,skin
1,4
")]).is_err());
    }

    #[test]
    fn sparse_player_edit_moves_and_renames() {
        let mut db = fake_db();
        let notes = apply_csv(&mut db, "sparse", &[("players.csv", "id,club_id,squad_slot,forename,surname,pace\n2,0,0.5,Bukayo,Saka,99\n")]).unwrap();
        assert_eq!(notes.len(), 2, "both name pools grew: {notes:?}");
        let db = Database::parse(&db.build().unwrap()).unwrap();
        assert_eq!(db.squad(0), [0, 2, 1]);
        assert_eq!(db.squad(1), Vec::<usize>::new());
        let p = db.player(2);
        assert_eq!((p.display_name().as_str(), p.ratings[11], p.age), ("Bukayo Saka", 99, 25));
        assert_eq!(db.player(0).display_name(), "Thierry Henry", "unlisted players are untouched");
    }

    #[test]
    fn dash_clears_the_forename() {
        let mut db = fake_db();
        apply_csv(&mut db, "dash", &[("players.csv", "id,forename,surname\n0,-,Henry\n1,,Cole\n")]).unwrap();
        assert_eq!((db.player(0).forename.as_str(), db.player(0).display_name().as_str()), (" ", "Henry"));
        assert_eq!(db.player(1).forename, "Ashley", "blank keeps the forename");
    }

    #[test]
    fn blank_club_makes_a_free_agent_and_missing_column_keeps_the_club() {
        let mut db = fake_db();
        apply_csv(&mut db, "free", &[("players.csv", "id,club_id,age\n0,,30\n1,0,\n")]).unwrap();
        assert_eq!(db.squad(0), [1]);
        assert_eq!(db.player(0).age, 30);
        apply_csv(&mut db, "keep", &[("players.csv", "id,age\n1,31\n")]).unwrap();
        assert_eq!(db.squad(0), [1]);
    }

    #[test]
    fn sparse_club_edit() {
        let mut db = fake_db();
        apply_csv(&mut db, "club", &[("clubs.csv", "id,name_E,wealth,manager,manager_rep,cap_north,cap_east,cap_south,cap_west\n1,Everton FC,9,Moyes,57,1,2,3,4\n")]).unwrap();
        let db = Database::parse(&db.build().unwrap()).unwrap();
        assert_eq!((db.club_name(1), db.club_names[1][1].as_str()), ("Everton FC", "Everton"));
        assert_eq!((db.club_wealth(1), db.manager_surname(1), db.manager_reputation(1)), (9, "Moyes", Some(57)));
        assert!(apply_csv(&mut fake_db(), "club", &[("clubs.csv", "id,manager_rep\n1,100\n")]).is_err(), "0-99");
        assert_eq!(db.stands(1), Some([1, 2, 3, 4]));
        assert_eq!(db.club_name(0), "Arsenal");
    }

    /// A full name pool (#31: the surname id is 15 bits) reuses the slots nothing uses, and
    /// refuses a new name only when every slot is in use (by a player or a manager).
    #[test]
    fn a_full_name_pool_reuses_unused_slots() {
        let mut db = fake_db();
        let n = db.surnames.names.len();
        db.surnames.capacity = n;
        // every surname is someone's (Wenger and Moyes are managers'): a new one is refused
        let err = apply_csv(&mut db, "pool", &[("players.csv", "id,surname\n3,Zidane\n")]).unwrap_err();
        assert!(err.to_string().contains("name pool full"), "{err}");
        // player 3 takes an existing name, so his old one ("Free") is nobody's; the new name takes its slot
        let mut db = fake_db();
        db.surnames.capacity = n;
        let managers: Vec<String> = (0..3).map(|i| db.manager_surname(i).to_string()).collect();
        apply_csv(&mut db, "pool2", &[("players.csv", "id,surname\n3,Cole\n")]).unwrap();
        apply_csv(&mut db, "pool3", &[("players.csv", "id,surname\n2,Zidane\n")]).unwrap();
        assert_eq!((db.surnames.names.len(), db.surnames.names[4].as_str(), db.player(2).surname.as_str()), (n, "Zidane", "Zidane"));
        assert_eq!((db.player(0).surname.as_str(), db.player(1).surname.as_str()), ("Henry", "Cole"));
        assert_eq!((0..3).map(|i| db.manager_surname(i).to_string()).collect::<Vec<_>>(), managers, "managers keep theirs");
    }

    /// record_of (#31 item 7): whole clubs change ids (record, squad, names); players.csv and the
    /// other club columns then name the new ids; a move that isn't a permutation is refused.
    #[test]
    fn record_of_moves_whole_clubs() {
        let mut db = fake_db();
        db.set_club_wealth(0, 15).unwrap();
        let squad0 = db.squad(0);
        let csvs = [("clubs.csv", "id,record_of,short_E\n0,1,\n1,0,ARX\n"), ("players.csv", "id,club_id,squad_slot\n3,1,9\n")];
        assert_eq!(apply_csv(&mut db, "record_of", &csvs).unwrap(), Vec::<String>::new());
        assert_eq!((db.club_name(0), db.club_name(1), db.short_names[0][1].as_str()), ("Everton", "Arsenal", "ARX"));
        assert_eq!(db.club_wealth(1), 15, "the record moved");
        assert_eq!((db.stadium_names[0].as_str(), db.stadium_names[1].as_str()), ("Goodison Park", "Highbury"), "and its stadium");
        let managers: Vec<String> = (0..2).map(|i| Database::parse(&fake_db().build().unwrap()).unwrap().manager_surname(i).to_string()).collect();
        assert_eq!((db.manager_surname(0), db.manager_surname(1)), (managers[1].as_str(), managers[0].as_str()), "and its manager");
        assert_eq!(db.squad(1)[..squad0.len()], squad0[..], "and its squad");
        assert!(db.squad(1).contains(&3), "players.csv names the new id");
        let err = apply_csv(&mut db, "record_of_bad", &[("clubs.csv", "id,record_of\n0,1\n")]).unwrap_err();
        assert!(err.to_string().contains("permutation"), "{err}");
    }

    /// country and division (#31 item 7): exported, applied, and the division's low two bits
    /// (unknown flags) survive a division change.
    #[test]
    fn club_country_and_division_round_trip() {
        let mut db = fake_db();
        db.set_club_division(1, 21).unwrap();
        assert_eq!(db.club_division(1), 21);
        let dir = tmp("country_division");
        export(&db, &dir).unwrap();
        let clubs = std::fs::read_to_string(dir.join("clubs.csv")).unwrap();
        assert!(clubs.lines().next().unwrap().contains(",wealth,country,division,manager,"), "{clubs}");
        assert!(clubs.contains("\r\n1,Everton,"), "{clubs}");
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(apply_csv(&mut db, "country_division", &[("clubs.csv", "id,country,division\n1,1,3\n")]).unwrap(), Vec::<String>::new());
        assert_eq!((db.club_country(1), db.club_division(1)), (1, 3));
        let err = apply_csv(&mut db, "country_division_bad", &[("clubs.csv", "id,country,division\n1,16,3\n")]).unwrap_err();
        assert!(err.to_string().contains("country 16 must be 0-15"), "{err}");
        assert!(db.set_club_division(0, 64).is_err() && db.set_club_country(0, 16).is_err());
    }

    #[test]
    fn errors_are_collected() {
        let mut db = fake_db();
        let e = apply_csv(&mut db, "err", &[
            ("players.csv", "id,club_id,pace,position\n9,,50,GK\n0,7,50,GK\n1,0,101,GK\n2,0,50,KEEPER\n"),
            ("clubs.csv", "id,name_E,wealth,cap_north\n0,A name that is far too long for the game,16,5\n2,,,5\n"),
        ]).unwrap_err().to_string();
        for want in ["player id \"9\" out of range", "club_id \"7\" out of range", "pace=101 must be 0-100", "position \"KEEPER\"", "longer than 25", "club 2: has no stadium record"] {
            assert!(e.contains(want), "missing {want:?} in:\n{e}");
        }
        assert!(e.starts_with("6 error(s)"), "{e}");
    }

    #[test]
    fn listing_a_player_twice_keeps_one_squad_entry() {
        let mut db = fake_db();
        apply_csv(&mut db, "twice", &[("players.csv", "id,club_id,squad_slot\n3,0,\n3,0,\n3,1,\n")]).unwrap();
        assert_eq!((db.squad(0), db.squad(1)), (vec![0, 1], vec![2, 3]));
    }

    /// A club that fielded a team keeps at least 11 players through an apply (#464): the match
    /// engine's event picker loops for ever on a side with no eligible player.
    #[test]
    fn an_apply_never_leaves_a_fielded_club_under_11() {
        let mut db = fake_db_with(14);
        let eleven: Vec<usize> = [0, 1].into_iter().chain(3..12).collect(); // player 2 stays Everton's
        db.set_squad(0, &eleven).unwrap();
        let err = apply_csv(&mut db, "short", &[("players.csv", "id,club_id\n11,\n")]).unwrap_err().to_string();
        assert!(err.contains("club 0 (Arsenal): 10 players left, a match needs 11"), "{err}");
        // a club below 11 before the apply (Everton's one player) may shrink: it never fielded a team
        let mut db = fake_db_with(14);
        db.set_squad(0, &eleven).unwrap();
        apply_csv(&mut db, "shrink", &[("players.csv", "id,club_id\n2,\n")]).unwrap();
        assert_eq!((db.squad(0).len(), db.squad(1).len()), (11, 0));
    }

    #[test]
    fn a_folder_without_csvs_is_an_error() {
        assert!(apply_csv(&mut fake_db(), "none", &[]).is_err());
    }
}
