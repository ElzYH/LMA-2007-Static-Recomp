//! Squad importer (#20): current real-world squads -> an LMA mod space.
//!
//! Inputs are a `lma-cli db-export` of the base database (players/clubs/nations CSVs), a
//! roster CSV (EA FC 27 format today; FM26 via the FM database reader later) and a mapping
//! config (TOML). The output is a mod space: `mod.toml` plus a sparse `database/players.csv`
//! that `runtime::mods` applies on top of the disc database at start-up.
//!
//! Nothing here contains game or roster data; all of it is read from the user's files and the
//! result stays in `mods/` (git-ignored).

pub mod divisions;

use lma_data::csv::{write_row, Sheet};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

pub type Result<T> = std::result::Result<T, String>;

/// The 17 LMA ratings, in `players.csv` column names.
pub const LMA_RATINGS: [&str; 17] = [
    "tackling", "marking", "passing", "technique", "dribbling", "shot_power", "shot_accuracy", "heading", "positioning", "runs",
    "creativity", "pace", "intelligence", "temperament", "stamina", "strength", "fitness",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Ages are computed on this date (start of the season being imported), YYYY-MM-DD.
    pub season_start: String,
    /// The season the game starts in, written to the mod's mod.toml (`runtime::mods`: dates,
    /// labels and contract years count from it); none keeps the game's 2006.
    #[serde(default)]
    pub base_year: Option<u32>,
    /// GBP per unit of the player record's stored value, written to the mod's mod.toml
    /// (`runtime::mods` makes the game's valuation use it); none keeps the game's 1000, which
    /// caps a value at GBP 65.5m (the maintainer, 11 Oct: Transfermarkt-level values, Yamal EUR 200m+).
    #[serde(default)]
    pub value_unit: Option<u32>,
    /// Factor on the clubs' gate receipts, sponsorship and prize money, written to mod.toml
    /// (`runtime::mods`, hook on the game's balance adder).
    #[serde(default)]
    pub income_scale: Option<f64>,
    /// A player with a stored value is worth exactly that (`runtime::mods`, hooks on the game's
    /// valuation), written to mod.toml.
    #[serde(default)]
    pub values_as_stored: bool,
    /// The share of a club's funds the board lends for transfers (vanilla 0.65), written to
    /// mod.toml (`runtime::mods`, #522).
    #[serde(default)]
    pub board_share: Option<f64>,
    /// Each club's least weekly wage budget is its real squad bill (wages.csv) times this, written
    /// as the mod's wage_budgets.csv (`runtime::mods`): with real wages the game's computed budget
    /// left ~GBP 100k of room for offers (the maintainer, 11 Oct).
    #[serde(default)]
    pub wage_headroom: Option<f64>,
    /// The clubs' weekly wage budget cap in GBP (vanilla 3.2m), written to mod.toml.
    #[serde(default)]
    pub wage_budget_cap: Option<u32>,
    /// Three-letter codes for every club's short name (the match scorebug showed the 2006 ones,
    /// "Ars'nl", "Ch'sea"): a club named here (its English name in the game) gets this code, any
    /// other the first three letters of its name's first word that isn't a prefix ("FC", "AC",
    /// ...). Empty: the short names stay the base's.
    #[serde(default)]
    pub club_codes: BTreeMap<String, String>,
    /// Game strings by their `LMA_?.lng` index, for the mod's strings.csv (merged into the one
    /// there): `"396" = "Champions League"` in every language, `"30:E"` in English only. Today's
    /// competition names (the maintainer, 12 Oct).
    #[serde(default)]
    pub strings: BTreeMap<String, String>,
    /// The contract screen's most a club may offer a week, by club status (best first; vanilla
    /// 100,000 / 50,000 / 25,000 / 10,000 / 10,000 / 5,000, the table at 0x8200af9c): written to
    /// mod.toml as six [[patch]] words. At most 524,287 (the offered wage is a 19-bit field).
    #[serde(default)]
    pub contract_wage_ceilings: Option<[u32; 6]>,
    #[serde(default = "default_max_squad")]
    pub max_squad: usize,
    /// Squad size per imported club: its current LMA squad size, at least `min_squad` while
    /// spare player records last (LMA has a fixed number of player records).
    #[serde(default = "default_min_squad")]
    pub min_squad: usize,
    /// Each imported club keeps its real squad (up to `max_squad`), taking spare player records
    /// beyond its current LMA squad size while they last, the clubs in record order (the maintainer, 11 Oct:
    /// "teams like Chelsea" carry more players than their 2006 squads).
    #[serde(default)]
    pub real_squads: bool,
    /// With `real_squads`, players beyond `max_squad` (the club record's 32 ids) are kept too, in
    /// spare records outside the squad array, listed in the mod's squad_overflow.csv: the runtime
    /// tells the game they are the club's (`runtime::hooks::player_club`, #538).
    #[serde(default)]
    pub squad_overflow: bool,

    /// Players a live Transfermarkt squad (`--tm-live`) lists at one of our clubs but the roster
    /// lacks are added from Transfermarkt alone: name, birth date, value, position, citizenship,
    /// and an overall from the roster's players of that line and a similar value (the maintainer, 12 Oct:
    /// "add some players not in the game like Max Dowman").
    #[serde(default)]
    pub live_add_missing: bool,
    /// `live_add_missing` adds only players the live squad values at this many EUR or more (spare
    /// player records are few: every one added is a free agent less).
    #[serde(default)]
    pub live_add_min_value_eur: f64,
    /// Roster CSV column names.
    pub columns: Columns,
    /// LMA rating -> source columns averaged (outfield players / goalkeepers), or a constant.
    pub ratings: BTreeMap<String, Source>,
    pub gk_ratings: BTreeMap<String, Source>,
    /// lma = clamp(round(value * mul + add), 1, 99)
    #[serde(default)]
    pub scale: Scale,
    /// Source position label -> GK / DEF / MID / ATT, or "main/second" ("MID/ATT": vanilla files
    /// wingers like Robben and Giggs as midfielders who also play up front).
    pub positions: BTreeMap<String, String>,
    /// Source position label -> the sides it is played on, as letters ("L", "R", "C"; "LRC" for a
    /// goalkeeper, as vanilla's are): a player's `sides` are those of his position and alternates
    /// (#31 item 3: Saka RW + RM -> right, not just "a midfielder").
    #[serde(default)]
    pub sides: BTreeMap<String, String>,
    /// A weak foot rated this or more counts as both feet (vanilla: 5% of players).
    #[serde(default = "default_both_feet_at")]
    pub both_feet_at: u32,
    /// Source club name -> LMA club name, for names normalisation can't match; "" for a club LMA
    /// doesn't have, so no word match lands it on a namesake; "#id" for one of two LMA clubs of
    /// the same name (the 2006 A-League's Newcastle United Jets is "#880", not England's).
    #[serde(default)]
    pub club_aliases: BTreeMap<String, String>,
    /// Source nationality -> LMA nation name.
    #[serde(default)]
    pub nation_aliases: BTreeMap<String, String>,
    /// Word replacements tried during club matching ("utd" -> "united").
    #[serde(default)]
    pub abbreviations: BTreeMap<String, String>,
    /// "club|stadium" as the database has them (the record's 2006 club: two clubs can share a
    /// ground's name, Doncaster's and Rhyl's Belle Vue) -> today's name, for grounds since
    /// replaced or renamed for good (Colchester United|Layer Road -> Colchester Community
    /// Stadium), applied to clubs Transfermarkt gives no stadium (with `--tm-clubs`); the stands
    /// keep their capacities.
    #[serde(default)]
    pub stadium_renames: BTreeMap<String, String>,
    /// Only import these source leagues (empty = all).
    #[serde(default)]
    pub leagues: Vec<String>,
    /// Only rows where `filter_column` equals `filter_value` (e.g. gender = "Men's Football").
    #[serde(default)]
    pub filter_column: Option<String>,
    #[serde(default)]
    pub filter_value: Option<String>,
    /// Attribute source merged over the roster (e.g. an FM26 export from `fmsave`): players
    /// found there by birth date + surname take its ratings and height.
    #[serde(default)]
    pub fm: Option<FmConfig>,
    /// Market values and positions (a Transfermarkt players export): a matched player's overall
    /// level comes from his value's rank in his position group, mapped onto vanilla's spread of
    /// levels (`level_remap`), and his position from the sub-position (#31, the maintainer 10 Oct).
    #[serde(default)]
    pub transfermarkt: Option<TmConfig>,
    /// League ladders re-seated from the roster's `league` column (#31 item 7); none = every club
    /// keeps its division.
    #[serde(default)]
    pub ladders: Vec<divisions::Ladder>,
    /// (country, division) of the pool clubs leave a ladder for.
    #[serde(default = "default_overflow")]
    pub overflow: (u32, u32),
}

/// One 32-bit word of the loaded image (`runtime::mods`'s mod.toml `[[patch]]`, e.g. the
/// formation slot roles). The repo holds no addresses: `squad-import --patches <toml>` copies them
/// from a local file, such as an earlier mod's mod.toml.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModPatch {
    pub addr: String,
    pub from: u32,
    pub to: u32,
}

/// The `[[patch]]` tables of a TOML file (its other keys are ignored).
pub fn mod_patches(toml_text: &str) -> std::result::Result<Vec<ModPatch>, String> {
    #[derive(Deserialize)]
    struct File {
        #[serde(default)]
        patch: Vec<ModPatch>,
    }
    toml::from_str::<File>(toml_text).map(|f| f.patch).map_err(|e| e.to_string())
}

/// wage_budgets.csv from the import's players.csv (`id`, `club_id`) and wages.csv (`id`, `wage`):
/// per club, its squad's weekly bill and that times `headroom`, biggest bills first. Players kept
/// beyond a club's 32 (`overflow`: player record, club id) have no club in players.csv but are paid
/// by it, so they count too (#546).
pub fn wage_budgets_csv(players: &str, wages: &str, headroom: f64, overflow: &[(u32, u32)]) -> std::result::Result<String, String> {
    let p = lma_data::csv::Sheet::parse(players).map_err(|e| format!("players.csv: {e}"))?;
    let w = lma_data::csv::Sheet::parse(wages).map_err(|e| format!("wages.csv: {e}"))?;
    let (pid, club) = (p.col("id"), p.col("club_id"));
    let (wid, wage) = (w.col("id"), w.col("wage"));
    let club_of: HashMap<&str, &str> = p.rows.iter().filter_map(|r| Some((p.get(r, pid)?, p.get(r, club)?))).filter(|(_, c)| *c != "-1").collect();
    let overflow_of: HashMap<String, u32> = overflow.iter().map(|&(p, c)| (p.to_string(), c)).collect();
    let mut bill: HashMap<u32, u64> = HashMap::new();
    for r in &w.rows {
        let (Some(id), Some(v)) = (w.get(r, wid), w.get(r, wage).and_then(|v| v.parse::<u64>().ok())) else { continue };
        if let Some(c) = club_of.get(id).and_then(|c| c.parse::<u32>().ok()).or_else(|| overflow_of.get(id).copied()) {
            *bill.entry(c).or_default() += v;
        }
    }
    let mut rows: Vec<(u32, u64)> = bill.into_iter().collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut s = String::from("id,bill_gbp_week,budget_gbp_week\n");
    for (c, b) in rows {
        s += &format!("{c},{b},{}\n", (b as f64 * headroom).round() as u64);
    }
    Ok(s)
}

/// squad_overflow.csv (`runtime::mods`): `id,club_id` for each player kept beyond his club's 32.
pub fn squad_overflow_csv(rows: &[(u32, u32)]) -> String {
    let mut s = String::from("id,club_id\n");
    for (p, c) in rows {
        s += &format!("{p},{c}\n");
    }
    s
}

/// The mod's strings.csv: `existing` (rows written another way, e.g. the sponsors' brands) with
/// the mapping's `[strings]` replacing theirs (same id and language) or added; None with no rows.
pub fn strings_csv(existing: Option<&str>, strings: &BTreeMap<String, String>) -> Result<Option<String>> {
    let mut rows: Vec<(String, String, String)> = Vec::new(); // id, text, lang
    if let Some(t) = existing {
        let s = Sheet::parse(t).map_err(|e| format!("strings.csv: {e}"))?;
        let (id, text, lang) = (s.col("id"), s.col("text"), s.col("lang"));
        for r in &s.rows {
            let text = text.and_then(|c| r.get(c)).cloned().unwrap_or_default();
            rows.push((s.get(r, id).unwrap_or("").to_string(), text, s.get(r, lang).unwrap_or("").to_uppercase()));
        }
    }
    for (key, text) in strings {
        let (id, lang) = key.split_once(':').unwrap_or((key, ""));
        let (id, lang) = (id.trim(), lang.trim().to_uppercase());
        id.parse::<u32>().map_err(|_| format!("[strings] {key:?}: not an id or id:language"))?;
        if !matches!(lang.as_str(), "" | "E" | "F" | "G" | "I" | "S") {
            return Err(format!("[strings] {key:?}: the languages are E, F, G, I and S"));
        }
        rows.retain(|(i, _, l)| !(i == id && *l == lang));
        rows.push((id.to_string(), text.clone(), lang));
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let mut out = String::new();
    write_row(&mut out, &["id", "text", "lang"]);
    for (i, t, l) in &rows {
        write_row(&mut out, &[i, t, l]);
    }
    Ok(Some(out))
}

/// A mod's mod.toml: name, version, base, the mapping's `base_year` and the patches.
pub fn mod_toml(name: &str, cfg: &Config, patches: &[ModPatch]) -> String {
    let mut s = format!("name = \"{name}\"\nversion = \"{}\"\nbase = \"Export.mng\"\n", cfg.season_start);
    if let Some(y) = cfg.base_year {
        s += &format!("base_year = {y}\n");
    }
    if let Some(u) = cfg.value_unit {
        s += &format!("value_unit = {u}\n");
    }
    if let Some(x) = cfg.income_scale {
        s += &format!("income_scale = {x:?}\n");
    }
    if cfg.values_as_stored {
        s += "values_as_stored = true\n";
    }
    if let Some(x) = cfg.board_share {
        s += &format!("board_share = {x:?}\n");
    }
    if let Some(c) = cfg.wage_budget_cap {
        s += &format!("wage_budget_cap = {c}\n");
    }
    for p in patches {
        s += &format!("\n[[patch]]\naddr = \"{}\"\nfrom = {}\nto = {}\n", p.addr, p.from, p.to);
    }
    if let Some(c) = cfg.contract_wage_ceilings {
        s += "\n# the contract screen's most a club may offer a week, by club status (0x82183190 reads it)\n";
        for (k, (from, to)) in CONTRACT_WAGE_CEILINGS.iter().zip(c).enumerate() {
            s += &format!("{}[[patch]]\naddr = \"{:#010x}\"\nfrom = {from}\nto = {}\n", if k > 0 { "\n" } else { "" }, CONTRACT_WAGE_TABLE + 4 * k as u32, to.min(CONTRACT_WAGE_MAX));
        }
    }
    s
}


/// mod.toml keys that patch a word of the image themselves, with the word (`runtime::mods`).
const KEY_PATCHES: &[(&str, &str)] = &[("board_share", "82003208")];

/// A new mod.toml with what an earlier mod keeps by hand (`squad-import --from <mod dir>`, #607):
/// every top-level key of `prev` that `new` lacks (the mapping writes none of `wage_cap`, `subs`,
/// `sponsor_unit`, `europe_places`, ...; the importer's own keys stay as written), then prev's
/// `[[patch]]` tables at addresses `new` doesn't patch (funds_fit's tier words, the board's share).
/// `from` names the earlier mod in the comments.
/// A mod's files the import never writes, kept by hand from one final to the next (`--from`
/// copies them when the new mod lacks them): the clubs' starting funds (#500) and shirt deals (#552).
pub const HAND_KEPT_FILES: [&str; 2] = ["funds.csv", "sponsors.csv"];

/// A hand-kept file carried from one mod to the next by club identity.
pub struct Carried {
    pub text: String,
    pub kept: usize,
    /// Rows whose club holds another id in the new mod (the league re-seat moves records).
    pub moved: usize,
    /// The clubs of the rows dropped: the new mod's clubs.csv doesn't name them.
    pub dropped: Vec<String>,
}

/// Carries a club-keyed csv (`id,...`, header first) from the mod whose `database/clubs.csv` is
/// `prev_clubs` to the one whose is `new_clubs`: each row's id becomes the id the new mod gives the
/// club with the same vanilla record (`record_of`: a club moves by taking an id in its new division
/// and keeps its record's identity; names change with the sources, "Wolves" to "Wolverhampton
/// Wanderers"), so the league re-seat's moves (#699) don't hand a club's funds or shirt deal to
/// whoever holds its old id. Without a `record_of` column the name (`name_E`) is the key. A row
/// whose club the new mod lacks is dropped; one whose id neither clubs.csv lists keeps its id.
pub fn carry_by_club(file: &str, prev_clubs: &str, new_clubs: &str) -> std::result::Result<Carried, String> {
    fn names(clubs: &str) -> std::result::Result<Vec<(u32, String, String)>, String> {
        let mut lines = clubs.lines();
        let header: Vec<&str> = lines.next().ok_or("clubs.csv is empty")?.trim_start_matches('\u{feff}').split(',').collect();
        let (id, name) = (
            header.iter().position(|h| *h == "id").ok_or("clubs.csv: no id column")?,
            header.iter().position(|h| *h == "name_E").ok_or("clubs.csv: no name_E column")?,
        );
        let record = header.iter().position(|h| *h == "record_of");
        let mut out = Vec::new();
        for l in lines.filter(|l| !l.trim().is_empty()) {
            let f: Vec<&str> = l.split(',').collect();
            let (Some(i), Some(n)) = (f.get(id), f.get(name)) else { continue };
            // a blank record_of is the club's own record (it didn't move): 905 of fc27final27's
            // 1161 rows; read as one shared key they collapsed 1097 funds rows onto 253 ids
            let key = match record.and_then(|r| f.get(r)) {
                Some(r) if !r.trim().is_empty() => format!("record {}", r.trim()),
                Some(_) => format!("record {}", i.trim()),
                None => n.trim().to_string(),
            };
            out.push((i.trim().parse::<u32>().map_err(|_| format!("clubs.csv: bad id {i:?}"))?, key, n.trim().to_string()));
        }
        Ok(out)
    }
    let (mut prev_rows, mut new_rows) = (names(prev_clubs)?, names(new_clubs)?);
    // one generation with record_of and one without would never match: both by name then
    let has_record = |rows: &[(u32, String, String)]| rows.iter().any(|(_, key, _)| key.starts_with("record "));
    if has_record(&prev_rows) != has_record(&new_rows) {
        for (_, key, name) in prev_rows.iter_mut().chain(new_rows.iter_mut()) {
            *key = name.clone();
        }
    }
    let prev: HashMap<u32, (String, String)> = prev_rows.into_iter().map(|(id, key, name)| (id, (key, name))).collect();
    let mut new_id: HashMap<String, u32> = HashMap::new();
    for (id, key, _) in new_rows {
        new_id.entry(key).or_insert(id);
    }
    let mut lines = file.lines();
    let header = lines.next().ok_or("empty file")?;
    if !header.trim_start_matches('\u{feff}').starts_with("id,") {
        return Err(format!("the first column is not id: {header:?}"));
    }
    let mut out = vec![header.to_string()];
    let (mut kept, mut moved, mut dropped) = (0, 0, Vec::new());
    for l in lines.filter(|l| !l.trim().is_empty()) {
        let (id, rest) = l.split_once(',').ok_or_else(|| format!("bad row {l:?}"))?;
        let id: u32 = id.trim().parse().map_err(|_| format!("bad id in {l:?}"))?;
        // a club neither clubs.csv lists (an unchanged record, if an import ever leaves those
        // out) keeps its row at the same id
        let Some((key, name)) = prev.get(&id) else {
            out.push(l.to_string());
            kept += 1;
            continue;
        };
        match new_id.get(key) {
            Some(&to) => {
                out.push(format!("{to},{rest}"));
                kept += 1;
                moved += (to != id) as usize;
            }
            None => dropped.push(if name.is_empty() { format!("id {id}") } else { name.clone() }),
        }
    }
    Ok(Carried { text: out.join("\n") + "\n", kept, moved, dropped })
}

pub fn carry_mod_toml(new: &str, prev: &str, from: &str) -> std::result::Result<String, String> {
    let new_t: toml::Table = toml::from_str(new).map_err(|e| format!("the new mod.toml: {e}"))?;
    let prev_t: toml::Table = toml::from_str(prev).map_err(|e| format!("{from}/mod.toml: {e}"))?;
    // the carried keys go before the first table header: after `[[patch]]` they would be its fields
    let split = new.find("\n[").map(|i| i + 1).or_else(|| new.starts_with('[').then_some(0)).unwrap_or(new.len());
    let (head, tail) = new.split_at(split);
    let mut out = head.trim_end().to_string();
    out.push('\n');
    let mut carried = Vec::new();
    for (k, v) in &prev_t {
        if k == "patch" || new_t.contains_key(k) {
            continue;
        }
        let one = toml::Table::from_iter([(k.clone(), v.clone())]);
        carried.push(toml::to_string(&one).map_err(|e| format!("{from}/mod.toml {k}: {e}"))?);
    }
    if !carried.is_empty() {
        out += &format!("\n# kept from {from}\n");
        for c in carried {
            out += c.trim_end();
            out.push('\n');
        }
    }
    if !tail.trim().is_empty() {
        out.push('\n');
        out += tail.trim_end();
        out.push('\n');
    }
    let addr_of = |t: &toml::Value| t.get("addr").and_then(|a| a.as_str()).map(|a| a.trim_start_matches("0x").to_ascii_lowercase());
    let mut patched: std::collections::HashSet<String> = new_t.get("patch").and_then(|p| p.as_array()).into_iter().flatten().filter_map(addr_of).collect();
    // a key of the new file that patches a word itself: the earlier mod's raw patch of that word
    // would find it changed and be refused (the board's share, #522)
    for (key, addr) in KEY_PATCHES {
        if new_t.contains_key(*key) {
            patched.insert(addr.to_string());
        }
    }
    let mut first = true;
    for p in prev_t.get("patch").and_then(|p| p.as_array()).into_iter().flatten() {
        let Some(addr) = addr_of(p) else { continue };
        if patched.contains(&addr) {
            continue;
        }
        if first {
            out += &format!("\n# patches kept from {from}\n");
            first = false;
        }
        let one = toml::Table::from_iter([("patch".to_string(), toml::Value::Array(vec![p.clone()]))]);
        out += toml::to_string(&one).map_err(|e| format!("{from}/mod.toml patch {addr}: {e}"))?.trim_end();
        out += "\n\n";
    }
    Ok(out.trim_end().to_string() + "\n")
}

/// The contract screen's per-status wage ceilings in the game's image (`Config::contract_wage_ceilings`).
const CONTRACT_WAGE_TABLE: u32 = 0x8200_AF9C;
const CONTRACT_WAGE_CEILINGS: [u32; 6] = [100_000, 50_000, 25_000, 10_000, 10_000, 5_000];
/// The offered wage's field holds 19 bits (`[0x8275478c] >> 13`).
const CONTRACT_WAGE_MAX: u32 = (1 << 19) - 1;

fn default_overflow() -> (u32, u32) {
    (15, 12)
}

fn default_both_feet_at() -> u32 {
    5
}

/// lma-data's `sides` (4 left, 2 right, 1 centre) for a set of position labels; None when the
/// mapping knows none of them.
fn sides_of<'a>(labels: impl Iterator<Item = &'a str>, map: &BTreeMap<String, String>) -> Option<u32> {
    let bits = labels.filter_map(|l| map.get(l.trim())).flat_map(|s| s.chars()).fold(0, |b, c| {
        b | match c.to_ascii_uppercase() {
            'L' => 4,
            'R' => 2,
            'C' => 1,
            _ => 0,
        }
    });
    (bits != 0).then_some(bits)
}

/// lma-data's `foot` (1 right, 2 left, 3 both) from a preferred foot and the weak foot's rating.
fn foot_of(preferred: &str, weak: Option<u32>, both_at: u32) -> Option<u32> {
    let foot = match preferred.trim().chars().next()?.to_ascii_lowercase() {
        'r' => 1,
        'l' => 2,
        _ => return None,
    };
    Some(if weak.is_some_and(|w| w >= both_at) { 3 } else { foot })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmConfig {
    pub last_name: String,
    pub name: String,
    /// Birth date column (`YYYY-MM-DD`, anything after the 10th character ignored).
    pub birth_date: String,
    pub value: String,
    /// Contract end date column (`contract_expiration_date`): the years left in wages.csv.
    #[serde(default)]
    pub contract_end: Option<String>,
    #[serde(default)]
    pub sub_position: Option<String>,
    #[serde(default)]
    pub height_cm: Option<String>,
    /// Sub-position label -> GK / DEF / MID / ATT, or "main/second" like `Config::positions`.
    #[serde(default)]
    pub positions: BTreeMap<String, String>,
    /// A market value already discounts age, and the game's own value formula discounts it again:
    /// the level is ranked on value / `age_decline`^(age - `peak_age`) for players past
    /// `peak_age` (Kane's EUR 60m at 32 ranks like ~146m), unchanged for younger ones.
    #[serde(default = "default_peak_age")]
    pub peak_age: u32,
    #[serde(default = "default_age_decline")]
    pub age_decline: f64,
    /// The player record's stored value (#31 item 1, £1000s) per unit of market value: the
    /// currency rate times the 2006 price level (default EUR -> GBP 0.85, divided by 4 for twenty
    /// years of fees, so a EUR 200m player stores £42.5m beside vanilla Henry's £45m).
    #[serde(default = "default_value_scale")]
    pub value_scale: f64,
    /// The roster's own overall's share of a matched player's level (0 = market value alone): a
    /// value can sink for reasons the pitch doesn't show (Rodri: FC 27 rates him 90 like Olise,
    /// Transfermarkt has EUR 50m after his knee against Olise's 150m, the maintainer's play-test).
    #[serde(default)]
    pub overall_weight: f64,
    /// Give unmatched players the median market value of matched peers (`fill_values`).
    #[serde(default)]
    pub fill_values: bool,
    /// With `--weights`: level each player so the overall the game computes is the source's own
    /// overall (the maintainer, 11 Oct: Saka 87 like FC 27, not 76), instead of vanilla's level at his rank.
    #[serde(default)]
    pub overall_from_source: bool,
    /// The players file's current-club id column: a club's Transfermarkt club is the one most of
    /// its matched players are at, for its stadium (`stadiums`).
    #[serde(default)]
    pub club: Option<String>,
    /// Columns of Transfermarkt's clubs file (`--tm-clubs`): each club with a Transfermarkt club
    /// takes its stadium's name and seats (2026's: White Hart Lane was replaced, new clubs come
    /// with a retired club's record and would play at its ground).
    #[serde(default)]
    pub stadiums: Option<TmStadiums>,
    /// The players file's player id column, for the shirt numbers (`lineups`).
    #[serde(default)]
    pub id: Option<String>,
    /// Columns of Transfermarkt's match lineups (`--tm-lineups`): a player wears the number of
    /// his last game for his club, when that club is his LMA club's Transfermarkt club (a player
    /// FC 27 moved since keeps no number from his old club).
    #[serde(default)]
    pub lineups: Option<TmLineups>,
    /// Membership from Transfermarkt over the roster: a matched player whose Transfermarkt club is
    /// another of our clubs moves there. Off by default: Transfermarkt's data ends with 2025/26
    /// and keeps loanees at their parent club, older than FC 27's snapshot (10 Oct check: 5,093
    /// moves, Arsenal filled with loanees, summer signings undone).
    #[serde(default)]
    pub membership: bool,
    /// Transfermarkt's latest-game managers over FM's (else only where FM has none).
    #[serde(default)]
    pub managers_over_fm: bool,
    /// Club funds by squad value (the maintainer, 10 Oct: per-club budgets, fallback scaled from the
    /// Transfermarkt squad value): the 16 wealth tiers' funds in GBP millions, ascending, as the
    /// mod's [[patch]] words set them; each club with valued players takes the highest tier at or
    /// under `budget_share` of its squad's value (stored-value units, so `value_scale` applies).
    /// Empty: wealth stays the database's.
    #[serde(default)]
    pub wealth_budgets: Vec<f64>,
    /// Tiers by rank (#704): each country's clubs keep the 2006 database's spread of wealth tiers,
    /// handed out in order of their funds, instead of the nearest `wealth_budgets` tier. The tier is
    /// also a club's stature for the AI (the stature check, its spending bands): nearest-funds tiers
    /// left 7 clubs at 13+ (2006: 40), so few clubs could bid for a >100k-a-week player and those
    /// that did failed the stature check after terms. Money stays exact through funds.csv.
    #[serde(default)]
    pub wealth_by_rank: bool,
    #[serde(default = "default_budget_share")]
    pub budget_share: f64,
    /// The share of a club's bank balance its board gives as transfer funds at the hub (0.65 in
    /// LMA 2007): a transfer budget from `--budgets` becomes funds of budget / this share.
    #[serde(default = "default_transfer_share")]
    pub transfer_share: f64,
    /// Factor on the `--budgets` list (the maintainer, 11 Oct: the online FC 27 lists' GBP 250m+ for the top
    /// clubs is too high; CMTracker has Man United at about half, so the FC 27 mapping uses 0.5).
    #[serde(default = "default_budgets_scale")]
    pub budgets_scale: f64,
    /// Clubs (by their disc name) whose funds the game multiplies on top of the country's
    /// multiplier (0x820eaa10: Chelsea x200 instead of x100 after the mod's patch, so 2.0; Celtic
    /// and Rangers x80, so 0.8): their tier is picked for funds / factor.
    #[serde(default)]
    pub funds_factors: BTreeMap<String, f64>,
    /// Columns of Transfermarkt's games (`--tm-games`) and the leagues they fill: each season's
    /// champion and its record go on top of that league's history (`history_csv`).
    #[serde(default)]
    pub history: Option<TmHistory>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmHistory {
    pub competition: String,
    /// The season's starting year (2025 for 2025/26).
    pub season: String,
    pub date: String,
    pub home_club: String,
    pub away_club: String,
    pub home_goals: String,
    pub away_goals: String,
    /// Competition id -> [country, tier] of the LMA league whose history it fills.
    pub leagues: BTreeMap<String, [usize; 2]>,
    /// Competitions whose tied points are split by the head-to-head record first (Spain, Italy,
    /// Portugal), for `last_tables_csv`.
    #[serde(default)]
    pub h2h: Vec<String>,
    /// The season `last_tables_csv` takes as last season (its starting year); none picks the latest
    /// season the games cover in full (a double round robin or more), so a season just begun in a
    /// newer pull doesn't replace the finished one.
    #[serde(default)]
    pub last_season: Option<u32>,
    /// Competition id -> the game's cup (`lma_data::db::CUPS` name) whose finals it fills
    /// (`round` and `game_id` name the columns: a cup's final is its game in round "Final").
    #[serde(default)]
    pub cups: BTreeMap<String, String>,
    #[serde(default)]
    pub round: Option<String>,
    #[serde(default)]
    pub game_id: Option<String>,
    /// The managers' names in each game: a club's manager is the one of its latest game, any
    /// competition (managers: TM > FM, the maintainer 10 Oct).
    #[serde(default)]
    pub home_manager: Option<String>,
    #[serde(default)]
    pub away_manager: Option<String>,
    /// The clubs' names: a champion none of our clubs' players vote for is the club of its name
    /// (Rangers 2020/21), when only one of ours has it.
    #[serde(default)]
    pub home_club_name: Option<String>,
    #[serde(default)]
    pub away_club_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmLineups {
    pub player: String,
    pub club: String,
    pub number: String,
    /// `YYYY-MM-DD`: the latest game's number wins.
    pub date: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TmStadiums {
    pub id: String,
    pub name: String,
    pub seats: String,
    /// The clubs file's club name column: a club whose players don't vote for a Transfermarkt
    /// club (Hull's are at no club the players file follows) takes the one of its source club's
    /// normalised name, when only one has it and no other club took it (Hull City, last up in
    /// 2016: MKM Stadium), or of the LMA name for the roster's own clubs; never a taken-over or
    /// new club's LMA name, which is the 2006 club's (Chile's Everton under Lugo).
    #[serde(default)]
    pub club: Option<String>,
}

fn default_value_scale() -> f64 {
    0.85 / 4.0
}

/// A stored value from GBP, in units of `unit` GBP (the game's own: 1000), rounded to vanilla's
/// steps (GBP 5k below 100k, 25k below 1m, 50k below 10m, 250k above); at least GBP 5k for any
/// priced player (0 means unpriced) and at most the field's 65535 units.
fn value_steps(gbp: f64, unit: u32) -> u32 {
    let step = match gbp {
        g if g < 100e3 => 5e3,
        g if g < 1e6 => 25e3,
        g if g < 10e6 => 50e3,
        _ => 250e3,
    };
    let unit = unit.max(1) as f64;
    ((gbp / step).round() * step / unit).round().clamp((5e3 / unit).ceil(), 65535.0) as u32
}

fn default_peak_age() -> u32 {
    28
}

fn default_age_decline() -> f64 {
    0.8
}

/// The market value a player's level is ranked on: `value` with the decline past `peak` undone.
fn age_adjusted(value: f64, age: Option<u32>, peak: u32, decline: f64) -> f64 {
    match age {
        // capped at 34: a veteran's tiny value would otherwise be multiplied many times over
        Some(a) if a > peak && decline > 0.0 => value / decline.powi((a.min(34) - peak) as i32),
        _ => value,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FmConfig {
    pub first_name: String,
    pub last_name: String,
    #[serde(default)]
    pub common_name: Option<String>,
    pub birth_date: String,
    #[serde(default)]
    pub height_cm: Option<String>,
    pub ratings: BTreeMap<String, Source>,
    pub gk_ratings: BTreeMap<String, Source>,
    #[serde(default)]
    pub scale: Scale,
    /// Per-player skin tone (#31 item 5), e.g. `tools/fm_skin.py`'s `skin_tone` column.
    #[serde(default)]
    pub skin: Option<FmSkin>,
    /// FM's hair colour, type and length -> the record's hair style and colour (#697).
    #[serde(default)]
    pub hair: Option<FmHair>,
    /// The weekly wage column (GBP; FM26 `contract_wage`): the players' real wages go to the
    /// mod's wages.csv (`runtime::mods`, hook on the game's wage function).
    #[serde(default)]
    pub wage: Option<String>,
    /// Contract end date column (FM26 `contract_end`), when Transfermarkt has none for him.
    #[serde(default)]
    pub contract_end: Option<String>,
    /// The transfer value column (GBP; FM26 `transfer_value`): a player's value when Transfermarkt
    /// has none for him (the maintainer, 21 Oct: values TM > FC27 > FM26), before the fill from peers.
    #[serde(default)]
    pub value: Option<String>,
    /// The attribute source as a roster too (#31, the maintainer: "world is 2026 only"): its players fill
    /// the LMA clubs the roster doesn't cover, and its free agents every spare record.
    #[serde(default)]
    pub fill: Option<FmFill>,
    /// Managers from the attribute source's staff export (`squad-import ... --staff <csv>`, #31
    /// decision 2): every club with a source takes its club's manager there.
    #[serde(default)]
    pub managers: Option<FmManagers>,
}

/// Columns of a staff export (`fmsave export <save> staff --all`). A manager is a club's staff
/// member whose `role` is one of `manager_roles` (in preference: the manager, then a caretaker)
/// who isn't on the club's staff lists (`listed` false: those are the coaches, physios and
/// scouts); a club with two of the same role (its B or youth side's on the club too) takes the
/// more able.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FmManagers {
    /// The club's uid: a women's section has the men's club's name, under its own uid.
    pub club_uid: String,
    pub role: String,
    pub manager_roles: Vec<String>,
    pub listed: String,
    pub ability: String,
    pub surname: String,
    #[serde(default)]
    pub common_name: Option<String>,
    /// Managers' reputations (0-99, the stars the game shows) from their ability: `[ability at 1,
    /// ability at 99]`, linear between; a manager the export doesn't know (FM 26 has no Ancelotti,
    /// Kompany or Maresca) gets one from his club's wealth tier, 20 + 5 a tier (Bayern's 15: 95),
    /// or `reputation_default` without the column. Without `reputation` a new manager keeps the 2006
    /// record's (Carrick at Man United had Ferguson's 90, 4.5 stars; the maintainer, 12 Oct).
    #[serde(default)]
    pub reputation: Option<[f64; 2]>,
    #[serde(default = "default_reputation")]
    pub reputation_default: u32,
}

fn default_reputation() -> u32 {
    50
}

/// Columns of the attribute source read when it fills squads (`FmConfig::fill`). Its players
/// become roster rows (same columns as the roster, so everything downstream treats them alike)
/// after the roster's own, and never take a club the roster has.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FmFill {
    pub club: String,
    /// Nation id: named through the players both sources have (FC 27's nationality of FM's id).
    pub nation: String,
    /// Positions, `;`-separated, the first the main one; `alt_positions` more of them.
    pub positions: String,
    #[serde(default)]
    pub alt_positions: Option<String>,
    /// 1-200 current ability: squad order (`overall` = ability / 2).
    pub ability: String,
    /// 1-20 foot ratings: the stronger is the preferred foot; the weaker sets the roster's 1-5
    /// weak foot.
    #[serde(default)]
    pub left_foot: Option<String>,
    #[serde(default)]
    pub right_foot: Option<String>,
    /// Source position label -> the roster's (DC -> CB, AMR -> RW).
    pub labels: BTreeMap<String, String>,
    /// Its unattached players become the free agents, in every record no squad uses.
    #[serde(default = "default_true")]
    pub free_agents: bool,
    /// FM 26 carries its women's football too, with no gender field. Clubs whose players' median
    /// height is below this are left out: women's sections sit at 160-174 cm, men's clubs at
    /// 176-188 (and a women's section has the men's club's name, under its own uid).
    #[serde(default)]
    pub min_club_height_cm: Option<f64>,
    /// The club's own id, to tell a club from its namesake section (without it, the name).
    #[serde(default)]
    pub club_uid: Option<String>,
    /// Free agents only from rows whose `uid` is below this: the women's database is all newer
    /// (uids from 2,000,000,000), and the older one's 5,239 free agents are men.
    #[serde(default)]
    pub uid: Option<String>,
    #[serde(default)]
    pub free_agent_uid_below: Option<u64>,
    /// Its clubs that LMA lacks take over the LMA clubs with a squad and no source outside the
    /// league ladders (#31, decision 1 phase B): the strongest of the club's nation (its vanilla
    /// squad's), else the strongest left; the club keeps its id, takes the name and the squad.
    #[serde(default = "default_true")]
    pub takeover: bool,
    /// Short club name, for a name longer than the game's 25 characters and the 7-letter short.
    #[serde(default)]
    pub club_short: Option<String>,
}

fn default_true() -> bool {
    true
}

/// FM's skin tone (1 lightest .. 20 darkest, 0 or blank unknown) -> LMA's `skin` (0-3):
/// `lma[tone - 1]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FmSkin {
    pub column: String,
    pub lma: Vec<u32>,
}

/// FM's hair (#697, the bytes beside the skin tone in the save: tools/fm_skin.py): the colour
/// column (1-18, low = light), the type column (0 straight, 2-3 afro/curly) and the length column
/// (0 bald, 1 short, 2 medium, 3-4 long), to the record's hair style (`HAIR`, 0 bald .. 14) and
/// colour (`HAIR_COLOUR`, 0-7). `colour_lma`: `[from, to, lma]` ranges of FM's colour; `style_lma`:
/// `[length, type, lma]` rows (type 255 = any); when several rows match, one is drawn by the
/// player's name hash, so short hair spreads over the game's short styles as vanilla's does.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FmHair {
    pub colour: String,
    pub kind: String,
    pub length: String,
    pub colour_lma: Vec<[u32; 3]>,
    pub style_lma: Vec<[u32; 3]>,
}

impl FmHair {
    /// (style, colour) for FM's values; None where a value is blank or no row matches. `seed`
    /// picks among the matching style rows.
    pub fn map(&self, colour: Option<u32>, kind: Option<u32>, length: Option<u32>, seed: u32) -> (Option<u32>, Option<u32>) {
        let style = match (length, kind) {
            (Some(l), k) => {
                // an exact type row wins over the any-type rows; among equals the seed decides
                let exact: Vec<u32> = self.style_lma.iter().filter(|r| r[0] == l && Some(r[1]) == k).map(|r| r[2]).collect();
                let any: Vec<u32> = self.style_lma.iter().filter(|r| r[0] == l && r[1] == 255).map(|r| r[2]).collect();
                let rows = if exact.is_empty() { any } else { exact };
                (!rows.is_empty()).then(|| rows[seed as usize % rows.len()])
            }
            _ => None,
        };
        let colour = colour.and_then(|c| self.colour_lma.iter().find(|r| r[0] <= c && c <= r[1]).map(|r| r[2]));
        (style, colour)
    }
}

fn default_max_squad() -> usize {
    32
}

fn default_min_squad() -> usize {
    18
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Columns {
    pub forename: String,
    pub surname: String,
    /// Single-name form (used as the surname when present), optional.
    #[serde(default)]
    pub common_name: Option<String>,
    pub birthdate: String,
    pub club: String,
    #[serde(default)]
    pub league: Option<String>,
    pub nationality: String,
    pub position: String,
    #[serde(default)]
    pub alt_positions: Option<String>,
    /// Sort key for picking each club's squad (higher first).
    pub overall: String,
    #[serde(default)]
    pub height_cm: Option<String>,
    /// Preferred foot ("Left" / "Right") and the weak foot's 1-5 rating (#31 item 3).
    #[serde(default)]
    pub foot: Option<String>,
    #[serde(default)]
    pub weak_foot: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Source {
    Constant(f64),
    Columns(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scale {
    pub mul: f64,
    pub add: f64,
    /// A curve on the level the game sees (`level_remap`'s target: the source's overall when
    /// `overall_from_source`): `[from, to]` points, straight lines between them and beyond the ends
    /// (the maintainer, 21 Oct: the spread: Mbappe, Haaland and Yamal clearly above an 80-rated squad player,
    /// League Two clearly below; 1:1 put a 91 ten points over an 80 and League Two's 58 at 50). The
    /// ratings themselves keep `mul` and `add` only. Empty = none.
    #[serde(default)]
    pub curve: Vec<[f64; 2]>,
}

impl Default for Scale {
    fn default() -> Self {
        Self { mul: 1.0, add: 0.0, curve: Vec::new() }
    }
}

impl Scale {
    /// `v` through the curve, then `mul` and `add` (the level target uses `curved` alone).
    pub fn apply(&self, v: f64) -> f64 {
        self.curved(v) * self.mul + self.add
    }

    /// `v` through the curve only: straight lines through the points, continued beyond the ends.
    pub fn curved(&self, v: f64) -> f64 {
        let mut pts: Vec<[f64; 2]> = self.curve.clone();
        pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(std::cmp::Ordering::Equal));
        let curved = match pts.len() {
            0 => v,
            1 => v + pts[0][1] - pts[0][0],
            n => {
                // the segment v falls in, or the end segment beyond the ends
                let i = pts.iter().position(|p| v < p[0]).unwrap_or(n).clamp(1, n - 1);
                let (a, b) = (pts[i - 1], pts[i]);
                if (b[0] - a[0]).abs() < f64::EPSILON {
                    a[1]
                } else {
                    a[1] + (v - a[0]) * (b[1] - a[1]) / (b[0] - a[0])
                }
            }
        };
        curved
    }
}

/// What an import did, for the CLI and the mod's `import-report.txt`.
#[derive(Debug, Default)]
pub struct Report {
    pub clubs_matched: Vec<(String, String, usize)>, // (source club, LMA club, players)
    pub clubs_unmatched: Vec<(String, usize)>,
    pub nations_unmatched: BTreeMap<String, usize>,
    pub players_written: usize,
    pub players_reused_same_person: usize,
    /// Squads cut short because every spare player record was in use.
    pub squads_capped: usize,
    /// Players kept beyond a club's 32 (`squad_overflow`): (player record, club id).
    pub squad_overflow: Vec<(u32, u32)>,

    /// Players added from the live squads (`live_add_missing`): "Name: club (overall)".
    pub live_added: Vec<String>,
    /// Live squad players not added because the roster has that surname born within a year.
    pub live_add_skipped: Vec<String>,
    /// Imported players whose ratings came from the attribute source (`fm`), and those that
    /// fell back to the roster's own attributes.
    pub fm_matched: usize,
    pub fm_unmatched: usize,
    /// Of those, players whose skin tone came from it.
    pub fm_skin: usize,
    /// Imported players whose hair style or colour came from the attribute source (#697).
    pub fm_hair: usize,
    pub tm_matched: usize,
    pub tm_unmatched: usize,
    /// Transfermarkt candidates dropped for being on the other side of the goalkeeper line.
    pub tm_other_line: usize,
    /// Players valued at EUR 20m or more with low ratings (average under 50): a likely wrong
    /// Transfermarkt match, as "name (club): EUR value, average rating".
    pub value_outliers: Vec<String>,
    /// The attribute source as a roster (`[fm.fill]`): players written into clubs the roster
    /// doesn't cover, those clubs, free agents written, records still holding a 2006 player, and
    /// LMA clubs with a squad that no source covers.
    pub fill_players: usize,
    pub fill_clubs: usize,
    pub free_agents: usize,
    pub records_kept: usize,
    pub clubs_without_source: Vec<String>,
    /// LMA clubs taken over by a club of the attribute source: (old name, new name).
    pub takeovers: Vec<(String, String)>,
    /// Clubs with no source anywhere whose 2006 squad gave way to the source's free agents.
    pub free_agent_squads: Vec<String>,
    /// Clubs whose manager came from the staff export.
    pub managers: usize,
    /// Managers whose reputation came from their ability, and those given the default.
    pub manager_reps: usize,
    pub manager_reps_default: Vec<String>,
    /// Managers whose reputation came from the managers list's `reputation` column.
    pub manager_reps_listed: usize,
    /// Clubs whose stadium name and seats came from Transfermarkt.
    pub stadiums: usize,
    /// Players wearing their own shirt number (Transfermarkt's lineups).
    pub shirt_numbers: usize,
    /// Players moved to their Transfermarkt club, and clubs whose manager is Transfermarkt's.
    pub tm_moves: Vec<String>,
    pub tm_managers: usize,
    /// Managers from the managers file, and its clubs that matched none (or several) of ours.
    pub listed_managers: usize,
    pub listed_managers_unmatched: Vec<String>,
    /// Live Transfermarkt squads: players found there, moved to their live club, found at a club
    /// that isn't ours (kept where they were), and live clubs none of ours is.
    pub live_matched: usize,
    /// Of those, players whose value came from the live page.
    pub live_values: usize,
    /// Our rows that were a second listing of a live player (FC 27 had him at two clubs): dropped.
    pub live_duplicates: usize,
    pub live_moves: Vec<String>,
    pub live_elsewhere: usize,
    pub live_clubs_unmatched: Vec<String>,
    /// Live clubs matched by their players' votes ("Real Valladolid CF -> Valladolid").
    pub live_clubs_voted: Vec<String>,
    /// Live clubs matched by a name word only one of our clubs has ("Royal Antwerp FC -> Antwerp").
    pub live_clubs_by_word: Vec<String>,
    /// Clubs without a source that no club of their nation could take over: they keep their 2006 squad.
    pub takeovers_kept: Vec<String>,
    /// Each ladder competition's live clubs and the LMA club each resolved to (#699 `comps`).
    pub live_comp_clubs: Vec<String>,
    /// Live ladder clubs LMA has no record of that came in as new clubs with their FM squads, and
    /// those with no FM squad of `min_squad` (#826: the National League's Gateshead, Sutton...).
    pub live_new_clubs: Vec<String>,
    pub live_new_missing: Vec<String>,
    /// Clubs the disc gave a made-up name, renamed to their live name ("Pamplona -> CA Osasuna").
    pub live_renamed: Vec<String>,
    /// Moves and departures not made because the club would have dropped under `MIN_SQUAD`.
    pub live_kept_for_size: usize,
    /// Clubs whose wealth tier came from their squad's value (`wealth_budgets`).
    pub wealth_set: usize,
    /// Of those, the clubs whose funds came from `--budgets`; and the budgets no club matched.
    pub wealth_from_budgets: usize,
    /// Players Transfermarkt didn't match who got their matched peers' median value.
    pub values_filled: usize,
    /// Players valued from the attribute source's transfer value (no Transfermarkt value).
    pub fm_values: usize,
    /// The FM/Transfermarkt value ratio the stand-ins were divided by: (band, median, pairs).
    pub fm_value_ratio: Vec<(String, f64, usize)>,
    pub budgets_unmatched: Vec<String>,
    /// League seasons added to the histories from Transfermarkt, and champions it names that
    /// none of our clubs is (left blank).
    pub history_seasons: usize,
    /// History rows filled from `--history-extra`.
    pub history_extra: usize,
    /// Cup finals added from Transfermarkt (of them, with the score in play from the events), from
    /// `--cup-extra`, and finals left out because a side isn't one of our clubs.
    pub cup_finals: usize,
    pub cup_shootouts: usize,
    pub cup_extra: usize,
    pub cup_unmatched: Vec<String>,
    pub history_unmatched: Vec<String>,
    /// The transfers file's moves made, as "player: from -> to", and its lines that matched no
    /// player or club.
    pub transfers: Vec<String>,
    pub transfers_unmatched: Vec<String>,
    /// Clubs with a stadium record and a source that keep the database's stadium (no Transfermarkt
    /// club for them), as "club (stadium)".
    pub stadiums_kept: Vec<String>,
    /// Clubs with a squad that kept the database's manager (no manager for them in the source).
    pub managers_kept: Vec<String>,
    /// Clubs of the roster LMA lacks, placed in their league: (club, league, the vanilla club
    /// whose id it takes; that club moved or retired).
    pub new_clubs: Vec<(String, String, String)>,
    /// Clubs moved to another division, as "club: from -> to", and notes on places the source
    /// couldn't fill.
    pub division_moves: Vec<String>,
    pub division_notes: Vec<String>,
    /// Last season's tables (`last_tables_csv`): one line per league the ladders know, and the
    /// Transfermarkt clubs that aren't ours.
    pub last_tables: Vec<String>,
}

/// What `import` writes into the mod space: the sparse `players.csv`, and a sparse `clubs.csv`
/// when a club changes division.
pub struct Imported {
    pub players: String,
    pub clubs: Option<String>,
    /// Each club with a known FM club: `id,name_E,fm_uid` at the final ids, with the name the
    /// game will show (for badges keyed by FM uid); None without the attribute source.
    pub clubs_fm: Option<String>,
    /// The league histories (`history_csv`): the base's moved to the clubs' new ids and shifted
    /// to the base year, the seasons since from Transfermarkt; None without a base history.csv.
    pub history: Option<String>,
    /// The cup finals (`cups.csv` for db-import): the base's moved to the base year, then the sources'.
    pub cups: Option<String>,
    /// Last season's final tables (`last_tables.csv` for db-import, #666): the base's moved to the
    /// clubs' new ids, the leagues the ladders know re-made from the mod's clubs and Transfermarkt.
    pub last_tables: Option<String>,
    /// Each club's target funds (`id,funds_m,source`: GBP m in the bank at team select, from its
    /// transfer budget or its squad's value), at the final ids, for `tools/funds_fit.py`.
    pub funds_targets: Option<String>,
    /// Weekly wages (`id,wage`, GBP) at the final player ids: the attribute source's, else the
    /// median wage-to-value ratio of the players with one times the player's market value.
    pub wages: Option<String>,
    pub report: Report,
}

impl Report {
    pub fn text(&self) -> String {
        let mut s = format!(
            "{} players written ({} kept their LMA record: same name and consistent age)\n{} clubs matched, {} source clubs not in the LMA database\n",
            self.players_written,
            self.players_reused_same_person,
            self.clubs_matched.len(),
            self.clubs_unmatched.len()
        );
        if self.tm_matched + self.tm_unmatched > 0 {
            s += &format!(
                "transfermarkt: {} players matched (level from market value), {} not (level from overall); {} candidates dropped across the goalkeeper line\n",
                self.tm_matched, self.tm_unmatched, self.tm_other_line
            );
        }
        if !self.value_outliers.is_empty() {
            s += &format!("value outliers (EUR 20m+, average rating under 50; check the Transfermarkt match): {}\n", self.value_outliers.join("; "));
        }
        if self.fm_matched + self.fm_unmatched > 0 {
            s += &format!(
                "attribute source: {} imported players matched ({} with a skin tone, {} with hair), {} use the roster's own attributes\n",
                self.fm_matched, self.fm_skin, self.fm_hair, self.fm_unmatched
            );
        }
        if self.fill_players + self.free_agents > 0 {
            s += &format!(
                "attribute source as roster: {} players at {} clubs the roster doesn't cover, {} free agents; {} records still hold a 2006 player ({} clubs with a squad and no source)\n",
                self.fill_players,
                self.fill_clubs,
                self.free_agents,
                self.records_kept,
                self.clubs_without_source.len()
            );
        }
        if self.managers > 0 {
            s += &format!("managers: {} clubs take their club's manager, {} keep the database's\n", self.managers, self.managers_kept.len());
            if self.manager_reps + self.manager_reps_default.len() + self.manager_reps_listed > 0 {
                s += &format!(
                    "manager reputations: {} from the managers list, {} from their ability, {} from their club's wealth (not in the staff export): {}\n",
                    self.manager_reps_listed,
                    self.manager_reps,
                    self.manager_reps_default.len(),
                    self.manager_reps_default.join(", ")
                );
            }
        }
        if !self.transfers.is_empty() || !self.transfers_unmatched.is_empty() {
            s += &format!("transfers: {} made, {} not matched\n", self.transfers.len(), self.transfers_unmatched.len());
            for t in self.transfers.iter().chain(&self.transfers_unmatched) {
                s += &format!("  {t}\n");
            }
        }
        if self.cup_finals + self.cup_extra > 0 || !self.cup_unmatched.is_empty() {
            s += &format!("cup finals: {} added ({} with a shoot-out, scored in play; {} from --cup-extra), {} with a side not ours: {}\n", self.cup_finals + self.cup_extra, self.cup_shootouts, self.cup_extra, self.cup_unmatched.len(), self.cup_unmatched.join(", "));
        }
        if self.history_seasons + self.history_extra > 0 || !self.history_unmatched.is_empty() {
            s += &format!("league histories: {} seasons added ({} from --history-extra), {} champions not ours: {}\n", self.history_seasons + self.history_extra, self.history_extra, self.history_unmatched.len(), self.history_unmatched.join(", "));
        }
        if self.live_matched > 0 {
            s += &format!("live squads: {} values updated; {} second listings dropped; {} kept to keep squads at {MIN_SQUAD}; ", self.live_values, self.live_duplicates, self.live_kept_for_size);
            s += &format!("live squads: {} players found, {} moved to their live club, {} at a club that isn't ours (left our squads); live clubs not ours: {}\n", self.live_matched, self.live_moves.len(), self.live_elsewhere, self.live_clubs_unmatched.join(", "));
            if !self.live_new_clubs.is_empty() || !self.live_new_missing.is_empty() {
                s += &format!("live clubs LMA lacks, in as new clubs with their FM squads ({}): {}; no FM squad ({}): {}\n", self.live_new_clubs.len(), self.live_new_clubs.join(", "), self.live_new_missing.len(), self.live_new_missing.join(", "));
            }
            if !self.live_comp_clubs.is_empty() {
                s += &format!("live competitions (ladder comps), each live club and ours:
  {}
", self.live_comp_clubs.join("
  "));
            }
            if !self.takeovers_kept.is_empty() {
                s += &format!("not taken over (no candidate left; they keep their 2006 squads): {}\n", self.takeovers_kept.join(", "));
            }
            if !self.live_clubs_by_word.is_empty() {
                s += &format!("live clubs matched by a name word: {}\n", self.live_clubs_by_word.join(", "));
            }
            if !self.live_clubs_voted.is_empty() {
                s += &format!("live clubs matched by their players: {}\n", self.live_clubs_voted.join(", "));
            }
            if !self.live_renamed.is_empty() {
                s += &format!("made-up names replaced by the live name ({}): {}\n", self.live_renamed.len(), self.live_renamed.join(", "));
            }
            for m in &self.live_moves {
                s += &format!("  {m}\n");
            }
        }
        if self.values_filled > 0 {
            s += &format!("stored values filled from matched peers (same group, overall and a similar age): {}\n", self.values_filled);
        }
        if self.fm_values > 0 {
            s += &format!("values from the attribute source's transfer value (no Transfermarkt value): {}\n", self.fm_values);
            if !self.fm_value_ratio.is_empty() {
                s += &format!("  divided by the FM/Transfermarkt ratio of the players with both: {}\n", self.fm_value_ratio.iter().map(|(b, r, n)| format!("{b} {r:.2} ({n})")).collect::<Vec<_>>().join(", "));
            }
        }
        if self.wealth_set > 0 {
            s += &format!("club funds set: {} ({} from transfer budgets); budgets no club matched ({}): {}\n", self.wealth_set, self.wealth_from_budgets, self.budgets_unmatched.len(), self.budgets_unmatched.join(", "));
        }
        if self.listed_managers > 0 || !self.listed_managers_unmatched.is_empty() {
            s += &format!("managers file: {} clubs set, not matched: {}\n", self.listed_managers, self.listed_managers_unmatched.join(", "));
        }
        if !self.tm_moves.is_empty() || self.tm_managers > 0 {
            s += &format!("transfermarkt: {} players moved to their Transfermarkt club, {} clubs take its manager\n", self.tm_moves.len(), self.tm_managers);
            for m in &self.tm_moves {
                s += &format!("  {m}\n");
            }
        }
        if self.shirt_numbers > 0 {
            s += &format!("shirt numbers: {} players wear their own\n", self.shirt_numbers);
        }
        if self.stadiums > 0 {
            s += &format!("stadiums: {} clubs take their Transfermarkt club's stadium, {} keep the database's\n", self.stadiums, self.stadiums_kept.len());
        }
        if !self.squad_overflow.is_empty() {
            s += &format!("{} players kept beyond their club's 32 (squad_overflow.csv)\n", self.squad_overflow.len());
        }

        if !self.live_added.is_empty() {
            s += &format!("live squads: {} players the roster lacks added from Transfermarkt: {}\n", self.live_added.len(), self.live_added.join(", "));
        }
        if !self.live_add_skipped.is_empty() {
            s += &format!("live squads: {} not added (the roster has that surname born within a year: another spelling, date or club): {}\n", self.live_add_skipped.len(), self.live_add_skipped.join(", "));
        }
        if self.squads_capped > 0 {
            s += &format!("{} squads were cut short: no spare player records left
", self.squads_capped);
        }
        if !self.division_moves.is_empty() || !self.division_notes.is_empty() {
            s += &format!("{} clubs change division\n", self.division_moves.len());
        }
        for n in &self.division_notes {
            s += &format!("  {n}\n");
        }
        if !self.last_tables.is_empty() {
            s += "last season's tables (last_tables.csv):\n";
            for l in &self.last_tables {
                s += &format!("  {l}\n");
            }
        }
        s += "\nunmatched clubs (add to club_aliases if they exist in LMA):\n";
        for (c, n) in &self.clubs_unmatched {
            s += &format!("  {c} ({n} players)\n");
        }
        s += "\nunmatched nationalities (add to nation_aliases):\n";
        for (n, k) in &self.nations_unmatched {
            s += &format!("  {n} ({k} players)\n");
        }
        if !self.division_moves.is_empty() {
            s += "\ndivision changes (country/division):\n";
            for m in &self.division_moves {
                s += &format!("  {m}\n");
            }
        }
        if !self.new_clubs.is_empty() {
            s += &format!("\n{} clubs LMA lacks placed in their league:\n", self.new_clubs.len());
            for (club, league, displaced) in &self.new_clubs {
                s += &format!("  {club} ({league}) at the id vanilla gives {displaced}\n");
            }
        }
        if !self.takeovers.is_empty() {
            s += &format!("\n{} clubs with no source taken over by clubs LMA lacks:\n", self.takeovers.len());
            for (old, new) in &self.takeovers {
                s += &format!("  {old} -> {new}\n");
            }
        }
        if !self.stadiums_kept.is_empty() {
            s += &format!("\n{} clubs keep the database's stadium (no Transfermarkt club):\n", self.stadiums_kept.len());
            for c in &self.stadiums_kept {
                s += &format!("  {c}\n");
            }
        }
        if !self.managers_kept.is_empty() {
            s += &format!("\n{} clubs keep the database's manager (none in the source):\n", self.managers_kept.len());
            for c in &self.managers_kept {
                s += &format!("  {c}\n");
            }
        }
        if !self.free_agent_squads.is_empty() {
            s += &format!("\n{} clubs with no source anywhere play with the source's free agents:\n", self.free_agent_squads.len());
            for c in &self.free_agent_squads {
                s += &format!("  {c}\n");
            }
        }
        if !self.clubs_without_source.is_empty() {
            s += "\nclubs with a squad and no source (their 2006 players stay):\n";
            for c in &self.clubs_without_source {
                s += &format!("  {c}\n");
            }
        }
        s += "\nmatched clubs:\n";
        for (src, lma, n) in &self.clubs_matched {
            s += &format!("  {src} -> {lma} ({n})\n");
        }
        s
    }
}

/// Lower-case, strip accents and punctuation, drop common club prefixes/suffixes.
/// The disc's name for a club shares no word with its real one: made up for an unlicensed club in
/// 2006 ("Pamplona" for CA Osasuna). A word counts as shared when one is the start of the other, 3+
/// letters, so "Man Utd" is Manchester United and "Spurs" vs "Tottenham Hotspur" is made up.
pub fn made_up_name(disc: &str, real: &str, abbreviations: &BTreeMap<String, String>) -> bool {
    let words = |s: &str| -> Vec<String> { s.split(|c: char| c.is_whitespace() || c == '-' || c == '.').map(|w| normalise(w, abbreviations)).filter(|w| w.len() >= 3).collect() };
    let (d, r) = (words(disc), words(real));
    !d.is_empty() && !d.iter().any(|a| r.iter().any(|b| a.starts_with(b.as_str()) || b.starts_with(a.as_str())))
}

pub fn normalise(s: &str, abbreviations: &BTreeMap<String, String>) -> String {
    let folded: String = s.chars().map(fold_accent).collect::<String>().to_lowercase();
    let words: Vec<String> = folded
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| abbreviations.get(w).cloned().unwrap_or_else(|| w.to_string()))
        .filter(|w| !matches!(w.as_str(), "fc" | "afc" | "cf" | "sc" | "ac" | "fk" | "sk" | "club" | "the" | "de"))
        .collect();
    words.concat()
}

/// A letter without its accent. Every letter a name can carry must fold to ASCII: `name_words`
/// splits on anything else, so one missing letter breaks the name into short pieces that match
/// nobody (Turkish "Yıldız" became "y", "ld", "z": Kenan Yildiz matched neither FM nor
/// Transfermarkt and took his nation's default skin, #614).
fn fold_accent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | 'æ' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' | 'Æ' => 'a',
        'ç' | 'Ç' | 'ć' | 'č' | 'Ć' | 'Č' => 'c',
        'ď' | 'đ' | 'ð' | 'Ď' | 'Đ' | 'Ð' => 'd',
        'è' | 'é' | 'ê' | 'ë' | 'ě' | 'ē' | 'ė' | 'ę' | 'È' | 'É' | 'Ê' | 'Ë' | 'Ě' | 'Ē' | 'Ė' | 'Ę' => 'e',
        'ğ' | 'Ğ' => 'g',
        'ì' | 'í' | 'î' | 'ï' | 'ı' | 'ī' | 'į' | 'Ì' | 'Í' | 'Î' | 'Ï' | 'İ' | 'Ī' | 'Į' => 'i',
        'ķ' | 'Ķ' => 'k',
        'ł' | 'ľ' | 'ĺ' | 'ļ' | 'Ł' | 'Ľ' | 'Ĺ' | 'Ļ' => 'l',
        'ñ' | 'ń' | 'ň' | 'ņ' | 'Ñ' | 'Ń' | 'Ň' | 'Ņ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ő' | 'ō' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ő' | 'Ō' => 'o',
        'ř' | 'ŕ' | 'Ř' | 'Ŕ' => 'r',
        'š' | 'ś' | 'ş' | 'ș' | 'Š' | 'Ś' | 'Ş' | 'Ș' | 'ß' => 's',
        'ť' | 'ț' | 'ţ' | 'þ' | 'Ť' | 'Ț' | 'Ţ' | 'Þ' => 't',
        'ù' | 'ú' | 'û' | 'ü' | 'ů' | 'ű' | 'ū' | 'ų' | 'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ů' | 'Ű' | 'Ū' | 'Ų' => 'u',
        'ý' | 'ÿ' | 'Ý' | 'Ÿ' => 'y',
        'ž' | 'ź' | 'ż' | 'Ž' | 'Ź' | 'Ż' => 'z',
        other => other,
    }
}

/// Whole years between a YYYY-MM-DD birthdate and the season start.
pub fn age_on(birth: &str, on: &str) -> Option<u32> {
    let p = |s: &str| -> Option<(i32, u32, u32)> {
        let mut it = s.split('-');
        Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?))
    };
    let (by, bm, bd) = p(birth)?;
    let (y, m, d) = p(on)?;
    let mut age = y - by;
    if (m, d) < (bm, bd) {
        age -= 1;
    }
    u32::try_from(age).ok()
}

/// Fallback club match: LMA often uses just the city ("Madrid", "Stuttgart", "Napoli"). If
/// exactly one LMA club's whole name equals one word (4+ letters) of the source name, use it.
fn word_match(name: &str, clubs: &HashMap<String, (u32, String)>, ab: &BTreeMap<String, String>) -> Option<(u32, String)> {
    let folded: String = name.chars().map(fold_accent).collect::<String>().to_lowercase();
    let mut found: Option<(u32, String)> = None;
    for word in folded.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 4) {
        let w = ab.get(word).cloned().unwrap_or_else(|| word.to_string());
        if let Some(hit) = clubs.get(&w) {
            if found.as_ref().is_some_and(|f| f.0 != hit.0) {
                return None; // ambiguous
            }
            found = Some(hit.clone());
        }
    }
    found
}

/// Make a name drawable by the game font (cp1252): characters it lacks become their base
/// letter, keeping case (Modrić -> Modric, Szczęsny -> Szczesny); é, ö, ñ ... stay.
pub fn game_safe(name: &str) -> String {
    name.chars()
        .filter_map(|c| {
            if lma_data::text::encode(&c.to_string()).is_some() {
                return Some(c);
            }
            let lower = c.to_lowercase().next().unwrap_or(c);
            let base = match lower {
                'ā' | 'ă' | 'ą' => 'a',
                'ć' | 'č' | 'ĉ' | 'ċ' => 'c',
                'ď' | 'đ' => 'd',
                'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => 'e',
                'ğ' | 'ģ' => 'g',
                'ī' | 'į' | 'ı' => 'i',
                'ķ' => 'k',
                'ĺ' | 'ļ' | 'ľ' | 'ł' => 'l',
                'ń' | 'ņ' | 'ň' => 'n',
                'ō' | 'ő' => 'o',
                'ŕ' | 'ř' => 'r',
                'ś' | 'ş' | 'ș' => 's',
                'ţ' | 'ť' | 'ț' => 't',
                'ū' | 'ů' | 'ű' | 'ų' => 'u',
                'ź' | 'ż' => 'z',
                other => fold_accent(other),
            };
            let out = if c.is_uppercase() { base.to_ascii_uppercase() } else { base };
            lma_data::text::encode(&out.to_string()).is_some().then_some(out)
        })
        .collect()
}

/// Lower-case, accent-free name words of 3+ letters (for matching people across sources).
fn name_words(s: &str) -> Vec<String> {
    s.chars()
        .map(fold_accent)
        .collect::<String>()
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_string)
        .collect()
}

struct Row {
    forename: String,
    surname: String,
    age: Option<u32>,
    nation: String,
    position: String,
    position2: String,
    overall: f64,
    height: Option<u32>,
    ratings: [u32; 17],
    /// Transfermarkt market value, when matched (sets the level in `level_remap`).
    tm_value: Option<f64>,
    /// The same value before the age adjustment: the stored value comes from it.
    market_value: Option<f64>,
    /// The attribute source's transfer value (GBP), when it has one for him: stands in for a
    /// missing Transfermarkt value (`fm_values`).
    fm_value: Option<f64>,
    /// LMA skin tone from the attribute source, when it has one for him.
    skin: Option<u32>,
    /// LMA hair style and colour from the attribute source's hair fields, when it has them (#697).
    hair: Option<u32>,
    hair_colour: Option<u32>,
    /// lma-data's `sides` and `foot`, when the source gives them.
    sides: Option<u32>,
    foot: Option<u32>,
    /// Transfermarkt's player id, when matched (his shirt numbers).
    tm_id: Option<String>,
    /// Moved here by the transfers file: kept when the squad is cut to its records.
    transferred: bool,
    /// The matched Transfermarkt player's current club (membership: TM > FC 27, the maintainer 10 Oct).
    tm_club: Option<String>,
    /// Birth date as the source has it (YYYY-MM-DD), for the live squads' match.
    birth: String,
    /// Weekly wage in GBP from the attribute source, when it has one for him.
    wage: Option<u32>,
    /// The year his contract ends (June): Transfermarkt's, else the attribute source's.
    contract_end: Option<u32>,
    /// The live squad's shirt number, first choice for the squad number.
    live_number: Option<u32>,
}

/// A position mapping's main group and its optional second: "MID/ATT" -> ("MID", "ATT").
fn groups(mapped: &str) -> (String, String) {
    match mapped.split_once('/') {
        Some((m, s)) => (m.trim().to_string(), s.trim().to_string()),
        None => (mapped.trim().to_string(), String::new()),
    }
}

/// A known-as name split into LMA's forename and surname: the first word, then the rest
/// ("Kevin De Bruyne" -> "Kevin", "De Bruyne"); a single word is a surname alone.
fn known_as(name: &str) -> (String, String) {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    match name.split_once(' ') {
        Some((f, l)) => (f.to_string(), l.to_string()),
        None => (String::new(), name),
    }
}

/// Vanilla's overall levels (mean of the 17 ratings) per position group, sorted ascending.
/// The overall the game shows and prices by (f8211f4c8): sum(w x rating) / sum(w) over 13 of the 17
/// ratings, with one weight row per role (8 rows, `--weights`: `row,` + `OVERALL_SLOTS` columns, read
/// from the user's own game). The role (f8211be88), as the PC's day-1 money table bears out for 75% of
/// players: goalkeeper 0, full-back 1, centre-back 2, central midfielder 4, wide midfielder 6, forward 7.
pub struct Overall {
    rows: Vec<[f64; 13]>,
}

/// The ratings in the game's weight order (indexes into `LMA_RATINGS`).
pub const OVERALL_SLOTS: [&str; 13] = ["tackling", "marking", "passing", "technique", "shot_power", "shot_accuracy", "heading", "pace", "creativity", "runs", "dribbling", "positioning", "intelligence"];

impl Overall {
    pub fn read(path: &Path) -> Result<Self> {
        let sheet = Sheet::read(path).map_err(|e| e.to_string())?;
        let cols: Vec<usize> = OVERALL_SLOTS.iter().map(|n| sheet.col(n).ok_or_else(|| format!("{}: no {n} column", path.display()))).collect::<Result<_>>()?;
        let rows: Vec<[f64; 13]> = sheet.rows.iter().map(|r| std::array::from_fn(|k| r[cols[k]].trim().parse().unwrap_or(0.0))).collect();
        if rows.len() < 8 {
            return Err(format!("{}: 8 weight rows needed", path.display()).into());
        }
        Ok(Self { rows })
    }

    pub fn row(position: &str, sides: Option<u32>) -> usize {
        let wide = matches!(sides, Some(2 | 4 | 6));
        match position {
            "GK" => 0,
            "DEF" if wide => 1,
            "DEF" => 2,
            "MID" if wide => 6,
            "MID" => 4,
            _ => 7,
        }
    }

    /// The overall of 17 ratings in `LMA_RATINGS` order.
    pub fn of(&self, ratings: &[f64], position: &str, sides: Option<u32>) -> f64 {
        let w = &self.rows[Self::row(position, sides)];
        let total: f64 = w.iter().sum();
        if total <= 0.0 {
            return ratings.iter().sum::<f64>() / ratings.len() as f64;
        }
        OVERALL_SLOTS.iter().enumerate().map(|(k, n)| w[k] * ratings[LMA_RATINGS.iter().position(|x| x == n).unwrap()]).sum::<f64>() / total
    }
}

fn vanilla_levels(players: &Sheet, overall: Option<&Overall>) -> Result<BTreeMap<String, Vec<f64>>> {
    let pos = players.col("position").ok_or("vanilla players.csv has no position column")?;
    let sides = players.col("sides");
    let cols: Vec<usize> = LMA_RATINGS.iter().map(|n| players.col(n).ok_or_else(|| format!("vanilla players.csv has no {n} column"))).collect::<Result<_>>()?;
    let mut out: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for r in &players.rows {
        let v: Vec<f64> = cols.iter().filter_map(|&c| r[c].trim().parse::<f64>().ok()).collect();
        if v.len() == cols.len() {
            let level = match overall {
                Some(o) => o.of(&v, r[pos].trim(), sides.and_then(|c| r[c].trim().parse().ok())),
                None => v.iter().sum::<f64>() / v.len() as f64,
            };
            out.entry(r[pos].trim().to_string()).or_default().push(level);
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    }
    Ok(out)
}

/// The value at quantile `q` (0..=1) of an ascending list.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let x = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let (i, f) = (x.floor() as usize, x.fract());
    sorted[i] + (sorted[(i + 1).min(sorted.len() - 1)] - sorted[i]) * f
}

/// Each value's quantile among `values` (ties share their middle rank), in the same order.
fn ranks(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n < 2 {
        return vec![0.5; n];
    }
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| values[a].partial_cmp(&values[b]).unwrap());
    let mut q = vec![0.0; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && values[idx[j + 1]] == values[idx[i]] {
            j += 1;
        }
        let mid = (i + j) as f64 / 2.0 / (n - 1) as f64;
        for &k in &idx[i..=j] {
            q[k] = mid;
        }
        i = j + 1;
    }
    q
}

/// Spread the imported players' overall levels like vanilla's (#31: FC's ratings sit too close
/// together, so stars, regulars and lower-league players looked alike). Per position group, a
/// player's quantile comes from his Transfermarkt value among the matched players, else from the
/// source's overall among all; his level becomes vanilla's level at that quantile, and his 17
/// ratings shift by the difference, keeping his own profile.
/// `live_add_missing`: each live squad row at one of our clubs that no roster row claimed becomes a
/// new player from Transfermarkt alone. The overall is the median of the roster's players of the
/// same line valued within 25% of his value (else the 5 nearest by value), so the level pass that
/// follows places him like them.
#[allow(clippy::too_many_arguments)]
fn add_live_missing(
    live: &Sheet,
    ours: &HashMap<String, Option<u32>>,
    claim: &HashMap<usize, (u32, usize)>,
    (lc, lplayer, lbirth, lvalue, lnum): (usize, usize, usize, Option<usize>, Option<usize>),
    tm: Option<(&Sheet, &TmConfig)>,
    tsp: Option<usize>,
    season_start: &str,
    min_value: f64,
    by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>,
    report: &mut Report,
) {
    let (lpid, tm_pid, tm_nat, tm_foot, tm_height) = match tm {
        Some((s, _)) => (live.col("player_id"), s.col("player_id"), s.col("country_of_citizenship"), s.col("foot"), s.col("height_in_cm")),
        None => (live.col("player_id"), None, None, None, None),
    };
    let tm_by_id: HashMap<&str, usize> = match (tm, tm_pid) {
        (Some((s, _)), Some(c)) => s.rows.iter().enumerate().map(|(i, r)| (r[c].trim(), i)).collect(),
        _ => HashMap::new(),
    };
    // (line, value, overall) of the roster's valued players, for the overall estimate
    let valued: Vec<(String, f64, f64)> = by_club.values().flat_map(|c| c.2.iter()).filter_map(|r| Some((r.position.clone(), r.tm_value?, r.overall))).filter(|v| v.2 > 0.0).collect();
    // every roster player's (surname, birth year), wherever he is: a live player with the same
    // surname born within a year is the roster's under another spelling, date or club (the first
    // fc27 re-import added 2,196, Alaba at Udinese and Ayew at Sheffield United among them)
    let known: HashMap<String, Vec<i32>> = by_club.values().flat_map(|c| c.2.iter()).fold(HashMap::new(), |mut m, x| {
        if let Some(y) = x.birth.get(..4).and_then(|y| y.parse::<i32>().ok()) {
            m.entry(normalise(&x.surname, &BTreeMap::new())).or_default().push(y);
        }
        m
    });
    // (line, overall, ratings) of every roster player, for the added players' rating profile
    let profiles: Vec<(String, f64, [u32; 17])> = by_club.values().flat_map(|c| c.2.iter()).filter(|r| r.overall > 0.0).map(|r| (r.position.clone(), r.overall, r.ratings)).collect();
    for (i, r) in live.rows.iter().enumerate() {
        if claim.contains_key(&i) {
            continue;
        }
        let Some(club) = ours.get(r[lc].trim()).copied().flatten() else { continue };
        let value = lvalue.and_then(|c| r[c].trim().parse::<f64>().ok()).filter(|v| *v > 0.0);
        if value.unwrap_or(0.0) < min_value {
            continue;
        }
        let tr = lpid.and_then(|c| tm_by_id.get(r[c].trim())).and_then(|&k| tm.map(|(s, _)| &s.rows[k]));
        // the line from Transfermarkt's sub-position (a winger is a midfielder who also plays up front)
        let (position, position2) = match (tr, tm, tsp) {
            (Some(tr), Some((_, tc)), Some(c)) => tc.positions.get(tr[c].trim()).map(|g| groups(g)).unwrap_or(("MID".into(), String::new())),
            _ => ("MID".into(), String::new()),
        };
        let mut near: Vec<f64> = match value {
            Some(v) => valued.iter().filter(|(g, x, _)| *g == position && *x >= v * 0.8 && *x <= v * 1.25).map(|e| e.2).collect(),
            None => Vec::new(),
        };
        if near.is_empty() {
            let mut by_gap: Vec<(f64, f64)> = valued.iter().filter(|(g, _, _)| *g == position).map(|(_, x, o)| ((x - value.unwrap_or(0.0)).abs(), *o)).collect();
            by_gap.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            near = by_gap.iter().take(5).map(|e| e.1).collect();
        }
        near.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let overall = near.get(near.len() / 2).copied().unwrap_or(55.0).round();
        let (forename, surname) = known_as(r[lplayer].trim());
        // the club already has someone with this surname and birth year: a spelling the live
        // step's name match didn't bridge (accents, a longer legal name), not a missing player
        let year = r[lbirth].trim().get(..4).and_then(|y| y.parse::<i32>().ok()).unwrap_or(0);
        let sk = normalise(&surname, &BTreeMap::new());
        // ... or someone at this club born the same day: the roster's known-as name ("Vini Jr." for
        // the live "Vinicius Junior")
        let born = r[lbirth].trim().get(..10).unwrap_or_default();
        let same_day = !born.is_empty() && by_club[&club].2.iter().any(|x| x.birth == born);
        if same_day || known.get(&sk).is_some_and(|ys| ys.iter().any(|y| (y - year).abs() <= 1)) {
            report.live_add_skipped.push(format!("{}: {}", r[lplayer].trim(), by_club[&club].0));
            continue;
        }
        // the rating profile of the 5 same-line players nearest his overall, averaged and shifted to
        // it (a flat profile made a 16-year-old winger tackle as well as he dribbles)
        let mut close: Vec<&(String, f64, [u32; 17])> = profiles.iter().filter(|e| e.0 == position).collect();
        close.sort_by(|a, b| (a.1 - overall).abs().partial_cmp(&(b.1 - overall).abs()).unwrap_or(std::cmp::Ordering::Equal));
        let ratings: [u32; 17] = if close.is_empty() {
            [overall as u32; 17]
        } else {
            let k = close.len().min(5);
            let mean: [f64; 17] = std::array::from_fn(|i| close[..k].iter().map(|e| e.2[i] as f64).sum::<f64>() / k as f64);
            let shift = overall - close[..k].iter().map(|e| e.1).sum::<f64>() / k as f64;
            std::array::from_fn(|i| (mean[i] + shift).round().clamp(1.0, 99.0) as u32)
        };
        let row = Row {
            forename: game_safe(&forename),
            surname: game_safe(&surname),
            age: age_on(r[lbirth].trim(), season_start),
            nation: tr.zip(tm_nat).map(|(tr, c)| tr[c].trim().to_string()).unwrap_or_default(),
            position,
            position2,
            overall,
            height: tr.zip(tm_height).and_then(|(tr, c)| tr[c].trim().parse::<f64>().ok()).filter(|h| *h > 0.0).map(|h| h.round() as u32),
            ratings,
            tm_value: value,
            market_value: value,
            fm_value: None,
            skin: None,
            hair: None,
            hair_colour: None,
            sides: None,
            foot: tr.zip(tm_foot).and_then(|(tr, c)| match tr[c].trim() {
                "right" => Some(1),
                "left" => Some(2),
                "both" => Some(3),
                _ => None,
            }),
            tm_id: lpid.map(|c| r[c].trim().to_string()).filter(|s| !s.is_empty()),
            transferred: false,
            tm_club: None,
            birth: r[lbirth].trim().chars().take(10).collect(),
            wage: None,
            contract_end: None,
            live_number: lnum.and_then(|c| r[c].trim().parse::<u32>().ok()).filter(|n| (1..=99).contains(n)),
        };
        report.live_added.push(format!("{} {}: {} ({})", row.forename, row.surname, by_club[&club].0, overall));
        by_club.get_mut(&club).unwrap().2.push(row);
    }
    report.live_added.sort();
}

fn level_remap(by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>, players: &Sheet, overall_weight: f64, game_overall: Option<&Overall>, from_source: bool, curve: &Scale) -> Result<()> {
    let levels = vanilla_levels(players, game_overall)?;
    for group in ["GK", "DEF", "MID", "ATT"] {
        let Some(target) = levels.get(group) else { continue };
        let rows: Vec<&mut Row> = by_club.values_mut().flat_map(|c| c.2.iter_mut()).filter(|r| r.position == group).collect();
        let valued: Vec<f64> = rows.iter().filter_map(|r| r.tm_value).collect();
        let overall: Vec<f64> = rows.iter().map(|r| r.overall).collect();
        let (qv, qo) = (ranks(&valued), ranks(&overall));
        let mut vi = 0;
        for (k, r) in rows.into_iter().enumerate() {
            let q = match r.tm_value {
                Some(_) => {
                    vi += 1;
                    (1.0 - overall_weight) * qv[vi - 1] + overall_weight * qo[k]
                }
                None => qo[k],
            };
            // the player's level as the game will see it: its own overall when the weights are
            // given (the maintainer, 11 Oct: Saka 76 in game, his FC 27 87), else the ratings' mean
            let level_of = |r: &Row| -> f64 {
                let ratings: Vec<f64> = r.ratings.iter().map(|&v| v as f64).collect();
                match game_overall {
                    Some(o) => o.of(&ratings, &r.position, r.sides),
                    None => ratings.iter().sum::<f64>() / 17.0,
                }
            };
            // the target: the source's own overall (`overall_from_source`: the game shows FC 27's
            // number), else vanilla's level at the player's quantile
            // (`[scale] curve` reshapes the source's overall: the top stretched, the bottom lowered)
            let goal = if from_source && game_overall.is_some() && r.overall > 0.0 { curve.curved(r.overall).min(99.0) } else { quantile(target, q) };
            // a shift clipped at 1 or 99 moves the overall less: repeat until it lands
            for _ in 0..4 {
                let shift = goal - level_of(r);
                if shift.abs() < 0.5 {
                    break;
                }
                for v in r.ratings.iter_mut() {
                    *v = (*v as f64 + shift).round().clamp(1.0, 99.0) as u32;
                }
            }
        }
    }
    Ok(())
}

/// A stored value for every player Transfermarkt didn't match: the median market value of the
/// matched players of his position group with the same source overall and a similar age (a
/// quantile between the 25th and 75th per player, so they are spread like the peers): first
/// within 2 years and 3 points of overall, then 4 years and 5 points, then any age within 10
/// points, until 5 peers are found. Without an age window a 22-year-old rated 50 got the median of
/// all 50s, most of them older and cheaper: 0.53-0.69x the matched under-30s of his band, and the
/// over-30s 1.2-2x (fc27final27, 21 Oct). Without a value the game prices him itself, from its own
/// rating, so a valued squad beside an unvalued one compared like two different economies (PC
/// check, 11 Oct: 1,245 of 7,276 day-1 players had none). Returns how many were filled.
/// The attribute source's transfer value (GBP) as the market value of a player Transfermarkt has
/// none for (the maintainer, 21 Oct: TM > FC27 > FM26): stored and levelled like a Transfermarkt value, so
/// it is kept in EUR terms (`value_scale` turns EUR into GBP at the store). Returns how many.
/// Before `fill_values`, which then estimates only the players neither source values.
/// FM's transfer value is the club's valuation, above Transfermarkt's market value (1.5-3x at big
/// clubs, win3 on #698), so a stand-in is divided by the median FM/TM ratio of the players who have
/// both, per 5-point band of the source overall (20 pairs or more, else every band's median), and
/// ranks beside his Transfermarkt-valued peers. Returns (stand-ins, [(band, ratio, pairs)]).
fn fm_values(by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>, value_scale: f64) -> (usize, Vec<(String, f64, usize)>) {
    if value_scale <= 0.0 {
        return (0, Vec::new());
    }
    // FM / TM (both in GBP) for the players with both, by band
    let mut pairs: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    for r in by_club.values().flat_map(|c| c.2.iter()) {
        if let (Some(tm), Some(fm)) = (r.market_value.filter(|v| *v > 0.0), r.fm_value.filter(|v| *v > 0.0)) {
            pairs.entry((r.overall / 5.0).floor() as i64 * 5).or_default().push(fm / (tm * value_scale));
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v[v.len() / 2]
    };
    let mut all: Vec<f64> = pairs.values().flatten().copied().collect();
    let overall = if all.is_empty() { 1.0 } else { median(&mut all) };
    let mut ratios: Vec<(String, f64, usize)> = vec![("all".to_string(), overall, all.len())];
    let mut by_band: BTreeMap<i64, f64> = BTreeMap::new();
    for (band, v) in pairs.iter_mut() {
        let r = if v.len() >= 20 { median(v) } else { overall };
        by_band.insert(*band, r);
        ratios.push((format!("{band}-{}", band + 4), r, v.len()));
    }
    let mut n = 0;
    for r in by_club.values_mut().flat_map(|c| c.2.iter_mut()) {
        if r.tm_value.is_none() {
            if let Some(gbp) = r.fm_value.filter(|v| *v > 0.0) {
                let ratio = by_band.get(&((r.overall / 5.0).floor() as i64 * 5)).copied().unwrap_or(overall).max(0.05);
                let eur = gbp / ratio / value_scale;
                (r.tm_value, r.market_value) = (Some(eur), Some(eur));
                n += 1;
            }
        }
    }
    (n, ratios)
}

fn fill_values(by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>) -> usize {
    let mut filled = 0;
    for group in ["GK", "DEF", "MID", "ATT"] {
        // (age, value) of the matched players by overall
        let mut by_overall: BTreeMap<i64, Vec<(Option<u32>, f64)>> = BTreeMap::new();
        for r in by_club.values().flat_map(|c| c.2.iter()).filter(|r| r.position == group) {
            if let Some(v) = r.market_value.filter(|v| *v > 0.0) {
                by_overall.entry(r.overall.round() as i64).or_default().push((r.age, v));
            }
        }
        for r in by_club.values_mut().flat_map(|c| c.2.iter_mut()).filter(|r| r.position == group && r.market_value.is_none()) {
            let o = r.overall.round() as i64;
            let mut peers: Vec<f64> = Vec::new();
            // (age window, widest overall window); None = any age
            'windows: for (years, widest) in [(Some(2u32), 3), (Some(4), 5), (None, 10)] {
                if years.is_some() && r.age.is_none() {
                    continue;
                }
                for w in 0..=widest {
                    let near = |a: &Option<u32>| match (years, r.age, *a) {
                        (None, _, _) => true,
                        (Some(y), Some(mine), Some(theirs)) => mine.abs_diff(theirs) <= y,
                        _ => false,
                    };
                    peers = by_overall.range(o - w..=o + w).flat_map(|(_, v)| v.iter().filter(|(a, _)| near(a)).map(|(_, v)| *v)).collect();
                    if peers.len() >= 5 {
                        break 'windows;
                    }
                }
            }
            if peers.is_empty() {
                continue;
            }
            peers.sort_by(f64::total_cmp);
            // spread like the peers themselves (the maintainer, 11 Oct: values not all the same): a quantile
            // between the 25th and 75th picked from the player's name and birth date, so the same
            // player gets the same value on every import
            let h = format!("{} {} {}", r.forename, r.surname, r.birth).bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
            let q = 0.25 + 0.5 * (h % 1000) as f64 / 999.0;
            r.market_value = Some(peers[((peers.len() - 1) as f64 * q).round() as usize]);
            filled += 1;
        }
    }
    filled
}

/// The year of a date cell (`2028-06-30 00:00:00`, `2028-06-30`).
fn year_of(cell: &str) -> Option<u32> {
    cell.trim().get(..4)?.parse().ok().filter(|y| (1990..2100).contains(y))
}

/// Run the import. `base` is a `db-export` directory; returns `players.csv` text and the report.
pub fn import(base: &Path, roster: &Path, fm_csv: Option<&Path>, tm_csv: Option<&Path>, cfg: &Config) -> Result<Imported> {
    import_with(base, roster, &Sources { fm: fm_csv, tm: tm_csv, ..Sources::default() }, cfg)
}

/// The optional source files besides the roster (each needs its mapping section).
#[derive(Clone, Copy, Default)]
pub struct Sources<'a> {
    /// FM attributes (`[fm]`).
    pub fm: Option<&'a Path>,
    /// Transfermarkt players (`[transfermarkt]`).
    pub tm: Option<&'a Path>,
    /// FM staff, for the managers (`[fm.managers]`).
    pub staff: Option<&'a Path>,
    /// Transfermarkt clubs, for the stadiums (`[transfermarkt.stadiums]`).
    pub tm_clubs: Option<&'a Path>,
    /// Transfermarkt match lineups, for the shirt numbers (`[transfermarkt.lineups]`).
    pub tm_lineups: Option<&'a Path>,
    /// Moves the sources don't have yet (`player,club` CSV, `apply_transfers`).
    pub transfers: Option<&'a Path>,
    /// Transfermarkt games, for the league histories (`[transfermarkt.history]`).
    pub tm_games: Option<&'a Path>,
    /// Live Transfermarkt squads (`tools/tm_live.py`): membership over the roster (the maintainer, 10 Oct).
    pub tm_live: Option<&'a Path>,
    /// Current managers (`club,manager` CSV, e.g. from Wikipedia's lists): over every other source.
    pub managers: Option<&'a Path>,
    /// Club transfer budgets (`club,budget_gbp` CSV, e.g. FC 27's career mode list): the clubs'
    /// funds (`TmConfig::wealth_budgets`) over the squad-value estimate.
    pub budgets: Option<&'a Path>,
    /// The game's overall weights (`Overall`): players are levelled on the overall the game computes.
    pub weights: Option<&'a Path>,
    /// Champions for seasons no other source covers (`country,tier,season,club,won,drawn,lost,for,
    /// against,points`, `season` = its starting year): Transfermarkt's games start in 2012/13 and the
    /// base's history ends in 2005/06, so 2006/07-2011/12 were empty (the maintainer, 12 Oct).
    pub history_extra: Option<&'a Path>,
    /// Transfermarkt's game events (`game_id,type,club_id` among its columns): a cup final with a
    /// shoot-out gets its score in play from the goals (Transfermarkt's goals count the kicks).
    pub tm_events: Option<&'a Path>,
    /// Cup finals no other source covers (`competition,season,winner,runner_up,winner_goals,
    /// runner_up_goals`; `competition` a Transfermarkt id of `[transfermarkt.history.cups]` or a
    /// cup's name, `season` its starting year; a club by name, or `#<id>` where a name is ambiguous).
    pub cup_extra: Option<&'a Path>,
}

/// `import`, with a staff export for the managers (`FmConfig::managers`) and Transfermarkt's clubs
/// file for the stadiums (`TmConfig::stadiums`).
pub fn import_with(base: &Path, roster: &Path, sources: &Sources, cfg: &Config) -> Result<Imported> {
    let Sources { fm: fm_csv, tm: tm_csv, staff: staff_csv, tm_clubs, tm_lineups, transfers, tm_games, managers: managers_csv, tm_live, budgets, weights, history_extra, tm_events, cup_extra } = *sources;
    let err = |e: lma_data::Error| e.to_string();
    let players = Sheet::read(&base.join("players.csv")).map_err(err)?;
    let clubs = Sheet::read(&base.join("clubs.csv")).map_err(err)?;
    let nations = Sheet::read(&base.join("nations.csv")).map_err(err)?;
    let mut src = Sheet::read(roster).map_err(err)?;
    let fc_rows = src.rows.len();
    let fm = match (fm_csv, &cfg.fm) {
        (Some(path), Some(fc)) => Some((Sheet::read(path).map_err(err)?, fc)),
        (Some(_), None) => return Err("--fm given but the mapping has no [fm] section".into()),
        _ => None,
    };
    // the fill's clubs' uids by name, for their managers
    let mut fill_uid_of: HashMap<String, String> = HashMap::new();
    if let Some((sheet, fill)) = fm.as_ref().and_then(|(sheet, fc)| Some((sheet, fc.fill.as_ref()?, fc))).map(|(s, f, fc)| (s, (f, fc))) {
        let (rows, uid_of) = fill_rows(&src, sheet, fill.0, fill.1, cfg)?;
        src.rows.extend(rows);
        fill_uid_of = uid_of;
    }
    let src = src;
    let ab = &cfg.abbreviations;
    let col = |s: &Sheet, name: &str| s.col(name).ok_or_else(|| format!("column {name:?} missing"));

    // LMA clubs by normalised name; not the squadless historic sides (FA XI, the 1880s Wimbledon),
    // which no league plays
    let (cid, cname) = (col(&clubs, "id")?, col(&clubs, "name_E")?);
    let squadless = |r: &[String]| clubs.col("squad_size").is_some_and(|i| r[i].trim() == "0");
    let mut lma_clubs: HashMap<String, (u32, String)> = HashMap::new();
    let mut lma_by_id: HashMap<u32, (u32, String)> = HashMap::new();
    for r in &clubs.rows {
        let name = r[cname].trim();
        if !name.is_empty() && !squadless(r) {
            let id: u32 = r[cid].parse().map_err(|_| format!("bad club id {:?}", r[cid]))?;
            lma_clubs.entry(normalise(name, ab)).or_insert((id, name.to_string()));
            lma_by_id.insert(id, (id, name.to_string()));
        }
    }
    // an alias "#id" names its club by id; one that names no club is a mapping error
    for (from, to) in &cfg.club_aliases {
        if let Some(id) = to.strip_prefix('#') {
            if !id.parse().is_ok_and(|id: u32| lma_by_id.contains_key(&id)) {
                return Err(format!("club_aliases: {from:?} = {to:?} names no LMA club with a squad"));
            }
        }
    }
    // LMA nations by normalised name
    let (nid, nname) = (col(&nations, "nation_id")?, col(&nations, "name")?);
    let lma_nations: HashMap<String, u32> =
        nations.rows.iter().filter_map(|r| Some((normalise(&r[nname], &BTreeMap::new()), r[nid].parse().ok()?))).collect();

    // roster rows grouped by LMA club
    let c = &cfg.columns;
    let sc = |name: &str| col(&src, name);
    let (fore, sur, birth, club, nat, pos, overall) = (sc(&c.forename)?, sc(&c.surname)?, sc(&c.birthdate)?, sc(&c.club)?, sc(&c.nationality)?, sc(&c.position)?, sc(&c.overall)?);
    let common = c.common_name.as_deref().and_then(|n| src.col(n));
    let league = c.league.as_deref().and_then(|n| src.col(n));
    let alt = c.alt_positions.as_deref().and_then(|n| src.col(n));
    let height = c.height_cm.as_deref().and_then(|n| src.col(n));
    let (foot_col, weak_col) = (c.foot.as_deref().and_then(|n| src.col(n)), c.weak_foot.as_deref().and_then(|n| src.col(n)));
    let filter = match (&cfg.filter_column, &cfg.filter_value) {
        (Some(k), Some(v)) => Some((sc(k)?, v.clone())),
        _ => None,
    };
    let rating_value = |r: &[String], s: &Source| -> Result<f64> {
        match s {
            Source::Constant(v) => Ok(*v),
            Source::Columns(cols) => {
                let mut sum = 0.0;
                for name in cols {
                    let i = src.col(name).ok_or_else(|| format!("rating column {name:?} missing"))?;
                    sum += r[i].trim().parse::<f64>().unwrap_or(0.0);
                }
                Ok(sum / cols.len().max(1) as f64)
            }
        }
    };

    // attribute source: rows indexed by (birth date, surname word)
    let mut fm_index: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let fm_skin = match fm.as_ref().and_then(|(sheet, fc)| fc.skin.as_ref().map(|sk| (sheet, sk))) {
        Some((sheet, sk)) => {
            if sk.lma.iter().any(|v| *v > 3) {
                return Err("[fm.skin] lma values must be 0-3".into());
            }
            // an export without the column (plain fmsave) leaves the nation default
            sheet.col(&sk.column).map(|c| (c, &sk.lma))
        }
        None => None,
    };
    let fm_cols = match &fm {
        Some((sheet, fc)) => {
            let need = |n: &str| sheet.col(n).ok_or_else(|| format!("attribute source column {n:?} missing"));
            let (f, l, b) = (need(&fc.first_name)?, need(&fc.last_name)?, need(&fc.birth_date)?);
            let cn = fc.common_name.as_deref().and_then(|n| sheet.col(n));
            let h = fc.height_cm.as_deref().and_then(|n| sheet.col(n));
            for (i, r) in sheet.rows.iter().enumerate() {
                let mut words: Vec<String> = name_words(&r[l]);
                if let Some(c) = cn {
                    words.extend(name_words(&r[c]));
                }
                words.sort();
                words.dedup();
                for w in words {
                    fm_index.entry((r[b].trim().to_string(), w)).or_default().push(i);
                }
            }
            Some((f, l, cn, h))
        }
        None => None,
    };
    let fm_value = |r: &[String], s: &Source| -> Result<f64> {
        let Some((sheet, _)) = &fm else { return Ok(0.0) };
        match s {
            Source::Constant(v) => Ok(*v),
            Source::Columns(cols) => {
                let mut sum = 0.0;
                for name in cols {
                    let i = sheet.col(name).ok_or_else(|| format!("attribute source column {name:?} missing"))?;
                    sum += r[i].trim().parse::<f64>().unwrap_or(0.0);
                }
                Ok(sum / cols.len().max(1) as f64)
            }
        }
    };

    // Transfermarkt: rows indexed by (birth date, surname word), as the attribute source
    let tm = match (tm_csv, &cfg.transfermarkt) {
        (Some(path), Some(tc)) => Some((Sheet::read(path).map_err(err)?, tc)),
        (Some(_), None) => return Err("--tm given but the mapping has no [transfermarkt] section".into()),
        _ => None,
    };
    let mut tm_index: HashMap<(String, String), Vec<usize>> = HashMap::new();
    let tm_cols = match &tm {
        Some((sheet, tc)) => {
            let need = |n: &str| sheet.col(n).ok_or_else(|| format!("transfermarkt column {n:?} missing"));
            let (ln, nm, b, v) = (need(&tc.last_name)?, need(&tc.name)?, need(&tc.birth_date)?, need(&tc.value)?);
            let sp = tc.sub_position.as_deref().and_then(|n| sheet.col(n));
            let h = tc.height_cm.as_deref().and_then(|n| sheet.col(n));
            for (i, r) in sheet.rows.iter().enumerate() {
                let birth: String = r[b].trim().chars().take(10).collect();
                let mut words = name_words(&r[ln]);
                words.extend(name_words(&r[nm]));
                words.sort();
                words.dedup();
                for w in words {
                    tm_index.entry((birth.clone(), w)).or_default().push(i);
                }
            }
            Some((nm, v, sp, h))
        }
        None => None,
    };

    let mut by_club: BTreeMap<u32, (String, String, Vec<Row>)> = BTreeMap::new();
    let mut league_of: HashMap<u32, String> = HashMap::new();
    let mut report = Report::default();
    let mut unmatched: BTreeMap<String, usize> = BTreeMap::new();
    let wanted = |r: &[String]| {
        filter.as_ref().is_none_or(|(i, v)| r[*i] == *v) && league.is_none_or(|l| cfg.leagues.is_empty() || cfg.leagues.iter().any(|x| *x == r[l]))
    };
    // each source club's LMA club, and one source club per LMA club: an exact or aliased name beats
    // a word match (FC 27's "R. Racing Club" is Santander, not LMA's Argentine Racing Club), and
    // otherwise the first in the file keeps it
    let mut target_of: HashMap<String, Option<(u32, String)>> = HashMap::new();
    let mut claim: HashMap<u32, (bool, String)> = HashMap::new();
    // LMA clubs the roster itself fills: the attribute source's players never join them
    let mut roster_clubs: HashSet<u32> = HashSet::new();
    for (k, r) in src.rows.iter().enumerate().filter(|(_, r)| wanted(r)) {
        let club_name = r[club].trim();
        if k >= fc_rows && club_name.is_empty() {
            continue; // a free agent
        }
        if target_of.contains_key(club_name) {
            if k < fc_rows {
                if let Some(Some((id, _))) = target_of.get(club_name) {
                    roster_clubs.insert(*id);
                }
            }
            continue;
        }
        let target = cfg.club_aliases.get(club_name).map(String::as_str).unwrap_or(club_name);
        let exact = match target.strip_prefix('#') {
            Some(id) => id.parse().ok().and_then(|id: u32| lma_by_id.get(&id).cloned()),
            None => Some(target).filter(|t| !t.trim().is_empty()).and_then(|t| lma_clubs.get(&normalise(t, ab)).cloned()),
        };
        let hit = match exact {
            Some(h) => Some((h, true)),
            None => Some(target).filter(|t| !t.trim().is_empty() && !t.starts_with('#')).and_then(|t| word_match(t, &lma_clubs, ab)).map(|h| (h, false)),
        };
        if let Some(((id, _), is_exact)) = &hit {
            let fill = k >= fc_rows;
            if (!fill && claim.get(id).is_none_or(|(held, _)| *is_exact && !held)) || (fill && !claim.contains_key(id)) {
                claim.insert(*id, (*is_exact, club_name.to_string()));
            }
            if !fill {
                roster_clubs.insert(*id);
            }
        }
        target_of.insert(club_name.to_string(), hit.map(|(h, _)| h));
    }
    let mut free_rows: Vec<Row> = Vec::new();
    let mut fill_unmatched = 0;
    // the attribute source's clubs LMA lacks, by name: candidates to take over a club
    let mut fm_other: BTreeMap<String, Vec<Row>> = BTreeMap::new();
    // the roster's clubs LMA lacks in a ladder's league (Bromley, Girona, Como): placed in it
    let ladder_leagues: HashSet<&str> = cfg.ladders.iter().flat_map(|l| l.leagues.iter()).map(String::as_str).filter(|l| !l.is_empty()).collect();
    let mut roster_new: BTreeMap<String, (String, Vec<Row>)> = BTreeMap::new();
    // how many of each such club's players each FM club (uid) has: its manager's club
    let mut new_club_votes: HashMap<String, HashMap<String, usize>> = HashMap::new();
    // the Transfermarkt club of each source club's matched players (source club -> TM id -> count)
    let mut tm_club_votes: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let tm_club_col = tm.as_ref().and_then(|(sheet, tc)| sheet.col(tc.club.as_deref()?));
    let tm_contract_col = tm.as_ref().and_then(|(sheet, tc)| sheet.col(tc.contract_end.as_deref()?));
    let fm_contract_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(fc.contract_end.as_deref()?));
    let tm_id_col = tm.as_ref().and_then(|(sheet, tc)| sheet.col(tc.id.as_deref()?));
    let fm_club_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(&fc.fill.as_ref()?.club));
    let fm_wage_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(fc.wage.as_ref()?));
    let fm_value_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(fc.value.as_ref()?));
    let fm_hair = fm.as_ref().and_then(|(sheet, fc)| fc.hair.as_ref().map(|h| (h, sheet.col(&h.colour), sheet.col(&h.kind), sheet.col(&h.length))));
    let (fc_first, fc_last) = fm.as_ref().map(|(sheet, fc)| (sheet.col(&fc.first_name).unwrap_or(0), sheet.col(&fc.last_name).unwrap_or(0))).unwrap_or((0, 0));
    let fm_club_uid_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(fc.fill.as_ref()?.club_uid.as_deref()?));
    let mut roster_fm_clubs: HashMap<String, usize> = HashMap::new();
    // per LMA club the roster fills, how many of its players each FM club (uid) has: its manager's
    let mut fm_club_votes: HashMap<u32, HashMap<String, usize>> = HashMap::new();
    let takeover = cfg.fm.as_ref().and_then(|fc| fc.fill.as_ref()).is_some_and(|f| f.takeover);
    // FM nation id -> FC nationality name, counted over the players both sources have
    let mut fm_nation_names: HashMap<String, HashMap<String, usize>> = HashMap::new();
    let fm_nation_col = fm.as_ref().and_then(|(sheet, fc)| sheet.col(&fc.fill.as_ref()?.nation));
    for (k, r) in src.rows.iter().enumerate() {
        if !wanted(r) {
            continue;
        }
        let fill = k >= fc_rows;
        let club_name = r[club].trim();
        // (LMA club id, name); None for a free agent from the attribute source, or (`other`) a
        // club of it that LMA lacks
        let mut other = false;
        let mut roster_club_new = false;
        let target: Option<(u32, String)> = if fill && club_name.is_empty() {
            None
        } else {
            match target_of.get(club_name).cloned().flatten() {
                Some((id, name)) if claim[&id].1 == club_name && !(fill && roster_clubs.contains(&id)) => Some((id, name)),
                Some((id, name)) if !fill => {
                    *unmatched.entry(format!("{club_name} (LMA's {name} is {}'s)", claim[&id].1)).or_default() += 1;
                    continue;
                }
                None if !fill => {
                    *unmatched.entry(club_name.to_string()).or_default() += 1;
                    if !league.is_some_and(|l| ladder_leagues.contains(r[l].trim())) {
                        continue;
                    }
                    roster_club_new = true;
                    None
                }
                None if takeover => {
                    other = true;
                    None
                }
                _ => {
                    fill_unmatched += 1;
                    continue;
                }
            }
        };
        let group = |label: &str| cfg.positions.get(label.trim()).map(|g| groups(g));
        let (position, second) = group(&r[pos]).unwrap_or_else(|| ("MID".into(), String::new()));
        let position2 = match alt {
            _ if !second.is_empty() => second,
            // the alternates' own lines first, then their second lines (Rogers' LW: a midfielder
            // who also plays up front); a CM who also plays RB stays a defender second
            Some(a) => {
                let alts: Vec<(String, String)> = r[a].split_whitespace().filter_map(group).collect();
                let differs = |g: &String| !g.is_empty() && *g != position && g != "GK";
                alts.iter().map(|g| &g.0).chain(alts.iter().map(|g| &g.1)).find(|g| differs(g)).cloned().unwrap_or_default()
            }
            None => String::new(),
        };
        let table = if position == "GK" { &cfg.gk_ratings } else { &cfg.ratings };
        let mut ratings = [0u32; 17];
        for (k, name) in LMA_RATINGS.iter().enumerate() {
            let s = table.get(*name).ok_or_else(|| format!("no mapping for rating {name:?}"))?;
            let v = rating_value(r, s)? * cfg.scale.mul + cfg.scale.add;
            ratings[k] = v.round().clamp(1.0, 99.0) as u32;
        }
        let mut height_cm = height.and_then(|i| r[i].trim().parse().ok());
        let mut skin = None;
        let (mut hair, mut hair_colour) = (None, None);
        let mut wage = None;
        let mut fm_transfer_value: Option<f64> = None;
        let mut fm_contract_end: Option<u32> = None;
        // attribute source: same birth date and a shared surname word (forename breaks ties)
        if let (Some((sheet, fc)), Some((ff, _, _, fh))) = (&fm, fm_cols) {
            let birthdate = r[birth].trim().to_string();
            let mut words = name_words(&r[sur]);
            if let Some(c) = common {
                words.extend(name_words(&r[c]));
            }
            let mut candidates: Vec<usize> = words.iter().filter_map(|w| fm_index.get(&(birthdate.clone(), w.clone()))).flatten().copied().collect();
            candidates.sort();
            candidates.dedup();
            let first = name_words(&r[fore]);
            let pick = match candidates.len() {
                0 => None,
                1 => Some(candidates[0]),
                _ => candidates.iter().copied().find(|&i| name_words(&sheet.rows[i][ff]).iter().any(|w| first.contains(w))),
            };
            match pick {
                Some(i) => {
                    let fr = &sheet.rows[i];
                    let table = if position == "GK" { &fc.gk_ratings } else { &fc.ratings };
                    for (k, name) in LMA_RATINGS.iter().enumerate() {
                        let s = table.get(*name).ok_or_else(|| format!("[fm] has no mapping for rating {name:?}"))?;
                        let v = fm_value(fr, s)? * fc.scale.mul + fc.scale.add;
                        ratings[k] = v.round().clamp(1.0, 99.0) as u32;
                    }
                    if let Some(h) = fh.and_then(|h| fr[h].trim().parse::<u32>().ok()).filter(|&h| h > 0) {
                        height_cm = Some(h);
                    }
                    if let Some(c) = fm_wage_col {
                        wage = fr[c].trim().parse::<f64>().ok().filter(|w| *w > 0.0).map(|w| w.round() as u32);
                    }
                    if let Some(c) = fm_value_col {
                        fm_transfer_value = fr[c].trim().replace(',', "").parse::<f64>().ok().filter(|v| *v > 0.0);
                    }
                    fm_contract_end = fm_contract_col.and_then(|c| year_of(&fr[c]));
                    if let Some((c, lma)) = fm_skin {
                        skin = fr[c].trim().parse::<usize>().ok().filter(|t| *t > 0).and_then(|t| lma.get(t - 1).copied());
                        report.fm_skin += skin.is_some() as usize;
                    }
                    if let Some((h, cc, ck, cl)) = fm_hair {
                        let num = |c: Option<usize>| c.and_then(|c| fr[c].trim().parse::<u32>().ok());
                        let seed = format!("{} {} hair", fr[fc_first].trim(), fr[fc_last].trim()).bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
                        (hair, hair_colour) = h.map(num(cc), num(ck), num(cl), seed);
                        report.fm_hair += (hair.is_some() || hair_colour.is_some()) as usize;
                    }
                    if let (false, Some(c)) = (fill, fm_nation_col) {
                        *fm_nation_names.entry(fr[c].trim().to_string()).or_default().entry(r[nat].trim().to_string()).or_default() += 1;
                    }
                    // the attribute source's club of a roster player: that club is the roster's
                    // already, whatever its name there ("Blu-neri Milano" is FC 27's Inter)
                    if let (false, Some(c)) = (fill, fm_club_col) {
                        *roster_fm_clubs.entry(fr[c].trim().to_string()).or_default() += 1;
                        if let (Some((id, _)), Some(u)) = (&target, fm_club_uid_col) {
                            *fm_club_votes.entry(*id).or_default().entry(fr[u].trim().to_string()).or_default() += 1;
                        }
                        if let (true, Some(u)) = (roster_club_new, fm_club_uid_col) {
                            *new_club_votes.entry(club_name.to_string()).or_default().entry(fr[u].trim().to_string()).or_default() += 1;
                        }
                    }
                    report.fm_matched += 1;
                }
                None => report.fm_unmatched += 1,
            }
        }
        // Transfermarkt: same birth date and a shared surname word (forename breaks ties)
        let mut tm_value = None;
        let mut tm_contract_end: Option<u32> = None;
        let mut tm_name: Option<String> = None;
        let mut tm_id: Option<String> = None;
        let mut tm_club: Option<String> = None;
        let (mut position, mut position2) = (position, position2);
        if let (Some((sheet, tc)), Some((tn, tv, tsp, th))) = (&tm, tm_cols) {
            let birthdate = r[birth].trim().to_string();
            let mut words = name_words(&r[sur]);
            if let Some(c) = common {
                words.extend(name_words(&r[c]));
            }
            let mut candidates: Vec<usize> = words.iter().filter_map(|w| tm_index.get(&(birthdate.clone(), w.clone()))).flatten().copied().collect();
            candidates.sort();
            candidates.dedup();
            let first = name_words(&r[fore]);
            // a surname word alone never matches across the goalkeeper line: a Tarazona goalkeeper
            // (Alejandro Fernandez Sainz-Pardo, "Amigo") born the same day as Enzo Fernandez took
            // his EUR 80m (fc27final24); a shared forename still does (one person, two sources
            // disagreeing on his line)
            let gk = position == "GK";
            let before = candidates.len();
            candidates.retain(|&i| {
                let other_line = tsp.and_then(|c| tc.positions.get(sheet.rows[i][c].trim())).is_some_and(|g| (groups(g).0 == "GK") != gk);
                !other_line || name_words(&sheet.rows[i][tn]).iter().any(|w| first.contains(w))
            });
            report.tm_other_line += before - candidates.len();
            let pick = match candidates.len() {
                0 => None,
                1 => Some(candidates[0]),
                _ => candidates.iter().copied().find(|&i| name_words(&sheet.rows[i][tn]).iter().any(|w| first.contains(w))),
            };
            match pick {
                Some(i) => {
                    let tr = &sheet.rows[i];
                    tm_value = tr[tv].trim().parse::<f64>().ok().filter(|v| *v > 0.0);
                    tm_name = Some(tr[tn].trim().to_string()).filter(|n| !n.is_empty());
                    tm_id = tm_id_col.map(|c| tr[c].trim().to_string()).filter(|n| !n.is_empty());
                    tm_club = tm_club_col.map(|c| tr[c].trim().to_string()).filter(|n| !n.is_empty());
                    tm_contract_end = tm_contract_col.and_then(|c| year_of(&tr[c]));
                    // the line from Transfermarkt's sub-position; the source's own group, when
                    // different, becomes the second position (a winger who plays up front)
                    // (or the mapping's own second: a winger is a midfielder who also plays up front),
                    // never across the goalkeeper line: the rating table was already chosen by the
                    // source's position, and LMA has no goalkeeper as a second position
                    if let Some((g, second)) = tsp.and_then(|c| tc.positions.get(tr[c].trim())).map(|g| groups(g)).filter(|(g, _)| g != "GK" && position != "GK") {
                        if g != position {
                            position2 = std::mem::replace(&mut position, g);
                        }
                        if !second.is_empty() && second != "GK" {
                            position2 = second;
                        }
                        if position2 == position {
                            position2.clear();
                        }
                    }
                    if height_cm.is_none() {
                        height_cm = th.and_then(|c| tr[c].trim().parse::<f64>().ok()).filter(|h| *h > 0.0).map(|h| h.round() as u32);
                    }
                    if let Some(c) = tm_club_col.filter(|&c| !tr[c].trim().is_empty()) {
                        *tm_club_votes.entry(club_name.to_string()).or_default().entry(tr[c].trim().to_string()).or_default() += 1;
                    }
                    report.tm_matched += 1;
                }
                None => report.tm_unmatched += 1,
            }
        }
        // the name players are known by (#31 item 4): the source's single-name form, else
        // Transfermarkt's name (its known-as form: "David Raya", not the legal "Raya Martin"),
        // else the source's first and last names
        let (forename, surname) = match common.map(|i| r[i].trim()).filter(|s| !s.is_empty()) {
            Some(single) => (String::new(), game_safe(single)),
            None => match tm_name.as_deref() {
                Some(n) => {
                    let (f, l) = known_as(n);
                    (game_safe(&f), game_safe(&l))
                }
                None => (game_safe(r[fore].trim()), game_safe(r[sur].trim())),
            },
        };
        let row = Row {
            forename,
            surname,
            age: age_on(r[birth].trim(), &cfg.season_start),
            // a filled player's nation is FM's id: named by the players both sources have
            nation: match r[nat].trim().strip_prefix("fm:") {
                Some(id) => fm_nation_names.get(id).and_then(|n| n.iter().max_by_key(|(name, c)| (**c, std::cmp::Reverse((*name).clone())))).map(|(name, _)| name.clone()).unwrap_or_default(),
                None => r[nat].trim().to_string(),
            },
            position,
            position2,
            overall: r[overall].trim().parse().unwrap_or(0.0),
            height: height_cm,
            ratings,
            tm_value,
            market_value: tm_value,
            fm_value: fm_transfer_value,
            skin,
            hair,
            hair_colour,
            sides: sides_of(std::iter::once(r[pos].as_str()).chain(alt.map(|a| r[a].split_whitespace()).into_iter().flatten()), &cfg.sides),
            foot: foot_col.and_then(|f| foot_of(&r[f], weak_col.and_then(|w| r[w].trim().parse().ok()), cfg.both_feet_at)),
            tm_id,
            transferred: false,
            tm_club,
            birth: r[birth].trim().chars().take(10).collect(),
            live_number: None,
            wage,
            contract_end: tm_contract_end.or(fm_contract_end),
        };
        if let Some(v) = row.tm_value.filter(|v| *v >= 20_000_000.0) {
            let average = row.ratings.iter().sum::<u32>() as f64 / row.ratings.len().max(1) as f64;
            if average < 50.0 {
                let club = target.as_ref().map_or(club_name, |(_, n)| n.as_str());
                report.value_outliers.push(format!("{} {} ({club}): EUR {:.1}m, average rating {average:.0}", row.forename, row.surname, v / 1e6).trim().to_string());
            }
        }
        let Some((club_id, lma_name)) = target else {
            if other {
                fm_other.entry(club_name.to_string()).or_default().push(row);
            } else if roster_club_new {
                roster_new.entry(club_name.to_string()).or_insert_with(|| (league.map(|l| r[l].trim().to_string()).unwrap_or_default(), Vec::new())).1.push(row);
            } else {
                free_rows.push(row);
            }
            continue;
        };
        if let Some(l) = league {
            league_of.entry(club_id).or_insert_with(|| r[l].trim().to_string());
        }
        if fill {
            report.fill_players += 1;
        }
        by_club.entry(club_id).or_insert_with(|| (club_name.to_string(), lma_name, Vec::new())).2.push(row);
    }
    // a filled club needs a squad: FM's non-league sides can have a player or two
    by_club.retain(|id, (_, _, rows)| {
        let keep = roster_clubs.contains(id) || rows.len() >= cfg.min_squad;
        if !keep {
            report.fill_players -= rows.len();
        }
        keep
    });
    report.fill_clubs = by_club.keys().filter(|id| !roster_clubs.contains(id)).count();
    let _ = fill_unmatched;
    // clubs the roster's own players play for in the attribute source are the roster's clubs under
    // another name: never a takeover (3 shared players tell; one or two could be transfers)
    fm_other.retain(|name, _| roster_fm_clubs.get(name).is_none_or(|n| *n < 3));
    let live_sheet = tm_live.map(Sheet::read).transpose().map_err(err)?;
    // live clubs of a ladder competition that LMA has no record of (#826: 9 of the National
    // League's 24, the 2006 database ends at its Conference) come in as new clubs, like the
    // roster's, with their FM squads, in the league the ladder names for that competition
    if let Some(live) = live_sheet.as_ref() {
        let key = |n: &str| normalise(n, &cfg.abbreviations);
        let mut known: HashSet<String> = clubs.col("name_E").map(|n| clubs.rows.iter().map(|r| key(r[n].trim())).collect()).unwrap_or_default();
        known.extend(cfg.club_aliases.keys().map(|a| key(a)));
        known.extend(by_club.values().map(|c| key(&c.0)));
        known.extend(roster_new.keys().map(|n| key(n)));
        let fm_by_key: HashMap<String, String> = fm_other.keys().map(|n| (key(n), n.clone())).collect();
        if let (Some(lc), Some(lname), Some(lcomp)) = (live.col("club_id"), live.col("club"), live.col("comp")) {
            // a live club the membership step below resolves to one of ours (by its players, or by
            // a name word: 'Atalanta BC' is LMA's Atalanta) is never new
            let (ours, by_word) = live_club_ids(live, lc, lname, &by_club, &tm_key_of(&by_club, &tm_club_votes), &HashMap::new(), &roster_clubs, cfg);
            let mut seen: HashSet<String> = HashSet::new();
            for r in &live.rows {
                let (cid, cname, comp) = (r[lc].trim(), r[lname].trim(), r[lcomp].trim());
                let k = key(cname);
                if k.is_empty() || known.contains(&k) || ours.get(cid).copied().flatten().is_some() || by_word.contains_key(cid) || !seen.insert(k.clone()) {
                    continue;
                }
                let league = cfg.ladders.iter().find_map(|l| l.comps.iter().position(|c| c == comp).and_then(|i| l.leagues.get(i)).filter(|s| !s.is_empty()).cloned());
                let Some(league) = league else { continue };
                match fm_by_key.get(&k).and_then(|fm| fm_other.remove(fm)).filter(|rows| rows.len() >= cfg.min_squad) {
                    Some(rows) => {
                        report.live_new_clubs.push(format!("{cname} ({league}, {} players)", rows.len()));
                        roster_new.insert(cname.to_string(), (league, rows));
                    }
                    None => report.live_new_missing.push(format!("{cname} ({comp})")),
                }
            }
        }
    }
    // the re-seat first (its fallback places are where the roster's new clubs belong), then those
    // clubs, then the takeovers of what's left
    let (new_clubs, retirees) = new_club_claims(&clubs, &players, &by_club, roster_new, cfg);
    let new_leagues: Vec<String> = new_clubs.iter().map(|c| c.1.clone()).collect();
    // the live Transfermarkt squads (used for membership below): each of our clubs' live
    // competition places it where a ladder names `comps` (#699: Scotland's second tier, which
    // FC 27 lacks; Nancy and Nantes in Ligue 2 where FC 27 had them). Matched before the re-seat,
    // on the base ids, by the players' votes and the clubs' names (no takeover names yet).
    let mut comp_of: HashMap<u32, String> = HashMap::new();
    if let Some(live) = live_sheet.as_ref().filter(|_| cfg.ladders.iter().any(|l| l.comps.iter().any(|c| !c.is_empty()))) {
        let path = tm_live.unwrap();
        let col = |n: &str| live.col(n).ok_or_else(|| format!("{}: no {n} column", path.display()));
        let (lc, lname, lcomp) = (col("club_id")?, col("club")?, col("comp")?);
        let (mut ours, by_word) = live_club_ids(live, lc, lname, &by_club, &tm_key_of(&by_club, &tm_club_votes), &HashMap::new(), &roster_clubs, cfg);
        // a live club no squad names: every LMA club's name (clubs.csv: a club whose FM squad came in
        // under another name isn't in by_club), with `[club_aliases]`, then a name-word candidate
        // (`by_word`) when the LMA club plays in the competition's country (EA Guingamp, Dijon FCO)
        let key = |n: &str| normalise(n, &cfg.abbreviations);
        let (ccid, ccname, cccountry) = (clubs.col("id"), clubs.col("name_E"), clubs.col("country"));
        let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
        let mut country_of: HashMap<u32, u32> = HashMap::new();
        let mut lma_name_of: HashMap<u32, String> = HashMap::new();
        if let (Some(i), Some(n), Some(c)) = (ccid, ccname, cccountry) {
            for r in &clubs.rows {
                if let (Ok(id), Ok(country)) = (r[i].trim().parse::<u32>(), r[c].trim().parse::<u32>()) {
                    by_name.entry(key(r[n].trim())).or_default().push(id);
                    country_of.insert(id, country);
                    lma_name_of.insert(id, r[n].trim().to_string());
                }
            }
        }
        // not a source club LMA lacks (AFC Wimbledon takes a shell as a new club; by name it is
        // 2006's dead Wimbledon record, which must not claim a League One place as well)
        let new_keys: HashSet<String> = new_clubs.iter().map(|c| key(&c.0)).collect();
        let mut how: HashMap<String, &str> = HashMap::new();
        for r in &live.rows {
            let (cid, cname, comp) = (r[lc].trim(), r[lname].trim(), r[lcomp].trim());
            if ours.get(cid).copied().flatten().is_some() || how.contains_key(cid) || new_keys.contains(&key(cname)) {
                continue;
            }
            let Some(ladder) = cfg.ladders.iter().find(|l| l.comps.iter().any(|c| c == comp)) else { continue };
            let names = [Some(key(cname)), cfg.club_aliases.get(cname).map(|a| key(a))];
            let named: Vec<u32> = names.iter().flatten().filter_map(|k| by_name.get(k)).flatten().copied().collect();
            let found = match named.as_slice() {
                [id] => Some((*id, "by name")),
                // the word must be the LMA club's whole name ("Guingamp" in "EA Guingamp"; not Juventus
        // for "SS Juve Stabia") and the club must play in the competition's country
        [] => by_word
            .get(cid)
            .copied()
            .filter(|id| country_of.get(id) == Some(&ladder.country))
            .filter(|id| {
                let lk = lma_name_of.get(id).map(|n| key(n)).unwrap_or_default();
                !lk.is_empty() && cname.split(|c: char| c.is_whitespace() || c == '-' || c == '.').map(key).any(|w| w == lk)
            })
            .map(|id| (id, "by a name word")),
                _ => None,
            };
            if let Some((id, way)) = found {
                ours.insert(cid.to_string(), Some(id));
                how.insert(cid.to_string(), way);
            }
        }
        let mut seen: HashSet<String> = HashSet::new();
        for r in &live.rows {
            let (cid, comp) = (r[lc].trim(), r[lcomp].trim());
            if let Some(Some(id)) = ours.get(cid) {
                comp_of.entry(*id).or_insert_with(|| comp.to_string());
            }
            if seen.insert(cid.to_string()) && cfg.ladders.iter().any(|l| l.comps.iter().any(|c| c == comp)) {
                let who = match ours.get(cid).copied().flatten() {
                    Some(id) => format!("{} ({id}){}", by_club.get(&id).map(|c| c.1.as_str()).or_else(|| lma_name_of.get(&id).map(String::as_str)).unwrap_or("?"), how.get(cid).map_or(String::new(), |w| format!(", {w}"))),
                    None => "unmatched".to_string(),
                };
                report.live_comp_clubs.push(format!("{comp}: {} -> {who}", r[lname].trim()));
            }
        }
    }
    let (moves, new_id, new_at) = club_divisions(&clubs, cfg, &league_of, &comp_of, &new_leagues, &retirees, &mut report)?;
    let name_of: HashMap<u32, String> = clubs
        .col("id")
        .zip(clubs.col("name_E"))
        .map(|(i, n)| clubs.rows.iter().filter_map(|r| Some((r[i].trim().parse().ok()?, r[n].trim().to_string()))).collect())
        .unwrap_or_default();
    // the new clubs placed: their squads on their shells' records, their names at their ids
    let mut new_names: Vec<(u32, String, String, String)> = Vec::new();
    let mut new_clubs: Vec<Option<(String, String, Vec<Row>)>> = new_clubs.into_iter().map(Some).collect();
    for (k, to, shell) in new_at {
        let Some((club, league, rows)) = new_clubs[k].take() else { continue };
        let full = game_safe(&club);
        let shown: String = full.chars().take(25).collect();
        let short: String = full.split_whitespace().find(|w| w.chars().count() >= 3).unwrap_or(&full).chars().take(7).collect();
        report.new_clubs.push((club.clone(), league, name_of.get(&to).cloned().unwrap_or_default()));
        by_club.insert(shell, (club.clone(), name_of.get(&shell).cloned().unwrap_or_default(), rows));
        new_names.push((to, shown, short, club));
    }
    let mut takeover_names = take_over(&clubs, &players, &mut by_club, fm_other, fm.as_ref().map(|(sheet, fc)| (sheet, *fc)), &lma_nations, cfg, &mut report);
    // a taken-over club's shown name ("Al-Hilal"), for the transfers
    let shown: HashMap<u32, String> = takeover_names.iter().map(|(id, name, _)| (*id, name.clone())).collect();
    takeover_names.extend(new_names.iter().map(|(id, name, short, _)| (*id, name.clone(), short.clone())));
    // membership: Transfermarkt over FC 27 (the maintainer, 10 Oct): a matched player whose Transfermarkt
    // club is another of our clubs moves there (kept through its squad cut, as a transfer)
    // Two of our clubs can vote for the same Transfermarkt club (a fill or takeover squad of its
    // former players): the one with the most votes keeps it. Collected in id order, the last had
    // won: Aberdeen's id went to Elgin City and Grazer AK's to First Vienna, and the live pass
    // moved both squads there (Aberdeen left with 3 players, Grazer AK with none).
    let tm_key = tm_key_of(&by_club, &tm_club_votes);
    let mut moving: Vec<(u32, usize, u32)> = Vec::new();
    let membership = cfg.transfermarkt.as_ref().is_some_and(|t| t.membership);
    for (id, (_, _, rows)) in by_club.iter().filter(|_| membership) {
        for (k, row) in rows.iter().enumerate() {
            if let Some(&to) = row.tm_club.as_ref().and_then(|t| tm_key.get(t)) {
                if to != *id {
                    moving.push((*id, k, to));
                }
            }
        }
    }
    // remove from the back so the indexes stay right
    moving.sort_by(|a, b| (a.0, std::cmp::Reverse(a.1)).cmp(&(b.0, std::cmp::Reverse(b.1))));
    for (from, k, to) in moving {
        let mut row = by_club.get_mut(&from).unwrap().2.remove(k);
        row.transferred = true;
        report.tm_moves.push(format!("{} {}: {} -> {}", row.forename, row.surname, by_club[&from].0, by_club[&to].0));
        by_club.get_mut(&to).unwrap().2.push(row);
    }
    report.tm_moves.sort();
    // membership from the live Transfermarkt squads (the maintainer, 10 Oct: live TM over FC 27; loanees at
    // the club they play for): a player found there by birth date and a shared name word moves
    // to that club when it is one of ours; players on no live squad keep their roster club
    // each of our clubs' live squad value (EUR), for the wealth tiers
    let mut live_squad_eur: HashMap<u32, f64> = HashMap::new();
    // our clubs whose disc name is made up (unlicensed in 2006: "Pamplona" for Osasuna), with their
    // live name; renamed at their final ids below
    let mut live_renames: Vec<(u32, String)> = Vec::new();
    // each of our clubs' live (Transfermarkt) name, for the budgets
    let mut live_name_of: HashMap<u32, String> = HashMap::new();
    if let (Some(path), Some(live)) = (tm_live, live_sheet.as_ref()) {
        let col = |n: &str| live.col(n).ok_or_else(|| format!("{}: no {n} column", path.display()));
        let (lc, lname, lplayer, lbirth) = (col("club_id")?, col("club")?, col("name")?, col("birth_date")?);
        let (lnum, lvalue) = (live.col("number"), live.col("market_value_eur"));
        let (mut ours, by_word) = live_club_ids(live, lc, lname, &by_club, &tm_key, &shown, &roster_clubs, cfg);
        let key = |n: &str| normalise(n, &cfg.abbreviations);
        for r in &live.rows {
            let (cid, cname) = (r[lc].trim(), r[lname].trim());
            if ours.get(cid).is_some_and(|o| o.is_none()) && !report.live_clubs_unmatched.iter().any(|n| n == cname) {
                report.live_clubs_unmatched.push(cname.to_string());
            }
        }
        // (birth date, name word) -> live rows
        let mut index: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (i, r) in live.rows.iter().enumerate() {
            for w in name_words(&r[lplayer]) {
                index.entry((r[lbirth].trim().to_string(), w)).or_default().push(i);
            }
        }
        let mut moving: Vec<(u32, usize, u32)> = Vec::new();
        // the live page's current value and shirt number, for each player found
        let mut updates: Vec<(u32, usize, Option<f64>, Option<u32>)> = Vec::new();
        // players found in the live squad of a club that isn't ours: they have left ours
        let mut leaving: Vec<(u32, usize)> = Vec::new();
        // each live player claims one of our rows: FC 27 lists some players at two clubs (a loanee
        // at both ends), and both rows moving to the live club put him there twice; the row
        // already at the live club wins, else the first, and the others leave our squads
        let mut found: Vec<(u32, usize, usize)> = Vec::new();
        for (id, (_, _, rows)) in &by_club {
            for (k, row) in rows.iter().enumerate() {
                let mut hits: Vec<usize> = name_words(&format!("{} {}", row.forename, row.surname)).into_iter().filter_map(|w| index.get(&(row.birth.clone(), w))).flatten().copied().collect();
                hits.sort();
                hits.dedup();
                if let [i] = hits.as_slice() {
                    found.push((*id, k, *i));
                }
            }
        }
        // a live club no name matched is the club of ours most of its found players are at, with 3
        // or more of them ("Real Valladolid CF" is our Valladolid, "Frosinone Calcio" our
        // Frosinone): unmatched, its players would leave our squads as gone elsewhere. Not for
        // youth or B sides, nor a club another live club already is.
        let taken: HashSet<u32> = ours.values().flatten().copied().collect();
        let mut votes: HashMap<&str, HashMap<u32, usize>> = HashMap::new();
        for &(id, _, i) in &found {
            let cid = live.rows[i][lc].trim();
            if ours.get(cid).copied().flatten().is_none() {
                *votes.entry(cid).or_default().entry(id).or_default() += 1;
            }
        }
        let mut voted: Vec<(&str, u32, usize)> = votes
            .iter()
            .filter_map(|(cid, v)| {
                let total: usize = v.values().sum();
                let (&id, &n) = v.iter().max_by_key(|(id, n)| (**n, std::cmp::Reverse(**id)))?;
                let named = by_word.get(*cid) == Some(&id);
                (n * 2 > total && (n >= 3 || named) && !taken.contains(&id)).then_some((*cid, id, n))
            })
            .collect();
        voted.sort_by_key(|&(cid, _, n)| (std::cmp::Reverse(n), cid));
        let mut voted_ids: HashSet<u32> = HashSet::new();
        for (cid, id, _) in voted {
            let cname = live.rows.iter().find(|r| r[lc].trim() == cid).map(|r| r[lname].trim().to_string()).unwrap_or_default();
            if cname.contains("U21") || cname.contains("U19") || cname.ends_with(" B") || cname.ends_with(" II") || !voted_ids.insert(id) {
                continue;
            }
            ours.insert(cid.to_string(), Some(id));
            report.live_clubs_unmatched.retain(|n| *n != cname);
            let how = if by_word.get(cid) == Some(&id) { &mut report.live_clubs_by_word } else { &mut report.live_clubs_voted };
            how.push(format!("{cname} -> {}", by_club[&id].0));
        }
        let renamed: HashSet<u32> = takeover_names.iter().map(|(id, _, _)| *id).collect();
        let mut seen: HashSet<u32> = HashSet::new();
        for r in &live.rows {
            let Some(id) = ours.get(r[lc].trim()).copied().flatten() else { continue };
            let live_name = r[lname].trim();
            if renamed.contains(&id) || !seen.insert(id) || live_name.is_empty() {
                continue;
            }
            live_name_of.insert(id, live_name.to_string());
            if made_up_name(&by_club[&id].1, live_name, &cfg.abbreviations) {
                live_renames.push((id, live_name.to_string()));
            }
        }
        let mut claim: HashMap<usize, (u32, usize)> = HashMap::new();
        for &(id, k, i) in &found {
            let live_club = ours.get(live.rows[i][lc].trim()).copied().flatten();
            let take = match claim.get(&i) {
                None => true,
                Some(&(held, _)) => live_club == Some(id) && live_club != Some(held),
            };
            if take {
                claim.insert(i, (id, k));
            }
        }
        for (id, k, i) in found {
            if claim[&i] != (id, k) {
                report.live_duplicates += 1;
                leaving.push((id, k));
                continue;
            }
            {
                report.live_matched += 1;
                let lr = &live.rows[i];
                updates.push((id, k, lvalue.and_then(|c| lr[c].trim().parse::<f64>().ok()).filter(|v| *v > 0.0), lnum.and_then(|c| lr[c].trim().parse::<u32>().ok()).filter(|n| (1..=99).contains(n))));
                match ours.get(live.rows[i][lc].trim()).copied().flatten() {
                    Some(to) if to != id => moving.push((id, k, to)),
                    Some(_) => {}
                    None => {
                        report.live_elsewhere += 1;
                        leaving.push((id, k));
                    }
                }
            }
        }
        for (id, k, value, number) in updates {
            let row = &mut by_club.get_mut(&id).unwrap().2[k];
            if let Some(v) = value {
                (row.tm_value, row.market_value) = (Some(v), Some(v));
                report.live_values += 1;
            }
            row.live_number = number.or(row.live_number);
        }
        // moves and departures together, removed from the back so the indexes stay right
        let mut out: Vec<(u32, usize, Option<u32>)> = moving.into_iter().map(|(f, k, t)| (f, k, Some(t))).chain(leaving.into_iter().map(|(f, k)| (f, k, None))).collect();
        out.sort_by(|a, b| (a.0, std::cmp::Reverse(a.1)).cmp(&(b.0, std::cmp::Reverse(b.1))));
        let start: HashMap<u32, usize> = by_club.iter().map(|(id, (_, _, rows))| (*id, rows.len())).collect();
        for (from, k, to) in out {
            // a squad of MIN_SQUAD or more never drops under it this way: a career builds every club's
            // fixtures and line-ups at new game, and squads of 1-3 (a wrong club id, #31 blocker,
            // 10 Oct) stopped it loading; the player stays where FC 27 has him
            if start[&from] >= MIN_SQUAD && by_club[&from].2.len() <= MIN_SQUAD {
                report.live_kept_for_size += 1;
                continue;
            }
            let Some(to) = to else {
                by_club.get_mut(&from).unwrap().2.remove(k);
                continue;
            };
            let mut row = by_club.get_mut(&from).unwrap().2.remove(k);
            // the live club already has him under a birth date a few days off (an FM fill row
            // moving to where FC 27 has him): his live number and value go to that row instead
            let same = |r: &Row| key(&r.forename) == key(&row.forename) && key(&r.surname) == key(&row.surname) && r.birth.get(..4) == row.birth.get(..4);
            if let Some(there) = by_club.get_mut(&to).unwrap().2.iter_mut().find(|r| same(r)) {
                there.live_number = row.live_number.or(there.live_number);
                if row.tm_value.is_some() {
                    (there.tm_value, there.market_value) = (row.tm_value, row.market_value);
                }
                report.live_duplicates += 1;
                continue;
            }
            row.transferred = true;
            report.live_moves.push(format!("{} {}: {} -> {}", row.forename, row.surname, by_club[&from].0, by_club[&to].0));
            by_club.get_mut(&to).unwrap().2.push(row);
        }
        if let Some(c) = lvalue {
            for r in &live.rows {
                if let (Some(Some(id)), Ok(v)) = (ours.get(r[lc].trim()), r[c].trim().parse::<f64>()) {
                    *live_squad_eur.entry(*id).or_default() += v;
                }
            }
        }
        if cfg.live_add_missing {
            add_live_missing(&live, &ours, &claim, (lc, lplayer, lbirth, lvalue, lnum), tm.as_ref().map(|(s, tc)| (s, *tc)), tm_cols.and_then(|c| c.2), &cfg.season_start, cfg.live_add_min_value_eur, &mut by_club, &mut report);
        }
        report.live_moves.sort();
        report.live_clubs_unmatched.sort();
    }
    if let Some(path) = transfers {
        apply_transfers(&mut by_club, &shown, &Sheet::read(path).map_err(err)?, cfg, &mut report)?;
    }
    // managers (decision 2): each club's FM club (the roster's: where most of its players are),
    // and that club's manager
    // each club's FM club uid: the roster's, the club most of its players are at; new clubs' the
    // same; the fill's and takeovers' their own (for the managers and clubs_fm.csv)
    let fm_uid_of: BTreeMap<u32, String> = by_club
        .iter()
        .filter_map(|(id, (src_name, _, _))| {
            let vote = |v: &HashMap<String, usize>| v.iter().max_by_key(|(n, c)| (**c, std::cmp::Reverse((*n).clone()))).map(|(n, _)| n.clone());
            let fm_club = match fm_club_votes.get(id).filter(|_| roster_clubs.contains(id)) {
                Some(v) => vote(v)?,
                None => match new_club_votes.get(src_name).filter(|_| new_names.iter().any(|n| &n.3 == src_name)) {
                    Some(v) => vote(v)?,
                    None => fill_uid_of.get(src_name)?.clone(),
                },
            };
            Some((*id, fm_club))
        })
        .collect();
    let managers: Vec<(u32, String)> = match (staff_csv, cfg.fm.as_ref().and_then(|fc| fc.managers.as_ref())) {
        (Some(path), Some(mc)) => {
            let staff = Sheet::read(path).map_err(err)?;
            let by_fm_club = club_managers(&staff, mc)?;
            fm_uid_of.iter().filter_map(|(id, uid)| Some((*id, by_fm_club.get(uid)?.clone()))).collect()
        }
        _ => Vec::new(),
    };
    // managers: Transfermarkt's latest game over FM (the maintainer, 10 Oct)
    let mut managers = managers;
    if let (Some(path), Some(th)) = (tm_games, cfg.transfermarkt.as_ref().and_then(|t| t.history.as_ref())) {
        if let (Some(hm), Some(am)) = (&th.home_manager, &th.away_manager) {
            let g = Sheet::read(path).map_err(err)?;
            let need = |n: &str| g.col(n).ok_or_else(|| format!("transfermarkt games column {n:?} missing"));
            let (cd, ch, ca, chm, cam) = (need(&th.date)?, need(&th.home_club)?, need(&th.away_club)?, need(hm)?, need(am)?);
            let mut latest: HashMap<&str, (&str, &str)> = HashMap::new();
            for r in &g.rows {
                for (c, m) in [(ch, chm), (ca, cam)] {
                    let (club, name, date) = (r[c].trim(), r[m].trim(), r[cd].trim());
                    if !name.is_empty() && latest.get(club).is_none_or(|e| date > e.0) {
                        latest.insert(club, (date, name));
                    }
                }
            }
            let mut by_id: BTreeMap<u32, String> = managers.into_iter().collect();
            let over = cfg.transfermarkt.as_ref().is_some_and(|t| t.managers_over_fm);
            for (tm, key) in &tm_key {
                if !over && by_id.contains_key(key) {
                    continue;
                }
                if let Some((_, name)) = latest.get(tm.as_str()) {
                    // LMA keeps a surname: the name's last word (Arteta, Guardiola)
                    let surname = game_safe(name.split_whitespace().last().unwrap_or(name));
                    by_id.insert(*key, surname);
                    report.tm_managers += 1;
                }
            }
            managers = by_id.into_iter().collect();
        }
    }
    // current managers from a list (the maintainer: Wikipedia's), over everything: by the club's source,
    // LMA or taken-over shown name, normalised; the manager's surname (last word), and with a
    // `reputation` column (0-99) his reputation over the ability and wealth ones
    let mut listed_reps: HashMap<u32, u32> = HashMap::new();
    if let Some(path) = managers_csv {
        let sheet = Sheet::read(path).map_err(err)?;
        let (cc, cm) = match (sheet.col("club"), sheet.col("manager")) {
            (Some(c), Some(m)) => (c, m),
            _ => return Err("managers file: needs club and manager columns".into()),
        };
        let crep = sheet.col("reputation");
        let key = |n: &str| normalise(n, &cfg.abbreviations);
        let mut by_id: BTreeMap<u32, String> = managers.into_iter().collect();
        let mut listed: HashSet<u32> = HashSet::new();
        for r in &sheet.rows {
            let (club, name) = (r[cc].trim(), r[cm].trim());
            let k = key(club);
            let names_of = |id: &u32, src: &String, lma: &String| -> Vec<String> { [Some(key(src)), Some(key(lma)), shown.get(id).map(|n| key(n))].into_iter().flatten().collect() };
            let mut ids: Vec<u32> = by_club.iter().filter(|(id, (src, lma, _))| names_of(id, src, lma).contains(&k)).map(|(id, _)| *id).collect();
            // several of that name: the one whose source club is so named ("Newcastle Utd" is FC 27's
            // Newcastle, not the Jets on LMA's other Newcastle United), then the roster's own club
            // (Arsenal, not a namesake abroad)
            if ids.len() > 1 {
                let by_src: Vec<u32> = ids.iter().copied().filter(|id| key(&by_club[id].0) == k).collect();
                if !by_src.is_empty() {
                    ids = by_src;
                }
            }
            if ids.len() > 1 {
                ids.retain(|id| roster_clubs.contains(id));
            }
            // none: a distinctive word (4+ letters) only one roster club's names hold ("Bayern
            // Munich" is LMA's Bayern, "Celta Vigo" its Vigo)
            if ids.is_empty() {
                let words: Vec<String> = club.split(|c: char| c.is_whitespace() || c == '-').map(key).filter(|w| w.chars().count() >= 4).collect();
                let hits: Vec<u32> = by_club
                    .iter()
                    .filter(|(id, (src, lma, _))| roster_clubs.contains(id) && words.iter().any(|w| names_of(id, src, lma).iter().any(|n| n.contains(w.as_str()))))
                    .map(|(id, _)| *id)
                    .collect();
                if hits.len() == 1 {
                    ids = hits;
                }
            }
            match ids.as_slice() {
                // a club an earlier row set keeps it: "Arsenal FC, Fabián Lisa" (Arsenal de Sarandí,
                // whose record was taken over) after "Arsenal, Mikel Arteta" finds Arsenal, and
                // fc27final32 gave Arsenal "Lisa"
                [id] if listed.contains(id) => report.listed_managers_unmatched.push(format!("{club} (its club already set by an earlier row)")),
                [id] if !name.is_empty() => {
                    by_id.insert(*id, game_safe(name.split_whitespace().last().unwrap_or(name)));
                    listed.insert(*id);
                    match crep.map(|c| r[c].trim()).filter(|v| !v.is_empty()) {
                        Some(v) => match v.parse::<u32>() {
                            Ok(rep) if rep <= 99 => {
                                listed_reps.insert(*id, rep);
                            }
                            _ => return Err(format!("managers file: {club}: reputation {v:?} must be 0-99").into()),
                        },
                        None => {
                            listed_reps.remove(id);
                        }
                    }
                    report.listed_managers += 1;
                }
                _ => report.listed_managers_unmatched.push(format!("{club} ({} clubs of that name)", ids.len())),
            }
        }
        managers = by_id.into_iter().collect();
    }
    report.managers = managers.len();
    // their reputations from their ability ([fm.managers] reputation)
    let mut manager_reps: Vec<(u32, u32)> = match (staff_csv, cfg.fm.as_ref().and_then(|fc| fc.managers.as_ref())) {
        (Some(path), Some(mc)) if mc.reputation.is_some() => {
            let staff = Sheet::read(path).map_err(err)?;
            let ability = manager_abilities(&staff, mc)?;
            let wealth_of: HashMap<u32, u32> = match (clubs.col("id"), clubs.col("wealth")) {
                (Some(i), Some(w)) => clubs.rows.iter().filter_map(|r| Some((r[i].trim().parse().ok()?, r[w].trim().parse().ok()?))).collect(),
                _ => HashMap::new(),
            };
            let default_for = |id: &u32| wealth_of.get(id).map_or(mc.reputation_default, |w| (20 + 5 * w).min(99));
            managers
                .iter()
                .map(|(id, surname)| {
                    if let Some(&r) = listed_reps.get(id) {
                        report.manager_reps_listed += 1;
                        return (*id, r);
                    }
                    match manager_reputation(&ability, fm_uid_of.get(id).map(String::as_str), surname, mc) {
                        Some(r) => {
                            report.manager_reps += 1;
                            (*id, r)
                        }
                        None => {
                            report.manager_reps_default.push(surname.clone());
                            (*id, default_for(id))
                        }
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    };
    // without the staff export, the list's reputations alone
    for (id, r) in &listed_reps {
        if !manager_reps.iter().any(|(i, _)| i == id) {
            manager_reps.push((*id, *r));
            report.manager_reps_listed += 1;
        }
    }
    manager_reps.sort();
    let (stadiums, stadiums_kept) = club_stadiums(&clubs, &by_club, &tm_club_votes, tm_clubs, cfg.transfermarkt.as_ref(), &cfg.abbreviations, &roster_clubs, &cfg.stadium_renames)?;
    report.stadiums = stadiums.len();
    report.stadiums_kept = stadiums_kept;
    if staff_csv.is_some() {
        let with_manager: HashSet<u32> = managers.iter().map(|m| m.0).collect();
        let (pclub, cid, cname) = (players.col("club_id"), clubs.col("id"), clubs.col("name_E"));
        let with_squad: HashSet<u32> = pclub.map(|c| players.rows.iter().filter_map(|r| r[c].trim().parse().ok()).collect()).unwrap_or_default();
        if let (Some(cid), Some(cname)) = (cid, cname) {
            report.managers_kept = clubs
                .rows
                .iter()
                .filter_map(|r| {
                    let id: u32 = r[cid].trim().parse().ok()?;
                    (with_squad.contains(&id) && !with_manager.contains(&id)).then(|| r[cname].trim().to_string())
                })
                .collect();
        }
    }
    // the clubs no source covers at all (the lower divisions neither source has: Conference, the
    // Regionalliga reserve sides) play with the free agents, the best to the richest, as many as
    // their 2006 squads had; the rest stay free agents (decision 1: no 2006 player anywhere)
    if cfg.fm.as_ref().and_then(|fc| fc.fill.as_ref()).is_some_and(|f| f.free_agents) {
        free_rows.sort_by(|a, b| b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal));
        let (Some(cid), Some(pclub)) = (clubs.col("id"), players.col("club_id")) else { return Err("clubs.csv/players.csv: id or club_id missing".into()) };
        let mut squad_size: HashMap<u32, usize> = HashMap::new();
        for r in &players.rows {
            if let Ok(c) = r[pclub].trim().parse::<u32>() {
                *squad_size.entry(c).or_default() += 1;
            }
        }
        let (cwealth, cname) = (clubs.col("wealth"), clubs.col("name_E"));
        let mut bare: Vec<(u32, u32, String)> = clubs
            .rows
            .iter()
            .filter_map(|r| {
                let id: u32 = r[cid].trim().parse().ok()?;
                (squad_size.contains_key(&id) && !by_club.contains_key(&id))
                    .then(|| (cwealth.and_then(|c| r[c].trim().parse().ok()).unwrap_or(0), id, cname.map(|c| r[c].trim().to_string()).unwrap_or_default()))
            })
            .collect();
        bare.sort_by_key(|b| (std::cmp::Reverse(b.0), b.1));
        let mut pool = std::mem::take(&mut free_rows).into_iter();
        for (_, id, name) in bare {
            let rows: Vec<Row> = pool.by_ref().take(squad_size[&id].max(cfg.min_squad)).collect();
            if rows.len() < cfg.min_squad {
                break;
            }
            report.free_agent_squads.push(name.clone());
            by_club.insert(id, ("free agents".into(), name, rows));
        }
        free_rows = pool.collect();
    }
    // free agents share the level remap with the squads, under a key no club has
    const FREE: u32 = u32::MAX;
    if !free_rows.is_empty() {
        by_club.insert(FREE, (String::new(), String::new(), std::mem::take(&mut free_rows)));
    }
    if let Some((_, tc)) = &tm {
        // the attribute source's values stand in before the age adjustment, so it covers them too
        // (mac-perf on #713: a stand-in skipped it and ranked below a Transfermarkt-valued peer)
        (report.fm_values, report.fm_value_ratio) = fm_values(&mut by_club, tc.value_scale);
        for c in by_club.values_mut() {
            for r in c.2.iter_mut() {
                r.tm_value = r.tm_value.map(|v| age_adjusted(v, r.age, tc.peak_age, tc.age_decline));
            }
        }
        let game_overall = weights.map(Overall::read).transpose()?;
        level_remap(&mut by_club, &players, tc.overall_weight.clamp(0.0, 1.0), game_overall.as_ref(), tc.overall_from_source, &cfg.scale)?;
        if tc.fill_values {
            report.values_filled = fill_values(&mut by_club);
        }
    }
    if let Some((_, _, rows)) = by_club.remove(&FREE) {
        free_rows = rows;
    }
    free_rows.sort_by(|a, b| b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal));
    report.clubs_unmatched = unmatched.into_iter().collect();
    report.clubs_unmatched.sort_by(|a, b| b.1.cmp(&a.1));

    // id pools: the current squad members of each imported club are replaced in place; extra
    // players take ids of unattached records. Same name + consistent age keeps the record.
    let (pid, pclub, pfore, psur, page) = (col(&players, "id")?, col(&players, "club_id")?, col(&players, "forename")?, col(&players, "surname")?, col(&players, "age")?);
    let mut squad_ids: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut free_ids: Vec<u32> = Vec::new();
    let mut by_name: HashMap<(String, String), Vec<(u32, u32)>> = HashMap::new();
    for r in &players.rows {
        let Ok(id) = r[pid].parse::<u32>() else { continue };
        match r[pclub].parse::<u32>() {
            Ok(c) => squad_ids.entry(c).or_default().push(id),
            Err(_) => free_ids.push(id),
        }
        by_name.entry((normalise(&r[pfore], &BTreeMap::new()), normalise(&r[psur], &BTreeMap::new()))).or_default().push((id, r[page].parse().unwrap_or(0)));
    }
    free_ids.reverse(); // pop() takes the lowest id first
    let years = cfg.season_start.get(..4).and_then(|y| y.parse::<u32>().ok()).unwrap_or(2026).saturating_sub(2006);

    // skin tone (#31 item 5): when the base export has the column (lma-data's `skin`), every
    // imported player gets one, so nothing is inherited from the 2006 record he replaces: the
    // attribute source's own (FM's per-player tone), else the same person's vanilla value, else
    // his nation's most common value in vanilla.
    let skin_col = players.col("skin");
    // hair style (the maintainer, 11 Oct: "Rice is bald"): the sources have none, so every imported player
    // gets one drawn from vanilla's own mix (by a hash of his name and birth date, the same on every
    // import), never bald under 30, instead of the 2006 slot's
    let hair_col = players.col("hair");
    let hair_mix: Vec<(u32, u32)> = match hair_col {
        Some(c) => {
            let mut t: BTreeMap<u32, u32> = BTreeMap::new();
            for r in &players.rows {
                if let Ok(h) = r[c].trim().parse::<u32>() {
                    *t.entry(h).or_default() += 1;
                }
            }
            t.into_iter().collect()
        }
        None => Vec::new(),
    };
    let hair_of = |row: &Row| -> Option<u32> {
        let pool: Vec<(u32, u32)> = hair_mix.iter().copied().filter(|&(h, _)| h != 0 || row.age.is_some_and(|a| a >= 30)).collect();
        let total: u32 = pool.iter().map(|p| p.1).sum();
        if total == 0 {
            return None;
        }
        let h = format!("{} {} {} hair", row.forename, row.surname, row.birth).bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
        let mut k = h % total;
        pool.into_iter().find(|&(_, n)| if k < n { true } else { k -= n; false }).map(|p| p.0)
    };
    let mut skin_of: HashMap<u32, u32> = HashMap::new();
    let mut nation_skin: HashMap<String, u32> = HashMap::new();
    // the whole face (#697, lma-data's `face`: bits 275-278, `skin` its middle two): written with
    // the skin, so no imported player keeps half his 2006 slot's face set. The same person's
    // vanilla face when he has one; else a face of the tone set his skin means (oriental for a
    // nation whose vanilla players mostly have one), the variant by a hash of his name
    let face_col = players.col("face");
    let mut face_of: HashMap<u32, u32> = HashMap::new();
    let mut oriental_nations: HashSet<String> = HashSet::new();
    if let (Some(fc), Some(nc)) = (face_col, players.col("nation_id")) {
        let mut tally: HashMap<String, (u32, u32)> = HashMap::new();
        for r in &players.rows {
            let (Ok(id), Ok(v)) = (r[pid].parse::<u32>(), r[fc].trim().parse::<u32>()) else { continue };
            face_of.insert(id, v);
            let t = tally.entry(r[nc].trim().to_string()).or_default();
            t.0 += 1;
            t.1 += lma_data::db::FACE_SETS[4].contains(&v) as u32;
        }
        oriental_nations = tally.into_iter().filter(|(_, (n, o))| *o * 2 > *n).map(|(k, _)| k).collect();
    }
    if let (Some(sc), Some(nc)) = (skin_col, players.col("nation_id")) {
        let mut tally: HashMap<String, [u32; 4]> = HashMap::new();
        for r in &players.rows {
            let (Ok(id), Ok(v)) = (r[pid].parse::<u32>(), r[sc].trim().parse::<u32>()) else { continue };
            skin_of.insert(id, v);
            if let Some(t) = tally.entry(r[nc].trim().to_string()).or_default().get_mut(v as usize) {
                *t += 1;
            }
        }
        nation_skin = tally.into_iter().map(|(n, t)| (n, (0..4).max_by_key(|&k| (t[k], std::cmp::Reverse(k))).unwrap() as u32)).collect();
    }
    // value (#31 item 1): likewise always written when the base has the column, from the market
    // value; 0 (unpriced, as in over half of vanilla's records) without one, never the old record's
    let value_col = players.col("value");
    let value_scale = tm.as_ref().map(|(_, tc)| tc.value_scale);

    // weekly wage per unit of market value among the players the attribute source gives one,
    // for those it doesn't (median)
    let wage_ratio = {
        let mut r: Vec<f64> = by_club.values().flat_map(|c| c.2.iter()).filter_map(|row| Some(row.wage? as f64 / row.market_value.filter(|v| *v > 0.0)?)).collect();
        r.sort_by(f64::total_cmp);
        (!r.is_empty()).then(|| r[r.len() / 2])
    };
    // filled by `player_cells` (a closure), hence the cell
    let wages_out = std::cell::RefCell::new(String::from("id,wage,years\n"));
    // contract years left as the game counts them: 1 = ends this season (June after the start)
    let start_year = cfg.season_start.get(..4).and_then(|y| y.parse::<u32>().ok()).unwrap_or(2026);
    let mut out = String::from("\u{feff}");
    let mut header: Vec<&str> = vec!["id", "club_id", "squad_slot", "forename", "surname", "squad_number", "position", "position2", "age", "nation_id"];
    header.push("height_cm");
    header.extend(LMA_RATINGS);
    if hair_col.is_some() {
        header.push("hair");
    }
    // hair colour (#697): written when the base has the column and the source the data
    let hair_colour_col = players.col("hair_colour").is_some();
    if hair_colour_col {
        header.push("hair_colour");
    }
    if skin_col.is_some() {
        header.push("skin");
        if players.col("face").is_some() {
            header.push("face");
        }
    }
    if value_col.is_some() {
        header.push("value");
    }
    // sides and foot (#31 item 3): written when the base has the columns and the source the data,
    // else left as they are (a blank cell keeps the record's)
    let (sides_col, foot_out) = (players.col("sides").is_some(), players.col("foot").is_some());
    if sides_col {
        header.push("sides");
    }
    if foot_out {
        header.push("foot");
    }
    write_row(&mut out, &header);
    // one player's row: record id, club id ("" for a free agent), squad slot and number
    let player_cells = |id: u32, club_id: String, slot: String, number: String, row: &Row, same: Option<u32>, report: &mut Report| -> Vec<String> {
        let nation = cfg.nation_aliases.get(&row.nation).cloned().unwrap_or_else(|| row.nation.clone());
        let nation_id = match lma_nations.get(&normalise(&nation, &BTreeMap::new())) {
            Some(n) => n.to_string(),
            None => {
                *report.nations_unmatched.entry(row.nation.clone()).or_default() += 1;
                String::new()
            }
        };
        let height = row.height.unwrap_or(match row.position.as_str() {
            "GK" => 188,
            "DEF" => 184,
            "ATT" => 181,
            _ => 178,
        });
        if let Some(w) = row.wage.or_else(|| row.market_value.zip(wage_ratio).map(|(v, k)| (v * k).round() as u32)).filter(|w| *w > 0) {
            let years = row.contract_end.filter(|y| *y > start_year).map(|y| (y - start_year).min(7).to_string()).unwrap_or_default();
            wages_out.borrow_mut().push_str(&format!("{id},{w},{years}\n"));
        }
        let mut cells = vec![
            id.to_string(),
            club_id,
            slot,
            // "-" clears the forename; a blank cell would keep the 2006 record's ("Ludovic Lamine
            // Yamal")
            if row.forename.is_empty() { "-".to_string() } else { row.forename.clone() },
            row.surname.clone(),
            number,
            row.position.clone(),
            row.position2.clone(),
            row.age.map(|a| a.clamp(16, 47).to_string()).unwrap_or_default(),
            nation_id,
            height.clamp(150, 213).to_string(),
        ];
        cells.extend(row.ratings.iter().map(u32::to_string));
        if hair_col.is_some() {
            cells.push(row.hair.or_else(|| hair_of(row)).map(|h| h.to_string()).unwrap_or_default());
        }
        if hair_colour_col {
            // a dark-skinned player's dark brown or brown is black (#697: FM's one value covers
            // Salah's and Foden's hair; the importer's skin 2 = FM 16-20, 3 = FM 11-15)
            let dark = matches!(row.skin.or_else(|| same.and_then(|id| skin_of.get(&id).copied())), Some(2 | 3));
            cells.push(row.hair_colour.map(|h| if dark && (h == 1 || h == 2) { 0 } else { h }).map(|h| h.to_string()).unwrap_or_default());
        }
        if skin_col.is_some() {
            let skin = row
                .skin
                .or_else(|| same.and_then(|id| skin_of.get(&id).copied()))
                .or_else(|| nation_skin.get(&cells[9]).copied())
                .unwrap_or(0);
            cells.push(skin.to_string());
            if face_col.is_some() {
                let vanilla = same.filter(|_| row.skin.is_none()).and_then(|id| face_of.get(&id).copied());
                let seed = format!("{} {} {} face", row.forename, row.surname, row.birth).bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
                cells.push(vanilla.unwrap_or_else(|| face_for(skin, oriental_nations.contains(&cells[9]), seed)).to_string());
            }
        }
        if value_col.is_some() {
            let unit = cfg.value_unit.unwrap_or(1000);
            let value = row.market_value.zip(value_scale).map_or(0, |(v, s)| {
                let gbp = v * s;
                if unit == 1000 {
                    return value_steps(gbp, unit);
                }
                // a per-player spread of +-8% (the maintainer, 11 Oct: "10 players with the same worth makes no
                // sense", Yamal and Haaland both 187m; Transfermarkt's round numbers and the old price
                // steps put 2,265 players at exactly GBP 175k), to the stored unit; the same player
                // gets the same value on every import
                let h = format!("{} {} {}", row.forename, row.surname, row.birth).bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
                let spread = 0.92 + 0.16 * (h % 1000) as f64 / 999.0;
                (gbp * spread / unit as f64).round().clamp(1.0, 65535.0) as u32
            });
            cells.push(value.to_string());
        }
        if sides_col {
            cells.push(row.sides.map(|v| v.to_string()).unwrap_or_default());
        }
        if foot_out {
            cells.push(row.foot.map(|v| v.to_string()).unwrap_or_default());
        }
        cells
    };
    // shirt numbers (the maintainer: "their real numbers"): each matched player's latest number at his
    // club's Transfermarkt club
    let shirts = match (tm_lineups, cfg.transfermarkt.as_ref().and_then(|tc| tc.lineups.as_ref())) {
        (Some(path), Some(cols)) => {
            if tm_id_col.is_none() {
                return Err("[transfermarkt.lineups] needs the players' id column ([transfermarkt] id)".into());
            }
            let ids: HashSet<&str> = by_club.values().flat_map(|c| c.2.iter()).filter_map(|r| r.tm_id.as_deref()).collect();
            tm_shirts(path, cols, &ids)?
        }
        (Some(_), None) => return Err("--tm-lineups given but the mapping has no [transfermarkt.lineups] section".into()),
        (None, _) => HashMap::new(),
    };
    let mut used: HashSet<u32> = HashSet::new();
    // records no squad uses: the imported clubs' leftovers, then the unattached records
    let mut spare: Vec<u32> = Vec::new();
    for (&club_id, (src_name, lma_name, rows)) in by_club.iter_mut() {
        // the best first, a transferred player ahead of everyone (a squad already full would cut
        // him: he is the reason the line is in the transfers file)
        rows.sort_by(|a, b| b.transferred.cmp(&a.transferred).then(b.overall.partial_cmp(&a.overall).unwrap_or(std::cmp::Ordering::Equal)));
        let current = squad_ids.get(&club_id).map_or(0, Vec::len);
        let overflow = cfg.real_squads && cfg.squad_overflow;
        let target = if overflow {
            rows.len().max(cfg.min_squad)
        } else if cfg.real_squads {
            rows.len().min(cfg.max_squad).max(cfg.min_squad)
        } else {
            cfg.min_squad
        };
        let extra = target.saturating_sub(current).min(free_ids.len());
        rows.truncate(if overflow { current + extra } else { cfg.max_squad.min(current + extra) });
        // the best max_squad go in the squad array, the rest (still best first) beyond it
        let beyond = if rows.len() > cfg.max_squad { rows.split_off(cfg.max_squad) } else { Vec::new() };
        // goalkeepers first, then defenders, midfielders, attackers (squad numbers follow)
        let order = |p: &str| ["GK", "DEF", "MID", "ATT"].iter().position(|x| *x == p).unwrap_or(2);
        rows.sort_by_key(|r| order(&r.position));
        let in_squad = rows.len();
        rows.extend(beyond);
        report.clubs_matched.push((src_name.clone(), lma_name.clone(), rows.len()));
        let mut pool: Vec<u32> = squad_ids.get(&club_id).cloned().unwrap_or_default();
        pool.reverse();
        let tm_club = tm_club_votes.get(src_name.as_str()).and_then(tm_club_of);
        let own: Vec<Option<u32>> = rows.iter().map(|r| r.live_number.or_else(|| shirts.get(&(r.tm_id.clone()?, tm_club?.to_string())).copied())).collect();
        report.shirt_numbers += own.iter().flatten().count();
        let numbers = shirt_numbers(&own);
        for (slot, row) in rows.iter().enumerate() {
            // same person still playing 20 years later?
            let key = (normalise(&row.forename, &BTreeMap::new()), normalise(&row.surname, &BTreeMap::new()));
            let same = row.age.and_then(|age| {
                by_name.get(&key)?.iter().find(|(id, a)| !used.contains(id) && (a + years).abs_diff(age) <= 1).map(|(id, _)| *id)
            });
            let id = match same {
                Some(id) => {
                    report.players_reused_same_person += 1;
                    id
                }
                None => {
                    let mut next = None;
                    while let Some(id) = pool.pop().or_else(|| free_ids.pop()) {
                        if !used.contains(&id) {
                            next = Some(id);
                            break;
                        }
                    }
                    match next {
                        Some(id) => id,
                        None => {
                            report.squads_capped += 1;
                            break; // no spare records left: this squad stays smaller
                        }
                    }
                }
            };
            used.insert(id);
            let club = *new_id.get(&club_id).unwrap_or(&club_id);
            // a player beyond the squad array has no club in the database: the runtime gives him one
            let cells = if slot < in_squad {
                player_cells(id, club.to_string(), slot.to_string(), numbers[slot].to_string(), row, same, &mut report)
            } else {
                report.squad_overflow.push((id, club));
                player_cells(id, String::new(), String::new(), numbers[slot].to_string(), row, same, &mut report)
            };
            write_row(&mut out, &cells);
            report.players_written += 1;
        }
        // records of this club that weren't reused become free agents
        spare.extend(pool.into_iter().filter(|id| !used.contains(id)));
    }
    // the unattached records too, when the attribute source has free agents to put in them
    let fill_free = cfg.fm.as_ref().and_then(|fc| fc.fill.as_ref()).is_some_and(|f| f.free_agents) && fm.is_some();
    if fill_free {
        spare.extend(free_ids.iter().rev().copied().filter(|id| !used.contains(id)));
    }
    let mut free_rows = if fill_free { free_rows } else { Vec::new() }.into_iter();
    for id in spare {
        match free_rows.next() {
            Some(row) => {
                let cells = player_cells(id, String::new(), String::new(), "0".into(), &row, None, &mut report);
                write_row(&mut out, &cells);
                report.free_agents += 1;
            }
            None => {
                write_row(&mut out, &[id.to_string(), String::new()]);
                report.records_kept += 1;
            }
        }
    }
    if !fill_free {
        report.records_kept += free_ids.iter().filter(|id| !used.contains(id)).count();
    }
    // clubs with a squad that no source covers keep their 2006 players
    let pname = clubs.col("name_E");
    for (club_id, ids) in &squad_ids {
        if !by_club.contains_key(club_id) {
            report.records_kept += ids.len();
            let name = clubs.rows.iter().find(|r| r[cid].trim() == club_id.to_string()).and_then(|r| pname.map(|c| r[c].clone())).unwrap_or_default();
            report.clubs_without_source.push(name);
        }
    }
    report.clubs_without_source.sort();
    // managers and their reputations by the clubs' new ids, like their squads
    let managers: Vec<(u32, String)> = managers.into_iter().map(|(id, m)| (*new_id.get(&id).unwrap_or(&id), m)).collect();
    let manager_reps: Vec<(u32, u32)> = manager_reps.into_iter().map(|(id, r)| (*new_id.get(&id).unwrap_or(&id), r)).collect();
    let stadiums: Vec<(u32, String, [u32; 4])> = stadiums.into_iter().map(|(id, n, c)| (*new_id.get(&id).unwrap_or(&id), n, c)).collect();
    // a club's transfer budget (`--budgets`) by any name we know it by: the roster's, the disc's,
    // the shown one and the live one; else the one club with a name whose words start with each of
    // the budget name's words, or the other way round ("Tottenham" is Tottenham Hotspur,
    // "Galatasaray Istanbul" is Galatasaray)
    let budget_of: HashMap<u32, f64> = match budgets {
        Some(path) => {
            let sheet = Sheet::read(path).map_err(err)?;
            let (bc, bv) = (sheet.col("club").ok_or("budgets: no club column")?, sheet.col("budget_gbp").ok_or("budgets: no budget_gbp column")?);
            let key = |n: &str| normalise(n, &cfg.abbreviations);
            // distinctive words only: "United" or "City" alone would make D.C. United Manchester United
            const GENERIC: &[&str] = &["united", "city", "town", "county", "athletic", "atletico", "rovers", "wanderers", "albion", "real", "sporting", "racing", "union", "olympique", "inter", "deportivo", "club", "calcio", "istanbul", "saint", "san", "santa", "royal", "borussia", "dynamo", "dinamo"];
            let words = |n: &str| -> Vec<String> { n.split(|c: char| c.is_whitespace() || c == '-' || c == '.' || c == '/').map(key).filter(|w| w.len() >= 3 && !GENERIC.contains(&w.as_str())).collect() };
            let names: Vec<(u32, Vec<&str>)> = by_club
                .iter()
                .map(|(id, (src, lma, _))| (*id, [Some(src.as_str()), Some(lma.as_str()), shown.get(id).map(|n| n.as_str()), live_name_of.get(id).map(|n| n.as_str())].into_iter().flatten().filter(|n| !n.is_empty()).collect()))
                .collect();
            let rows: Vec<(&str, f64)> = sheet.rows.iter().map(|r| (r[bc].trim(), r[bv].trim().parse::<f64>().unwrap_or(0.0))).filter(|(c, g)| !c.is_empty() && !c.starts_with("(W)") && *g > 0.0).collect();
            let mut out: HashMap<u32, f64> = HashMap::new();
            let mut done = vec![false; rows.len()];
            // exact names first, for every budget; then a word match among the clubs left
            for exact in [true, false] {
                for (i, &(club, gbp)) in rows.iter().enumerate() {
                    if done[i] {
                        continue;
                    }
                    let k = key(club);
                    let bw = words(club);
                    let mut ids: Vec<u32> = names
                        .iter()
                        .filter(|(id, _)| !out.contains_key(id))
                        .filter(|(_, ns)| {
                            ns.iter().any(|n| {
                                if exact {
                                    return key(n) == k;
                                }
                                let nw = words(n);
                                let within = |a: &[String], b: &[String]| !a.is_empty() && a.iter().all(|x| b.iter().any(|y| x.starts_with(y.as_str()) || y.starts_with(x.as_str())));
                                within(&bw, &nw) || within(&nw, &bw)
                            })
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    if ids.len() > 1 {
                        ids.retain(|id| roster_clubs.contains(id));
                    }
                    if let [id] = ids.as_slice() {
                        out.insert(*id, gbp);
                        done[i] = true;
                    } else if !exact {
                        report.budgets_unmatched.push(format!("{club} ({})", ids.len()));
                    }
                }
            }
            out
        }
        None => HashMap::new(),
    };
    // wealth tiers at the clubs' new ids: the club's transfer budget as funds, else its live
    // squad's value (else the matched players' values) scaled; the nearest tier by ratio
    let mut targets = String::from("id,funds_m,source\n");
    let mut wealth: Vec<(u32, u8, f64)> = match cfg.transfermarkt.as_ref().filter(|t| t.wealth_budgets.len() == 16) {
        Some(t) => by_club
            .iter()
            .filter_map(|(id, (_, lma, rows))| {
                let (funds_m, source) = match budget_of.get(id) {
                    Some(gbp) => {
                        report.wealth_from_budgets += 1;
                        (gbp * t.budgets_scale / t.transfer_share / 1e6, "budget")
                    }
                    None => {
                        let eur = live_squad_eur.get(id).copied().unwrap_or_else(|| rows.iter().filter_map(|r| r.tm_value).sum());
                        (eur * t.value_scale * t.budget_share / 1e6, "squad")
                    }
                };
                if funds_m > 0.0 {
                    targets += &format!("{},{funds_m:.4},{source}\n", new_id.get(id).unwrap_or(id));
                }
                let funds_m = funds_m / t.funds_factors.get(lma.as_str()).copied().unwrap_or(1.0);
                (funds_m > 0.0).then(|| {
                    let off = |k: usize| (t.wealth_budgets[k] / funds_m).ln().abs();
                    let tier = (0..16).min_by(|&a, &b| off(a).total_cmp(&off(b))).unwrap_or(0) as u8;
                    (*new_id.get(id).unwrap_or(id), tier, funds_m)
                })
            })
            .collect(),
        None => Vec::new(),
    };
    if cfg.transfermarkt.as_ref().is_some_and(|t| t.wealth_by_rank) {
        rank_tiers(&mut wealth, &clubs);
    }
    let wealth: Vec<(u32, u8)> = wealth.into_iter().map(|(id, tier, _)| (id, tier)).collect();
    report.wealth_set = wealth.len();
    // clubs_fm.csv (badges keyed by FM club uid): id, the English name the game will show, uid
    let base_name: HashMap<String, String> = match (clubs.col("id"), clubs.col("name_E")) {
        (Some(i), Some(n)) => clubs.rows.iter().map(|r| (r[i].trim().to_string(), r[n].trim().to_string())).collect(),
        _ => HashMap::new(),
    };
    // two clubs left with the same shown name (2006's "Newcastle United" is also the Australian
    // Newcastle United Jets, id 880): every one of them with a different live name takes it, or the
    // badges, keyed by name, land on the wrong club (PC, 11 Oct: Newcastle showed Nadroga FA's crest)
    {
        let renamed: HashSet<u32> = takeover_names.iter().map(|(id, _, _)| *id).chain(live_renames.iter().map(|(id, _)| *new_id.get(id).unwrap_or(id))).collect();
        let shown_of = |id: &u32| -> Option<String> {
            let to = *new_id.get(id).unwrap_or(id);
            takeover_names.iter().find(|(i, _, _)| *i == to).map(|(_, n, _)| n.clone()).or_else(|| base_name.get(&to.to_string()).cloned())
        };
        let mut by_shown: HashMap<String, Vec<u32>> = HashMap::new();
        for id in by_club.keys() {
            if let Some(n) = shown_of(id) {
                by_shown.entry(n.to_lowercase()).or_default().push(*id);
            }
        }
        for (n, ids) in by_shown.iter().filter(|(_, v)| v.len() > 1) {
            for id in ids {
                let to = *new_id.get(id).unwrap_or(id);
                // the live name, else the source's own ("Newcastle Jets", "Club America")
                let src = by_club.get(id).map(|c| c.0.clone()).filter(|s| !s.is_empty() && s != "free agents");
                if let Some(name) = live_name_of.get(id).cloned().or(src).filter(|l| l.to_lowercase() != *n && !renamed.contains(&to)) {
                    live_renames.push((*id, name));
                }
            }
        }
    }
    for (id, name) in &live_renames {
        let full = game_safe(name);
        let shown: String = full.chars().take(25).collect();
        let short: String = full.split_whitespace().find(|w| w.chars().count() >= 3).unwrap_or(&full).chars().take(7).collect();
        report.live_renamed.push(format!("{} -> {shown}", by_club.get(id).map(|c| c.1.as_str()).unwrap_or("?")));
        takeover_names.push((*new_id.get(id).unwrap_or(id), shown, short));
    }
    let shown_name: HashMap<u32, &str> = takeover_names.iter().map(|(id, name, _)| (*id, name.as_str())).collect();
    let clubs_fm = (!fm_uid_of.is_empty()).then(|| {
        let mut s = String::from("\u{feff}");
        write_row(&mut s, &["id", "name_E", "fm_uid"]);
        let mut rows: Vec<(u32, String, &String)> = fm_uid_of
            .iter()
            .map(|(id, uid)| {
                let to = *new_id.get(id).unwrap_or(id);
                let name = shown_name.get(&to).map(|n| n.to_string()).unwrap_or_else(|| base_name.get(&id.to_string()).cloned().unwrap_or_default());
                (to, name, uid)
            })
            .collect();
        rows.sort();
        for (id, name, uid) in rows {
            write_row(&mut s, &[id.to_string(), name, uid.clone()]);
        }
        s
    });
    // league histories: the Transfermarkt club of each of our clubs, at its final id
    let tm_final: HashMap<String, u32> = tm_key.iter().map(|(tm, id)| (tm.clone(), *new_id.get(id).unwrap_or(id))).collect();
    // and our clubs by name (the roster's: source and LMA names; the others' source names)
    let mut final_by_name: HashMap<String, Option<u32>> = HashMap::new();
    for (id, (src, lma, _)) in &by_club {
        let to = *new_id.get(id).unwrap_or(id);
        let names = [Some(src), roster_clubs.contains(id).then_some(lma)];
        let keys: HashSet<String> = names.into_iter().flatten().map(|n| normalise(n, &cfg.abbreviations)).filter(|k| !k.is_empty()).collect();
        for k in keys {
            final_by_name.entry(k).and_modify(|e| *e = None).or_insert(Some(to));
        }
    }
    // then the names the game shows (the base's, for clubs nobody renamed) where the source names
    // missed or tied: a B team shares its club's source name (Bayern, Bordeaux: 40 champions went
    // unmatched in fc27final30), and clubs with no source squad (Gretna, Chievo) have none
    let renamed: HashSet<u32> = takeover_names.iter().map(|(id, _, _)| *id).collect();
    let mut shown_by_name: HashMap<String, Option<u32>> = HashMap::new();
    if let (Some(ci), Some(cn)) = (clubs.col("id"), clubs.col("name_E")) {
        for r in &clubs.rows {
            let Ok(id) = r[ci].trim().parse::<u32>() else { continue };
            let to = *new_id.get(&id).unwrap_or(&id);
            let k = normalise(r[cn].trim(), &cfg.abbreviations);
            if k.is_empty() || renamed.contains(&id) || renamed.contains(&to) {
                continue;
            }
            shown_by_name.entry(k).and_modify(|e| if *e != Some(to) { *e = None }).or_insert(Some(to));
        }
    }
    for (k, v) in shown_by_name {
        if v.is_some() && !matches!(final_by_name.get(&k), Some(Some(_))) {
            final_by_name.insert(k, v);
        }
    }
    let history = history_csv(base, tm_games, history_extra, cfg, &new_id, &tm_final, &final_by_name, &mut report)?;
    let cups = cups_csv(base, tm_games, tm_events, cup_extra, cfg, &new_id, &tm_final, &final_by_name, &mut report)?;
    let last_tables = last_tables_csv(base, tm_games, cfg, &new_id, &tm_final, &final_by_name, &moves, &takeover_names, &clubs, &mut report)?;
    if !cfg.club_codes.is_empty() {
        // every club's English name in the game: its new name, else the base name of the record now
        // at its id (a division move puts another club's record there)
        let from: HashMap<u32, u32> = moves.iter().map(|&(to, from, _, _)| (to, from)).collect();
        let renamed: HashMap<u32, String> = takeover_names.iter().filter(|t| !t.1.is_empty()).map(|(id, n, _)| (*id, n.clone())).collect();
        let ids: std::collections::BTreeSet<u32> = name_of.keys().copied().collect();
        for id in ids {
            let name = renamed.get(&id).or_else(|| name_of.get(from.get(&id).unwrap_or(&id))).cloned().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let code = club_code(&name, &cfg.club_codes);
            match takeover_names.iter_mut().find(|t| t.0 == id) {
                Some(t) => t.2 = code,
                None => takeover_names.push((id, String::new(), code)),
            }
        }
    }
    Ok(Imported { players: out, clubs: clubs_csv(&moves, &takeover_names, &managers, &manager_reps, &stadiums, &wealth), clubs_fm, history, cups, last_tables, funds_targets: (targets.lines().count() > 1).then_some(targets), wages: Some(wages_out.into_inner()).filter(|w| w.lines().count() > 1), report })
}

/// The attribute source's players as rows of the roster (`FmFill`): the roster's columns, its
/// position labels, FM's nation id as "fm:<id>" (named later), ability / 2 as the overall, the
/// filter column set so the rows pass it. Players without a known position are left out.
fn fill_rows(src: &Sheet, sheet: &Sheet, fill: &FmFill, fc: &FmConfig, cfg: &Config) -> Result<(Vec<Vec<String>>, HashMap<String, String>)> {
    let need = |s: &Sheet, n: &str| s.col(n).ok_or_else(|| format!("[fm.fill]: column {n:?} missing"));
    let c = &cfg.columns;
    let (fore, sur, birth, club, nat, pos, overall) = (need(src, &c.forename)?, need(src, &c.surname)?, need(src, &c.birthdate)?, need(src, &c.club)?, need(src, &c.nationality)?, need(src, &c.position)?, need(src, &c.overall)?);
    let (common, alt, height) = (c.common_name.as_deref().and_then(|n| src.col(n)), c.alt_positions.as_deref().and_then(|n| src.col(n)), c.height_cm.as_deref().and_then(|n| src.col(n)));
    let (foot, weak) = (c.foot.as_deref().and_then(|n| src.col(n)), c.weak_foot.as_deref().and_then(|n| src.col(n)));
    let filter = match (&cfg.filter_column, &cfg.filter_value) {
        (Some(k), Some(v)) => Some((need(src, k)?, v.clone())),
        _ => None,
    };
    let (f_fore, f_sur, f_birth) = (need(sheet, &fc.first_name)?, need(sheet, &fc.last_name)?, need(sheet, &fc.birth_date)?);
    let f_common = fc.common_name.as_deref().and_then(|n| sheet.col(n));
    let f_height = fc.height_cm.as_deref().and_then(|n| sheet.col(n));
    let (f_club, f_nat, f_pos, f_ab) = (need(sheet, &fill.club)?, need(sheet, &fill.nation)?, need(sheet, &fill.positions)?, need(sheet, &fill.ability)?);
    let f_alt = fill.alt_positions.as_deref().and_then(|n| sheet.col(n));
    let (f_left, f_right) = (fill.left_foot.as_deref().and_then(|n| sheet.col(n)), fill.right_foot.as_deref().and_then(|n| sheet.col(n)));
    let free_uid = match (fill.uid.as_deref(), fill.free_agent_uid_below) {
        (Some(n), Some(below)) => Some((need(sheet, n)?, below)),
        _ => None,
    };
    // each club's median player height, for `min_club_height_cm`
    let f_club_key = fill.club_uid.as_deref().and_then(|n| sheet.col(n)).unwrap_or(f_club);
    let mut heights: HashMap<&str, Vec<f64>> = HashMap::new();
    if let (Some(_), Some(h)) = (fill.min_club_height_cm, f_height) {
        for fr in &sheet.rows {
            if let Ok(v) = fr[h].trim().parse::<f64>() {
                heights.entry(fr[f_club_key].trim()).or_default().push(v);
            }
        }
    }
    let median = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let short_clubs: HashSet<&str> = match fill.min_club_height_cm {
        Some(min) => heights.iter_mut().filter_map(|(c, v)| (!c.is_empty() && median(v) < min).then_some(*c)).collect(),
        None => HashSet::new(),
    };
    let mut out = Vec::new();
    // the uid of each club kept, by name (the men's club where a women's section shares it)
    let mut uid_of: HashMap<String, String> = HashMap::new();
    for fr in &sheet.rows {
        let club_name = fr[f_club].trim();
        if short_clubs.contains(fr[f_club_key].trim()) || (club_name.is_empty() && free_uid.is_some_and(|(c, below)| fr[c].trim().parse::<u64>().map_or(true, |u| u >= below))) {
            continue;
        }
        let mut labels: Vec<&str> = fr[f_pos].split(';').chain(f_alt.map(|a| fr[a].split(';')).into_iter().flatten()).filter_map(|l| fill.labels.get(l.trim()).map(String::as_str)).collect();
        labels.dedup();
        let Some(&main) = labels.first() else { continue };
        let mut row = vec![String::new(); src.header.len()];
        row[fore] = fr[f_fore].trim().to_string();
        row[sur] = fr[f_sur].trim().to_string();
        if let (Some(c), Some(f)) = (common, f_common) {
            row[c] = fr[f].trim().to_string();
        }
        row[birth] = fr[f_birth].trim().to_string();
        row[club] = fr[f_club].trim().to_string();
        row[nat] = format!("fm:{}", fr[f_nat].trim());
        row[pos] = main.to_string();
        if let Some(a) = alt {
            row[a] = labels[1..].iter().filter(|l| **l != main).copied().collect::<Vec<_>>().join(" ");
        }
        row[overall] = (fr[f_ab].trim().parse::<f64>().unwrap_or(0.0) / 2.0).to_string();
        if let (Some(h), Some(f)) = (height, f_height) {
            row[h] = fr[f].trim().to_string();
        }
        if let (Some(fo), Some(l), Some(r)) = (foot, f_left, f_right) {
            let (l, r) = (fr[l].trim().parse::<u32>().unwrap_or(0), fr[r].trim().parse::<u32>().unwrap_or(0));
            row[fo] = if l > r { "Left" } else { "Right" }.to_string();
            if let Some(w) = weak {
                row[w] = (l.min(r) as f64 / 4.0).round().clamp(1.0, 5.0).to_string();
            }
        }
        if let Some((i, v)) = &filter {
            row[*i] = v.clone();
        }
        if !club_name.is_empty() {
            uid_of.entry(club_name.to_string()).or_insert_with(|| fr[f_club_key].trim().to_string());
        }
        out.push(row);
    }
    Ok((out, uid_of))
}

/// The sparse `clubs.csv` of division changes (`divisions::reseat`), or None when nothing moves.
type NewAt = Vec<(usize, u32, u32)>;

fn club_divisions(
    clubs: &Sheet,
    cfg: &Config,
    league_of: &HashMap<u32, String>,
    comp_of: &HashMap<u32, String>,
    new_leagues: &[String],
    retirees: &[u32],
    report: &mut Report,
) -> Result<(Vec<(u32, u32, u32, u32)>, HashMap<u32, u32>, NewAt)> {
    if cfg.ladders.is_empty() {
        return Ok((Vec::new(), HashMap::new(), Vec::new()));
    }
    let col = |name: &str| clubs.col(name).ok_or_else(|| format!("clubs.csv: column {name:?} missing (re-run db-export)"));
    let (cid, cname, ccountry, cdivision) = (col("id")?, col("name_E")?, col("country")?, col("division")?);
    let squad = clubs.col("squad_size");
    let num = |r: &[String], i: usize| r[i].trim().parse::<u32>().map_err(|_| format!("clubs.csv: bad number {:?}", r[i]));
    let mut slots = Vec::new();
    for r in &clubs.rows {
        slots.push(divisions::Slot {
            id: num(r, cid)?,
            name: r[cname].trim().to_string(),
            country: num(r, ccountry)?,
            division: num(r, cdivision)?,
            playing: squad.is_none_or(|i| r[i].trim() != "0"),
        });
    }
    let reseat = divisions::reseat(&slots, &cfg.ladders, cfg.overflow, league_of, comp_of, new_leagues);
    report.division_notes = reseat.notes;
    if reseat.moves.is_empty() && reseat.placed.is_empty() {
        return Ok((Vec::new(), HashMap::new(), Vec::new()));
    }
    // leagues are id ranges: a club moves by taking an id in its new division (record_of), with
    // that division's country and division bytes
    let by_id: HashMap<u32, &divisions::Slot> = slots.iter().map(|s| (s.id, s)).collect();
    let (moves, new_at) = divisions::id_moves(&slots, &reseat.moves, &reseat.placed, cfg.overflow, retirees);
    // every league block must still hold its clubs: the cup draws count them (#735)
    let problems = divisions::check_blocks(&slots, &cfg.ladders, &moves);
    if !problems.is_empty() {
        return Err(format!("the league blocks don't hold after the re-seat (cup draws would come up short, #735):
  {}", problems.join("
  ")).into());
    }
    report.division_notes.push("league blocks checked: every ladder division holds the game's club count, each club at an id of its own block".to_string());
    let shells: HashSet<u32> = new_at.iter().map(|n| n.2).collect();
    let mut new_id = HashMap::new();
    for &(to, from, country, division) in &moves {
        let s = by_id[&from];
        if !shells.contains(&from) {
            report.division_moves.push(format!("{}: id {from} -> {to} ({}/{} -> {country}/{division})", s.name, s.country, s.division));
        }
        new_id.insert(from, to);
    }
    Ok((moves, new_id, new_at))
}

/// The roster's clubs LMA lacks that can claim a place in their league (`min_squad` players),
/// strongest first, and the retired clubs whose records they take as shells: 2006 clubs outside
/// the leagues with a squad and no source, poorest first. At most as many claims as retirees.
#[allow(clippy::type_complexity)]
fn new_club_claims(clubs: &Sheet, players: &Sheet, by_club: &BTreeMap<u32, (String, String, Vec<Row>)>, new: BTreeMap<String, (String, Vec<Row>)>, cfg: &Config) -> (Vec<(String, String, Vec<Row>)>, Vec<u32>) {
    let (Some(cid), Some(pclub)) = (clubs.col("id"), players.col("club_id")) else { return (Vec::new(), Vec::new()) };
    let (ccountry, cwealth) = (clubs.col("country"), clubs.col("wealth"));
    let with_squad: HashSet<u32> = players.rows.iter().filter_map(|r| r[pclub].trim().parse().ok()).collect();
    let mut retirees: Vec<(u32, u32)> = clubs
        .rows
        .iter()
        .filter_map(|r| {
            let id: u32 = r[cid].trim().parse().ok()?;
            let rest = ccountry.is_some_and(|c| r[c].trim() == "15");
            (rest && with_squad.contains(&id) && !by_club.contains_key(&id)).then(|| (cwealth.and_then(|c| r[c].trim().parse().ok()).unwrap_or(0), id))
        })
        .collect();
    retirees.sort();
    let strength = |rows: &[Row]| {
        let mut o: Vec<f64> = rows.iter().map(|r| r.overall).collect();
        o.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
        o.iter().take(11).sum::<f64>() / o.len().clamp(1, 11) as f64
    };
    let mut claims: Vec<(String, String, Vec<Row>)> = new.into_iter().filter(|(_, (_, rows))| rows.len() >= cfg.min_squad).map(|(club, (league, rows))| (club, league, rows)).collect();
    claims.sort_by(|a, b| strength(&b.2).partial_cmp(&strength(&a.2)).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
    claims.truncate(retirees.len());
    (claims, retirees.into_iter().map(|(_, id)| id).collect())
}


/// A club name's words that can tell one club from another: five letters or more, accents
/// folded, without the words every club uses.
fn distinctive_words(name: &str) -> Vec<String> {
    const COMMON: [&str; 16] =
        ["club", "sport", "sports", "sporting", "football", "futbol", "futebol", "united", "athletic", "atletico", "deportivo", "calcio", "olympique", "dynamo", "dinamo", "racing"];
    name.chars()
        .map(fold_accent)
        .collect::<String>()
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 5 && !COMMON.contains(w))
        .map(String::from)
        .collect()
}

/// The mod's sparse `clubs.csv`: the re-seat's moves (`record_of` with the new country and
/// division) and the taken-over clubs' names; None when there are neither.
/// `wealth_by_rank`: within each country (the slot's, from the base clubs.csv), the clubs' tiers
/// become the base tiers of those same slots, sorted, handed out richest first.
fn rank_tiers(wealth: &mut [(u32, u8, f64)], clubs: &Sheet) {
    let (Some(ci), Some(cc), Some(cw)) = (clubs.col("id"), clubs.col("country"), clubs.col("wealth")) else { return };
    let base: HashMap<u32, (String, u8)> = clubs.rows.iter().filter_map(|r| Some((r[ci].trim().parse().ok()?, (r[cc].trim().to_string(), r[cw].trim().parse().unwrap_or(0))))).collect();
    let mut by_country: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (k, w) in wealth.iter().enumerate() {
        if let Some((country, _)) = base.get(&w.0) {
            by_country.entry(country.clone()).or_default().push(k);
        }
    }
    for idx in by_country.values_mut() {
        let mut tiers: Vec<u8> = idx.iter().map(|&k| base[&wealth[k].0].1).collect();
        tiers.sort_unstable_by(|a, b| b.cmp(a));
        idx.sort_by(|&a, &b| wealth[b].2.total_cmp(&wealth[a].2).then(wealth[a].0.cmp(&wealth[b].0)));
        for (&k, t) in idx.iter().zip(tiers) {
            wealth[k].1 = t;
        }
    }
}

fn clubs_csv(moves: &[(u32, u32, u32, u32)], names: &[(u32, String, String)], managers: &[(u32, String)], manager_reps: &[(u32, u32)], stadiums: &[(u32, String, [u32; 4])], wealth: &[(u32, u8)]) -> Option<String> {
    if moves.is_empty() && names.is_empty() && managers.is_empty() && stadiums.is_empty() && wealth.is_empty() {
        return None;
    }
    let mut header: Vec<String> = ["id", "record_of", "country", "division"].map(String::from).to_vec();
    header.extend(lma_data::db::LANGS.iter().map(|l| format!("name_{l}")));
    header.extend(lma_data::db::LANGS.iter().map(|l| format!("short_{l}")));
    header.push("manager".into());
    if !manager_reps.is_empty() {
        header.push("manager_rep".into());
    }
    header.extend(["stadium"].into_iter().chain(STANDS).map(String::from));
    header.push("wealth".into());
    let col = |n: &str| header.iter().position(|h| h == n).unwrap();
    // one row per club id, the cells each part sets
    let mut rows: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    fn row(rows: &mut BTreeMap<u32, Vec<String>>, id: u32, width: usize) -> &mut Vec<String> {
        rows.entry(id).or_insert_with(|| {
            let mut r = vec![String::new(); width];
            r[0] = id.to_string();
            r
        })
    }
    let width = header.len();
    for &(to, from, country, division) in moves {
        let r = row(&mut rows, to, width);
        r[1] = from.to_string();
        r[2] = country.to_string();
        r[3] = division.to_string();
    }
    for (id, name, short) in names {
        let r = row(&mut rows, *id, width);
        for l in lma_data::db::LANGS {
            // an empty name: only the short name changes (`Config::club_codes`)
            if !name.is_empty() {
                r[col(&format!("name_{l}"))] = name.clone();
            }
            r[col(&format!("short_{l}"))] = short.clone();
        }
    }
    for (id, manager) in managers {
        row(&mut rows, *id, width)[col("manager")] = manager.clone();
    }
    for (id, rep) in manager_reps {
        row(&mut rows, *id, width)[col("manager_rep")] = rep.to_string();
    }
    for (id, tier) in wealth {
        row(&mut rows, *id, width)[col("wealth")] = tier.to_string();
    }
    for (id, name, stands) in stadiums {
        let r = row(&mut rows, *id, width);
        r[col("stadium")] = name.clone();
        for (k, c) in STANDS.iter().enumerate() {
            r[col(c)] = stands[k].to_string();
        }
    }
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &header);
    for r in rows.values() {
        write_row(&mut out, r);
    }
    Some(out)
}

/// Moves the sources don't have yet (the maintainer: Martinelli to Al Hilal, Gabriel Jesus to Barcelona):
/// a `player,club` file, applied to the final squads (after the takeovers, so a club the attribute
/// source brings, like Al Hilal, can receive). The player is the one squad player whose full
/// name, as written, is the line's (known-as names: "Gabriel Martinelli"); the club the one
/// squad whose source, LMA or (taken over) shown name normalises like the line's. A line matching no one, or more
/// than one, is listed in the report and left out.
fn apply_transfers(by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>, shown: &HashMap<u32, String>, sheet: &Sheet, cfg: &Config, report: &mut Report) -> Result<()> {
    let (pc, cc) = match (sheet.col("player"), sheet.col("club").or_else(|| sheet.col("to_club"))) {
        (Some(p), Some(c)) => (p, c),
        _ => return Err("transfers file: needs player and club (or to_club) columns".into()),
    };
    let key = |s: &str| normalise(s, &BTreeMap::new());
    for r in &sheet.rows {
        let (player, club) = (r[pc].trim(), r[cc].trim());
        let target = key(club);
        let to: Vec<u32> = by_club
            .iter()
            .filter(|(id, (src, lma, _))| {
                normalise(src, &cfg.abbreviations) == normalise(club, &cfg.abbreviations) || key(src) == target || key(lma) == target || shown.get(*id).is_some_and(|n| key(n) == target)
            })
            .map(|(id, _)| *id)
            .collect();
        let wanted = key(player);
        let from: Vec<(u32, usize)> = by_club
            .iter()
            .flat_map(|(id, (_, _, rows))| rows.iter().enumerate().filter(|(_, row)| key(&format!("{} {}", row.forename, row.surname)) == wanted).map(move |(k, _)| (*id, k)))
            .collect();
        match (from.as_slice(), to.as_slice()) {
            ([(f, k)], [t]) if f != t => {
                let mut row = by_club.get_mut(f).unwrap().2.remove(*k);
                row.transferred = true;
                let from_name = by_club[f].0.clone();
                by_club.get_mut(t).unwrap().2.push(row);
                report.transfers.push(format!("{player}: {from_name} -> {}", by_club[t].0));
            }
            ([(f, _)], [t]) if f == t => report.transfers.push(format!("{player}: already at {}", by_club[t].0)),
            _ => report.transfers_unmatched.push(format!("{player} -> {club} ({} players of that name, {} clubs)", from.len(), to.len())),
        }
    }
    Ok(())
}

/// The mod's history.csv (lma-data's league histories, Records > League Champions, which shows
/// row `i` as the season ending in base year - `i`): the base export's rows with their clubs at
/// their new ids (a moved club keeps its titles) and moved down by the years `base_year` adds
/// to 2006, then each Transfermarkt season (`[transfermarkt.history]`) on its row: the champion
/// (most points, then goal difference, then goals) and its league record, when it is one of our
/// clubs. Seasons no source has stay blank, as the war years are; rows past the 150 drop off.
/// None when the base has no history.csv.
#[allow(clippy::too_many_arguments)]
fn history_csv(base: &Path, games: Option<&Path>, extra: Option<&Path>, cfg: &Config, new_id: &HashMap<u32, u32>, tm_final: &HashMap<String, u32>, by_name: &HashMap<String, Option<u32>>, report: &mut Report) -> Result<Option<String>> {
    let path = base.join("history.csv");
    if !path.exists() {
        return Ok(None);
    }
    let sheet = Sheet::read(&path).map_err(|e| e.to_string())?;
    let shift = cfg.base_year.map_or(0, |y| y.saturating_sub(2006)) as usize;
    const ROWS: usize = 150;
    // (country, tier) -> row -> cells [club_id, uid, W, D, L, F, A, Pts]
    let mut leagues: BTreeMap<(usize, usize), BTreeMap<usize, [String; 8]>> = BTreeMap::new();
    let col = |n: &str| sheet.col(n).ok_or_else(|| format!("history.csv: no {n} column"));
    let cols: Vec<usize> = ["country", "tier", "row", "club_id", "uid", "won", "drawn", "lost", "for", "against", "points"].iter().map(|c| col(c)).collect::<Result<_>>()?;
    for r in &sheet.rows {
        let num = |k: usize| r[cols[k]].trim().parse::<usize>().map_err(|_| format!("history.csv: bad {:?}", r[cols[k]]));
        let (country, tier, row) = (num(0)?, num(1)?, num(2)? + shift);
        if row >= ROWS {
            continue;
        }
        let club = r[cols[3]].trim();
        let club = match club.parse::<u32>() {
            Ok(id) => new_id.get(&id).unwrap_or(&id).to_string(),
            Err(_) => String::new(),
        };
        let mut cells: [String; 8] = std::array::from_fn(|k| r[cols[k + 3]].trim().to_string());
        cells[0] = club;
        if !cells[0].is_empty() {
            cells[1].clear(); // db-import looks the uid up by club id
        }
        leagues.entry((country, tier)).or_default().insert(row, cells);
    }
    // the leagues being filled keep their place in the file even when the base has no rows
    if let (Some(path), Some(th), Some(year)) = (games, cfg.transfermarkt.as_ref().and_then(|t| t.history.as_ref()), cfg.base_year) {
        let g = Sheet::read(path).map_err(|e| e.to_string())?;
        let need = |n: &str| g.col(n).ok_or_else(|| format!("transfermarkt games column {n:?} missing"));
        let (cc, cs, ch, ca, chg, cag) = (need(&th.competition)?, need(&th.season)?, need(&th.home_club)?, need(&th.away_club)?, need(&th.home_goals)?, need(&th.away_goals)?);
        need(&th.date)?;
        let (chn, can) = (th.home_club_name.as_deref().and_then(|n| g.col(n)), th.away_club_name.as_deref().and_then(|n| g.col(n)));
        // Transfermarkt club -> its name, for the fallback by name
        let mut tm_name: HashMap<String, String> = HashMap::new();
        // (competition, season) -> club -> [W, D, L, F, A]
        let mut tables: BTreeMap<(String, u32), HashMap<String, [u32; 5]>> = BTreeMap::new();
        for r in &g.rows {
            let comp = r[cc].trim();
            if !th.leagues.contains_key(comp) {
                continue;
            }
            let (Ok(season), Ok(hg), Ok(ag)) = (r[cs].trim().parse::<u32>(), r[chg].trim().parse::<u32>(), r[cag].trim().parse::<u32>()) else { continue };
            for (c, n) in [(ch, chn), (ca, can)] {
                if let Some(n) = n {
                    tm_name.entry(r[c].trim().to_string()).or_insert_with(|| r[n].trim().to_string());
                }
            }
            let t = tables.entry((comp.to_string(), season)).or_default();
            for (club, gf, ga) in [(&r[ch], hg, ag), (&r[ca], ag, hg)] {
                let e = t.entry(club.trim().to_string()).or_default();
                e[(gf <= ga) as usize + (gf < ga) as usize] += 1;
                e[3] += gf;
                e[4] += ga;
            }
        }
        for ((comp, season), table) in tables {
            let row = match year.checked_sub(season + 1) {
                Some(r) if (r as usize) < ROWS => r as usize,
                _ => continue,
            };
            let Some((club, rec)) = table.iter().max_by_key(|(c, r)| (3 * r[0] + r[1], r[3] as i64 - r[4] as i64, r[3], std::cmp::Reverse((*c).clone()))) else { continue };
            let [country, tier] = th.leagues[&comp];
            let league = leagues.entry((country, tier)).or_default();
            let named = || tm_name.get(club).and_then(|n| *by_name.get(&normalise(n, &cfg.abbreviations))?);
            match tm_final.get(club).copied().or_else(named) {
                Some(id) => {
                    let pts = 3 * rec[0] + rec[1];
                    let id = &id;
                    let cells = [id.to_string(), String::new(), rec[0].to_string(), rec[1].to_string(), rec[2].to_string(), rec[3].to_string(), rec[4].to_string(), pts.to_string()];
                    league.insert(row, cells.map(|c| c.parse::<u32>().map_or(c.clone(), |v| v.min(255).to_string())));
                    report.history_seasons += 1;
                }
                None => {
                    league.remove(&row);
                    let name = tm_name.get(club).cloned().unwrap_or_else(|| format!("club {club}"));
                    report.history_unmatched.push(format!("{comp} {season}/{}: {name}", (season + 1) % 100));
                }
            }
        }
    }
    // `--history-extra`: champions for the rows still empty (no source covers them), by club name
    if let (Some(path), Some(year)) = (extra, cfg.base_year) {
        let x = Sheet::read(path).map_err(|e| e.to_string())?;
        let need = |n: &str| x.col(n).ok_or_else(|| format!("{}: no {n} column", path.display()));
        let c: Vec<usize> = ["country", "tier", "season", "club", "won", "drawn", "lost", "for", "against", "points"].iter().map(|n| need(n)).collect::<Result<_>>()?;
        for r in &x.rows {
            let num = |k: usize| r[c[k]].trim().parse::<u32>().ok();
            let (Some(country), Some(tier), Some(season)) = (num(0), num(1), num(2)) else { continue };
            let Some(row) = year.checked_sub(season + 1).map(|r| r as usize).filter(|&r| r < ROWS) else { continue };
            let league = leagues.entry((country as usize, tier as usize)).or_default();
            if league.contains_key(&row) {
                continue; // a source already has that season
            }
            let name = r[c[3]].trim();
            match by_name.get(&normalise(name, &cfg.abbreviations)).copied().flatten() {
                Some(id) => {
                    let mut cells: [String; 8] = std::array::from_fn(|_| String::new());
                    cells[0] = id.to_string();
                    for k in 0..6 {
                        cells[k + 2] = num(k + 4).map(|v| v.min(255).to_string()).unwrap_or_default();
                    }
                    league.insert(row, cells);
                    report.history_extra += 1;
                }
                None => report.history_unmatched.push(format!("extra {country}/{tier} {season}/{}: {name}", (season + 1) % 100)),
            }
        }
    }
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &["country", "tier", "row", "club_id", "uid", "won", "drawn", "lost", "for", "against", "points"]);
    for ((country, tier), rows) in &leagues {
        for (row, cells) in rows {
            let mut line = vec![country.to_string(), tier.to_string(), row.to_string()];
            line.extend(cells.iter().cloned());
            write_row(&mut out, &line);
        }
    }
    Ok(Some(out))
}

/// A club's three-letter code (`Config::club_codes`): the table's, else the first three letters of
/// the first word of its name that isn't a prefix, in capitals.
fn club_code(name: &str, table: &BTreeMap<String, String>) -> String {
    if let Some(c) = table.iter().find(|(n, _)| n.eq_ignore_ascii_case(name.trim())).map(|(_, c)| c) {
        return c.clone();
    }
    const PREFIXES: [&str; 26] = ["fc", "afc", "cf", "sc", "ac", "as", "sv", "vfb", "vfl", "fk", "sk", "rc", "rcd", "cd", "ud", "sd", "ca", "ss", "us", "sl", "bv", "tsg", "club", "de", "the", "1"];
    let word = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty() && !PREFIXES.contains(&w.to_lowercase().as_str()) && !w.chars().all(|c| c.is_ascii_digit()))
        .find(|w| w.chars().count() >= 3)
        .unwrap_or(name);
    // plain letters, as on a broadcast scorebug (Köln: KOL)
    let plain = |c: char| match c.to_lowercase().next().unwrap_or(c) {
        'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'ø' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        'ß' => 's',
        l => l,
    };
    word.chars().map(plain).filter(|c| c.is_ascii_alphabetic()).take(3).collect::<String>().to_uppercase()
}

/// The mod's last_tables.csv (db-import, #666): last season's final tables, from which a new
/// career draws its European entrants. The base's tables (db-export) are moved to the clubs' new
/// ids; then every league a ladder names is re-made from the clubs the mod puts in it: a league
/// with a Transfermarkt competition (`transfermarkt.history.leagues`) takes the final table of
/// `last_season`, else the latest season the games cover in full, at least a double round robin
/// (points, goal difference, goals for; tied points in an `h2h`
/// competition by the head-to-head record first), every club of ours wherever the mod has it now
/// (the relegated ones are in last season's table, as the disc's 2005/06 tables keep theirs); the
/// league's other clubs (promoted since, `squad_size` > 0) follow in the base's order, and the
/// league keeps the game's size (`LAST_TABLE_SIZES`): clubs beyond it are cut, places left are
/// filled with the base's rows (clubs the mod moved out). None without a base last_tables.csv.
#[allow(clippy::too_many_arguments)]
fn last_tables_csv(base: &Path, games: Option<&Path>, cfg: &Config, new_id: &HashMap<u32, u32>, tm_final: &HashMap<String, u32>, by_name: &HashMap<String, Option<u32>>, moves: &[(u32, u32, u32, u32)], takeover_names: &[(u32, String, String)], clubs: &Sheet, report: &mut Report) -> Result<Option<String>> {
    use lma_data::db::{LAST_TABLE_NATIONS, LAST_TABLE_SIZES};
    /// The countries in the club record's (and the ladders') order.
    const CLUB_COUNTRIES: [&str; 8] = ["England", "Scotland", "Italy", "Spain", "Germany", "France", "Netherlands", "Portugal"];
    let path = base.join("last_tables.csv");
    if !path.exists() {
        return Ok(None);
    }
    let sheet = Sheet::read(&path).map_err(|e| e.to_string())?;
    let col = |n: &str| sheet.col(n).ok_or_else(|| format!("last_tables.csv: no {n} column"));
    let (cc, ct, cp, cid, cuid) = (col("country")?, col("tier")?, col("position")?, col("club_id")?, col("uid")?);
    // the base's rows by league, in position order: (the club at its new id, the uid)
    let mut leagues: BTreeMap<(usize, usize), Vec<(usize, Option<u32>, String)>> = BTreeMap::new();
    for r in &sheet.rows {
        let num = |i: usize| r[i].trim().parse::<usize>().map_err(|_| format!("last_tables.csv: bad number {:?}", r[i]));
        let club = r[cid].trim().parse::<u32>().ok().map(|id| *new_id.get(&id).unwrap_or(&id));
        leagues.entry((num(cc)?, num(ct)?)).or_default().push((num(cp)?, club, r[cuid].trim().to_string()));
    }
    for rows in leagues.values_mut() {
        rows.sort_by_key(|r| r.0);
    }
    // each club's (country, division) now, and its name: the base's, the moved ones at their new place
    let (ccid, cco, cdv) = match (clubs.col("id"), clubs.col("country"), clubs.col("division")) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => return Err("clubs.csv: needs id, country and division columns (re-run db-export)".into()),
    };
    let (cname, csquad) = (clubs.col("name_E"), clubs.col("squad_size"));
    let mut place: BTreeMap<u32, (u32, u32)> = BTreeMap::new();
    let mut names: HashMap<u32, String> = HashMap::new();
    for r in &clubs.rows {
        if let (Ok(id), Ok(c), Ok(d)) = (r[ccid].trim().parse::<u32>(), r[cco].trim().parse(), r[cdv].trim().parse()) {
            // the historic sides (FA XI, Old Etonians) have no squad and read as country 0 division 0
            if csquad.is_some_and(|i| r[i].trim() == "0") {
                continue;
            }
            place.insert(id, (c, d));
            if let Some(n) = cname {
                names.insert(id, r[n].trim().to_string());
            }
        }
    }
    let base_names = names.clone();
    for &(to, from, c, d) in moves {
        place.insert(to, (c, d));
        if let Some(n) = base_names.get(&from) {
            names.insert(to, n.clone());
        }
    }
    for (id, name, _) in takeover_names {
        if !name.is_empty() {
            names.insert(*id, name.clone());
        }
    }
    // the latest season's final table of each competition that fills a league: clubs by position
    let th = cfg.transfermarkt.as_ref().and_then(|t| t.history.as_ref());
    let mut tm_tables: HashMap<(usize, usize), (String, u32, usize, Vec<u32>)> = HashMap::new();
    if let (Some(path), Some(th)) = (games, th) {
        let g = Sheet::read(path).map_err(|e| e.to_string())?;
        let need = |n: &str| g.col(n).ok_or_else(|| format!("transfermarkt games column {n:?} missing"));
        let (cco, cs, ch, ca, chg, cag) = (need(&th.competition)?, need(&th.season)?, need(&th.home_club)?, need(&th.away_club)?, need(&th.home_goals)?, need(&th.away_goals)?);
        let (chn, can) = (th.home_club_name.as_deref().and_then(|n| g.col(n)), th.away_club_name.as_deref().and_then(|n| g.col(n)));
        let mut tm_name: HashMap<String, String> = HashMap::new();
        // (competition, season) -> its games (home, away, home goals, away goals)
        let mut seasons: BTreeMap<(String, u32), Vec<(String, String, u32, u32)>> = BTreeMap::new();
        for r in &g.rows {
            let comp = r[cco].trim();
            if !th.leagues.contains_key(comp) {
                continue;
            }
            let (Ok(season), Ok(hg), Ok(ag)) = (r[cs].trim().parse::<u32>(), r[chg].trim().parse::<u32>(), r[cag].trim().parse::<u32>()) else { continue };
            for (c, n) in [(ch, chn), (ca, can)] {
                if let Some(n) = n {
                    tm_name.entry(r[c].trim().to_string()).or_insert_with(|| r[n].trim().to_string());
                }
            }
            seasons.entry((comp.to_string(), season)).or_default().push((r[ch].trim().to_string(), r[ca].trim().to_string(), hg, ag));
        }
        // a table from games: club -> (points, goal difference, goals for)
        let table = |games: &[(String, String, u32, u32)], among: Option<&[String]>| -> HashMap<String, (i64, i64, i64)> {
            let mut t: HashMap<String, (i64, i64, i64)> = HashMap::new();
            for (h, a, hg, ag) in games {
                if among.is_some_and(|s| !s.contains(h) || !s.contains(a)) {
                    continue;
                }
                for (club, gf, ga) in [(h, *hg as i64, *ag as i64), (a, *ag as i64, *hg as i64)] {
                    let e = t.entry(club.clone()).or_default();
                    e.0 += if gf > ga { 3 } else if gf == ga { 1 } else { 0 };
                    e.1 += gf - ga;
                    e.2 += gf;
                }
            }
            t
        };
        // the season of each competition: the pinned one, else the latest played in full (a season
        // just begun in a newer pull has a few games), else the latest
        let mut latest: BTreeMap<String, u32> = BTreeMap::new();
        for ((comp, season), games) in &seasons {
            let clubs = games.iter().flat_map(|(h, a, _, _)| [h, a]).collect::<HashSet<_>>().len();
            let full = games.len() >= clubs * clubs.saturating_sub(1);
            match (th.last_season, latest.get(comp).map(|s| (seasons[&(comp.clone(), *s)].len(), *s))) {
                (Some(pinned), _) if pinned != *season => continue,
                (Some(_), _) => {}
                (None, Some((n, s))) => {
                    let was_full = n >= clubs * clubs.saturating_sub(1);
                    if (was_full && !full) || (was_full == full && *season < s) {
                        continue;
                    }
                }
                (None, None) => {}
            }
            latest.insert(comp.clone(), *season);
        }
        for (comp, season) in latest {
            let games = &seasons[&(comp.clone(), season)];
            let t = table(games, None);
            let mut order: Vec<String> = t.keys().cloned().collect();
            order.sort_by_key(|c| (std::cmp::Reverse(t[c]), c.clone()));
            if th.h2h.contains(&comp) {
                // tied points: the record of the games among the tied clubs decides first
                let mut sorted = Vec::with_capacity(order.len());
                let mut k = 0;
                while k < order.len() {
                    let pts = t[&order[k]].0;
                    let mut group: Vec<String> = order[k..].iter().take_while(|c| t[*c].0 == pts).cloned().collect();
                    k += group.len();
                    if group.len() > 1 {
                        let h = table(games, Some(&group));
                        group.sort_by_key(|c| (std::cmp::Reverse(h.get(c).copied().unwrap_or_default()), std::cmp::Reverse(t[c]), c.clone()));
                    }
                    sorted.extend(group);
                }
                order = sorted;
            }
            let [country, tier] = th.leagues[&comp];
            let Some(nation) = LAST_TABLE_NATIONS.iter().position(|n| Some(n) == CLUB_COUNTRIES.get(country)) else { continue };
            let named = |club: &str| tm_name.get(club).and_then(|n| *by_name.get(&normalise(n, &cfg.abbreviations))?);
            let mut ids = Vec::new();
            for club in &order {
                match tm_final.get(club).copied().or_else(|| named(club)) {
                    Some(id) => ids.push(id),
                    None => report.last_tables.push(format!("{} tier {tier}: {} ({comp} {season}/{:02}) is not one of our clubs", LAST_TABLE_NATIONS[nation], tm_name.get(club).cloned().unwrap_or_else(|| format!("club {club}")), (season + 1) % 100)),
                }
            }
            tm_tables.insert((nation, tier), (comp, season, games.len(), ids));
        }
    }
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &["country", "tier", "position", "club_id", "club", "uid"]);
    for (nation, tiers) in LAST_TABLE_SIZES.iter().enumerate() {
        for (tier, &size) in tiers.iter().enumerate() {
            let Some(rows) = leagues.get(&(nation, tier)) else { continue };
            let country = CLUB_COUNTRIES.iter().position(|c| *c == LAST_TABLE_NATIONS[nation]).map(|c| c as u32);
            let division = country.and_then(|c| cfg.ladders.iter().find(|l| l.country == c)).and_then(|l| l.divisions.get(tier).copied());
            let mut table: Vec<(Option<u32>, String)> = Vec::new();
            if let (Some(c), Some(d)) = (country, division) {
                // last season's table (its clubs wherever the mod has them now), then the league's
                // other clubs in the base's order, then the rest of them
                let members: Vec<u32> = place.iter().filter(|(_, p)| **p == (c, d)).map(|(id, _)| *id).collect();
                let mut seen: HashSet<u32> = HashSet::new();
                let mut counts = [0usize; 3];
                if let Some((_, _, _, ids)) = tm_tables.get(&(nation, tier)) {
                    for &id in ids {
                        if place.contains_key(&id) && seen.insert(id) {
                            table.push((Some(id), String::new()));
                            counts[0] += 1;
                        }
                    }
                }
                for id in rows.iter().filter_map(|r| r.1).chain(members.iter().copied()) {
                    if members.contains(&id) && seen.insert(id) {
                        table.push((Some(id), String::new()));
                        counts[1] += 1;
                    }
                }
                table.truncate(size);
                for (_, club, uid) in rows {
                    if table.len() >= size {
                        break;
                    }
                    if club.is_none_or(|id| seen.insert(id)) {
                        table.push((*club, uid.clone()));
                        counts[2] += 1;
                    }
                }
                let from = match tm_tables.get(&(nation, tier)) {
                    Some((comp, season, n, _)) => format!("{} from {comp} {season}/{:02} ({n} games)", counts[0], (season + 1) % 100),
                    None => "no Transfermarkt table".to_string(),
                };
                report.last_tables.push(format!("{} tier {tier}: {size} places: {from}, {} of the league's {} clubs in the base's order, {} base rows kept", LAST_TABLE_NATIONS[nation], counts[1], members.len(), counts[2]));
            } else {
                table = rows.iter().map(|(_, club, uid)| (*club, uid.clone())).collect();
            }
            if table.len() != size {
                return Err(format!("last_tables.csv: {} tier {tier} has {} rows for {size} places", LAST_TABLE_NATIONS[nation], table.len()));
            }
            for (k, (club, uid)) in table.iter().enumerate() {
                let name = club.and_then(|id| names.get(&id)).cloned().unwrap_or_default();
                write_row(&mut out, &[nation.to_string(), tier.to_string(), (k + 1).to_string(), club.map(|c| c.to_string()).unwrap_or_default(), name, if club.is_some() { String::new() } else { uid.clone() }]);
            }
        }
    }
    Ok(Some(out))
}

/// cups.csv for db-import (the maintainer, 12 Oct: the finals were still 2006's, the 2025/26 Champions League
/// final shown as an old one): the base's finals moved down to `base_year` (as the league
/// histories), then each Transfermarkt final of a cup in `[transfermarkt.history.cups]` on its row
/// (season s: row base_year - (s + 1)), then `--cup-extra` for the rows still empty. A final with
/// a side that isn't one of our clubs is left out and reported.
#[allow(clippy::too_many_arguments)]
fn cups_csv(base: &Path, games: Option<&Path>, events: Option<&Path>, extra: Option<&Path>, cfg: &Config, new_id: &HashMap<u32, u32>, tm_final: &HashMap<String, u32>, by_name: &HashMap<String, Option<u32>>, report: &mut Report) -> Result<Option<String>> {
    use lma_data::db::{CUPS, CUP_ROWS};
    let path = base.join("cups.csv");
    if !path.exists() {
        return Ok(None);
    }
    let sheet = Sheet::read(&path).map_err(|e| e.to_string())?;
    let shift = cfg.base_year.map_or(0, |y| y.saturating_sub(2006)) as usize;
    // cup -> row -> [winner_id, winner_uid, runner_up_id, runner_up_uid, winner_goals, runner_up_goals]
    let mut cups: BTreeMap<String, BTreeMap<usize, [String; 6]>> = BTreeMap::new();
    let col = |n: &str| sheet.col(n).ok_or_else(|| format!("cups.csv: no {n} column"));
    let cols: Vec<usize> = ["cup", "row", "winner_id", "winner_uid", "runner_up_id", "runner_up_uid", "winner_goals", "runner_up_goals"].iter().map(|c| col(c)).collect::<Result<_>>()?;
    for r in &sheet.rows {
        let row = r[cols[1]].trim().parse::<usize>().map_err(|_| format!("cups.csv: bad row {:?}", r[cols[1]]))? + shift;
        if row >= CUP_ROWS {
            continue;
        }
        let mut cells: [String; 6] = std::array::from_fn(|k| r[cols[k + 2]].trim().to_string());
        for k in [0, 2] {
            if let Ok(id) = cells[k].parse::<u32>() {
                cells[k] = new_id.get(&id).unwrap_or(&id).to_string();
                cells[k + 1].clear(); // db-import looks the uid up by club id
            }
        }
        cups.entry(r[cols[0]].trim().to_string()).or_default().insert(row, cells);
    }
    let th = cfg.transfermarkt.as_ref().and_then(|t| t.history.as_ref());
    let cup_of = |comp: &str| -> Option<String> {
        th.and_then(|th| th.cups.get(comp).cloned()).or_else(|| CUPS.iter().any(|c| c.0 == comp).then(|| comp.to_string()))
    };
    if let (Some(path), Some(th), Some(year)) = (games, th, cfg.base_year) {
        let g = Sheet::read(path).map_err(|e| e.to_string())?;
        let need = |n: &str| g.col(n).ok_or_else(|| format!("transfermarkt games column {n:?} missing"));
        let (cc, cs, ch, ca, chg, cag) = (need(&th.competition)?, need(&th.season)?, need(&th.home_club)?, need(&th.away_club)?, need(&th.home_goals)?, need(&th.away_goals)?);
        let (cr, cg) = (th.round.as_deref().map(need).transpose()?, th.game_id.as_deref().map(need).transpose()?);
        let (chn, can) = (th.home_club_name.as_deref().and_then(|n| g.col(n)), th.away_club_name.as_deref().and_then(|n| g.col(n)));
        let finals: Vec<&Vec<String>> = match cr {
            Some(cr) if !th.cups.is_empty() => g.rows.iter().filter(|r| th.cups.contains_key(r[cc].trim()) && r[cr].trim().eq_ignore_ascii_case("final")).collect(),
            _ => Vec::new(),
        };
        // the finals with a shoot-out: their goals in play, per club
        let mut in_play: HashMap<String, HashMap<String, u32>> = HashMap::new();
        if let (Some(events), Some(cg)) = (events, cg) {
            let ids: HashSet<&str> = finals.iter().map(|r| r[cg].trim()).collect();
            in_play = final_goals(events, &ids)?;
        }
        for r in finals {
            let (Ok(season), Ok(hg), Ok(ag)) = (r[cs].trim().parse::<u32>(), r[chg].trim().parse::<u32>(), r[cag].trim().parse::<u32>()) else { continue };
            let Some(row) = year.checked_sub(season + 1).map(|r| r as usize).filter(|&r| r < CUP_ROWS) else { continue };
            let cup = th.cups[r[cc].trim()].clone();
            let ((w, wn, mut wg), (l, ln, mut lg)) = {
                let home = (r[ch].trim(), chn.map(|c| r[c].trim()).unwrap_or_default(), hg);
                let away = (r[ca].trim(), can.map(|c| r[c].trim()).unwrap_or_default(), ag);
                if hg >= ag { (home, away) } else { (away, home) }
            };
            // a shoot-out: Transfermarkt's goals count the kicks; the events count the goals in play
            if let Some(goals) = cg.and_then(|cg| in_play.get(r[cg].trim())) {
                let (gw, gl) = (goals.get(w).copied().unwrap_or(0), goals.get(l).copied().unwrap_or(0));
                if gw == gl && gw + gl < wg + lg {
                    (wg, lg) = (gw, gl);
                    report.cup_shootouts += 1;
                }
            }
            let ours = |club: &str, name: &str| tm_final.get(club).copied().or_else(|| *by_name.get(&normalise(name, &cfg.abbreviations))?);
            match (ours(w, wn), ours(l, ln)) {
                (Some(wi), Some(li)) => {
                    cups.entry(cup).or_default().insert(row, [wi.to_string(), String::new(), li.to_string(), String::new(), wg.min(255).to_string(), lg.min(255).to_string()]);
                    report.cup_finals += 1;
                }
                _ => {
                    cups.entry(cup.clone()).or_default().remove(&row);
                    report.cup_unmatched.push(format!("{cup} {season}/{}: {wn} v {ln}", (season + 1) % 100));
                }
            }
        }
    }
    if let (Some(path), Some(year)) = (extra, cfg.base_year) {
        let x = Sheet::read(path).map_err(|e| e.to_string())?;
        let need = |n: &str| x.col(n).ok_or_else(|| format!("{}: no {n} column", path.display()));
        let c: Vec<usize> = ["competition", "season", "winner", "runner_up", "winner_goals", "runner_up_goals"].iter().map(|n| need(n)).collect::<Result<_>>()?;
        for r in &x.rows {
            let (Some(cup), Ok(season)) = (cup_of(r[c[0]].trim()), r[c[1]].trim().parse::<u32>()) else { continue };
            let Some(row) = year.checked_sub(season + 1).map(|r| r as usize).filter(|&r| r < CUP_ROWS) else { continue };
            if cups.get(&cup).is_some_and(|m| m.contains_key(&row)) {
                continue; // a source already has that final
            }
            let (wn, ln) = (r[c[2]].trim(), r[c[3]].trim());
            // a club by name, or "#<id>" (its final club id) where a name is ambiguous
            let id = |n: &str| match n.strip_prefix('#') {
                Some(i) => i.trim().parse::<u32>().ok(),
                None => by_name.get(&normalise(n, &cfg.abbreviations)).copied().flatten(),
            };
            match (id(wn), id(ln)) {
                (Some(wi), Some(li)) => {
                    let goals = |k: usize| r[c[k]].trim().parse::<u32>().map_or(String::new(), |v| v.min(255).to_string());
                    cups.entry(cup).or_default().insert(row, [wi.to_string(), String::new(), li.to_string(), String::new(), goals(4), goals(5)]);
                    report.cup_extra += 1;
                }
                _ => report.cup_unmatched.push(format!("extra {cup} {season}/{}: {wn} v {ln}", (season + 1) % 100)),
            }
        }
    }
    let mut out = String::from("\u{feff}");
    write_row(&mut out, &["cup", "row", "winner_id", "winner_uid", "runner_up_id", "runner_up_uid", "winner_goals", "runner_up_goals"]);
    for (cup, rows) in &cups {
        for (row, cells) in rows {
            let mut line = vec![cup.clone(), row.to_string()];
            line.extend(cells.iter().cloned());
            write_row(&mut out, &line);
        }
    }
    Ok(Some(out))
}

/// Goals in play per (game, club) for the games in `ids` that had a shoot-out, from Transfermarkt's
/// game events (`game_id`, `type`, `club_id`), read a line at a time: the file has millions of rows
/// and only a few dozen finals matter. A game without a "Shootout" event isn't returned.
fn final_goals(path: &Path, ids: &HashSet<&str>) -> Result<HashMap<String, HashMap<String, u32>>> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = std::io::BufReader::new(f).lines();
    let header = lines.next().ok_or_else(|| format!("{}: empty", path.display()))?.map_err(|e| e.to_string())?;
    let mut text = header.clone();
    text.push('\n');
    let head = Sheet::parse(&text).map_err(|e| e.to_string())?;
    let gi = head.col("game_id").ok_or_else(|| format!("{}: no game_id column", path.display()))?;
    for line in lines {
        let line = line.map_err(|e| e.to_string())?;
        // the game id is a plain field (the description further on may hold quoted commas)
        let id = line.split(',').nth(gi).map(|s| s.trim_matches('"'));
        if id.is_some_and(|id| ids.contains(id)) {
            text.push_str(&line);
            text.push('\n');
        }
    }
    let s = Sheet::parse(&text).map_err(|e| e.to_string())?;
    let (ct, cc) = (s.col("type").ok_or("game events: no type column")?, s.col("club_id").ok_or("game events: no club_id column")?);
    let mut goals: HashMap<String, HashMap<String, u32>> = HashMap::new();
    let mut shootout: HashSet<String> = HashSet::new();
    for r in &s.rows {
        match r[ct].trim() {
            "Goals" => *goals.entry(r[gi].trim().to_string()).or_default().entry(r[cc].trim().to_string()).or_default() += 1,
            "Shootout" => {
                shootout.insert(r[gi].trim().to_string());
            }
            _ => {}
        }
    }
    goals.retain(|g, _| shootout.contains(g));
    for g in shootout {
        goals.entry(g).or_default(); // 0-0 in play
    }
    Ok(goals)
}

/// A club's Transfermarkt club from its matched players' clubs: the most common, when three at
/// least and more than half of them are there.
/// The fewest players a club keeps through the live pass's moves and departures.
pub const MIN_SQUAD: usize = 16;

fn default_budgets_scale() -> f64 {
    1.0
}

fn default_transfer_share() -> f64 {
    0.65
}

fn default_budget_share() -> f64 {
    0.15
}

/// Each Transfermarkt club id -> ours: the club whose players vote for it most (two of ours can
/// vote for the same Transfermarkt club: a fill or takeover squad of its former players).
fn tm_key_of<R>(by_club: &BTreeMap<u32, (String, String, Vec<R>)>, tm_club_votes: &HashMap<String, HashMap<String, usize>>) -> HashMap<String, u32> {
    let mut tm_best: HashMap<String, (usize, u32)> = HashMap::new();
    for (id, (src, _, _)) in by_club {
        let Some(votes) = tm_club_votes.get(src) else { continue };
        let Some(tm) = tm_club_of(votes) else { continue };
        let n = votes[tm];
        let e = tm_best.entry(tm.to_string()).or_insert((n, *id));
        if n > e.0 {
            *e = (n, *id);
        }
    }
    tm_best.into_iter().map(|(tm, (_, id))| (tm, id)).collect()
}

/// Each live (Transfermarkt) club -> our club: the club its players vote for (`tm_key`), else the
/// one whose source, LMA or shown name is the live name (as the managers file). The second map
/// holds the clubs a distinctive name word matched: candidates the club's players must confirm.
#[allow(clippy::too_many_arguments)]
fn live_club_ids<R>(
    live: &Sheet,
    lc: usize,
    lname: usize,
    by_club: &BTreeMap<u32, (String, String, Vec<R>)>,
    tm_key: &HashMap<String, u32>,
    shown: &HashMap<u32, String>,
    roster_clubs: &HashSet<u32>,
    cfg: &Config,
) -> (HashMap<String, Option<u32>>, HashMap<String, u32>) {
    let key = |n: &str| normalise(n, &cfg.abbreviations);
    let mut ours: HashMap<String, Option<u32>> = HashMap::new();
    let mut by_word: HashMap<String, u32> = HashMap::new();
    for r in &live.rows {
        let (cid, cname) = (r[lc].trim(), r[lname].trim());
        if ours.contains_key(cid) {
            continue;
        }
        let found = tm_key.get(cid).copied().or_else(|| {
            let k = key(cname);
            // `[club_aliases]` names the LMA club too (#699: "Inverness Caledonian Thistle FC" is LMA's "Inverness CT")
            let alias = cfg.club_aliases.get(cname).map(|a| key(a));
            let mut ids: Vec<u32> = by_club
                .iter()
                .filter(|(id, (src, lma, _))| key(src) == k || key(lma) == k || alias.as_deref().is_some_and(|a| key(lma) == a) || shown.get(*id).is_some_and(|n| key(n) == k))
                .map(|(id, _)| *id)
                .collect();
            if ids.len() > 1 {
                ids.retain(|id| roster_clubs.contains(id));
            }
            // none: a distinctive word (4+ letters, not a youth or B side) only one of our
            // clubs holds ("1.FC Nuremberg" is LMA's Nuremberg) is a candidate the club's players
            // must confirm: alone it matched LASK to Slask Wroclaw and Vissel Kobe to
            // Kobenhavn (substrings), and Cruzeiro to the FM club that took its 2006 record
            if ids.is_empty() && !cname.contains("U21") && !cname.contains("U19") && !cname.ends_with(" B") && !cname.ends_with(" II") {
                let words: Vec<String> = cname.split(|c: char| c.is_whitespace() || c == '-' || c == '.').map(key).filter(|w| w.chars().count() >= 4).collect();
                let hits: Vec<u32> = by_club
                    .iter()
                    .filter(|(id, (src, lma, _))| words.iter().any(|w| [key(src), key(lma)].into_iter().chain(shown.get(*id).map(|n| key(n))).any(|n| n.contains(w.as_str()))))
                    .map(|(id, _)| *id)
                    .collect();
                if hits.len() == 1 {
                    by_word.insert(cid.to_string(), hits[0]);
                }
            }
            (ids.len() == 1).then(|| ids[0])
        });
        ours.insert(cid.to_string(), found);
    }
    (ours, by_word)
}

fn tm_club_of(votes: &HashMap<String, usize>) -> Option<&str> {
    let total: usize = votes.values().sum();
    let (tm_id, n) = votes.iter().max_by_key(|(t, n)| (**n, std::cmp::Reverse((*t).clone())))?;
    (*n >= 3 && 2 * n > total).then_some(tm_id.as_str())
}

/// Shirt numbers from Transfermarkt's lineups, read a line at a time (3 million games' rows):
/// (player, club) -> the number of his latest game there, for the given players only.
fn tm_shirts(path: &Path, cols: &TmLineups, players: &HashSet<&str>) -> Result<HashMap<(String, String), u32>> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = std::io::BufReader::new(f).lines();
    let header = lma_data::csv::Sheet::parse(&lines.next().ok_or("empty lineups file")?.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?.header;
    let need = |n: &str| header.iter().position(|h| h == n).ok_or_else(|| format!("lineups column {n:?} missing"));
    let (cp, cc, cn, cd) = (need(&cols.player)?, need(&cols.club)?, need(&cols.number)?, need(&cols.date)?);
    let mut latest: HashMap<(String, String), (String, u32)> = HashMap::new();
    for line in lines {
        let line = line.map_err(|e| e.to_string())?;
        let cells = split_csv_line(&line);
        let (Some(p), Some(c), Some(n), Some(d)) = (cells.get(cp), cells.get(cc), cells.get(cn), cells.get(cd)) else { continue };
        if !players.contains(p.as_str()) {
            continue;
        }
        let Some(n) = n.trim().parse::<u32>().ok().filter(|n| (1..=99).contains(n)) else { continue };
        let e = latest.entry((p.clone(), c.clone())).or_insert_with(|| (String::new(), 0));
        if *d > e.0 {
            *e = (d.clone(), n);
        }
    }
    Ok(latest.into_iter().map(|(k, (_, n))| (k, n)).collect())
}

/// One CSV line's cells (quoted cells with doubled quotes; no line breaks inside a cell).
fn split_csv_line(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = line.trim_end_matches('\r').chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '"') if chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            (true, '"') => quoted = false,
            (false, '"') if cell.is_empty() => quoted = true,
            (false, ',') => cells.push(std::mem::take(&mut cell)),
            (_, c) => cell.push(c),
        }
    }
    cells.push(cell);
    cells
}

/// Shirt numbers for a squad in its order: a player's own number when he has one there and no
/// one before him took it, the rest the lowest numbers left (from 1).
fn shirt_numbers(own: &[Option<u32>]) -> Vec<u32> {
    let mut taken: HashSet<u32> = HashSet::new();
    let mine: Vec<Option<u32>> = own.iter().map(|n| n.filter(|n| taken.insert(*n))).collect();
    let mut next = 1;
    mine.into_iter()
        .map(|n| {
            n.unwrap_or_else(|| {
                while taken.contains(&next) {
                    next += 1;
                }
                taken.insert(next);
                next
            })
        })
        .collect()
}

/// The four stand capacity columns of clubs.csv, all written together.
const STANDS: [&str; 4] = ["cap_north", "cap_east", "cap_south", "cap_west"];

/// Each club's Transfermarkt stadium, by the club's record id (its stadium record travels with
/// it): the Transfermarkt club most of its matched players are at (three at least, and more than
/// half of them), that club's stadium name (fit to 39 characters) and its seats spread over the
/// four stands in the proportions the record already has (equal when it has none). Clubs without
/// a stadium record (the historic sides) are skipped.
fn club_stadiums<R>(clubs: &Sheet, by_club: &BTreeMap<u32, (String, String, Vec<R>)>, votes: &HashMap<String, HashMap<String, usize>>, tm_clubs: Option<&Path>, tc: Option<&TmConfig>, ab: &BTreeMap<String, String>, roster_clubs: &HashSet<u32>, renames: &BTreeMap<String, String>) -> Result<(Vec<(u32, String, [u32; 4])>, Vec<String>)> {
    let (path, ts) = match (tm_clubs, tc.and_then(|tc| tc.stadiums.as_ref())) {
        (Some(p), Some(ts)) => (p, ts),
        (Some(_), None) => return Err("--tm-clubs given but the mapping has no [transfermarkt.stadiums] section".into()),
        (None, _) => return Ok((Vec::new(), Vec::new())),
    };
    if tc.and_then(|tc| tc.club.as_ref()).is_none() {
        return Err("[transfermarkt.stadiums] needs the players' club column ([transfermarkt] club)".into());
    }
    let sheet = Sheet::read(path).map_err(|e| e.to_string())?;
    let need = |n: &str| sheet.col(n).ok_or_else(|| format!("transfermarkt clubs column {n:?} missing"));
    let (ci, cn, cs) = (need(&ts.id)?, need(&ts.name)?, need(&ts.seats)?);
    let stadium_of: HashMap<&str, (String, u32)> = sheet
        .rows
        .iter()
        .filter_map(|r| {
            let name = fit_words(&game_safe(r[cn].trim()), 39);
            let seats: u32 = r[cs].trim().parse().ok().filter(|s| *s > 0)?;
            (!name.is_empty()).then(|| (r[ci].trim(), (name, seats)))
        })
        .collect();
    // normalised club name -> its Transfermarkt id, for names only one club has
    let mut by_name: HashMap<String, Option<&str>> = HashMap::new();
    if let Some(cc) = ts.club.as_deref() {
        let cc = need(cc)?;
        for r in &sheet.rows {
            let key = normalise(&r[cc], ab);
            if !key.is_empty() {
                by_name.entry(key).and_modify(|e| *e = None).or_insert(Some(r[ci].trim()));
            }
        }
    }
    let (id_col, stand_cols) = (clubs.col("id"), STANDS.map(|c| clubs.col(c)));
    let stands_of: HashMap<u32, [u32; 4]> = match (id_col, stand_cols) {
        (Some(i), [Some(n), Some(e), Some(s), Some(w)]) => clubs
            .rows
            .iter()
            .filter_map(|r| Some((r[i].trim().parse().ok()?, [n, e, s, w].map(|c| r[c].trim().parse::<u32>().ok()).into_iter().collect::<Option<Vec<u32>>>()?.try_into().ok()?)))
            .collect(),
        _ => HashMap::new(),
    };
    // the votes first, then the names, each Transfermarkt club to one club only (a Chilean
    // Everton never takes Goodison's successor from Everton)
    let mut out = Vec::new();
    let mut taken: HashSet<&str> = HashSet::new();
    let mut by_vote: Vec<(u32, &str)> = Vec::new();
    let mut by_name_left: Vec<(u32, &str)> = Vec::new();
    for (id, (src, lma, _)) in by_club {
        if !stands_of.contains_key(id) {
            continue;
        }
        let voted = votes.get(src).and_then(tm_club_of);
        // (a voted club the clubs file lacks gives way to the name too)
        let voted = voted.filter(|t| stadium_of.contains_key(t));
        match voted {
            Some(t) => by_vote.push((*id, t)),
            None => {
                // the LMA name too for the roster's own clubs (FC's "Ipswich" is LMA's Ipswich Town)
                let names = [Some(src), roster_clubs.contains(id).then_some(lma)];
                if let Some(t) = names.into_iter().flatten().find_map(|n| by_name.get(&normalise(n, ab)).copied().flatten()) {
                    by_name_left.push((*id, t));
                }
            }
        }
    }
    for (id, tm_id) in by_vote.into_iter().chain(by_name_left) {
        if !taken.insert(tm_id) {
            continue;
        }
        let Some((name, seats)) = stadium_of.get(tm_id) else { continue };
        out.push((id, name.clone(), spread(*seats, stands_of[&id])));
    }
    out.sort_by_key(|o| o.0);
    // the clubs with a stadium record left as they were: a ground since replaced takes today's
    // name (`stadium_renames`), the rest are listed with their stadium
    let given: HashSet<u32> = out.iter().map(|o| o.0).collect();
    let stadium_col = clubs.col("stadium");
    let mut kept = Vec::new();
    for (id, (src, _, _)) in by_club.iter().filter(|(id, _)| stands_of.contains_key(id) && !given.contains(id)) {
        let record = id_col.and_then(|i| clubs.rows.iter().find(|r| r[i].trim() == id.to_string()));
        let cell = |c: Option<usize>| record.zip(c).map(|(r, c)| r[c].trim().to_string()).unwrap_or_default();
        let old = cell(stadium_col);
        match renames.get(&format!("{}|{old}", cell(clubs.col("name_E")))) {
            Some(new) => out.push((*id, fit_words(&game_safe(new), 39), stands_of[id])),
            None => kept.push(format!("{src} ({old})")),
        }
    }
    out.sort_by_key(|o| o.0);
    Ok((out, kept))
}

/// `seats` over four stands in the proportions of `like` (equal when it sums to 0), the rounding
/// on the largest, each at most the field's 65535.
fn spread(seats: u32, like: [u32; 4]) -> [u32; 4] {
    let like = if like.iter().all(|&c| c == 0) { [1; 4] } else { like };
    let sum: u64 = like.iter().map(|&c| c as u64).sum();
    let mut out = like.map(|c| (seats as u64 * c as u64 / sum) as u32);
    let largest = (0..4).max_by_key(|&k| (like[k], std::cmp::Reverse(k))).unwrap_or(0);
    out[largest] += seats - out.iter().sum::<u32>();
    out.map(|c| c.min(65535))
}

/// `name` cut to `max` characters at a word break (a hard cut for a single long word).
fn fit_words(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let hard: String = name.chars().take(max).collect();
    let cut = match name.chars().nth(max) {
        Some(' ') => hard.trim_end().to_string(),
        _ => hard.rsplit_once(' ').map(|(a, _)| a.trim_end().to_string()).unwrap_or(hard.clone()),
    };
    cut.trim_end_matches([' ', '-', ',']).to_string()
}

/// Each club's manager in a staff export (`FmManagers`): club uid -> surname (or single name).
/// FM's staff abilities for the managers' reputations: by (club uid, surname as the game holds it,
/// lower case), and by surname alone for the manager roles (a manager listed at another club).
struct ManagerAbility {
    by_club: HashMap<(String, String), f64>,
    by_name: HashMap<String, Vec<f64>>,
}

fn manager_abilities(staff: &Sheet, mc: &FmManagers) -> Result<ManagerAbility> {
    let need = |n: &str| staff.col(n).ok_or_else(|| format!("staff export: column {n:?} missing"));
    let (club, role, ability, surname) = (need(&mc.club_uid)?, need(&mc.role)?, need(&mc.ability)?, need(&mc.surname)?);
    let common = mc.common_name.as_deref().and_then(|n| staff.col(n));
    let mut ab = ManagerAbility { by_club: HashMap::new(), by_name: HashMap::new() };
    for r in &staff.rows {
        let Ok(a) = r[ability].trim().parse::<f64>() else { continue };
        let is_manager = mc.manager_roles.iter().any(|m| *m == r[role].trim());
        // the whole name and its last word: the game keeps FM's common name ("Luis Enrique") or a
        // list's last word ("Enrique")
        let mut keys: Vec<String> = Vec::new();
        for name in [common.map(|i| r[i].trim()), Some(r[surname].trim())].into_iter().flatten().filter(|n| !n.is_empty()) {
            let name = game_safe(name).to_lowercase();
            if let Some(last) = name.split_whitespace().last() {
                keys.push(last.to_string());
            }
            keys.push(name);
        }
        keys.sort();
        keys.dedup();
        for key in keys {
            let e = ab.by_club.entry((r[club].trim().to_string(), key.clone())).or_insert(a);
            *e = e.max(a);
            if is_manager {
                ab.by_name.entry(key).or_default().push(a);
            }
        }
    }
    Ok(ab)
}

/// A manager's reputation (0-99) from his ability at his club (`fm_club`, the club's FM uid), else
/// the one manager of that surname; None when neither knows him.
/// A face (`lma_data::db::FACE`) for the importer's skin value (`[fm.skin] lma`: 0 light, 1 olive,
/// 3 FM 11-15, 2 FM 16-20): a white, medium, brown or black set, oriental for a light or olive
/// player of an oriental nation; the set's variant by `seed`.
fn face_for(skin: u32, oriental: bool, seed: u32) -> u32 {
    let set = match (skin, oriental) {
        (0 | 1, true) => 4,
        (0, _) => 0,
        (1, _) => 1,
        (3, _) => 2,
        _ => 3,
    };
    lma_data::db::FACE_SETS[set][(seed % 3) as usize]
}

fn manager_reputation(ab: &ManagerAbility, fm_club: Option<&str>, surname: &str, mc: &FmManagers) -> Option<u32> {
    let [lo, hi] = mc.reputation?;
    let key = surname.to_lowercase();
    let a = fm_club.and_then(|c| ab.by_club.get(&(c.to_string(), key.clone()))).copied().or_else(|| ab.by_name.get(&key).filter(|v| v.len() == 1).map(|v| v[0]))?;
    Some((1.0 + (a - lo) / (hi - lo) * 98.0).round().clamp(1.0, 99.0) as u32)
}

fn club_managers(staff: &Sheet, mc: &FmManagers) -> Result<HashMap<String, String>> {
    let need = |n: &str| staff.col(n).ok_or_else(|| format!("staff export: column {n:?} missing"));
    let (club, role, listed, ability, surname) = (need(&mc.club_uid)?, need(&mc.role)?, need(&mc.listed)?, need(&mc.ability)?, need(&mc.surname)?);
    let common = mc.common_name.as_deref().and_then(|n| staff.col(n));
    // per club: (role rank, ability, name); a lower rank (earlier in manager_roles) wins
    let mut best: HashMap<String, (usize, f64, String)> = HashMap::new();
    for r in &staff.rows {
        let c = r[club].trim();
        let Some(rank) = mc.manager_roles.iter().position(|m| *m == r[role].trim()) else { continue };
        if c.is_empty() || r[listed].trim().eq_ignore_ascii_case("true") {
            continue;
        }
        let name = common.map(|i| r[i].trim()).filter(|n| !n.is_empty()).unwrap_or(r[surname].trim());
        if name.is_empty() {
            continue;
        }
        let a = r[ability].trim().parse::<f64>().unwrap_or(0.0);
        if best.get(c).is_none_or(|(br, ba, _)| rank < *br || (rank == *br && a > *ba)) {
            best.insert(c.to_string(), (rank, a, game_safe(name)));
        }
    }
    Ok(best.into_iter().map(|(c, (_, _, n))| (c, n)).collect())
}

/// Decision 1 phase B (#31): the LMA clubs with a squad but no source, outside the league ladders
/// (country 15 when the base has the column), are taken over by the attribute source's clubs that
/// LMA lacks and that have a squad (`min_squad`): richest LMA club first, each taking the same
/// club under another spelling if there is one (same nation, a distinctive name word in common:
/// "Crvena Zvezda" and "FK Crvena zvezda Beograd", "Kryliya Sovetov" and "Krylja Sovetov Samara"),
/// else the strongest such club of its nation (its vanilla squad's most common), else the
/// strongest left from outside the eight league nations (they belong in their own leagues).
/// The club keeps its id (its league, if any, and its record); its squad and names are the new
/// club's. Returns (id, name, short name) for clubs.csv.
#[allow(clippy::too_many_arguments)]
fn take_over(
    clubs: &Sheet,
    players: &Sheet,
    by_club: &mut BTreeMap<u32, (String, String, Vec<Row>)>,
    other: BTreeMap<String, Vec<Row>>,
    fm: Option<(&Sheet, &FmConfig)>,
    lma_nations: &HashMap<String, u32>,
    cfg: &Config,
    report: &mut Report,
) -> Vec<(u32, String, String)> {
    let (Some(cid), Some(pclub), Some(pnat)) = (clubs.col("id"), players.col("club_id"), players.col("nation_id")) else { return Vec::new() };
    if other.is_empty() {
        return Vec::new();
    }
    let (ccountry, cwealth, cname) = (clubs.col("country"), clubs.col("wealth"), clubs.col("name_E"));
    // each LMA club's squad size and most common vanilla nation
    let mut squads: HashMap<u32, HashMap<String, usize>> = HashMap::new();
    for r in &players.rows {
        if let Ok(c) = r[pclub].trim().parse::<u32>() {
            *squads.entry(c).or_default().entry(r[pnat].trim().to_string()).or_default() += 1;
        }
    }
    let majority = |t: &HashMap<String, usize>| t.iter().max_by_key(|(n, c)| (**c, std::cmp::Reverse((*n).clone()))).map(|(n, _)| n.clone()).unwrap_or_default();
    let mut targets: Vec<(u32, u32, String, String)> = clubs
        .rows
        .iter()
        .filter_map(|r| {
            let id = r[cid].trim().parse::<u32>().ok()?;
            let rest_of_world = ccountry.is_none_or(|c| r[c].trim() == "15");
            (rest_of_world && squads.contains_key(&id) && !by_club.contains_key(&id)).then(|| {
                let wealth = cwealth.and_then(|c| r[c].trim().parse().ok()).unwrap_or(0);
                (id, wealth, majority(&squads[&id]), cname.map(|c| r[c].trim().to_string()).unwrap_or_default())
            })
        })
        .collect();
    targets.sort_by_key(|t| (std::cmp::Reverse(t.1), t.0));
    // the candidates: their nation (most common among their players) and strength (the best XI's
    // mean overall)
    let mut cands: Vec<(String, String, f64, Vec<Row>)> = other
        .into_iter()
        .filter(|(_, rows)| rows.len() >= cfg.min_squad)
        .map(|(name, rows)| {
            let mut t: HashMap<String, usize> = HashMap::new();
            for r in &rows {
                *t.entry(r.nation.clone()).or_default() += 1;
            }
            let nation = majority(&t);
            let nation = cfg.nation_aliases.get(&nation).cloned().unwrap_or(nation);
            let nation_id = lma_nations.get(&normalise(&nation, &BTreeMap::new())).map(u32::to_string).unwrap_or_default();
            let mut o: Vec<f64> = rows.iter().map(|r| r.overall).collect();
            o.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
            let best = o.iter().take(11).sum::<f64>() / o.len().clamp(1, 11) as f64;
            (name, nation_id, best, rows)
        })
        .collect();
    cands.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.0.cmp(&b.0)));
    // short names from the attribute source
    let short: HashMap<String, String> = fm
        .and_then(|(sheet, fc)| {
            let f = fc.fill.as_ref()?;
            let (c, s) = (sheet.col(&f.club)?, sheet.col(f.club_short.as_deref()?)?);
            Some(sheet.rows.iter().map(|r| (r[c].trim().to_string(), r[s].trim().to_string())).collect())
        })
        .unwrap_or_default();
    // the same club under another spelling first, for every club, so a richer club can't take it
    // as merely the strongest left; then the rest by nation and strength. The words a club shares
    // with the old name tell only if no other club of the nation has them all: a city alone does
    // not ("sofia" is CSKA's and Levski's too), "lokomotiv" + "sofia" does
    let words: Vec<Vec<String>> = cands.iter().map(|c| distinctive_words(&c.0)).collect();
    let mut picks: Vec<(u32, String, usize)> = Vec::new();
    let mut taken: HashSet<usize> = HashSet::new();
    let mut rest = Vec::new();
    for (id, _, nation, old) in targets {
        let mine = distinctive_words(&old);
        let telling = |k: usize| {
            let shared: Vec<&String> = words[k].iter().filter(|w| mine.contains(w)).collect();
            !shared.is_empty() && cands.iter().zip(&words).filter(|(c, ws)| c.1 == cands[k].1 && shared.iter().all(|w| ws.contains(w))).count() == 1
        };
        match cands.iter().enumerate().position(|(k, c)| !taken.contains(&k) && !nation.is_empty() && c.1 == nation && telling(k)) {
            Some(k) => {
                taken.insert(k);
                picks.push((id, old, k));
            }
            None => rest.push((id, nation, old)),
        }
    }
    // the strongest club of the same nation, else the strongest left that isn't from one of the
    // league nations: a club of a nation with LMA leagues belongs in those (the maintainer, 22 Oct: Pineto
    // Calcio, an Italian Serie C side, took UCD's Irish slot and played its European ties). With
    // none left the club keeps its 2006 squad.
    let league_nations: HashSet<String> = ["England", "Scotland", "Italy", "Spain", "Germany", "France", "Netherlands", "Holland", "Portugal"]
        .iter()
        .filter_map(|n| lma_nations.get(&normalise(n, &BTreeMap::new())).map(u32::to_string))
        .collect();
    for (id, nation, old) in rest {
        let free = |k: &usize| !taken.contains(k);
        let same = (0..cands.len()).filter(free).find(|&k| !nation.is_empty() && cands[k].1 == nation);
        let Some(k) = same.or_else(|| (0..cands.len()).filter(free).find(|&k| !league_nations.contains(&cands[k].1))) else {
            report.takeovers_kept.push(old);
            continue;
        };
        taken.insert(k);
        picks.push((id, old, k));
    }
    let mut cands: Vec<Option<(String, String, f64, Vec<Row>)>> = cands.into_iter().map(Some).collect();
    let mut names = Vec::new();
    for (id, old, k) in picks {
        let Some((name, _, _, rows)) = cands[k].take() else { continue };
        let short_name = short.get(&name).filter(|s| !s.is_empty()).cloned().unwrap_or_else(|| name.clone());
        let full = game_safe(&name);
        let shown = if full.chars().count() <= 25 { full } else { game_safe(&short_name).chars().take(25).collect() };
        let short7: String = game_safe(&short_name).chars().take(7).collect();
        report.takeovers.push((old.clone(), shown.clone()));
        names.push((id, shown, short7));
        by_club.insert(id, (name, old, rows));
    }
    names
}

#[cfg(test)]
mod level_tests {
    use super::*;

    fn peer(name: &str, age: Option<u32>, overall: f64, value: Option<f64>) -> Row {
        Row { forename: name.into(), surname: "X".into(), age, nation: String::new(), position: "MID".into(), position2: String::new(), overall, height: None, ratings: [50; 17], tm_value: value, market_value: value, fm_value: None, skin: None, hair: None, hair_colour: None, sides: None, foot: None, tm_id: None, transferred: false, tm_club: None, birth: String::new(), wage: None, contract_end: None, live_number: None }
    }

    /// A player Transfermarkt has no value for takes the attribute source's transfer value (GBP, kept
    /// in EUR terms for the store), and the fill then sees him as valued.
    #[test]
    fn fm_values_stand_in_for_transfermarkt() {
        let mut rows: Vec<Row> = vec![peer("matched", Some(25), 70.0, Some(10_000_000.0)), peer("fm", Some(25), 70.0, None), peer("nobody", Some(25), 70.0, None)];
        rows[0].fm_value = Some(17_000_000.0); // FM says GBP 17m where Transfermarkt's EUR 10m is GBP 8.5m: ratio 2
        rows[1].fm_value = Some(8_500_000.0);
        rows[2].fm_value = Some(0.0);
        let mut by_club: BTreeMap<u32, (String, String, Vec<Row>)> = BTreeMap::new();
        by_club.insert(1, (String::new(), String::new(), rows));
        let (n, ratios) = fm_values(&mut by_club, 0.85);
        assert_eq!(n, 1);
        assert_eq!((ratios[0].0.as_str(), ratios[0].1, ratios[0].2), ("all", 2.0, 1), "{ratios:?}");
        let rows = &by_club[&1].2;
        assert_eq!(rows[0].tm_value, Some(10_000_000.0), "a Transfermarkt value stays");
        assert!((rows[1].tm_value.unwrap() - 5_000_000.0).abs() < 1.0, "GBP 8.5m / ratio 2 at 0.85 = EUR 5m: {:?}", rows[1].tm_value);
        assert_eq!(rows[1].market_value, rows[1].tm_value);
        assert_eq!(rows[2].tm_value, None, "a zero value isn't one");
    }

    /// An unmatched player's value comes from matched peers of his age first (fc27final27, 21 Oct:
    /// the under-30s got 0.53-0.69x their matched band, the over-30s up to 2x), and from any age
    /// when no peer of his age is near his overall.
    #[test]
    fn filled_values_follow_age() {
        let mut rows: Vec<Row> = Vec::new();
        for i in 0..6 {
            rows.push(peer(&format!("young{i}"), Some(21 + i % 2), 50.0, Some(10_000_000.0 + i as f64)));
            rows.push(peer(&format!("old{i}"), Some(32 + i % 2), 50.0, Some(1_000_000.0 + i as f64)));
        }
        rows.push(peer("youngster", Some(22), 50.0, None));
        rows.push(peer("veteran", Some(33), 51.0, None));
        rows.push(peer("ageless", None, 50.0, None));
        rows.push(peer("teen", Some(16), 50.0, None)); // no peer within 4 years: any age
        let mut by_club: BTreeMap<u32, (String, String, Vec<Row>)> = BTreeMap::new();
        by_club.insert(1, (String::new(), String::new(), rows));
        assert_eq!(fill_values(&mut by_club), 4);
        let value = |n: &str| by_club[&1].2.iter().find(|r| r.forename == n).unwrap().market_value.unwrap();
        assert!(value("youngster") >= 10_000_000.0, "a young player's peers are the young ones");
        assert!(value("veteran") < 2_000_000.0, "an old player's peers are the old ones");
        for n in ["ageless", "teen"] {
            assert!((1_000_000.0..10_000_006.0).contains(&value(n)), "{n}: from every peer");
        }
    }

    /// Club codes: the table's for a club it names (any case), else the first word that isn't a
    /// prefix, three letters in capitals.
    #[test]
    fn strings_merge_into_the_mods_strings_csv() {
        let cfg: BTreeMap<String, String> = [("396", "Champions League"), ("30:e", "Champions League, again"), ("1135", "Adidas")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let out = strings_csv(Some("id,text\n1135,Nike\n1136,Standard Chartered\n"), &cfg).unwrap().unwrap();
        assert_eq!(out, "id,text,lang\r\n1136,Standard Chartered,\r\n1135,Adidas,\r\n30,\"Champions League, again\",E\r\n396,Champions League,\r\n");
        assert_eq!(strings_csv(None, &BTreeMap::new()).unwrap(), None);
        let bad: BTreeMap<String, String> = [("396:X".to_string(), "x".to_string())].into();
        assert!(strings_csv(None, &bad).is_err());
    }

    /// `[scale] curve`: straight lines through the points, the end segments continued beyond them,
    /// then `mul` and `add`; no curve = the old linear scale.
    #[test]
    fn rating_curve_stretches_the_top_and_lowers_the_bottom() {
        let s = Scale { mul: 1.0, add: 0.0, curve: vec![[60.0, 50.0], [80.0, 76.0], [90.0, 92.0]] };
        assert_eq!(s.curved(60.0), 50.0);
        assert_eq!(s.apply(60.0), 50.0);
        assert_eq!(s.apply(70.0), 63.0, "halfway on the first segment");
        assert_eq!(s.apply(80.0), 76.0);
        assert_eq!(s.apply(85.0), 84.0, "halfway on the second");
        assert_eq!(s.apply(95.0), 100.0, "the last segment continued");
        assert_eq!(s.apply(50.0), 37.0, "the first segment continued");
        let unsorted = Scale { mul: 2.0, add: 1.0, curve: vec![[90.0, 92.0], [60.0, 50.0]] };
        assert_eq!(unsorted.apply(75.0), (50.0 + 15.0 * 42.0 / 30.0) * 2.0 + 1.0, "sorted first, mul and add after");
        assert_eq!(Scale::default().apply(73.0), 73.0);
        assert_eq!(Scale { mul: 1.0, add: 0.0, curve: vec![[70.0, 65.0]] }.apply(80.0), 75.0, "one point: a shift");
    }

    /// `[fm.hair]`: FM's colour through the ranges, the style from length and type (type 255 = any),
    /// None for a blank value or no matching row.
    #[test]
    fn fm_hair_maps_colour_ranges_and_style_rows() {
        let h = FmHair { colour: "c".into(), kind: "k".into(), length: "l".into(), colour_lma: vec![[1, 8, 6], [9, 10, 4], [15, 18, 0]], style_lma: vec![[0, 255, 0], [1, 3, 5], [1, 255, 14], [1, 255, 1], [3, 255, 6]] };
        assert_eq!(h.map(Some(7), Some(0), Some(1), 0), (Some(14), Some(6)), "Haaland: blond, short (seed 0: the first short row)");
        assert_eq!(h.map(Some(7), Some(0), Some(1), 1), (Some(1), Some(6)), "seed 1: the second short row");
        assert_eq!(h.map(Some(12), Some(3), Some(1), 7), (Some(5), None), "Saka: the curly row wins over the any-type rows; colour 12 has no range here");
        assert_eq!(h.map(Some(15), Some(0), Some(0), 0), (Some(0), Some(0)), "bald, black");
        assert_eq!(h.map(Some(9), None, Some(3), 0), (Some(6), Some(4)), "long, light brown, type unknown");
        assert_eq!(h.map(None, Some(0), Some(2), 0), (None, None), "length 2 has no row; no colour");
    }

    #[test]
    fn name_words_fold_every_letter_a_name_carries() {
        // #614: "Yıldız" split into "y", "ld", "z" and matched nobody
        for (name, words) in [
            ("Yıldız", vec!["yildiz"]),
            ("Çalhanoğlu", vec!["calhanoglu"]),
            ("İlkay Gündoğan", vec!["ilkay", "gundogan"]),
            ("Ødegaard", vec!["odegaard"]),
            ("Đurđević", vec!["durdevic"]),
            ("Szoboszlai Dominik Ő", vec!["szoboszlai", "dominik"]),
            ("Schick Řezník", vec!["schick", "reznik"]),
            ("Drăgușin Ștefan", vec!["dragusin", "stefan"]),
            ("Þórðarson", vec!["tordarson"]),
            ("Bednarek Jędrzejczyk", vec!["bednarek", "jedrzejczyk"]),
        ] {
            assert_eq!(name_words(name), words, "{name}");
        }
    }

    #[test]
    fn manager_reputation_from_ability_at_the_club_or_by_name() {
        let staff = Sheet::parse("uid,club_uid,role,listed,ca,last_name,common_name
1,602,16,false,127,Carrick,
2,700,16,false,190,Guardiola,
3,700,5,false,100,Lijnders,
4,800,16,false,90,Smith,
5,801,16,false,95,Smith,
").unwrap();
        let mc: FmManagers = toml::from_str("club_uid = \"club_uid\"
role = \"role\"
manager_roles = [\"16\"]
listed = \"listed\"
ability = \"ca\"
surname = \"last_name\"
common_name = \"common_name\"
reputation = [40.0, 190.0]
").unwrap();
        let ab = manager_abilities(&staff, &mc).unwrap();
        // 1 + (127 - 40) / 150 x 98 = 57.8: three stars, not Ferguson's 4.5
        assert_eq!(manager_reputation(&ab, Some("602"), "Carrick", &mc), Some(58));
        assert_eq!(manager_reputation(&ab, None, "Guardiola", &mc), Some(99), "by name: the only manager so called");
        assert_eq!(manager_reputation(&ab, Some("700"), "Lijnders", &mc), Some(40), "a coach at his club");
        assert_eq!(manager_reputation(&ab, None, "Lijnders", &mc), None, "not a manager role, so not by name");
        assert_eq!(manager_reputation(&ab, None, "Smith", &mc), None, "two managers so called");
        let staff = Sheet::parse("uid,club_uid,role,listed,ca,last_name,common_name\n1,800,16,false,171,Martinez Garcia,Luis Enrique\n").unwrap();
        let ab = manager_abilities(&staff, &mc).unwrap();
        assert_eq!(manager_reputation(&ab, None, "Enrique", &mc), Some(87), "a common name's last word");
        assert_eq!(mc.reputation_default, 50);
    }

    #[test]
    fn club_codes_from_the_table_or_the_name() {
        let table: BTreeMap<String, String> = [("Manchester United".to_string(), "MUN".to_string())].into();
        assert_eq!(club_code("manchester united", &table), "MUN");
        assert_eq!(club_code("Manchester City", &table), "MAN");
        assert_eq!(club_code("FC Porto", &table), "POR");
        assert_eq!(club_code("1. FC Köln", &table), "KOL");
        assert_eq!(club_code("AC Milan", &table), "MIL");
        assert_eq!(club_code("Arsenal", &table), "ARS");
    }

    /// The FC 27 mapping's mod keys reach the mod's mod.toml: the money keys `runtime::mods`
    /// reads (value unit, income, stored values, the board's share, the wage budget cap).
    #[test]
    fn fc27_mapping_writes_the_money_keys() {
        let cfg: Config = toml::from_str(include_str!("../../../mappings/fc27.toml")).unwrap();
        let s = mod_toml("fc27", &cfg, &[]);
        for key in ["base_year = 2026", "value_unit = 5000", "income_scale = 4.0", "values_as_stored = true", "board_share = 1.0", "wage_budget_cap = 10000000"] {
            assert!(s.lines().any(|l| l == key), "{key} missing from:\n{s}");
        }
        // the contract screen's wage ceilings as six words, the first raising 100,000 to 500,000
        assert_eq!(s.matches("[[patch]]").count(), 6, "{s}");
        assert!(s.contains("[[patch]]\naddr = \"0x8200af9c\"\nfrom = 100000\nto = 500000\n"), "{s}");
        assert!(s.contains("addr = \"0x8200afb0\"\nfrom = 5000\nto = 20000\n"), "{s}");
        // and the mod loads: the [[patch]] tables after every key
        let parsed: toml::Value = toml::from_str(&s).unwrap();
        assert_eq!(parsed["patch"].as_array().unwrap().len(), 6);
        assert_eq!(parsed["wage_budget_cap"].as_integer(), Some(10_000_000));
    }

    /// wage_budgets.csv sums each club's wages and applies the headroom; free agents and players
    /// without a wage don't count.
    #[test]
    fn wage_budgets_sum_each_squad() {
        let players = "id,club_id,forename\n1,0,A\n2,0,B\n3,9,C\n4,-1,D\n5,9,E\n";
        let wages = "id,wage,years\n1,100000,3\n2,50000,2\n3,300000,5\n4,10000,1\n";
        let s = wage_budgets_csv(players, wages, 1.25, &[]).unwrap();
        assert_eq!(s, "id,bill_gbp_week,budget_gbp_week\n9,300000,375000\n0,150000,187500\n");
        // a player kept beyond club 0's 32 (no club in players.csv) is on club 0's bill
        let players = "id,club_id,forename\n1,0,A\n2,0,B\n6,,F\n";
        let wages = "id,wage,years\n1,100000,3\n2,50000,2\n6,20000,1\n";
        let s = wage_budgets_csv(players, wages, 1.0, &[(6, 0)]).unwrap();
        assert_eq!(s, "id,bill_gbp_week,budget_gbp_week\n0,170000,170000\n");
    }

    #[test]
    fn contract_dates_give_their_year() {
        assert_eq!(year_of("2028-06-30 00:00:00"), Some(2028));
        assert_eq!(year_of(" 2031-06-30"), Some(2031));
        assert_eq!(year_of(""), None);
        assert_eq!(year_of("n/a"), None);
        assert_eq!(year_of("1850-01-01"), None, "out of range");
    }

    /// The game's overall (f8211f4c8) by role: weights over 13 of the 17 ratings.
    #[test]
    fn game_overall_by_role() {
        assert_eq!(Overall::row("GK", Some(7)), 0);
        assert_eq!((Overall::row("DEF", Some(2)), Overall::row("DEF", Some(1))), (1, 2), "full-back, centre-back");
        assert_eq!((Overall::row("MID", Some(4)), Overall::row("MID", Some(1))), (6, 4), "wide, central");
        assert_eq!(Overall::row("ATT", None), 7);
        let mut w = [[0.0; 13]; 8];
        w[7][3] = 1.0; // forwards: technique only
        w[7][5] = 1.0; // and shot accuracy
        let o = Overall { rows: w.to_vec() };
        let mut r = [50.0; 17];
        r[LMA_RATINGS.iter().position(|n| *n == "technique").unwrap()] = 90.0;
        r[LMA_RATINGS.iter().position(|n| *n == "shot_accuracy").unwrap()] = 70.0;
        assert_eq!(o.of(&r, "ATT", None), 80.0);
        assert_eq!(o.of(&r, "GK", None), r.iter().sum::<f64>() / 17.0, "an all-zero row falls back to the mean");
    }

    #[test]
    fn made_up_names_are_the_ones_sharing_no_word() {
        let ab = BTreeMap::new();
        assert!(made_up_name("Pamplona", "CA Osasuna", &ab));
        assert!(made_up_name("Spurs", "Tottenham Hotspur", &ab));
        assert!(!made_up_name("Man Utd", "Manchester United", &ab));
        assert!(!made_up_name("Bayern Munich", "FC Bayern München", &ab));
        assert!(!made_up_name("Valladolid", "Real Valladolid CF", &ab));
        assert!(!made_up_name("FC", "FC Porto", &ab), "no word to compare: keep the disc's");
    }

    #[test]
    fn ranks_share_ties_and_quantiles_interpolate() {
        assert_eq!(ranks(&[30.0, 10.0, 20.0]), vec![1.0, 0.0, 0.5]);
        assert_eq!(ranks(&[5.0, 5.0, 9.0]), vec![0.25, 0.25, 1.0]);
        let v = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!((quantile(&v, 0.0), quantile(&v, 1.0), quantile(&v, 0.5), quantile(&v, 0.125)), (10.0, 50.0, 30.0, 15.0));
    }

    #[test]
    fn known_as_names_split_first_word_and_rest() {
        assert_eq!(known_as("David Raya"), ("David".into(), "Raya".into()));
        assert_eq!(known_as("Kevin  De Bruyne"), ("Kevin".into(), "De Bruyne".into()));
        assert_eq!(known_as("Rodri"), (String::new(), "Rodri".into()));
    }

    #[test]
    fn age_adjustment_only_past_the_peak() {
        assert_eq!(age_adjusted(60.0, Some(32), 28, 0.5), 960.0);
        assert_eq!(age_adjusted(1.0, Some(40), 28, 0.5), 64.0, "capped at 34");
        assert_eq!(age_adjusted(60.0, Some(28), 28, 0.8), 60.0);
        assert_eq!(age_adjusted(200.0, Some(18), 28, 0.8), 200.0);
        assert_eq!(age_adjusted(60.0, None, 28, 0.8), 60.0);
    }

    /// One clubs.csv row per club id, whatever sets it: a re-seat move, a taken-over club's names,
    /// a manager and a stadium on the same id merge; ids come out in order; nothing to write is None.
    #[test]
    fn clubs_csv_merges_the_parts_of_one_club() {
        assert!(clubs_csv(&[], &[], &[], &[], &[], &[]).is_none());
        let csv = clubs_csv(
            &[(41, 2, 0, 19)],
            &[(41, "Newtown FC".into(), "Newtwn".into()), (7, "Other".into(), "Oth".into())],
            &[(41, "Smith".into()), (3, "Jones".into())],
            &[],
            &[(41, "New Ground".into(), [1, 2, 3, 4])],
            &[(41, 9)],
        )
        .unwrap();
        let rows: Vec<&str> = csv.trim_start_matches('\u{feff}').lines().collect();
        assert_eq!(rows[0], "id,record_of,country,division,name_E,name_F,name_G,name_I,name_S,short_E,short_F,short_G,short_I,short_S,manager,stadium,cap_north,cap_east,cap_south,cap_west,wealth");
        assert_eq!(rows[1..], ["3,,,,,,,,,,,,,,Jones,,,,,,", "7,,,,Other,Other,Other,Other,Other,Oth,Oth,Oth,Oth,Oth,,,,,,,", "41,2,0,19,Newtown FC,Newtown FC,Newtown FC,Newtown FC,Newtown FC,Newtwn,Newtwn,Newtwn,Newtwn,Newtwn,Smith,New Ground,1,2,3,4,9"]);
    }

    /// Managers' reputations (#628) get their own column right after `manager`, only when there
    /// are any, on the manager's row; a club without one leaves the cell empty (the base's stays).
    #[test]
    fn clubs_csv_writes_manager_reputations() {
        let csv = clubs_csv(&[], &[], &[(41, "Carrick".into()), (3, "Jones".into())], &[(41, 58)], &[], &[]).unwrap();
        let rows: Vec<&str> = csv.trim_start_matches('\u{feff}').lines().collect();
        let head: Vec<&str> = rows[0].split(',').collect();
        let (m, r) = (head.iter().position(|h| *h == "manager").unwrap(), head.iter().position(|h| *h == "manager_rep").unwrap());
        assert_eq!(r, m + 1, "right after manager");
        let cells = |id: &str| rows[1..].iter().map(|l| l.split(',').collect::<Vec<_>>()).find(|c| c[0] == id).unwrap();
        assert_eq!((cells("41")[m], cells("41")[r]), ("Carrick", "58"));
        assert_eq!((cells("3")[m], cells("3")[r]), ("Jones", ""));
        let without = clubs_csv(&[], &[], &[(3, "Jones".into())], &[], &[], &[]).unwrap();
        assert!(!without.contains("manager_rep"), "no column without reputations");
    }

    /// Real shirt numbers first (a duplicate goes to the first in squad order), the rest the
    /// lowest left; lineups lines split with quoted commas.
    #[test]
    fn shirt_numbers_keep_the_real_ones_and_fill_the_gaps() {
        assert_eq!(shirt_numbers(&[None, Some(1), Some(7), None, Some(7), None]), vec![2, 1, 7, 3, 4, 5]);
        assert_eq!(shirt_numbers(&[None, None]), vec![1, 2]);
        assert_eq!(split_csv_line("\"a\",\"b, c\",3,\"say \"\"hi\"\"\"\r"), vec!["a", "b, c", "3", "say \"hi\""]);
    }

    /// A Transfermarkt stadium's seats keep the record's stand proportions (equal without any),
    /// the rounding on the last stand; a long name is cut at a word.
    #[test]
    fn stadium_seats_spread_like_the_stands_and_names_fit() {
        assert_eq!(spread(60704, [0, 0, 0, 0]), [15176, 15176, 15176, 15176]);
        assert_eq!(spread(1000, [1, 1, 2, 0]), [250, 250, 500, 0]);
        assert_eq!(spread(1001, [1, 1, 1, 1]), [251, 250, 250, 250]);
        assert_eq!(spread(1003, [1, 3, 0, 0]), [250, 753, 0, 0]);
        assert_eq!(spread(200000, [1, 0, 0, 0]), [65535, 0, 0, 0]);
        assert_eq!(fit_words("Mapei Stadium - Citta del Tricolore", 39), "Mapei Stadium - Citta del Tricolore");
        assert_eq!(fit_words("Estadio Municipal de Futebol de Braga Axa", 39), "Estadio Municipal de Futebol de Braga");
        assert_eq!(fit_words("Stadion - Something Very Long Name Here", 20), "Stadion - Something");
        assert_eq!(fit_words("Abcdefghijklmnopqrstuvwxyz", 10), "Abcdefghij");
    }

    /// Managers (#31 decision 2): by club uid (the women's section of the same name is another
    /// club), never a listed coach, the more able of a club's two (its B side's manager).
    #[test]
    fn a_clubs_manager_is_its_unlisted_role_16_by_uid() {
        let staff = Sheet::parse(
            "club_uid,unknown_r4,in_club_lists,ability_current,last_name,common_name\n\
             630,16,false,140,Rosenior,\n2000099863,16,false,150,Bompastor,\n630,16,true,160,Terry,\n\
             757,16,false,150,Kovac,\n757,16,false,115,Rios,\n5879,16,false,155,Sanchez,Michel\n\
             713,2,false,125,Eckert,\n713,10,false,130,Bitcon,\n630,2,false,170,Caretaker,\n",
        )
        .unwrap();
        let mc = FmManagers {
            club_uid: "club_uid".into(),
            role: "unknown_r4".into(),
            manager_roles: vec!["16".into(), "2".into()],
            listed: "in_club_lists".into(),
            ability: "ability_current".into(),
            surname: "last_name".into(),
            common_name: Some("common_name".into()),
            reputation: None,
            reputation_default: 50,
        };
        let m = club_managers(&staff, &mc).unwrap();
        assert_eq!((m["630"].as_str(), m["2000099863"].as_str(), m["757"].as_str(), m["5879"].as_str()), ("Rosenior", "Bompastor", "Kovac", "Michel"));
        // no manager: the caretaker (role 2), not the more able director (role 10)
        assert_eq!(m["713"], "Eckert");
    }

    #[test]
    fn sides_are_the_union_of_the_positions_and_feet_come_with_both() {
        let map: BTreeMap<String, String> = [("RW", "R"), ("RM", "R"), ("ST", "C"), ("LW", "L"), ("GK", "LRC")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        assert_eq!(sides_of(["RW", "RM"].into_iter(), &map), Some(2), "Saka: right");
        assert_eq!(sides_of(["ST", "LW"].into_iter(), &map), Some(5), "Mbappé: left and centre");
        assert_eq!(sides_of(["GK"].into_iter(), &map), Some(7));
        assert_eq!(sides_of(["XX"].into_iter(), &map), None, "unknown labels leave the record's");
        assert_eq!((foot_of("Left", Some(4), 5), foot_of("Right", Some(2), 5), foot_of("right", Some(5), 5)), (Some(2), Some(1), Some(3)));
        assert_eq!(foot_of("", None, 5), None);
    }

    #[test]
    fn values_round_to_vanilla_steps_within_the_field() {
        let k = |eur: f64| value_steps(eur * default_value_scale(), 1000);
        assert_eq!(k(200e6), 42500);
        assert_eq!(k(60e6), 12750);
        assert_eq!(k(7.5e6), 1600, "1593.75 to the nearest 50");
        assert_eq!(k(300e3), 65, "63.75 to the nearest 5");
        assert_eq!(k(10e3), 5, "a priced player stays priced");
        assert_eq!(k(400e6), 65535, "the u16 caps it");
        // GBP 5,000 units (mod.toml value_unit): real-level values fit, Yamal's EUR 200m too
        let u = |eur: f64| value_steps(eur * 0.85, 5000);
        assert_eq!(u(200e6), 34000, "GBP 170m");
        assert_eq!(u(110e6), 18700, "Saka: GBP 93.5m");
        assert_eq!(u(5e3), 1, "a priced player stays priced");
        assert_eq!(u(400e6), 65535, "GBP 327.7m at most");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The face follows the skin's tone set (#697): white, medium, brown, black by the importer's
    /// skin value, oriental for a light or olive player of an oriental nation, the variant by seed.
    #[test]
    fn faces_follow_the_skin() {
        use lma_data::db::FACE_SETS;
        for seed in 0..6 {
            assert!(FACE_SETS[0].contains(&face_for(0, false, seed)));
            assert!(FACE_SETS[1].contains(&face_for(1, false, seed)));
            assert!(FACE_SETS[2].contains(&face_for(3, false, seed)), "FM 11-15: brown");
            assert!(FACE_SETS[3].contains(&face_for(2, false, seed)), "FM 16-20: black");
            assert!(FACE_SETS[4].contains(&face_for(0, true, seed)));
            assert!(FACE_SETS[3].contains(&face_for(2, true, seed)), "a dark player of an oriental nation stays dark");
        }
        assert_ne!(face_for(0, false, 0), face_for(0, false, 1), "variants spread");
    }

    #[test]
    fn wealth_by_rank_keeps_each_countrys_2006_spread() {
        // country 0: base tiers 15, 9, 3; country 1: 8, 2. Funds order differs from the base order.
        let clubs = Sheet::parse("id,country,wealth
0,0,15
1,0,9
2,0,3
3,1,8
4,1,2
").unwrap();
        let mut w = vec![(0, 0, 1.0), (1, 0, 50.0), (2, 0, 5.0), (3, 1, 0.5), (4, 1, 0.9)];
        rank_tiers(&mut w, &clubs);
        assert_eq!(w.iter().map(|x| x.1).collect::<Vec<_>>(), [3, 15, 9, 2, 8]);
    }

    #[test]
    fn names_normalise() {
        let mut ab = BTreeMap::new();
        ab.insert("utd".into(), "united".into());
        ab.insert("man".into(), "manchester".into());
        assert_eq!(normalise("Man Utd", &ab), normalise("Manchester United FC", &ab));
        assert_eq!(normalise("Atlético de Madrid", &ab), "atleticomadrid");
    }

    #[test]
    fn names_fit_the_game_font() {
        assert_eq!(game_safe("Modrić"), "Modric");
        assert_eq!(game_safe("Szczęsny"), "Szczesny");
        assert_eq!(game_safe("Ødegaard"), "Ødegaard"); // in cp1252
        assert_eq!(game_safe("Łukasz"), "Lukasz");
    }

    #[test]
    fn ages() {
        assert_eq!(age_on("1998-12-20", "2026-07-01"), Some(27));
        assert_eq!(age_on("2000-07-01", "2026-07-01"), Some(26));
        assert_eq!(age_on("bad", "2026-07-01"), None);
    }
}

#[cfg(test)]
mod carry_tests {
    use super::*;

    /// `--from`: the earlier mod's hand-kept keys and its patches at addresses the new file doesn't
    /// patch come over; the importer's own keys and patches stay as written.
    #[test]
    fn hand_kept_rows_follow_the_club_to_its_new_id() {
        // Nantes is renamed "FC Nantes" by the new import: its record (251) still carries the row
        let prev = "id,record_of,name_E\n0,0,Arsenal\n244,244,AS Nancy-Lorraine\n251,251,Nantes\n999,999,Gone FC\n";
        let new = "id,record_of,name_E\n0,0,Arsenal\n263,244,AS Nancy-Lorraine\n264,251,FC Nantes\n";
        let c = carry_by_club("id,funds_m,source\n0,197.59,budget\n244,5.0,budget\n251,6.0,budget\n999,1.0,budget\n", prev, new).unwrap();
        assert_eq!(c.text, "id,funds_m,source\n0,197.59,budget\n263,5.0,budget\n264,6.0,budget\n");
        assert_eq!((c.kept, c.moved), (3, 2));
        assert_eq!(c.dropped, vec!["Gone FC".to_string()]);
        assert!(carry_by_club("club,x\nArsenal,1\n", prev, new).is_err(), "the first column must be id");
        // an older clubs.csv without record_of: by name
        let c = carry_by_club("id,x\n0,1\n5,2\n", "id,name_E\n0,Arsenal\n5,Luton\n", "id,name_E\n0,Arsenal\n9,Luton\n").unwrap();
        assert_eq!(c.text, "id,x\n0,1\n9,2\n");
        // record_of in one generation only: by name too, instead of dropping every row
        let c = carry_by_club("id,x\n0,1\n5,2\n", "id,name_E\n0,Arsenal\n5,Luton\n", "id,record_of,name_E\n0,0,Arsenal\n9,5,Luton\n").unwrap();
        assert_eq!(c.text, "id,x\n0,1\n9,2\n");
        // an id neither clubs.csv lists keeps its row as it is
        let c = carry_by_club("id,x\n0,1\n77,3\n", "id,name_E\n0,Arsenal\n", "id,name_E\n0,Arsenal\n").unwrap();
        assert_eq!(c.text, "id,x\n0,1\n77,3\n");
        assert_eq!((c.kept, c.moved), (2, 0));
        // a blank record_of is the club's own record: unmoved clubs keep their ids, a moved one follows
        let c = carry_by_club("id,x\n0,1\n1,2\n2,3\n", "id,record_of,name_E\n0,,A\n1,,B\n2,,C\n", "id,record_of,name_E\n0,,A\n1,2,C\n2,1,B\n").unwrap();
        assert_eq!(c.text, "id,x\n0,1\n2,2\n1,3\n");
        assert_eq!((c.kept, c.moved), (3, 2));
    }

    #[test]
    fn carry_keeps_an_earlier_mods_keys_and_patches() {
        let new = "name = \"fc27final28\"\nversion = \"2026-07-01\"\nbase_year = 2026\nvalue_unit = 5000\nboard_share = 1.0\n\n[[patch]]\naddr = \"0x8200af9c\"\nfrom = 100000\nto = 500000\n";
        let prev = "name = \"fc27final27\"\nbase_year = 2026\nvalue_unit = 4000\nwage_cap = 2000000\nsubs = 5\neurope_places = [[4, 0, 1], [0, 2, 1]]\n[[patch]]\naddr = \"0x8200AF9C\"\nfrom = 100000\nto = 400000\n[[patch]]\naddr = \"0x82003208\"\nfrom = 1059481190\nto = 1065353216\n[[patch]]\naddr = \"0x82703118\"\nfrom = 300\nto = 71\n";
        let out = carry_mod_toml(new, prev, "fc27final27").unwrap();
        let t: toml::Table = toml::from_str(&out).unwrap();
        assert_eq!(t["name"].as_str(), Some("fc27final28"), "{out}");
        assert_eq!(t["value_unit"].as_integer(), Some(5000), "the new file's own keys stay");
        assert_eq!((t["wage_cap"].as_integer(), t["subs"].as_integer()), (Some(2000000), Some(5)));
        assert_eq!(t["europe_places"].as_array().map(|a| a.len()), Some(2));
        let patches: Vec<(&str, i64)> = t["patch"].as_array().unwrap().iter().map(|p| (p["addr"].as_str().unwrap(), p["to"].as_integer().unwrap())).collect();
        assert_eq!(patches, vec![("0x8200af9c", 500000), ("0x82703118", 71)], "the board share's raw patch yields to the key: {out}");
        assert!(out.contains("# kept from fc27final27"), "{out}");
        assert!(carry_mod_toml(new, "wage_cap = [", "x").is_err());
    }
}

#[cfg(test)]
mod last_tables_tests {
    use super::*;

    /// England: the Transfermarkt order first, the mod's other members in the base's order, the
    /// base's rows for the places left (clubs the mod relegated); an older season doesn't count.
    /// Spain: 22 members cut to the game's 20, and tied points split head-to-head.
    #[test]
    fn last_tables_follow_transfermarkt_then_the_base() {
        let mut cfg: Config = toml::from_str(include_str!("../../../mappings/fc27.toml")).unwrap();
        cfg.transfermarkt.as_mut().unwrap().history.as_mut().unwrap().last_season = Some(2025);
        let dir = std::env::temp_dir().join(format!("lma_last_tables_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut base = String::from("country,tier,position,club_id,club,uid\n");
        for k in 1..=20 {
            base += &format!("0,0,{k},{k},Club {k},{}\n", 5000 + k);
        }
        for k in 1..=20 {
            base += &format!("2,0,{k},{},Club {},{}\n", 100 + k, 100 + k, 6000 + k);
        }
        std::fs::write(dir.join("last_tables.csv"), base).unwrap();
        let mut games = String::from("competition_id,season,date,home_club_id,away_club_id,home_club_goals,away_club_goals,home_club_name,away_club_name\n");
        // GB1 2025/26: t5 (6 points, +4) over t4 (6, +2), t3 (3, 0) over t2 (3, -2), t1 last
        // t19 (relegated by the mod: division 19 now) drew with t1: in last season's table above it
        for (h, a, hg, ag) in [("t5", "t4", 2, 0), ("t5", "t3", 2, 0), ("t4", "t3", 1, 0), ("t3", "t2", 3, 0), ("t2", "t1", 1, 0), ("t1", "t4", 0, 2), ("t19", "t1", 0, 0)] {
            games += &format!("GB1,2025,2026-05-01,{h},{a},{hg},{ag},{h},{a}\n");
        }
        games += "GB1,2024,2025-05-01,t1,t5,9,0,t1,t5\n";
        // a 2026/27 just begun (one game) is not last season
        games += "GB1,2026,2026-08-15,t1,t5,9,0,t1,t5\n";
        // ES1: t101 and t102 tie on 4 points; t102 has the better goal difference, t101 won their game
        for (h, a, hg, ag) in [("t101", "t102", 1, 0), ("t102", "t103", 5, 0), ("t101", "t103", 0, 0), ("t103", "t102", 0, 0)] {
            games += &format!("ES1,2025,2026-05-01,{h},{a},{hg},{ag},{h},{a}\n");
        }
        std::fs::write(dir.join("games.csv"), games).unwrap();
        let mut clubs = String::from("id,name_E,country,division\n");
        for k in 1..=18 {
            clubs += &format!("{k},Club {k},0,0\n");
        }
        clubs += "19,Club 19,0,19\n20,Club 20,0,19\n21,FA XI,0,0\n";
        for k in 101..=122 {
            clubs += &format!("{k},Club {k},3,6\n");
        }
        let clubs = clubs.lines().enumerate().map(|(n, l)| format!("{l},{}\n", if n == 0 { "squad_size" } else if l.starts_with("21,") { "0" } else { "25" })).collect::<String>();
        let clubs = Sheet::parse(&clubs).unwrap();
        let tm_final: HashMap<String, u32> = [("t1", 1), ("t2", 2), ("t3", 3), ("t4", 4), ("t5", 5), ("t19", 19), ("t101", 101), ("t102", 102), ("t103", 103)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let mut report = Report::default();
        let takeovers = vec![(5u32, "Club Five".to_string(), String::new())];
        let out = last_tables_csv(&dir, Some(&dir.join("games.csv")), &cfg, &HashMap::new(), &tm_final, &HashMap::new(), &[], &takeovers, &clubs, &mut report).unwrap().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let rows: Vec<Vec<&str>> = out.trim_start_matches('\u{feff}').lines().skip(1).map(|l| l.split(',').collect()).collect();
        let league = |c: &str| rows.iter().filter(|r| r[0] == c && r[1] == "0").map(|r| r[3].parse::<u32>().unwrap()).collect::<Vec<_>>();
        let england = league("0");
        assert_eq!(&england[..6], &[5, 4, 3, 2, 19, 1], "the Transfermarkt order, the relegated club included");
        assert_eq!(&england[6..19], &(6..=18).collect::<Vec<u32>>(), "the league's other clubs in the base's order");
        assert_eq!(&england[19..], &[20], "the base's rows fill the places left; the historic side never");
        let spain = league("2");
        assert_eq!(spain.len(), 20, "the game's size");
        assert_eq!(&spain[..3], &[101, 102, 103], "tied points: the head-to-head winner first");
        assert!(!spain.contains(&121) && !spain.contains(&122), "members beyond the size are cut");
        assert!(report.last_tables.iter().any(|l| l.starts_with("England tier 0: 20 places: 6 from GB1 2025/26 (7 games), 13 of the league's 18 clubs in the base's order, 1 base rows kept")), "{:?}", report.last_tables);
        assert!(out.contains("0,0,1,5,Club Five,\r\n") && out.contains("0,0,5,19,Club 19,\r\n"), "{out}");
    }

    /// Without a pinned season the latest one played in full is last season: a 2026/27 with one game
    /// doesn't replace the finished 2025/26, and a pinned season wins over both.
    #[test]
    fn last_tables_take_the_latest_full_season() {
        let mut cfg: Config = toml::from_str(include_str!("../../../mappings/fc27.toml")).unwrap();
        let dir = std::env::temp_dir().join(format!("lma_last_season_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut base = String::from("country,tier,position,club_id,club,uid
");
        for k in 1..=20 {
            base += &format!("0,0,{k},{k},Club {k},{}
", 5000 + k);
        }
        std::fs::write(dir.join("last_tables.csv"), base).unwrap();
        let mut games = String::from("competition_id,season,date,home_club_id,away_club_id,home_club_goals,away_club_goals
");
        // 2025/26: three clubs, a double round robin, t3 wins every game; 2024/25 the same with t2 on top
        for (season, top) in [(2025, "t3"), (2024, "t2")] {
            for h in ["t1", "t2", "t3"] {
                for a in ["t1", "t2", "t3"] {
                    if h != a {
                        games += &format!("GB1,{season},{season}-10-01,{h},{a},{},{}
", (h == top) as u32 * 2, (a == top) as u32 * 2);
                    }
                }
            }
        }
        games += "GB1,2026,2026-08-15,t1,t3,9,0
";
        std::fs::write(dir.join("games.csv"), games).unwrap();
        let clubs = Sheet::parse(&(1..=20).fold(String::from("id,name_E,country,division
"), |s, k| s + &format!("{k},Club {k},0,0
"))).unwrap();
        let tm_final: HashMap<String, u32> = [("t1", 1), ("t2", 2), ("t3", 3)].iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let first = |cfg: &Config| {
            let mut report = Report::default();
            let out = last_tables_csv(&dir, Some(&dir.join("games.csv")), cfg, &HashMap::new(), &tm_final, &HashMap::new(), &[], &[], &clubs, &mut report).unwrap().unwrap();
            (out.lines().nth(1).unwrap().to_string(), report.last_tables.iter().find(|l| l.starts_with("England tier 0")).unwrap().clone())
        };
        let (row, line) = first(&cfg);
        assert!(row.starts_with("0,0,1,3,"), "2025/26 in full is last season: {row}");
        assert!(line.contains("from GB1 2025/26 (6 games)"), "{line}");
        cfg.transfermarkt.as_mut().unwrap().history.as_mut().unwrap().last_season = Some(2024);
        let (row, line) = first(&cfg);
        assert!(row.starts_with("0,0,1,2,"), "the pinned season: {row}");
        assert!(line.contains("from GB1 2024/25"), "{line}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Without a base last_tables.csv there is nothing to write.
    #[test]
    fn last_tables_need_a_base() {
        let cfg: Config = toml::from_str(include_str!("../../../mappings/fc27.toml")).unwrap();
        let clubs = Sheet::parse("id,name_E,country,division\n1,Club 1,0,0\n").unwrap();
        let mut report = Report::default();
        assert_eq!(last_tables_csv(Path::new("no-such-base"), None, &cfg, &HashMap::new(), &HashMap::new(), &HashMap::new(), &[], &[], &clubs, &mut report).unwrap(), None);
    }
}
