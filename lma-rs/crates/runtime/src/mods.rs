//! Mod spaces: the game folder is never modified; each mod lives in `mods/<name>/` and is
//! applied in memory when the game starts.
//!
//! ```text
//! mods/<name>/
//!   mod.toml        name = "2025-26", version = "1.0", base = "Export.mng" | "ExportPR.mng"
//!                   base_year = 2026              (optional: the season the game starts in, #31)
//!                   calendar_weekdays = true      (optional, with base_year: every round on its 2006 weekday, #703)
//!                   calendar_weeks = 1            (optional, with calendar_weekdays: every round a week later,
//!                                                  2026/27's real opening days, PL Sat 22 Aug, EFL Sat 15 Aug; a
//!                                                  season whose late-June rounds can't move keeps its weekdays only)
//!                   value_unit = 5000             (optional: GBP per unit of a player's stored value, vanilla 1000)
//!                   income_scale = 4.0            (optional: factor on gate receipts, sponsorship, prize money)
//!                   gate_scale = 8.0              (optional: the gate receipts' own factor instead)
//!                   values_as_stored = true       (optional: a player with a stored value is worth exactly that)
//!                   wage_budget_cap = 10000000    (optional: the clubs' weekly wage budget cap, vanilla GBP 3.2m)
//!                   wage_cap = 2000000            (optional: the most a contract offer may pay a week, vanilla GBP 250,000)
//!                   ai_signing_wage_cap = 1000000 (optional: the most an AI club's signing may pay a week, vanilla GBP 100,000)
//!                   sponsor_offer_scale = [24.0, 6.0, 3.0]  (optional: shirt sponsorship offers x this by division, top first)
//!                   sponsor_offer_country = [1.0, 0.15, 0.6, 0.8, 0.8, 0.6, 0.2, 0.2]  (optional, with the above: x this
//!                                                 per country: England, Scotland, Italy, Spain, Germany, France,
//!                                                 Netherlands, Portugal)
//!                   subs = 5                      (optional: substitutions in a competitive match, 3-5, vanilla 3)
//!                   bench = 9                     (optional: substitutes a match can bring on, 5-9, vanilla 5; the
//!                                                 ones after the 5 are the first reserves after the matchday 16)
//!                   ai_wage_offers = true         (optional: AI clubs offer at least the player's wage + 10%, wages above GBP 250k a week possible)
//!                   formations = "formations.csv" (optional: formation rows added or changed, see below)
//!                   board_share = 1.0             (optional: the share of a club's funds the board releases for transfers, vanilla 0.65)
//!                   sponsor_unit = 100            (optional: GBP per stored unit of a sponsorship deal's weekly amount, vanilla 10)
//!                   [[patch]] addr = "0x82005280", from = 30000000, to = 60000000   (optional)
//!   database/       players.csv / clubs.csv (lma-data CSV format, only what changes)
//!   funds.csv       optional: `id,funds_m[,source]`, clubs' starting funds in GBP millions by
//!                   club record index (`hooks::funds`; the importer's funds_targets.csv)
//!   wages.csv       optional: `id,wage[,years]`, players' weekly wages in GBP and contract
//!                   seasons left (`hooks::wage`)
//!   squad_overflow.csv  optional: `id,club_id`, players a club owns beyond its 32-id squad array
//!                   (`hooks::player_club`, #538)
//!   formations.csv  optional (the file `formations` names): `id,name,roles`, one formation per row.
//!                   id 0-13 changes a vanilla shape, 14 and 15 add one (a team's choice is a 4-bit
//!                   field, so 16 is the limit); roles = the 11 slot roles, GK FB SW CB WB CM WM AM
//!                   CF WF (or 0-9), e.g. `14,3-4-2-1,GK WB WB CB CB CB CM AM CF AM CM`. Both tables
//!                   are copied into free memory with the rows applied (`guest::rec::FORMATION_*`).
//! ```
//!
//! `base_year` rewrites every `li`/`addi`/`cmpi`/`cmpli` immediate 2006 in the game's code to
//! the year, in guest memory (the interpreter) and in the recompiled code (`guest::rec::BASE_YEAR`,
//! which the recompiler reads at those sites), so dates, season labels, ages and contract years
//! start from it. `[[patch]]` entries overwrite single 32-bit words of the loaded image, checked
//! against `from` first; the user's own mod.toml holds them, the repo holds no addresses.
//!
//! The database is built from the base file on the disc (`Export.mng` = start of season,
//! `ExportPR.mng` = transfer deadline) plus the CSVs, then served through the VFS whenever
//! the game opens `d:\Common\GameData\Misc\Export.mng`. Saves go to `userdata/<mod>/`.

use crate::vfs::Vfs;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// The guest path the game reads the database from.
pub const DATABASE: &str = "d:\\Common\\GameData\\Misc\\Export.mng";
const BASES: [&str; 2] = ["Export.mng", "ExportPR.mng"];
/// The club badge textures the mod's `badges/` folder is spliced into.
pub const BADGES: &str = "d:\\Common\\Frontend\\Textures\\Badge.mng";
/// The string tables a mod's strings.csv edits (`lma_data::lang`), one per language letter.
pub const LANGS: [char; 5] = ['E', 'F', 'G', 'I', 'S'];
/// `Export.mng` with its `Diary20.mng` retimed for `year` (`lma_data::diary::retime_archive`).
fn retime_diary(export: &[u8], year: i32, weeks: u32) -> Result<Vec<u8>, String> {
    let mut ar = lma_data::Archive::parse(export).map_err(|e| e.to_string())?;
    let diary = ar.get("Diary20.mng").ok_or("Export.mng has no Diary20.mng")?;
    let retimed = lma_data::diary::retime_archive(diary, year, weeks).map_err(|e| e.to_string())?;
    ar.set("Diary20.mng", retimed);
    Ok(ar.build())
}

fn lang_path(l: char) -> String {
    format!("d:\\Common\\GameData\\Misc\\Lang\\LMA_{l}.lng")
}
/// One badge entry: 0x100 header, 128 x 256 8-bit pixels, 256 x RGBA palette.
const BADGE_SIZE: usize = 34048;
/// The disc's `ft_*` fantasy shields, entries 8..=199: no code path gives a club one (the only
/// writers of the record's badge field are the two loops at database load: country crests and
/// the 72-club table), so a mod's club badges take these slots first, then new entries.
const BADGE_FREE_SLOTS: std::ops::RangeInclusive<usize> = 8..=199;

/// The mod's badges spliced into the disc's `Badge.mng` (`ModSpace::build_badges`).
#[derive(Debug)]
pub struct Badges {
    pub archive: Vec<u8>,
    /// Badge files used.
    pub count: usize,
    /// Club record index -> entry, for the clubs whose badge the mod provides (`hooks::badge_set`).
    pub clubs: Vec<(u16, u32)>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    name: Option<String>,
    version: Option<String>,
    base: Option<String>,
    base_year: Option<u32>,
    calendar_weekdays: Option<bool>,
    calendar_weeks: Option<u32>,
    value_unit: Option<u32>,
    income_scale: Option<f64>,
    gate_scale: Option<f64>,
    wage_cap: Option<u32>,
    ai_signing_wage_cap: Option<u32>,
    sponsor_offer_scale: Option<Vec<f64>>,
    sponsor_offer_country: Option<Vec<f64>>,
    board_share: Option<f64>,
    sponsor_unit: Option<u32>,
    #[serde(default)]
    values_as_stored: bool,
    wage_budget_cap: Option<u32>,
    subs: Option<u32>,
    bench: Option<u32>,
    #[serde(default)]
    ai_wage_offers: bool,
    europe_places: Option<Vec<[u32; 3]>>,
    formations: Option<String>,
    #[serde(default)]
    patch: Vec<Patch>,
}

/// One 32-bit word of the loaded image, replaced when the mod starts (`from` must match).
#[derive(Clone, Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Patch {
    pub addr: String,
    pub from: u32,
    pub to: u32,
}

/// The year the vanilla game's code is written for (its `li rN,2006` and compares).
pub const VANILLA_YEAR: u32 = 2006;

/// The float 0.65 that `Club_TransferBudgetGet` (0x820eb058) multiplies the club's funds by: the
/// board's share of the funds for transfers. One reader in the whole image.
pub const BOARD_SHARE_SITE: u32 = 0x8200_3208;
/// Its vanilla word: 0.65f.
pub const BOARD_SHARE_VANILLA: u32 = 0x3F26_6666;

#[derive(Clone, Debug)]
pub struct ModSpace {
    /// Folder name under `mods/`, used for `--mod` and `userdata/<id>/`.
    pub id: String,
    pub name: String,
    pub version: String,
    /// Database file on the disc the mod starts from.
    pub base: String,
    pub dir: PathBuf,
    /// The season the game starts in (None: the vanilla 2006).
    pub base_year: Option<u32>,
    /// With `base_year`: the diary's rounds moved to the weekdays they had in 2006 + season
    /// (`lma_data::diary::retime_archive`, #703); without it 2006's Saturday 12 Aug is a Wednesday in 2026.
    pub calendar_weekdays: bool,
    /// With `calendar_weekdays`: every round that many weeks later (0 = none).
    pub calendar_weeks: u32,
    /// GBP per unit of a player's stored u16 value (None: the vanilla 1000; `guest::rec::VALUE_UNIT`).
    pub value_unit: Option<u32>,
    /// Factor on the clubs' gate receipts, sponsorship and prize money (`hooks::add_funds`).
    pub income_scale: Option<f64>,
    /// Factor on the gate receipts alone, over `income_scale` (the modern matchday is about twice
    /// income_scale's x4 on 2006: Forest ~13 m a season against ~25 m, #619).
    pub gate_scale: Option<f64>,
    /// A player with a stored value is worth exactly that (`hooks::valuation`).
    pub values_as_stored: bool,
    /// The clubs' weekly wage budget cap in GBP (`guest::rec::WAGE_BUDGET_CAP`; vanilla 3,200,000).
    pub wage_budget_cap: Option<u32>,
    /// The most a contract offer may pay a week, GBP (`guest::rec::WAGE_OFFER_CAP`; vanilla 250,000).
    pub wage_cap: Option<u32>,
    /// The most an AI club's signing may pay a week, GBP (`guest::rec::AI_SIGNING_WAGE_CAP`; vanilla 100,000).
    pub ai_signing_wage_cap: Option<u32>,
    /// Multipliers on the shirt sponsorship offers' base table, per division (top first; the last
    /// one covers the divisions below): the game's 2006 offers (best ~GBP 80k a week) to today's.
    pub sponsor_offer_scale: Option<Vec<f64>>,
    /// With `sponsor_offer_scale`, a further multiplier per country (the table's 8, LMA's country
    /// order): vanilla's table is nearly flat across countries, so one scale gave Scottish, Dutch and
    /// Portuguese top clubs Premier League shirt money.
    pub sponsor_offer_country: Option<Vec<f64>>,
    /// Substitutions in a competitive match (`guest::rec::SUBS`; vanilla 3, the bench holds 5).
    pub subs: Option<u32>,
    /// Substitutes a match can bring on (`hooks::BENCH_EXTRA`, #530; vanilla 5): those after the 5
    /// on the bench are the first reserves after the matchday 16 on the team sheet.
    pub bench: Option<u32>,
    /// AI clubs' wage offers in transfers at least the player's current wage + 10%, and wages
    /// rounded up to `hooks::WAGE_MAX` instead of GBP 250,000 (`hooks::AI_WAGE_OFFERS`): the game
    /// offers a 2006-scale wage from his rating, so with wages.csv's real wages every player refused
    /// (the maintainer, 13 Oct: a GBP 331k player offered 26k, #663).
    pub ai_wage_offers: bool,
    /// European places per league (`hooks::EUROPE_PLACES`, #607): [Champions League group stage,
    /// its qualifying round, UEFA Cup] for each of the 8 nations in LMA's country order (England,
    /// Scotland, Italy, Spain, Germany, France, Netherlands, Portugal). Vanilla (2006): England
    /// [2, 2, 1]; the cup winners' UEFA Cup places come on top.
    pub europe_places: Option<Vec<[u32; 3]>>,
    /// The share of a club's funds the board releases for transfers (`BOARD_SHARE_SITE`; vanilla 0.65).
    pub board_share: Option<f64>,
    /// GBP per stored unit of a sponsorship deal's weekly amount (`hooks::sponsor_*`; vanilla 10).
    pub sponsor_unit: Option<u32>,
    /// formations.csv's rows (`patch_formations`): vanilla shapes changed (ids 0-13) and new ones
    /// (14, 15), from the file `formations` names in mod.toml.
    pub formations: Option<Vec<Formation>>,
    /// The string ids of the new formations' names, in id order: `install` appends them to every
    /// language's table (`build_strings`), `patch_formations` writes them into the caption's id list.
    pub formation_string_ids: std::sync::OnceLock<Vec<u32>>,
    pub patches: Vec<Patch>,
}

/// One row of a mod's formations.csv: the formation's slot in the game's tables (0-13 vanilla,
/// 14 and 15 new), the role of each of the eleven on the pitch, in the role table's slot order
/// (`guest::rec::FORMATION_ROLES_VANILLA`: 0 GK, 1-2 the wide defenders, 3 a central, 4-5 CB, 6
/// and 10 the wide midfielders, 7-9 central; the pitch position follows the role), and the Team
/// Tactics diagram's spot for each slot (`FORMATION_SPOT_ROWS`; given or laid out from the name).
#[derive(Clone, Debug, PartialEq)]
pub struct Formation {
    pub id: usize,
    pub name: String,
    pub roles: [u32; guest::rec::FORMATION_SLOTS],
    pub spots: [u32; guest::rec::FORMATION_SLOTS],
}

/// The slot roles' names in the game's order: the role table's values 0-9.
pub const ROLE_NAMES: [&str; 10] = ["GK", "FB", "SW", "CB", "WB", "CM", "WM", "AM", "CF", "WF"];

/// The diagram's pitch grid by row (`guest::rec::FORMATION_SPOTS_VANILLA` indexes it): the goalkeeper's
/// spot, the sweeper's, then for the defence, the holding line, the midfield, the line behind the
/// strikers and the attack, the spots a line of 1-5 players takes, left to right.
pub const FORMATION_SPOT_GK: u32 = 26;
pub const FORMATION_SPOT_SW: u32 = 27;
/// New string ids start this far past the longest language table: the game passes the table's
/// length (6812 in vanilla, 388 `li` sites) as "no string", so the next ids must stay empty.
pub const FORMATION_STRING_ID_GAP: usize = 128;
pub const FORMATION_SPOT_ROWS: [[&[u32]; 5]; 5] = [
    [&[23], &[10, 11], &[0, 1, 2], &[9, 10, 11, 12], &[21, 22, 23, 24, 25]], // defence, y 64
    [&[29], &[28, 30], &[28, 29, 30], &[], &[]],                              // holding, y 51
    [&[4], &[14, 15], &[3, 4, 5], &[13, 14, 15, 16], &[28, 3, 4, 5, 30]],     // midfield, y 39
    [&[31], &[32, 33], &[32, 31, 33], &[], &[]],                              // behind the strikers, y 26
    [&[7], &[18, 19], &[6, 7, 8], &[17, 18, 19, 20], &[]],                    // attack, y 14
];

/// The diagram spots for a shape named by its lines, e.g. "4-2-3-1" or "3-5-2 (IW)": the digits
/// before any space are the lines from the back, summing to 10; a leading 1 is a sweeper; four
/// lines put the third behind the strikers when it is three or fewer (4-2-3-1), else the second
/// on the holding row (4-1-4-1). Reproduces 11 of the game's 14 rows exactly. None when the name isn't such
/// a shape or a line is too long for its row.
pub fn default_spots(name: &str) -> Option<[u32; guest::rec::FORMATION_SLOTS]> {
    let shape = name.split_whitespace().next()?;
    let mut lines: Vec<usize> = shape.split('-').map(|d| d.parse().ok()).collect::<Option<_>>()?;
    if lines.iter().sum::<usize>() != 10 || lines.iter().any(|&n| n == 0) {
        return None;
    }
    let mut out = vec![FORMATION_SPOT_GK];
    if lines.len() >= 4 && lines[0] == 1 {
        out.push(FORMATION_SPOT_SW);
        lines.remove(0);
    }
    let rows: Vec<usize> = match lines.len() {
        2 => vec![0, 4],
        3 => vec![0, 2, 4],
        4 if lines[2] <= 3 => vec![0, 2, 3, 4],
        4 => vec![0, 1, 2, 4],
        5 => vec![0, 1, 2, 3, 4],
        _ => return None,
    };
    for (&n, &row) in lines.iter().zip(&rows) {
        let spots = FORMATION_SPOT_ROWS[row].get(n - 1).copied().filter(|s| !s.is_empty())?;
        out.extend_from_slice(spots);
    }
    out.try_into().ok()
}

/// A mod's formations.csv: `id,name,roles[,spots]` (named columns, others ignored). An id 0-13
/// changes that vanilla shape, 14 and 15 add one (15 needs 14: the slots fill in order); `roles`
/// holds the 11 slot roles as names (`ROLE_NAMES`) or numbers, slot 1 the goalkeeper and no
/// other; `spots`, the diagram's 11 grid spots (0-33), may be left out for a name like "4-2-3-1"
/// (`default_spots`).
fn read_formations(path: &Path) -> Result<Vec<Formation>, String> {
    use guest::rec::{FORMATION_COUNT_VANILLA, FORMATION_MAX, FORMATION_SLOTS};
    let sheet = lma_data::csv::Sheet::read(path).map_err(|e| e.to_string())?;
    let (id, name, roles) = (sheet.col("id"), sheet.col("name"), sheet.col("roles"));
    if id.is_none() || name.is_none() || roles.is_none() {
        return Err(format!("{}: needs id, name and roles columns", path.display()));
    }
    let mut out: Vec<Formation> = Vec::new();
    for (n, row) in sheet.rows.iter().enumerate() {
        let at = || format!("{}: line {}", path.display(), n + 2);
        let i: usize = sheet.get(row, id).and_then(|v| v.parse().ok()).filter(|&i| i < FORMATION_MAX).ok_or_else(|| format!("{}: id must be 0-{} (0-{} a vanilla shape to change, {} and {} new)", at(), FORMATION_MAX - 1, FORMATION_COUNT_VANILLA - 1, FORMATION_COUNT_VANILLA, FORMATION_MAX - 1))?;
        if out.iter().any(|f| f.id == i) {
            return Err(format!("{}: id {i} twice", at()));
        }
        let name = sheet.get(row, name).unwrap_or("").trim().to_string();
        if name.is_empty() || name.len() > 11 || !name.is_ascii() || name.bytes().any(|b| b < 0x20) {
            return Err(format!("{}: name must be 1-11 ASCII characters", at()));
        }
        let words: Vec<&str> = sheet.get(row, roles).unwrap_or("").split_whitespace().collect();
        if words.len() != FORMATION_SLOTS {
            return Err(format!("{}: roles needs {FORMATION_SLOTS} entries (GK FB SW CB WB CM WM AM CF WF, or 0-9), not {}", at(), words.len()));
        }
        let mut r = [0u32; FORMATION_SLOTS];
        for (k, w) in words.iter().enumerate() {
            r[k] = ROLE_NAMES.iter().position(|x| x.eq_ignore_ascii_case(w)).map(|p| p as u32).or_else(|| w.parse::<u32>().ok().filter(|&v| v < 10)).ok_or_else(|| format!("{}: role {w:?} is not one of GK FB SW CB WB CM WM AM CF WF (or 0-9)", at()))?;
        }
        if r[0] != 0 || r[1..].contains(&0) {
            return Err(format!("{}: slot 1 is the goalkeeper (GK) and no other slot is", at()));
        }
        let spots = match sheet.get(row, sheet.col("spots")).map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => {
                let v: Vec<u32> = s.split_whitespace().map(|w| w.parse::<u32>().ok().filter(|&v| v < guest::rec::FORMATION_SPOT_COUNT)).collect::<Option<_>>().ok_or_else(|| format!("{}: spots needs 11 grid spots 0-{}", at(), guest::rec::FORMATION_SPOT_COUNT - 1))?;
                v.try_into().map_err(|_| format!("{}: spots needs 11 entries", at()))?
            }
            None => default_spots(&name).ok_or_else(|| format!("{}: name {name:?} isn't a shape like 4-2-3-1 (lines summing to 10): give its 11 diagram spots in a spots column", at()))?,
        };
        out.push(Formation { id: i, name, roles: r, spots });
    }
    if out.is_empty() {
        return Err(format!("{}: no rows", path.display()));
    }
    if out.iter().any(|f| f.id == FORMATION_MAX - 1) && !out.iter().any(|f| f.id == FORMATION_COUNT_VANILLA as usize) {
        return Err(format!("{}: id {} needs id {} (the slots fill in order)", path.display(), FORMATION_MAX - 1, FORMATION_COUNT_VANILLA));
    }
    Ok(out)
}

/// Where `patch_formations` puts the copied tables in `guest::rec::FORMATION_REGION`: the role
/// table (16 rows of 11 words), the name pointers (16), the mod's name strings (16 x 12 bytes at
/// most), the caption's string-id list (16 ids and a terminator), then the diagram's spot table
/// (16 rows of 11 words).
pub fn formation_layout() -> (u32, u32, u32, u32, u32, u32) {
    use guest::rec::{FORMATION_MAX, FORMATION_REGION, FORMATION_SLOTS};
    let roles = FORMATION_REGION.0;
    let names = roles + (FORMATION_MAX * FORMATION_SLOTS * 4) as u32;
    let strings = names + (FORMATION_MAX * 4) as u32;
    let ids = strings + (FORMATION_MAX * 12) as u32;
    let spots = ids + (FORMATION_MAX + 1) as u32 * 4;
    (roles, names, strings, ids, spots, spots + (FORMATION_MAX * FORMATION_SLOTS * 4) as u32)
}

impl ModSpace {
    /// Read `<mods_root>/<id>/mod.toml`.
    pub fn load(mods_root: &Path, id: &str) -> Result<Self, String> {
        if id.is_empty() || id.contains(['/', '\\']) || id.starts_with('.') {
            return Err(format!("bad mod name {id:?}"));
        }
        let dir = mods_root.join(id);
        let path = dir.join("mod.toml");
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let m: Manifest = toml::from_str(&text).map_err(|e| {
            let e = e.to_string();
            // a key this build doesn't know (the manifest is deny_unknown_fields): say so plainly, the
            // launcher shows this text and nothing else (12 Oct: `sponsor_unit` on a rolled-back build)
            let hint = if e.contains("unknown field") { "; this build does not know that key: remove it from mod.toml or run a newer build" } else { "" };
            format!("{}: {e}{hint}", path.display())
        })?;
        let base = m.base.unwrap_or_else(|| BASES[0].into());
        if !BASES.iter().any(|b| b.eq_ignore_ascii_case(&base)) {
            return Err(format!("{}: base must be Export.mng or ExportPR.mng, not {base:?}", path.display()));
        }
        if let Some(y) = m.base_year {
            if !(1990..=2099).contains(&y) {
                return Err(format!("{}: base_year {y} must be 1990-2099", path.display()));
            }
        }
        if m.calendar_weekdays == Some(true) && m.base_year.is_none() {
            return Err(format!("{}: calendar_weekdays needs base_year", path.display()));
        }
        if let Some(w) = m.calendar_weeks {
            if m.calendar_weekdays != Some(true) {
                return Err(format!("{}: calendar_weeks needs calendar_weekdays", path.display()));
            }
            // 2 or more would take late-June rounds past the season's end, and bringing them back
            // reorders a competition's rounds (the disc's 2006/07, #858)
            if w > 1 {
                return Err(format!("{}: calendar_weeks {w} must be 0 or 1", path.display()));
            }
        }
        if let Some(u) = m.value_unit {
            if !(1000..=100_000).contains(&u) || 1_000_000 % u != 0 {
                return Err(format!("{}: value_unit {u} must be 1000-100000 and divide 1,000,000 (e.g. 2000, 5000, 10000)", path.display()));
            }
        }
        if let Some(s) = m.income_scale {
            if !(0.1..=100.0).contains(&s) {
                return Err(format!("{}: income_scale {s} must be 0.1-100", path.display()));
            }
        }
        if let Some(s) = m.gate_scale {
            if !(0.1..=100.0).contains(&s) {
                return Err(format!("{}: gate_scale {s} must be 0.1-100", path.display()));
            }
        }
        if let Some(s) = &m.sponsor_offer_scale {
            if s.is_empty() || s.len() > 5 || s.iter().any(|v| !(0.1..=100.0).contains(v)) {
                return Err(format!("{}: sponsor_offer_scale {s:?} must be 1-5 values of 0.1-100 (top division first)", path.display()));
            }
        }
        if let Some(c) = &m.sponsor_offer_country {
            if m.sponsor_offer_scale.is_none() || c.len() != 8 || c.iter().any(|v| !(0.01..=10.0).contains(v)) {
                return Err(format!("{}: sponsor_offer_country {c:?} must be 8 values of 0.01-10 (England, Scotland, Italy, Spain, Germany, France, Netherlands, Portugal), with sponsor_offer_scale", path.display()));
            }
        }
        if let Some(c) = m.ai_signing_wage_cap {
            if !(100_000..=10_000_000).contains(&c) {
                return Err(format!("{}: ai_signing_wage_cap {c} must be 100000-10000000 (vanilla 100,000)", path.display()));
            }
        }
        if let Some(c) = m.wage_cap {
            if !(250_000..=10_000_000).contains(&c) {
                return Err(format!("{}: wage_cap {c} must be 250000-10000000 (vanilla 250,000)", path.display()));
            }
        }
        if let Some(b) = m.board_share {
            if !(0.1..=1.0).contains(&b) {
                return Err(format!("{}: board_share {b} must be 0.1-1.0 (vanilla 0.65)", path.display()));
            }
        }
        if let Some(u) = m.sponsor_unit {
            if !(10..=10_000).contains(&u) || u % 10 != 0 {
                return Err(format!("{}: sponsor_unit {u} must be 10-10000 and a multiple of 10 (vanilla 10)", path.display()));
            }
        }
        if let Some(n) = m.subs {
            // the bench holds 5, so 5 is the most a match can use without a bigger squad (#530)
            if !(3..=5).contains(&n) {
                return Err(format!("{}: subs {n} must be 3-5 (vanilla 3; the bench holds 5)", path.display()));
            }
        }
        if let Some(n) = m.bench {
            if !(5..=9).contains(&n) {
                return Err(format!("{}: bench {n} must be 5-9 (vanilla 5)", path.display()));
            }
        }
        if let Some(places) = &m.europe_places {
            // the group stage has 16 direct places and the qualifying round 32, the two holders
            // included (a full list drops the rest); a nation's record holds its top 10 and up to
            // two cup winners
            let (group, qualifying): (u32, u32) = (places.iter().map(|p| p[0]).sum(), places.iter().map(|p| p[1]).sum());
            if places.len() != 8 {
                return Err(format!("{}: europe_places needs 8 [group, qualifying, uefa] entries (England, Scotland, Italy, Spain, Germany, France, Netherlands, Portugal), not {}", path.display(), places.len()));
            }
            if let Some(i) = places.iter().position(|p| p.iter().sum::<u32>() > 8) {
                return Err(format!("{}: europe_places entry {i} gives {} places: at most 8 (the record holds the top 10 and two cup winners)", path.display(), places[i].iter().sum::<u32>()));
            }
            if group > 14 || qualifying > 30 {
                return Err(format!("{}: europe_places: {group} group stage and {qualifying} qualifying places; at most 14 and 30 (16 and 32 with the two holders)", path.display()));
            }
        }
        for p in &m.patch {
            parse_addr(&p.addr).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        let formations = m.formations.as_deref().map(|f| read_formations(&dir.join(f))).transpose()?;
        Ok(Self { id: id.to_string(), name: m.name.unwrap_or_else(|| id.to_string()), version: m.version.unwrap_or_default(), base, dir, base_year: m.base_year, calendar_weekdays: m.calendar_weekdays.unwrap_or(false), calendar_weeks: m.calendar_weeks.unwrap_or(0), ai_signing_wage_cap: m.ai_signing_wage_cap, value_unit: m.value_unit, income_scale: m.income_scale, gate_scale: m.gate_scale, wage_cap: m.wage_cap, sponsor_offer_scale: m.sponsor_offer_scale, sponsor_offer_country: m.sponsor_offer_country, sponsor_unit: m.sponsor_unit, board_share: m.board_share, values_as_stored: m.values_as_stored, wage_budget_cap: m.wage_budget_cap, subs: m.subs, bench: m.bench, ai_wage_offers: m.ai_wage_offers, europe_places: m.europe_places, formations, formation_string_ids: std::sync::OnceLock::new(), patches: m.patch })
    }

    /// Apply the mod's code patches to the loaded image in `mem`: the base year over the `.text`
    /// range `text` (start, size), then the explicit word patches. Returns notes for the log.
    pub fn patch_image(&self, mem: &mut guest::mem::Memory, text: (u32, u32)) -> Result<Vec<String>, String> {
        let mut notes = Vec::new();
        if let Some(year) = self.base_year {
            let (mut li, mut cmp, mut last, mut next) = (0, 0, 0, 0);
            for pc in (text.0..text.0.saturating_add(text.1)).step_by(4) {
                if guest::rec::NOT_YEAR_SITES.contains(&pc) {
                    continue; // a resource id, not a year
                }
                let w = mem.read32(pc);
                // 2006 is the first season; 2026 = 2006 + the 20 diary seasons is the date
                // validators' upper bound (every date past it is rejected, e.g. 1 Jan of the
                // first season's second year once the start moves), so it moves by the same offset
                // and `addi rD,rA,2007` with rA a loaded season index (a label's second half, the
                // season count under the 2006 guard: three sites, #706) is 2007 = the next year
                let shift = match w & 0xFFFF {
                    VANILLA_YEAR => 0,
                    v if v == VANILLA_YEAR + 1 && w >> 26 == 14 && (w >> 16) & 31 != 0 => 1,
                    v if v == VANILLA_YEAR + 20 => 20,
                    _ => continue,
                };
                match w >> 26 {
                    14 => li += 1,
                    10 | 11 => cmp += 1,
                    _ => continue,
                }
                match shift {
                    1 => next += 1,
                    20 => last += 1,
                    _ => {}
                }
                mem.write32(pc, (w & 0xFFFF_0000) | ((year + shift) & 0xFFFF));
            }
            guest::rec::BASE_YEAR.store(year, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("base year {year}: {li} loads and {cmp} compares of {VANILLA_YEAR} rewritten, {last} of them the last season ({VANILLA_YEAR} + 20), {next} the next year ({VANILLA_YEAR} + 1)"));
        }
        if let Some(unit) = self.value_unit {
            // the stored player value's unit (`guest::rec::VALUE_UNIT`): the two multiplies after
            // the getter and the setter's floor, rewritten for the interpreter; the recompiled code
            // reads the atomics at the same sites
            for &pc in guest::rec::VALUE_UNIT_SITES {
                let w = mem.read32(pc);
                if w >> 26 != 7 || w & 0xFFFF != 1000 {
                    return Err(format!("mod {}: value_unit: {pc:#x} is {w:#x}, not a mulli by 1000", self.id));
                }
                mem.write32(pc, (w & 0xFFFF_0000) | unit);
            }
            let pc = guest::rec::VALUE_FLOOR_SITE;
            let w = mem.read32(pc);
            if w >> 26 != 11 || w & 0xFFFF != 1000 {
                return Err(format!("mod {}: value_unit: {pc:#x} is {w:#x}, not a cmpwi with 1000", self.id));
            }
            mem.write32(pc, (w & 0xFFFF_0000) | (1_000_000 / unit));
            // the valuation's cap (GBP 40m x unit / 1000): lis + ori
            let cap = 40_000_000u64 * unit as u64 / 1000;
            let (lis, ori) = (guest::rec::VALUE_CAP_LIS, guest::rec::VALUE_CAP_SITE);
            let (wl, wo) = (mem.read32(lis), mem.read32(ori));
            if wl != 0x3D60_0262 || wo != 0x616B_5A00 {
                return Err(format!("mod {}: value_unit: {lis:#x}/{ori:#x} are {wl:#x}/{wo:#x}, not the GBP 40m cap", self.id));
            }
            mem.write32(lis, 0x3D60_0000 | (cap >> 16) as u32);
            mem.write32(ori, 0x616B_0000 | (cap & 0xFFFF) as u32);
            // the transfer offer cap (GBP 80m x unit / 1000) at its seven lis/ori pairs
            let offer = (80_000_000u64 * unit as u64 / 1000).min(0x7FFF_FFFF);
            for &(lis, ori) in guest::rec::OFFER_CAP_SITES.iter() {
                let (wl, wo) = (mem.read32(lis), mem.read32(ori));
                if wl >> 26 != 15 || wl & 0xFFFF != 0x04C4 || wo >> 26 != 24 || wo & 0xFFFF != 0xB400 {
                    return Err(format!("mod {}: value_unit: {lis:#x}/{ori:#x} are {wl:#x}/{wo:#x}, not the GBP 80m offer cap", self.id));
                }
                mem.write32(lis, (wl & 0xFFFF_0000) | (offer >> 16) as u32);
                mem.write32(ori, (wo & 0xFFFF_0000) | (offer & 0xFFFF) as u32);
            }
            guest::rec::VALUE_UNIT.store(unit, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("value unit GBP {unit}: stored values reach GBP {}, the game's own valuation GBP {cap}, transfer offers GBP {offer}, values under GBP {} are dropped", 65535u64 * unit as u64, 1_000_000 / unit * unit));
        }
        if let Some(share) = self.board_share {
            // Club_TransferBudgetGet (0x820eb058): budget = funds x 0.65 - the fees still owed on
            // pending transfers; the 0.65 is a float literal with that one reader (Ghidra xrefs,
            // 11 Oct: the maintainer's board refused 180m of 198m, every offer capped at the budget)
            let (w, bits) = (mem.read32(BOARD_SHARE_SITE), (share as f32).to_bits());
            if w == bits {
                // a `[[patch]]` of the same mod (fc27final20/22 carry the raw word) got there first
                notes.push(format!("board share {share}: already in place"));
            } else if w != BOARD_SHARE_VANILLA {
                return Err(format!("mod {}: board_share: {BOARD_SHARE_SITE:#x} is {w:#x}, not the float 0.65", self.id));
            } else {
                mem.write32(BOARD_SHARE_SITE, bits);
                notes.push(format!("board share {share}: the transfer budget is {:.0}% of the club's funds minus committed fees (vanilla 65%)", share * 100.0));
            }
        }
        if let Some(n) = self.subs {
            // `addi r16,0,3` in match setup: the limit word competitive matches get (#530)
            let w = mem.read32(guest::rec::SUBS_SITE);
            if w != 0x3A00_0003 {
                return Err(format!("mod {}: subs: {:#x} is {w:#x}, not addi r16,0,3", self.id, guest::rec::SUBS_SITE));
            }
            // and the quick-sim planner's 2-or-3 picks become n - 1 or n
            let (lo, hi) = (mem.read32(guest::rec::SUBS_SIM_LOW), mem.read32(guest::rec::SUBS_SIM_HIGH));
            if lo != 0x3BE0_0002 || hi != 0x3BE0_0003 {
                return Err(format!("mod {}: subs: {:#x}/{:#x} are {lo:#x}/{hi:#x}, not the sim planner's addi r31,0,2/3", self.id, guest::rec::SUBS_SIM_LOW, guest::rec::SUBS_SIM_HIGH));
            }
            mem.write32(guest::rec::SUBS_SITE, 0x3A00_0000 | n);
            mem.write32(guest::rec::SUBS_SIM_LOW, 0x3BE0_0000 | (n - 1));
            mem.write32(guest::rec::SUBS_SIM_HIGH, 0x3BE0_0000 | n);
            guest::rec::SUBS.store(n, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("{n} substitutions in a competitive match (vanilla 3); simulated matches plan {} or {n} when tired", n - 1));
        }
        if let Some(n) = self.bench {
            // the 16 match slots stay; reserves 16.. come on through the native slot lookup (#530)
            crate::hooks::BENCH_EXTRA.store(n - 5, std::sync::atomic::Ordering::Relaxed);
            if n > 5 {
                notes.push(format!("{n} substitutes: the {} reserves after the matchday 16 can come on too", n - 5));
            }
        }
        if let Some(scale) = self.sponsor_offer_scale.as_ref().filter(|s| !s.is_empty()) {
            // the shirt sponsorship base table: [country][division 0-4][16 club levels], x GBP 250 a
            // week; its readers (the offer list 0x8214d538, the AI's deals 0x8212b6f0) then draw
            // 45-70% of it per offer
            let first: Vec<u32> = (0..16).map(|k| mem.read32(SPONSOR_OFFER_TABLE + 4 * k)).collect();
            if first != SPONSOR_OFFER_ROW0 {
                return Err(format!("mod {}: sponsor_offer_scale: the table at {SPONSOR_OFFER_TABLE:#x} isn't the game's ({first:?})", self.id));
            }
            for row in 0..SPONSOR_OFFER_ROWS {
                let s = scale[((row % 5) as usize).min(scale.len() - 1)] * self.sponsor_offer_country.as_ref().map_or(1.0, |c| c[(row / 5) as usize]);
                for k in 0..16 {
                    let a = SPONSOR_OFFER_TABLE + 4 * (row * 16 + k);
                    let v = mem.read32(a);
                    mem.write32(a, (v as f64 * s).round().min(6_000.0) as u32);
                }
            }
            let top = (320.0 * scale[0]).round().min(6_000.0) * 250.0;
            notes.push(format!("sponsor offers x{scale:?} by division: the best top-division base GBP {top:.0} a week (vanilla 80,000)"));
            if let Some(c) = &self.sponsor_offer_country {
                notes.push(format!("sponsor offers x{c:?} by country (England, Scotland, Italy, Spain, Germany, France, Netherlands, Portugal)"));
            }
        }
        if let Some(cap) = self.wage_cap {
            // the contract offer's weekly wage cap at its twelve lis/ori pairs (the registers kept)
            for &(lis, ori) in guest::rec::WAGE_OFFER_CAP_SITES.iter() {
                let (wl, wo) = (mem.read32(lis), mem.read32(ori));
                if wl >> 26 != 15 || wl & 0xFFFF != 3 || wo >> 26 != 24 || wo & 0xFFFF != 0xD090 {
                    return Err(format!("mod {}: wage_cap: {lis:#x}/{ori:#x} are {wl:#x}/{wo:#x}, not the GBP 250,000 wage cap", self.id));
                }
                mem.write32(lis, (wl & 0xFFFF_0000) | (cap >> 16));
                mem.write32(ori, (wo & 0xFFFF_0000) | (cap & 0xFFFF));
            }
            guest::rec::WAGE_OFFER_CAP.store(cap, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("wage cap GBP {cap} a week on contract offers (vanilla 250,000)"));
        }
        if let Some(cap) = self.ai_signing_wage_cap {
            // the completion check's GBP 100,000 ceiling for AI buyers (#704)
            let (lis, ori) = guest::rec::AI_SIGNING_WAGE_CAP_SITE;
            let (wl, wo) = (mem.read32(lis), mem.read32(ori));
            if wl != 0x3C60_0001 || wo != 0x6063_86A0 {
                return Err(format!("mod {}: ai_signing_wage_cap: {lis:#x}/{ori:#x} are {wl:#x}/{wo:#x}, not the GBP 100,000 ceiling", self.id));
            }
            mem.write32(lis, 0x3C60_0000 | (cap >> 16));
            mem.write32(ori, 0x6063_0000 | (cap & 0xFFFF));
            guest::rec::AI_SIGNING_WAGE_CAP.store(cap, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("AI signings complete up to GBP {cap} a week (vanilla 100,000)"));
        }
        if let Some(cap) = self.wage_budget_cap {
            let (lis, ori) = (guest::rec::WAGE_CAP_LIS, guest::rec::WAGE_CAP_SITE);
            let (wl, wo) = (mem.read32(lis), mem.read32(ori));
            if wl != 0x3D60_0030 || wo != 0x617E_D400 {
                return Err(format!("mod {}: wage_budget_cap: {lis:#x}/{ori:#x} are {wl:#x}/{wo:#x}, not the GBP 3.2m cap", self.id));
            }
            mem.write32(lis, 0x3D60_0000 | (cap >> 16));
            mem.write32(ori, 0x617E_0000 | (cap & 0xFFFF));
            guest::rec::WAGE_BUDGET_CAP.store(cap, std::sync::atomic::Ordering::Relaxed);
            notes.push(format!("wage budget cap GBP {cap} a week"));
        }
        if let Some(rows) = &self.formations {
            notes.push(self.patch_formations(mem, rows)?);
        }
        for p in &self.patches {
            let addr = parse_addr(&p.addr)?;
            let w = mem.read32(addr);
            if w != p.from {
                return Err(format!("mod {}: patch {}: the word is {w:#x}, not {:#x}", self.id, p.addr, p.from));
            }
            mem.write32(addr, p.to);
            notes.push(format!("patched {}: {:#x} -> {:#x}", p.addr, p.from, p.to));
        }
        Ok(notes)
    }

    /// Every mod space under `mods_root` (folders with a mod.toml), sorted by id. Broken
    /// manifests are returned as errors so a launcher can show them.
    pub fn list(mods_root: &Path) -> Vec<Result<Self, String>> {
        let Ok(rd) = std::fs::read_dir(mods_root) else { return vec![] };
        let mut ids: Vec<String> = rd.flatten().filter(|e| e.path().join("mod.toml").is_file()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        ids.sort();
        ids.iter().map(|id| Self::load(mods_root, id)).collect()
    }

    /// Build the modded database from the base file in `game_dir`. Returns the bytes and notes.
    /// Returns the archive bytes, notes for the log and the clubs' English names by record index.
    pub fn build_database(&self, game_dir: &Path) -> Result<(Vec<u8>, Vec<String>, Vec<String>), String> {
        let vfs = Vfs::new(game_dir, game_dir);
        let (mut base_path, _) = vfs.resolve(&format!("d:\\Common\\GameData\\Misc\\{}", self.base));
        // A pre-built `Export.mng` in the mod folder replaces the disc's as the base (experiments
        // on tables lma-data does not edit yet, e.g. fixtures); the CSVs still apply on top.
        let mut notes = Vec::new();
        if self.dir.join(&self.base).is_file() {
            base_path = self.dir.join(&self.base);
            notes.push(format!("base {} from the mod folder", self.base));
        }
        let bytes = std::fs::read(&base_path).map_err(|e| format!("{}: {e}", base_path.display()))?;
        let mut db = lma_data::Database::parse(&bytes).map_err(|e| format!("{}: {e}", base_path.display()))?;
        let csvs = self.dir.join("database");
        if csvs.is_dir() {
            notes.extend(lma_data::edit::apply(&mut db, &csvs).map_err(|e| format!("mod {}: {e}", self.id))?);
        }
        // a club with fewer than 11 players hangs the match engine: its event picker (0x822a16f8)
        // loops until it finds an eligible player, so a fixture with such a club never finishes
        // loading (the maintainer's career on fc27final3, 10 Oct: 15 clubs with 1-7 players, fixed by #449)
        let small: Vec<String> = (0..db.club_count())
            .map(|ci| (ci, db.squad(ci).len()))
            .filter(|&(_, n)| n > 0 && n < 11)
            .map(|(ci, n)| format!("{} ({n})", db.club_name(ci)))
            .collect();
        if !small.is_empty() {
            notes.push(format!("WARNING: {} club(s) under 11 players, a match with one of them never loads: {}", small.len(), small.join(", ")));
        }
        let names = (0..db.club_count()).map(|ci| db.club_name(ci).to_string()).collect();
        let mut built = db.build().map_err(|e| format!("mod {}: {e}", self.id))?;
        if let (true, Some(year)) = (self.calendar_weekdays, self.base_year) {
            built = retime_diary(&built, year as i32, self.calendar_weeks).map_err(|e| format!("mod {}: calendar_weekdays: {e}", self.id))?;
            notes.push(format!("calendar: every round on its weekday in {year}/{} (shifted {:+} days)", (year + 1) % 100, lma_data::diary::weekday_shift(lma_data::diary::FIRST_YEAR, year as i32) + 7 * self.calendar_weeks as i64));
        }
        Ok((built, notes, names))
    }

    /// Build the database and the badges, serve them through the machine's `vfs` and hook the
    /// game's badge setter for the mod's club badges.
    pub fn install(&self, m: &mut crate::machine::Machine, game_dir: &Path) -> Result<Vec<String>, String> {
        let (db, mut notes, names) = self.build_database(game_dir)?;
        if self.values_as_stored {
            // each player's level in this database: the baseline his value moves from
            if let Ok(parsed) = lma_data::Database::parse(&db) {
                m.value_base_levels = (0..parsed.player_count()).map(|p| crate::hooks::database_level(&parsed, p)).collect();
            }
        }
        m.vfs.add_overlay(DATABASE, db);
        if let Some(b) = self.build_badges(game_dir, &names)? {
            m.vfs.add_overlay(BADGES, b.archive);
            notes.push(format!("{} badge(s) from badges/, {} of them club badges", b.count, b.clubs.len()));
            if !b.clubs.is_empty() {
                m.badge_entries = b.clubs.into_iter().collect();
                m.hook(crate::hooks::BADGE_SET, crate::hooks::badge_set);
                m.hook(crate::hooks::BADGE_GET, crate::hooks::badge_get);
            }
        }
        if let Some((tables, count)) = self.build_strings(game_dir)? {
            for (l, bytes) in tables {
                m.vfs.add_overlay(&lang_path(l), bytes);
            }
            if count > 0 {
                notes.push(format!("strings.csv: {count} string(s) replaced"));
            }
            if let Some(ids) = self.formation_string_ids.get() {
                notes.push(format!("formations: {} name string(s) appended to every language as ids {}+", ids.len(), ids[0]));
            }
        }
        let sponsors = self.dir.join("sponsors.csv");
        if sponsors.is_file() {
            m.sponsors = read_sponsors(&sponsors)?;
            notes.push(format!("sponsors.csv: {} clubs' shirt deals (GBP per season, paid weekly)", m.sponsors.len()));
        }
        // the accessors take the unit (vanilla 10); sponsors.csv needs the shirt setter's hook too.
        // The shirt field holds 14 bits of units a week: without a sponsor_unit, sponsors.csv's
        // biggest deal picks the unit (10, 100, 1000, ...), so no deal is clamped; a given unit too
        // small for it gets a warning (review on #552: a 50m deal at unit 10 stopped at 163,830)
        let need = m.sponsors.values().map(|&(shirt, _)| (shirt / 52).div_ceil(0x3fff)).max().unwrap_or(0);
        let unit = self.sponsor_unit.or((!m.sponsors.is_empty()).then(|| {
            let mut u = 10;
            while u < need {
                u *= 10;
            }
            u
        }));
        if let Some(u) = self.sponsor_unit.filter(|&u| u < need) {
            notes.push(format!("WARNING: sponsor_unit {u} is too small for sponsors.csv's biggest shirt deal (needs {need}): it stops at GBP {} a week", 0x3fffu32 * u));
        }
        if let Some(u) = unit {
            m.sponsor_unit = u;
            m.hook(crate::hooks::SPONSOR_HOARDING_GET, crate::hooks::sponsor_hoarding_get);
            m.hook(crate::hooks::SPONSOR_HOARDING_SET, crate::hooks::sponsor_hoarding_set);
            m.hook(crate::hooks::SPONSOR_SHIRT_GET, crate::hooks::sponsor_shirt_get);
            m.hook(crate::hooks::SPONSOR_SHIRT_SET, crate::hooks::sponsor_shirt_set);
            m.hook(crate::hooks::SPONSOR_HOARDING_TOTAL, crate::hooks::sponsor_hoarding_total);
            m.hook(crate::hooks::SPONSOR_TOTAL, crate::hooks::sponsor_total);
            notes.push(format!("sponsor unit GBP {u}: shirt deals reach GBP {} a week, hoardings GBP {} (vanilla 163,830 / 20,470)", 0x3fffu32 * u, 0x7ffu32 * u));
        }
        if let Some(s) = self.income_scale {
            m.income_scale = s;
            m.hook(crate::hooks::ADD_FUNDS, crate::hooks::add_funds);
            m.hook(crate::hooks::STATEMENT, crate::hooks::statement);
            notes.push(format!("income scale x{s}: gate receipts, sponsorship and prize money"));
        }
        if let Some(s) = self.gate_scale {
            m.gate_scale = Some(s);
            m.hook(crate::hooks::ADD_FUNDS, crate::hooks::add_funds);
            m.hook(crate::hooks::STATEMENT, crate::hooks::statement);
            notes.push(format!("gate scale x{s}: gate receipts"));
        }
        let budgets = self.dir.join("wage_budgets.csv");
        if budgets.is_file() {
            m.wage_budgets = read_wage_budgets(&budgets)?;
            m.hook(crate::hooks::WAGE_BUDGET_SET, crate::hooks::wage_budget_set);
            m.hook(crate::hooks::WAGE_BUDGET_GET, crate::hooks::wage_budget_get);
            notes.push(format!("wage_budgets.csv: {} clubs' least weekly wage budget", m.wage_budgets.len()));
        }
        let funds = self.dir.join("funds.csv");
        if funds.is_file() {
            m.funds = read_funds(&funds)?;
            m.hook(crate::hooks::FUNDS, crate::hooks::funds);
            notes.push(format!("funds.csv: {} clubs' starting funds", m.funds.len()));
        }
        let overflow = self.dir.join("squad_overflow.csv");
        if overflow.is_file() {
            m.squad_overflow = read_squad_overflow(&overflow)?;
            // a native block (Machine::install_blocks) unless asked to hook
            if !crate::hooks::native_mod_blocks() {
                m.hook(crate::hooks::PLAYER_CLUB, crate::hooks::player_club);
            }
            notes.push(format!("squad_overflow.csv: {} players beyond their club's 32 (the club's, but not in its squad screens, team selection or wage budget)", m.squad_overflow.len()));
        }
        let wages = self.dir.join("wages.csv");
        if wages.is_file() {
            m.wages = read_wages(&wages)?;
            // a native block (Machine::install_blocks) unless asked to hook
            if !crate::hooks::native_mod_blocks() {
                m.hook(crate::hooks::WAGE, crate::hooks::wage);
            }
            m.hook(crate::hooks::CONTRACT_SET, crate::hooks::contract_set);
            let over = m.wages.values().filter(|&&(w, _)| w > crate::hooks::WAGE_MAX).count();
            notes.push(format!("wages.csv: {} players' weekly wages{}", m.wages.len(), if over > 0 { format!(" ({over} above GBP {} kept at it)", crate::hooks::WAGE_MAX) } else { String::new() }));
        }
        crate::hooks::AI_WAGE_OFFERS.store(self.ai_wage_offers, std::sync::atomic::Ordering::Relaxed);
        if self.ai_wage_offers {
            notes.push(format!("AI wage offers: at least the player's wage + 10%, wages up to GBP {} a week", crate::hooks::WAGE_MAX));
        }
        if let Some(places) = &self.europe_places {
            *crate::hooks::EUROPE_PLACES_TABLE.write().unwrap() = places.clone();
            m.hook(crate::hooks::EUROPE_PLACES, crate::hooks::europe_places);
            notes.push(format!("European places [group stage, qualifying, UEFA Cup] per league: {places:?}"));
        }
        if self.values_as_stored {
            m.values_as_stored = true;
            // the valuation runs as a native block (Machine::install_blocks) unless asked to hook
            if !crate::hooks::native_mod_blocks() {
                m.hook(crate::hooks::VALUATION, crate::hooks::valuation);
            }
            m.hook(crate::hooks::VALUE_SET, crate::hooks::value_set);
            notes.push("values as stored: a player with a stored value is worth exactly that".to_string());
        }
        Ok(notes)
    }

    /// Club badges (#31 item 9) from the mod's `badges/` folder (`tools/badge_tm2.py` makes a .tm2
    /// from a 128x128 PNG). A file named after a club's English name, letters and digits only
    /// (`AstonVilla.tm2`, `WestBromwichAlbion.tm2`, any case), is that club's badge: it goes into a
    /// `ft_*` shield slot (8..199, unused by the game) or a new entry of the disc's `Badge.mng`,
    /// in club order while entries last (the record's 9-bit field ends at 511; the disc has 323),
    /// or replaces the disc's entry of that name in place (`Walsall.tm2`), and `hooks::badge_set`
    /// points the club at it. The
    /// game itself only knows the retail badges of 72 clubs (entries 251..322) and shows the
    /// country's crest for everyone else. Any other file replaces or adds the entry of its name
    /// (`england.tm2`, `ft_01_08rb.tm2`). None without a `badges/` folder.
    pub fn build_badges(&self, game_dir: &Path, club_names: &[String]) -> Result<Option<Badges>, String> {
        let dir = self.dir.join("badges");
        if !dir.is_dir() {
            return Ok(None);
        }
        let vfs = Vfs::new(game_dir, game_dir);
        let (path, _) = vfs.resolve(BADGES);
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut ar = lma_data::archive::Archive::parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let key = |s: &str| s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect::<String>();
        let mut by_club: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
        let mut others = Vec::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("tm2"))).collect();
        files.sort();
        let club_keys: std::collections::HashSet<String> = club_names.iter().map(|n| key(n)).collect();
        for f in files {
            let k = key(&f.file_stem().unwrap().to_string_lossy());
            if club_keys.contains(&k) {
                by_club.insert(k, f);
            } else {
                others.push(f);
            }
        }
        let read = |f: &Path| -> Result<Vec<u8>, String> {
            let data = std::fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?;
            if data.len() != BADGE_SIZE || &data[..4] != b"TIM2" {
                return Err(format!("mod {}: {} is not a badge texture ({} bytes, want {BADGE_SIZE} starting with TIM2; see tools/badge_tm2.py)", self.id, f.display(), data.len()));
            }
            Ok(data)
        };
        for f in &others {
            ar.set(&f.file_name().unwrap().to_string_lossy(), read(f)?);
        }
        // a club the disc has a badge entry for (`Walsall.tm2`) gets it replaced in place
        let retail: std::collections::HashMap<String, usize> = ar.entries.iter().enumerate().map(|(i, (n, _))| (key(n.trim_end_matches(".tm2")), i)).collect();
        let (mut clubs, mut count) = (Vec::new(), others.len());
        let free_slots: Vec<usize> = BADGE_FREE_SLOTS.filter(|&i| i < ar.entries.len() && ar.entries[i].0.starts_with("ft_")).collect();
        let mut free = free_slots.into_iter();
        for (ci, name) in club_names.iter().enumerate() {
            let k = key(name);
            let Some(f) = by_club.get(&k) else { continue };
            count += 1;
            if let Some(&i) = retail.get(&k) {
                clubs.push((ci as u16, i as u32));
                ar.entries[i].1 = read(f)?;
            } else if let Some(i) = free.next() {
                clubs.push((ci as u16, i as u32));
                ar.entries[i] = (format!("club{ci}.tm2"), read(f)?);
            } else {
                // above entry 511 too: the club record's 9-bit field can't hold it, so the getter
                // is hooked (`hooks::badge_get`) and the setter's copy only matters to the game
                // (and the save, which keeps 9 bits)
                clubs.push((ci as u16, ar.entries.len() as u32));
                ar.set(&format!("club{ci}.tm2"), read(f)?);
            }
        }
        Ok(Some(Badges { archive: ar.build(), count, clubs }))
    }

    /// Save folder for this mod space, so vanilla saves stay separate.
    pub fn user_dir(&self, userdata: &Path) -> PathBuf {
        userdata.join(&self.id)
    }
}

fn parse_addr(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x"), 16).ok().filter(|a| a % 4 == 0).ok_or_else(|| format!("patch addr {s:?} must be a hex word address"))
}

/// The mod chosen with `--mod <id>` in `args` (removed from it) or `LMA_MOD`; None = vanilla.
pub fn selected(args: &mut Vec<String>) -> Option<String> {
    if let Some(i) = args.iter().position(|a| a == "--mod") {
        args.remove(i);
        if i < args.len() {
            return Some(args.remove(i)).filter(|m| !m.is_empty() && m != "vanilla");
        }
    }
    std::env::var("LMA_MOD").ok().filter(|m| !m.is_empty() && m != "vanilla")
}

impl ModSpace {
    /// A mod's strings.csv: `id,text[,lang]` (header first), the game's string table index and its
    /// new text, in every language or only the one named (E, F, G, I, S). E.g. the 59 sponsors'
    /// names are strings (`Sponsors.bin` holds their ids): a mod gives them today's brands. The
    /// new formations' names (formations.csv ids 14, 15) are appended to every language as new
    /// ids past the longest table (`formation_string_ids`). None without either.
    pub fn build_strings(&self, game_dir: &Path) -> Result<Option<(Vec<(char, Vec<u8>)>, usize)>, String> {
        let path = self.dir.join("strings.csv");
        let mut appended: Vec<(usize, &str)> = self.formations.iter().flatten().filter(|f| f.id >= guest::rec::FORMATION_COUNT_VANILLA as usize).map(|f| (f.id, f.name.as_str())).collect();
        appended.sort();
        if !path.exists() && appended.is_empty() {
            return Ok(None);
        }
        let sheet = if path.exists() { Some(lma_data::csv::Sheet::read(&path).map_err(|e| e.to_string())?) } else { None };
        let cols = sheet.as_ref().map(|s| (s.col("id"), s.col("text"), s.col("lang")));
        if let Some((id, text, _)) = cols {
            if id.is_none() || text.is_none() {
                return Err(format!("{}: needs id and text columns", path.display()));
            }
        }
        let vfs = Vfs::new(game_dir, game_dir);
        let mut parsed = Vec::new();
        for l in LANGS {
            let (file, _) = vfs.resolve(&lang_path(l));
            let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            parsed.push((l, lma_data::lang::Lang::parse(&bytes).map_err(|e| format!("{}: {e}", file.display()))?));
        }
        // the appended ids: past every language's table and the gap, the same in each
        let base = parsed.iter().map(|(_, t)| t.strings.len()).max().unwrap_or(0) + FORMATION_STRING_ID_GAP;
        let mut tables = Vec::new();
        let mut count = 0;
        for (l, mut t) in parsed {
            if let (Some(sheet), Some((id, text, lang))) = (&sheet, cols) {
                for (n, row) in sheet.rows.iter().enumerate() {
                    if sheet.get(row, lang).is_some_and(|x| !x.eq_ignore_ascii_case(&l.to_string())) {
                        continue;
                    }
                    let i: usize = sheet.get(row, id).and_then(|v| v.parse().ok()).ok_or_else(|| format!("{}: line {}: no numeric id", path.display(), n + 2))?;
                    t.set(i, sheet.get(row, text).unwrap_or("")).map_err(|e| format!("{}: line {}: {e}", path.display(), n + 2))?;
                    count += 1;
                }
            }
            if !appended.is_empty() {
                t.strings.resize(base, Vec::new());
                t.strings.extend(appended.iter().map(|(_, s)| s.as_bytes().to_vec()));
            }
            tables.push((l, t.build()));
        }
        if !appended.is_empty() {
            let _ = self.formation_string_ids.set((0..appended.len()).map(|k| (base + k) as u32).collect());
        }
        Ok(Some((tables, count)))
    }
}

/// A mod's sponsors.csv: `id,...,shirt_gbp,ground_gbp` (named columns, others ignored), club record
/// index -> its shirt and ground sponsorship in GBP per season.
fn read_sponsors(path: &Path) -> Result<HashMap<u16, (u32, u32)>, String> {
    let sheet = lma_data::csv::Sheet::read(path).map_err(|e| e.to_string())?;
    let (id, shirt, ground) = (sheet.col("id"), sheet.col("shirt_gbp"), sheet.col("ground_gbp"));
    if id.is_none() || shirt.is_none() || ground.is_none() {
        return Err(format!("{}: needs id, shirt_gbp and ground_gbp columns", path.display()));
    }
    let mut out = HashMap::new();
    for (n, row) in sheet.rows.iter().enumerate() {
        let bad = || format!("{}: line {}: id, shirt_gbp and ground_gbp must be whole numbers", path.display(), n + 2);
        let num = |c| sheet.get(row, c).and_then(|v: &str| v.parse::<u64>().ok());
        let (Some(i), Some(s), Some(g)) = (num(id), num(shirt), num(ground)) else { return Err(bad()) };
        let i = u16::try_from(i).map_err(|_| bad())?;
        out.insert(i, (s.min(u32::MAX as u64) as u32, g.min(u32::MAX as u64) as u32));
    }
    Ok(out)
}

/// A mod's wage_budgets.csv: `id,...,budget_gbp_week` (named columns, others ignored), club record
/// index -> the least weekly wage budget in GBP.
fn read_wage_budgets(path: &Path) -> Result<HashMap<u16, u32>, String> {
    let sheet = lma_data::csv::Sheet::read(path).map_err(|e| e.to_string())?;
    let (id, budget) = (sheet.col("id"), sheet.col("budget_gbp_week"));
    if id.is_none() || budget.is_none() {
        return Err(format!("{}: needs id and budget_gbp_week columns", path.display()));
    }
    let mut out = HashMap::new();
    for (n, row) in sheet.rows.iter().enumerate() {
        let bad = || format!("{}: line {}: id and budget_gbp_week must be whole numbers", path.display(), n + 2);
        let (Some(i), Some(b)) = (sheet.get(row, id).and_then(|v| v.parse::<u16>().ok()), sheet.get(row, budget).and_then(|v| v.parse::<u32>().ok())) else { return Err(bad()) };
        out.insert(i, b);
    }
    Ok(out)
}

impl ModSpace {
    /// formations.csv: both formation tables copied into `guest::rec::FORMATION_REGION` with the
    /// mod's rows applied and the sites that address them rewritten for the
    /// interpreter, and the atomics set for the recompiled code (the recompiler reads them at
    /// the same sites). Every word is checked against the vanilla game first.
    fn patch_formations(&self, mem: &mut guest::mem::Memory, rows: &[Formation]) -> Result<String, String> {
        use guest::rec::*;
        let vanilla = FORMATION_COUNT_VANILLA as usize;
        let (new, changed) = (rows.iter().filter(|f| f.id >= vanilla).count(), rows.iter().filter(|f| f.id < vanilla).count());
        let count = vanilla + new;
        let (roles_at, names_at, strings_at, ids_at, spots_at, order_at) = formation_layout();
        for a in (FORMATION_REGION.0..FORMATION_REGION.1).step_by(4) {
            let w = mem.read32(a);
            if w != 0 {
                return Err(format!("mod {}: formations: {a:#x} holds {w:#x}; the region {:#x}-{:#x} isn't free", self.id, FORMATION_REGION.0, FORMATION_REGION.1));
            }
        }
        let mut roles: Vec<[u32; FORMATION_SLOTS]> = (0..vanilla).map(|f| std::array::from_fn(|s| mem.read32(FORMATION_ROLES_VANILLA + ((f * FORMATION_SLOTS + s) * 4) as u32))).collect();
        let mut names: Vec<u32> = (0..vanilla).map(|f| mem.read32(FORMATION_NAMES_VANILLA + 4 * f as u32)).collect();
        if roles.iter().any(|r| r[0] != 0 || r.iter().any(|&v| v > 9)) || names.iter().any(|&p| !(0x8200_0000..0x8300_0000).contains(&p)) {
            return Err(format!("mod {}: formations: the tables at {FORMATION_ROLES_VANILLA:#x} / {FORMATION_NAMES_VANILLA:#x} aren't the game's", self.id));
        }
        roles.resize(count, [0; FORMATION_SLOTS]);
        names.resize(count, 0);
        let mut strings = Vec::new();
        for f in rows {
            roles[f.id] = f.roles;
            names[f.id] = strings_at + strings.len() as u32;
            strings.extend_from_slice(f.name.as_bytes());
            strings.push(0);
            strings.resize(strings.len().next_multiple_of(4), 0);
        }
        if strings_at + strings.len() as u32 > FORMATION_REGION.1 {
            return Err(format!("mod {}: formations: the names don't fit the region", self.id));
        }
        for (f, r) in roles.iter().enumerate() {
            for (s, &v) in r.iter().enumerate() {
                mem.write32(roles_at + ((f * FORMATION_SLOTS + s) * 4) as u32, v);
            }
        }
        for (f, &p) in names.iter().enumerate() {
            mem.write32(names_at + 4 * f as u32, p);
        }
        mem.write(strings_at, &strings);
        // the caption's string-id list: the vanilla 14 copied, the new names' appended ids (0 before
        // `install` ran), a terminator
        let mut ids: Vec<u32> = (0..vanilla).map(|f| mem.read32(FORMATION_IDS_VANILLA + 4 * f as u32)).collect();
        if ids.iter().any(|&i| i == 0 || i > 0xFFFF) || mem.read32(FORMATION_IDS_VANILLA + 4 * vanilla as u32) != 0 {
            return Err(format!("mod {}: formations: the id list at {FORMATION_IDS_VANILLA:#x} isn't the game's", self.id));
        }
        let appended = self.formation_string_ids.get().cloned().unwrap_or_default();
        ids.extend((vanilla..count).map(|f| appended.get(f - vanilla).copied().unwrap_or(0)));
        ids.push(0);
        for (k, &i) in ids.iter().enumerate() {
            mem.write32(ids_at + 4 * k as u32, i);
        }
        // the diagram's spot table: the vanilla rows copied, the mod's rows applied
        let mut spots: Vec<[u32; FORMATION_SLOTS]> = (0..vanilla).map(|f| std::array::from_fn(|s| mem.read32(FORMATION_SPOTS_VANILLA + ((f * FORMATION_SLOTS + s) * 4) as u32))).collect();
        if spots.iter().any(|r| r.iter().any(|&v| v >= FORMATION_SPOT_COUNT)) {
            return Err(format!("mod {}: formations: the spot table at {FORMATION_SPOTS_VANILLA:#x} isn't the game's", self.id));
        }
        spots.resize(count, [0; FORMATION_SLOTS]);
        for f in rows {
            spots[f.id] = f.spots;
        }
        for (f, r) in spots.iter().enumerate() {
            for (s, &v) in r.iter().enumerate() {
                mem.write32(spots_at + ((f * FORMATION_SLOTS + s) * 4) as u32, v);
            }
        }
        // the team-sheet order: the vanilla rows copied; a new shape takes the order of the vanilla
        // shape whose slot roles match it most (the slots keep their meaning across shapes: 0 GK,
        // 1-2 wide defenders, 4-5 centre backs, ...)
        let vanilla_roles: Vec<[u32; FORMATION_SLOTS]> = (0..vanilla).map(|f| std::array::from_fn(|s| mem.read32(FORMATION_ROLES_VANILLA + ((f * FORMATION_SLOTS + s) * 4) as u32))).collect();
        let mut order: Vec<[u32; FORMATION_SLOTS]> = (0..vanilla).map(|f| std::array::from_fn(|s| mem.read32(FORMATION_ORDER_VANILLA + ((f * FORMATION_SLOTS + s) * 4) as u32))).collect();
        if order.iter().any(|r| r.iter().any(|&v| v as usize >= FORMATION_SLOTS)) {
            return Err(format!("mod {}: formations: the team-sheet order at {FORMATION_ORDER_VANILLA:#x} isn't the game's", self.id));
        }
        for f in rows.iter().filter(|f| f.id >= vanilla) {
            let best = (0..vanilla).max_by_key(|&v| (vanilla_roles[v].iter().zip(&f.roles).filter(|(a, b)| a == b).count(), std::cmp::Reverse(v))).unwrap_or(0);
            order.resize(f.id + 1, [0; FORMATION_SLOTS]);
            order[f.id] = order[best];
        }
        order.resize(count, [0; FORMATION_SLOTS]);
        for (f, r) in order.iter().enumerate() {
            for (s, &v) in r.iter().enumerate() {
                mem.write32(order_at + ((f * FORMATION_SLOTS + s) * 4) as u32, v);
            }
        }
        // the code: `lis rA,hi` / `addi rD,rA,lo` pairs keep their registers
        let site = |mem: &mut guest::mem::Memory, pc: u32, want: u32, mask: u32, what: &str| -> Result<u32, String> {
            let w = mem.read32(pc);
            if w & mask != want {
                return Err(format!("mod {}: formations: {pc:#x} is {w:#x}, not {what}", self.id));
            }
            Ok(w)
        };
        let (rh, rl) = hi_lo(roles_at);
        let (nh, nl) = hi_lo(names_at);
        let hi16 = |hi: i64| (hi >> 16) as u32 & 0xFFFF;
        let lo16 = |lo: i64| lo as u32 & 0xFFFF;
        for &(lis, addi) in FORMATION_ROLES_SITES {
            let wl = site(mem, lis, 0x3C00_8271, 0xFC1F_FFFF, "lis rN,0x8271")?;
            let wa = site(mem, addi, 0x3800_0000 | (wl & 0x03E0_0000) >> 5 | 3928, 0xFC1F_FFFF, "addi rD,rN,3928")?;
            mem.write32(lis, (wl & 0xFFFF_0000) | hi16(rh));
            mem.write32(addi, (wa & 0xFFFF_0000) | lo16(rl));
        }
        // the shared site: the dead addi before its lis becomes `lis r29,hi`, the addi `addi r29,r29,lo`
        site(mem, FORMATION_ROLES_SHARED.0, 0x3B8B_FF1C, 0xFFFF_FFFF, "addi r28,r11,-228")?;
        site(mem, FORMATION_ROLES_SHARED.1, 0x3BAB_0F58, 0xFFFF_FFFF, "addi r29,r11,3928")?;
        mem.write32(FORMATION_ROLES_SHARED.0, 0x3FA0_0000 | hi16(rh));
        mem.write32(FORMATION_ROLES_SHARED.1, 0x3BBD_0000 | lo16(rl));
        let wl = site(mem, FORMATION_NAMES_SITE.0, 0x3D60_8270, 0xFFFF_FFFF, "lis r11,0x8270")?;
        let wa = site(mem, FORMATION_NAMES_SITE.1, 0x396B_23F0, 0xFFFF_FFFF, "addi r11,r11,9200")?;
        mem.write32(FORMATION_NAMES_SITE.0, (wl & 0xFFFF_0000) | hi16(nh));
        mem.write32(FORMATION_NAMES_SITE.1, (wa & 0xFFFF_0000) | lo16(nl));
        let wl = site(mem, FORMATION_NAMES2_SITE.0, 0x3D60_8271, 0xFFFF_FFFF, "lis r11,0x8271")?;
        let wa = site(mem, FORMATION_NAMES2_SITE.1, 0x396B_35BC, 0xFFFF_FFFF, "addi r11,r11,13756")?;
        mem.write32(FORMATION_NAMES2_SITE.0, (wl & 0xFFFF_0000) | hi16(nh));
        mem.write32(FORMATION_NAMES2_SITE.1, (wa & 0xFFFF_0000) | lo16(nl));
        let (ih, il) = hi_lo(ids_at);
        let wl = site(mem, FORMATION_IDS_SITE.0, 0x3D60_8202, 0xFFFF_FFFF, "lis r11,0x8202")?;
        let wa = site(mem, FORMATION_IDS_SITE.1, 0x396B_7DCC, 0xFFFF_FFFF, "addi r11,r11,32204")?;
        mem.write32(FORMATION_IDS_SITE.0, (wl & 0xFFFF_0000) | hi16(ih));
        mem.write32(FORMATION_IDS_SITE.1, (wa & 0xFFFF_0000) | lo16(il));
        let (sh, sl) = hi_lo(spots_at);
        let wl = site(mem, FORMATION_SPOTS_SITE.0, 0x3D60_8201, 0xFFFF_FFFF, "lis r11,0x8201")?;
        let wa = site(mem, FORMATION_SPOTS_SITE.1, 0x396B_B370, 0xFFFF_FFFF, "addi r11,r11,-19600")?;
        mem.write32(FORMATION_SPOTS_SITE.0, (wl & 0xFFFF_0000) | hi16(sh));
        mem.write32(FORMATION_SPOTS_SITE.1, (wa & 0xFFFF_0000) | lo16(sl));
        let (oh, ol) = hi_lo(order_at);
        for &(lis, addi) in &FORMATION_ORDER_SITES {
            let wl = site(mem, lis, 0x3D60_8203, 0xFFFF_FFFF, "lis r11,0x8203")?;
            let wa = site(mem, addi, 0x396B_CD38, 0xFFFF_FFFF, "addi r11,r11,-13000")?;
            mem.write32(lis, (wl & 0xFFFF_0000) | hi16(oh));
            mem.write32(addi, (wa & 0xFFFF_0000) | lo16(ol));
        }
        // the count: `li rD,14` and `cmpwi crN,rA,14` keep their registers
        for &pc in FORMATION_COUNT_LI_SITES {
            let w = site(mem, pc, 0x3800_000E, 0xFC1F_FFFF, "li rD,14")?;
            mem.write32(pc, (w & 0xFFFF_0000) | count as u32);
        }
        for &pc in FORMATION_COUNT_CMP_SITES {
            let w = site(mem, pc, 0x2C00_000E, 0xFC00_FFFF, "cmpwi crN,rA,14")?;
            mem.write32(pc, (w & 0xFFFF_0000) | count as u32);
        }
        FORMATION_ROLES.store(roles_at, std::sync::atomic::Ordering::Relaxed);
        FORMATION_NAMES.store(names_at, std::sync::atomic::Ordering::Relaxed);
        FORMATION_IDS.store(ids_at, std::sync::atomic::Ordering::Relaxed);
        FORMATION_SPOTS.store(spots_at, std::sync::atomic::Ordering::Relaxed);
        FORMATION_ORDER.store(order_at, std::sync::atomic::Ordering::Relaxed);
        FORMATION_COUNT.store(count as u32, std::sync::atomic::Ordering::Relaxed);
        Ok(format!("formations: {count} ({new} new, {changed} changed; tables at {roles_at:#x})"))
    }

    /// The words `patch_formations` writes: the whole region and the rewritten code words.
    fn formation_words(&self) -> Vec<u32> {
        use guest::rec::*;
        let mut out: Vec<u32> = (FORMATION_REGION.0..FORMATION_REGION.1).step_by(4).collect();
        out.extend(FORMATION_ROLES_SITES.iter().flat_map(|&(l, a)| [l, a]));
        out.extend([FORMATION_ROLES_SHARED.0, FORMATION_ROLES_SHARED.1, FORMATION_NAMES_SITE.0, FORMATION_NAMES_SITE.1, FORMATION_NAMES2_SITE.0, FORMATION_NAMES2_SITE.1, FORMATION_IDS_SITE.0, FORMATION_IDS_SITE.1]);
        out.extend(FORMATION_COUNT_LI_SITES.iter().chain(FORMATION_COUNT_CMP_SITES).chain(&[FORMATION_SPOTS_SITE.0, FORMATION_SPOTS_SITE.1]).chain(FORMATION_ORDER_SITES.iter().flat_map(|s| [&s.0, &s.1])));
        out
    }


    /// The data words `patch_image` rewrote that a state load must restore (`Machine::after_load_writes`):
    /// the scaled sponsor offer table, read back from `mem` after `patch_image`, and the [[patch]] words.
    pub fn data_words(&self, mem: &mut guest::mem::Memory) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = Vec::new();
        if self.sponsor_offer_scale.as_ref().is_some_and(|s| !s.is_empty()) {
            out.extend((0..SPONSOR_OFFER_ROWS * 16).map(|k| SPONSOR_OFFER_TABLE + 4 * k).map(|a| (a, mem.read32(a))));
        }
        // and every [[patch]] word: data tables such as the contract screen's wage ceilings
        // (0x8200af9c) live in pages a state restores (12 Oct: an F9 state brought back the 100k cap)
        out.extend(self.patches.iter().filter_map(|p| parse_addr(&p.addr).ok().map(|a| (a, p.to))));
        // formations.csv: the copied tables and the code words that address them (the region is
        // image memory a state restores to zero)
        if self.formations.is_some() {
            out.extend(self.formation_words().into_iter().map(|a| (a, mem.read32(a))));
        }
        out
    }
}

/// The shirt sponsorship offers' base table in the game's image: 8 countries x 5 divisions x 16 club
/// levels of u32, each x GBP 250 a week (`ModSpace::sponsor_offer_scale`; the offer list 0x8214d538
/// and the AI's deals 0x8212b6f0 read it). Its first row, checked before rewriting.
const SPONSOR_OFFER_TABLE: u32 = 0x8200_6AF0;
const SPONSOR_OFFER_ROWS: u32 = 40;
const SPONSOR_OFFER_ROW0: [u32; 16] = [0, 0, 0, 0, 0, 0, 40, 50, 60, 76, 96, 120, 160, 200, 260, 320];

/// A mod's funds.csv: `id,funds_m[,...]` (header first), club record index -> funds in GBP.
fn read_funds(path: &Path) -> Result<HashMap<u16, u32>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let mut cells = line.split(',');
        let (Some(id), Some(f)) = (cells.next(), cells.next()) else { continue };
        let (Ok(id), Ok(f)) = (id.trim().parse::<u16>(), f.trim().parse::<f64>()) else {
            return Err(format!("{}: line {}: {line:?} is not id,funds_m", path.display(), n + 1));
        };
        out.insert(id, (f * 1e6).round().clamp(0.0, 999_999_999.0) as u32);
    }
    Ok(out)
}

/// A mod's squad_overflow.csv: `id,club_id[,...]` (header first), player id -> club record index.
fn read_squad_overflow(path: &Path) -> Result<HashMap<u16, u16>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let mut cells = line.split(',');
        let (Some(id), Some(c)) = (cells.next(), cells.next()) else { continue };
        let (Ok(id), Ok(c)) = (id.trim().parse::<u16>(), c.trim().parse::<u16>()) else {
            return Err(format!("{}: line {}: {line:?} is not id,club_id", path.display(), n + 1));
        };
        out.insert(id, c);
    }
    Ok(out)
}

/// A mod's wages.csv: `id,wage[,years]` (header first), player id -> weekly wage in GBP and, when
/// given, the contract's seasons left (1-7).
fn read_wages(path: &Path) -> Result<HashMap<u32, (u32, Option<u32>)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut out = HashMap::new();
    for (n, line) in text.lines().enumerate().skip(1) {
        let mut cells = line.split(',');
        let (Some(id), Some(w)) = (cells.next(), cells.next()) else { continue };
        let years = cells.next().map(str::trim).filter(|y| !y.is_empty());
        let bad = || format!("{}: line {}: {line:?} is not id,wage[,years]", path.display(), n + 1);
        let (Ok(id), Ok(w)) = (id.trim().parse::<u32>(), w.trim().parse::<u32>()) else { return Err(bad()) };
        let years = match years.map(str::parse::<u32>) {
            None => None,
            Some(Ok(y)) if (1..=7).contains(&y) => Some(y),
            Some(_) => return Err(bad()),
        };
        out.insert(id, (w, years));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// base_year rewrites the 2006 immediates of li/addi and the compares in .text, nothing
    /// else; a word patch checks the old word first.
    /// A hand-written BG2 archive (entries 0x800-aligned), as lma-data's own tests build them.
    fn bg2(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let (mut table, mut body) = (Vec::new(), Vec::new());
        for (name, data) in entries {
            let mut n = name.as_bytes().to_vec();
            n.resize(0x100, 0);
            table.extend_from_slice(&n);
            table.extend_from_slice(&(body.len() as u32).to_be_bytes());
            table.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            while body.len() % 0x800 != 0 {
                body.push(0);
            }
        }
        let data_start = (0x14 + table.len()).next_multiple_of(0x800);
        let mut out = b"BG2\0".to_vec();
        out.extend_from_slice(&0x14u32.to_be_bytes());
        out.extend_from_slice(&(table.len() as u32).to_be_bytes());
        out.extend_from_slice(&(data_start as u32).to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&table);
        out.resize(data_start, 0);
        out.extend_from_slice(&body);
        out
    }

    /// badges/<ClubName>.tm2 of a club is a new entry the club is pointed at; any other name
    /// replaces or adds the disc's entry of that name; a file that isn't a badge texture is
    /// refused with the tool's name.
    #[test]
    fn badges_folder_is_spliced_into_the_archive() {
        let game = std::env::temp_dir().join(format!("lma-badge-game-{}", std::process::id()));
        let tex = game.join("Common").join("Frontend").join("Textures");
        std::fs::create_dir_all(&tex).unwrap();
        let mut old = b"TIM2".to_vec();
        old.resize(34048, 1);
        // entries 0..7 and the disc's layout matter: 8 country crests, then ft_ shields (two
        // here, at 8 and 9), then a retail club and a country
        let mut entries: Vec<(&str, &[u8])> = vec![("england.tm2", &old); 8];
        entries.extend([("ft_01_08rb.tm2", &old[..]), ("ft_01_08bw.tm2", &old[..]), ("Walsall.tm2", &old[..]), ("scotland.tm2", &old[..])]);
        std::fs::write(tex.join("Badge.mng"), bg2(&entries)).unwrap();
        let r = root("badges", &[("b", "")]);
        std::fs::create_dir_all(r.join("b/badges")).unwrap();
        let mut new = b"TIM2".to_vec();
        new.resize(34048, 2);
        std::fs::write(r.join("b/badges/Walsall.tm2"), &new).unwrap();
        std::fs::write(r.join("b/badges/Wrexham.tm2"), &new).unwrap();
        std::fs::write(r.join("b/badges/astonvilla.tm2"), &new).unwrap();
        let m = ModSpace::load(&r, "b").unwrap();
        let clubs: Vec<String> = ["Arsenal", "Aston Villa", "Walsall"].map(String::from).to_vec();
        let b = m.build_badges(&game, &clubs).unwrap().unwrap();
        assert_eq!(b.count, 3);
        // Aston Villa (club 1) takes the first ft_ slot (8); Walsall (club 2) has a retail entry,
        // replaced in place (10); Wrexham is no club here, so its file is an entry of its own name
        assert_eq!(b.clubs, vec![(1, 8), (2, 10)]);
        let ar = lma_data::archive::Archive::parse(&b.archive).unwrap();
        assert_eq!(ar.entries.iter().map(|(n, _)| n.as_str()).skip(8).collect::<Vec<_>>(), ["club1.tm2", "ft_01_08bw.tm2", "Walsall.tm2", "scotland.tm2", "Wrexham.tm2"]);
        assert_eq!(ar.get("Walsall.tm2").unwrap()[4], 2, "replaced in place");
        assert_eq!(ar.get("club1.tm2").unwrap()[4], 2);
        assert_eq!(ar.get("Wrexham.tm2").unwrap().len(), 34048, "added by name");
        std::fs::write(r.join("b/badges/Bad.tm2"), b"not a texture").unwrap();
        assert!(m.build_badges(&game, &clubs).unwrap_err().contains("badge_tm2"));
        std::fs::remove_dir_all(&r).unwrap();
        std::fs::remove_dir_all(&game).unwrap();
    }

    /// A mod.toml key this build doesn't know fails the load with the key named and the hint that
    /// a newer build (or removing the key) is the fix: the launcher shows only this text.
    #[test]
    fn unknown_manifest_key_names_itself() {
        let r = root("unknown-key", &[("u", "base_year = 2026\nfuture_key = 1\n")]);
        let e = ModSpace::load(&r, "u").unwrap_err();
        assert!(e.contains("future_key") && e.contains("does not know that key"), "{e}");
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn calendar_weekdays_needs_base_year() {
        let r = root("calendar", &[("c", "base_year = 2026\ncalendar_weekdays = true\n"), ("n", "calendar_weekdays = true\n")]);
        assert!(ModSpace::load(&r, "c").unwrap().calendar_weekdays);
        let e = ModSpace::load(&r, "n").unwrap_err();
        assert!(e.contains("calendar_weekdays needs base_year"), "{e}");
        let r2 = root("calendar_w", &[("w", "base_year = 2026\ncalendar_weekdays = true\ncalendar_weeks = 1\n"), ("x", "base_year = 2026\ncalendar_weeks = 1\n"), ("y", "base_year = 2026\ncalendar_weekdays = true\ncalendar_weeks = 2\n")]);
        assert_eq!(ModSpace::load(&r2, "w").unwrap().calendar_weeks, 1);
        assert!(ModSpace::load(&r2, "x").unwrap_err().contains("calendar_weeks needs calendar_weekdays"));
        assert!(ModSpace::load(&r2, "y").unwrap_err().contains("must be 0 or 1"));
        std::fs::remove_dir_all(&r2).unwrap();
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn europe_places_are_checked() {
        let ok = "europe_places = [[2, 2, 2], [0, 2, 1], [2, 2, 2], [2, 2, 2], [2, 2, 2], [2, 2, 1], [1, 2, 2], [2, 0, 2]]\n";
        let r = root("europe", &[("ok", ok), ("short", "europe_places = [[4, 0, 2]]\n"), ("many", &ok.replace("[2, 0, 2]", "[4, 4, 2]")), ("full", &ok.replace("[2, 0, 2]", "[4, 0, 2]"))]);
        let m = ModSpace::load(&r, "ok").unwrap();
        assert_eq!(m.europe_places.as_ref().map(|p| (p.len(), p[7])), Some((8, [2, 0, 2])));
        assert!(ModSpace::load(&r, "short").unwrap_err().contains("needs 8"));
        assert!(ModSpace::load(&r, "many").unwrap_err().contains("at most 8"));
        assert!(ModSpace::load(&r, "full").unwrap_err().contains("15 group stage"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `addi rD,rA,2007` with rA a loaded index is the next year (a season label's second half, the
    /// season count: #706) and follows the base year; `li` 2007 is an id and stays.
    #[test]
    fn addi_2007_follows_the_base_year() {
        let _serial = BASE_YEAR_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let r = root("year2007", &[("y", "base_year = 2026
")]);
        let m = ModSpace::load(&r, "y").unwrap();
        let mut mem = guest::mem::Memory::new();
        for (i, w) in [0x3BCB_07D7u32, 0x3860_07D7, 0x3863_07D6].iter().enumerate() {
            mem.write32(0x8200_0000 + 4 * i as u32, *w);
        }
        let notes = m.patch_image(&mut mem, (0x8200_0000, 12)).unwrap();
        assert_eq!(notes, vec!["base year 2026: 2 loads and 0 compares of 2006 rewritten, 0 of them the last season (2006 + 20), 1 the next year (2006 + 1)".to_string()]);
        assert_eq!((0..3).map(|i| mem.read32(0x8200_0000 + 4 * i)).collect::<Vec<u32>>(), [0x3BCB_07EB, 0x3860_07D7, 0x3863_07EA]);
        guest::rec::BASE_YEAR.store(2006, std::sync::atomic::Ordering::Relaxed);
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// The two tests that set `guest::rec::BASE_YEAR` (a global) run one at a time: one asserts 2026
    /// after its patch while the other may have just reset it to 2006 (the test-thread race of #728/#734).
    static BASE_YEAR_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn base_year_and_word_patches() {
        let _serial = BASE_YEAR_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let r = root("patch", &[("y", "base_year = 2026\n[[patch]]\naddr = \"0x82000018\"\nfrom = 30000000\nto = 60000000\n")]);
        let m = ModSpace::load(&r, "y").unwrap();
        assert_eq!((m.base_year, m.patches.len()), (Some(2026), 1));
        let mut mem = guest::mem::Memory::new();
        let words = [0x3860_07D6u32, 0x3863_07D6, 0x2C03_07D6, 0x2803_07EA, 0x6000_07D6, 0x3860_07D7, 30_000_000, 0x1234_5678];
        for (i, w) in words.iter().enumerate() {
            mem.write32(0x8200_0000 + 4 * i as u32, *w);
        }
        let notes = m.patch_image(&mut mem, (0x8200_0000, 24)).unwrap();
        assert_eq!(notes, vec!["base year 2026: 2 loads and 2 compares of 2006 rewritten, 1 of them the last season (2006 + 20), 0 the next year (2006 + 1)".to_string(), "patched 0x82000018: 0x1c9c380 -> 0x3938700".to_string()]);
        let got: Vec<u32> = (0..8).map(|i| mem.read32(0x8200_0000 + 4 * i)).collect();
        assert_eq!(got, [0x3860_07EA, 0x3863_07EA, 0x2C03_07EA, 0x2803_07FE, 0x6000_07D6, 0x3860_07D7, 60_000_000, 0x1234_5678]);
        assert_eq!(guest::rec::BASE_YEAR.load(std::sync::atomic::Ordering::Relaxed), 2026);
        guest::rec::BASE_YEAR.store(2006, std::sync::atomic::Ordering::Relaxed);
        // the patched word is written again after a state load (`Machine::after_load_writes`)
        assert_eq!(m.data_words(&mut mem), vec![(0x8200_0018, 60_000_000)]);
        let r = root("patch_bad", &[("y", "[[patch]]\naddr = \"0x82000018\"\nfrom = 1\nto = 2\n"), ("z", "base_year = 1900\n"), ("w", "[[patch]]\naddr = \"0x82000011\"\nfrom = 1\nto = 2\n")]);
        let m = ModSpace::load(&r, "y").unwrap();
        assert!(m.patch_image(&mut mem, (0x8200_0000, 24)).unwrap_err().contains("not 0x1"));
        assert!(ModSpace::load(&r, "z").unwrap_err().contains("1990-2099"));
        assert!(ModSpace::load(&r, "w").unwrap_err().contains("hex word address"));
    }

    fn root(name: &str, mods: &[(&str, &str)]) -> PathBuf {
        let r = std::env::temp_dir().join(format!("lma-mods-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&r);
        for (id, toml) in mods {
            std::fs::create_dir_all(r.join(id)).unwrap();
            std::fs::write(r.join(id).join("mod.toml"), toml).unwrap();
        }
        r
    }

    /// A test image with the vanilla formation tables (14 rows; names pointing at strings in the
    /// image) and the code words `patch_formations` rewrites.
    fn formation_image(mem: &mut guest::mem::Memory) {
        use guest::rec::*;
        for f in 0..14usize {
            let row = [0u32, 1, 1, 5, 3, 3, 6, 5, 8, 8, 6];
            for (s, &v) in row.iter().enumerate() {
                mem.write32(FORMATION_ROLES_VANILLA + ((f * 11 + s) * 4) as u32, if f == 13 && s == 3 { 7 } else { v });
            }
            mem.write32(FORMATION_NAMES_VANILLA + 4 * f as u32, 0x8200_1a48 - 8 * f as u32);
        }
        mem.write(0x8200_1a48, b"4-4-2\0");
        for &(lis, addi) in FORMATION_ROLES_SITES {
            mem.write32(lis, 0x3D60_8271);
            mem.write32(addi, 0x396B_0F58);
        }
        mem.write32(FORMATION_ROLES_SITES[0].0, 0x3D60_8271);
        mem.write32(FORMATION_ROLES_SITES[0].1, 0x3B2B_0F58); // addi r25,r11,3928
        mem.write32(FORMATION_ROLES_SHARED.0, 0x3B8B_FF1C);
        mem.write32(FORMATION_ROLES_SHARED.1, 0x3BAB_0F58);
        mem.write32(FORMATION_NAMES_SITE.0, 0x3D60_8270);
        mem.write32(FORMATION_NAMES_SITE.1, 0x396B_23F0);
        mem.write32(FORMATION_NAMES2_SITE.0, 0x3D60_8271);
        mem.write32(FORMATION_NAMES2_SITE.1, 0x396B_35BC);
        mem.write32(FORMATION_IDS_SITE.0, 0x3D60_8202);
        mem.write32(FORMATION_IDS_SITE.1, 0x396B_7DCC);
        mem.write32(FORMATION_SPOTS_SITE.0, 0x3D60_8201);
        mem.write32(FORMATION_SPOTS_SITE.1, 0x396B_B370);
        for &(lis, addi) in &FORMATION_ORDER_SITES {
            mem.write32(lis, 0x3D60_8203);
            mem.write32(addi, 0x396B_CD38);
        }
        for f in 0..14u32 {
            for (s, v) in [0u32, 2, 4, 5, 1, 10, 3, 7, 6, 8, 9].into_iter().enumerate() {
                // 4-4-2's order for all but 3-5-2 (row 5), which the 3-4-2-1 row matches best
                mem.write32(FORMATION_ORDER_VANILLA + (f * 11 + s as u32) * 4, if f == 5 { [0u32, 3, 4, 5, 2, 10, 7, 6, 1, 8, 9][s] } else { v });
            }
        }
        // 3-5-2's roles (GK WB WB CB CB CB CM CM CF CF CM) so the new 3-4-2-1 row picks its order
        for (s, v) in [0u32, 4, 4, 3, 3, 3, 5, 5, 8, 8, 5].into_iter().enumerate() {
            mem.write32(FORMATION_ROLES_VANILLA + (5 * 11 + s as u32) * 4, v);
        }
        for f in 0..14u32 {
            for s in 0..11u32 {
                mem.write32(FORMATION_SPOTS_VANILLA + (f * 11 + s) * 4, if s == 0 { 26 } else { s + 8 }); // 4-4-2's spots, or near enough
            }
        }
        for f in 0..14u32 {
            mem.write32(FORMATION_IDS_VANILLA + 4 * f, 3322 + f);
        }
        for &pc in FORMATION_COUNT_LI_SITES {
            mem.write32(pc, 0x3880_000E); // li r4,14
        }
        for &pc in FORMATION_COUNT_CMP_SITES {
            mem.write32(pc, 0x2F1F_000E); // cmpwi cr6,r31,14
        }
    }

    /// formations.csv: a changed vanilla row and two new ones land in the region's copies of both
    /// tables, every site addresses the copies (its registers kept), the count sites say 16, the
    /// caption's id list carries the appended ids, and the data words restore it all after a state load.
    #[test]
    fn formations_csv_extends_both_tables() {
        use guest::rec::*;
        let r = root("formations", &[("f", "formations = \"formations.csv\"\n")]);
        std::fs::write(r.join("f/formations.csv"), "id,name,roles\n14,3-4-2-1,GK WB WB CB CB CB CM AM CF AM CM\n2,4-3-3,gk fb fb cm cb cb wf cm cm cf wf\n15,4-1-4-1,0 1 1 5 3 3 6 5 8 7 6\n").unwrap();
        let m = ModSpace::load(&r, "f").unwrap();
        let rows = m.formations.clone().unwrap();
        m.formation_string_ids.set(vec![9000, 9001]).unwrap(); // what `install` finds
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].id, rows[0].name.as_str(), rows[0].roles, rows[0].spots), (14, "3-4-2-1", [0, 4, 4, 3, 3, 3, 5, 7, 8, 7, 5], [26, 0, 1, 2, 13, 14, 15, 16, 32, 33, 7]), "the spots laid out from the name");
        assert_eq!(rows[1].roles, [0, 1, 1, 5, 3, 3, 9, 5, 5, 8, 9], "role names in any case");
        let mut mem = guest::mem::Memory::new();
        formation_image(&mut mem);
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["formations: 16 (2 new, 1 changed; tables at 0x826ff400)".to_string()]);
        let (roles_at, names_at, strings_at, ids_at, spots_at, order_at) = formation_layout();
        assert_eq!((roles_at, names_at, strings_at, ids_at, spots_at, order_at), (0x826f_f400, 0x826f_f6c0, 0x826f_f700, 0x826f_f7c0, 0x826f_f804, 0x826f_fac4));
        assert!(order_at + 16 * 44 <= FORMATION_REGION.1, "the region holds all six tables");
        let mut row = |f: u32| -> Vec<u32> { (0..11).map(|s| mem.read32(roles_at + (f * 11 + s) * 4)).collect() };
        assert_eq!(row(0), vec![0, 1, 1, 5, 3, 3, 6, 5, 8, 8, 6], "a vanilla row copied");
        assert_eq!(row(13)[3], 7, "the last vanilla row copied");
        assert_eq!(row(2), vec![0, 1, 1, 5, 3, 3, 9, 5, 5, 8, 9], "a changed row");
        assert_eq!(row(14), vec![0, 4, 4, 3, 3, 3, 5, 7, 8, 7, 5]);
        assert_eq!(row(15), vec![0, 1, 1, 5, 3, 3, 6, 5, 8, 7, 6]);
        assert_eq!(mem.read32(names_at), 0x8200_1a48, "a vanilla name keeps its pointer");
        let mut name = |f: u32| -> String { let p = mem.read32(names_at + 4 * f); let mut b = [0u8; 12]; mem.read(p, &mut b); String::from_utf8_lossy(&b[..b.iter().position(|&c| c == 0).unwrap()]).into_owned() };
        assert_eq!((name(0), name(2), name(14), name(15)), ("4-4-2".into(), "4-3-3".into(), "3-4-2-1".into(), "4-1-4-1".into()));
        assert!(mem.read32(names_at + 4 * 14) >= strings_at && mem.read32(names_at + 4 * 15) < FORMATION_REGION.1);
        let mut spot = |f: u32| -> Vec<u32> { (0..11).map(|s| mem.read32(spots_at + (f * 11 + s) * 4)).collect() };
        assert_eq!((spot(0)[1], spot(13)[10]), (9, 18), "vanilla spot rows copied");
        assert_eq!(spot(14), vec![26, 0, 1, 2, 13, 14, 15, 16, 32, 33, 7], "3-4-2-1 laid out");
        assert_eq!(spot(15), vec![26, 9, 10, 11, 12, 29, 13, 14, 15, 16, 7], "4-1-4-1 laid out: one on the holding row");
        assert_eq!(spot(2), vec![26, 9, 10, 11, 12, 3, 4, 5, 6, 7, 8], "the changed row's spots from its name");
        assert_eq!((mem.read32(FORMATION_SPOTS_SITE.0), mem.read32(FORMATION_SPOTS_SITE.1)), (0x3D60_8270, 0x396B_F804), "the spot table's pair: addi r11,r11,-2044");
        assert!(FORMATION_ORDER_SITES.iter().all(|s| (mem.read32(s.0), mem.read32(s.1)) == (0x3D60_8270, 0x396B_FAC4)), "the order pairs: addi r11,r11,-1340");
        let mut ord = |f: u32| -> Vec<u32> { (0..11).map(|s| mem.read32(order_at + (f * 11 + s) * 4)).collect() };
        assert_eq!(ord(0), vec![0, 2, 4, 5, 1, 10, 3, 7, 6, 8, 9], "a vanilla order copied");
        assert_eq!(ord(14), vec![0, 3, 4, 5, 2, 10, 7, 6, 1, 8, 9], "3-4-2-1 takes 3-5-2's order (the closest roles)");
        assert_eq!(ord(15), ord(0), "4-1-4-1 takes a back-four order");
        // the sites: lis 0x8270 / addi -3072 reach 0x826ff400, registers kept
        assert_eq!((mem.read32(FORMATION_ROLES_SITES[1].0), mem.read32(FORMATION_ROLES_SITES[1].1)), (0x3D60_8270, 0x396B_F400));
        assert_eq!((mem.read32(FORMATION_ROLES_SITES[0].0), mem.read32(FORMATION_ROLES_SITES[0].1)), (0x3D60_8270, 0x3B2B_F400), "addi r25,r11,-3072");
        assert_eq!((mem.read32(FORMATION_ROLES_SHARED.0), mem.read32(FORMATION_ROLES_SHARED.1)), (0x3FA0_8270, 0x3BBD_F400), "lis r29,0x8270 / addi r29,r29,-3072");
        assert_eq!((mem.read32(FORMATION_NAMES_SITE.0), mem.read32(FORMATION_NAMES_SITE.1)), (0x3D60_8270, 0x396B_F6C0), "addi r11,r11,-2368");
        assert_eq!((mem.read32(FORMATION_NAMES2_SITE.0), mem.read32(FORMATION_NAMES2_SITE.1)), (0x3D60_8270, 0x396B_F6C0), "the second name table's pair");
        assert_eq!((mem.read32(FORMATION_IDS_SITE.0), mem.read32(FORMATION_IDS_SITE.1)), (0x3D60_8270, 0x396B_F7C0), "the id list's pair: addi r11,r11,-2112");
        let ids: Vec<u32> = (0..17).map(|k| mem.read32(ids_at + 4 * k)).collect();
        assert_eq!(ids, (3322..3336).chain([9000, 9001, 0]).collect::<Vec<u32>>(), "the vanilla ids, the appended ids, the terminator");
        assert!(FORMATION_COUNT_LI_SITES.iter().all(|&pc| mem.read32(pc) == 0x3880_0010) && FORMATION_COUNT_CMP_SITES.iter().all(|&pc| mem.read32(pc) == 0x2F1F_0010), "li r4,16 / cmpwi cr6,r31,16");
        assert_eq!((hi_lo(0x826f_f400), hi_lo(0x826f_f6c0)), ((0x8270_0000u32 as i32 as i64, -3072), (0x8270_0000u32 as i32 as i64, -2368)));
        assert_eq!((formation_roles_hi(), formation_roles_lo(), formation_names_lo(), formation_ids_hi(), formation_ids_lo(), formation_count(), formation_roles()), (0x8270_0000u32 as i32 as i64, -3072, -2368, 0x8270_0000u32 as i32 as i64, -2112, 16, 0x826f_f400u32 as i32 as i64));
        let words = m.data_words(&mut mem);
        assert!(words.contains(&(roles_at + 14 * 44, 0)) && words.contains(&(roles_at + 14 * 44 + 4, 4)) && words.contains(&(FORMATION_NAMES_SITE.1, 0x396B_F6C0)) && words.contains(&(FORMATION_ROLES_SHARED.0, 0x3FA0_8270)) && words.contains(&(ids_at + 4 * 14, 9000)) && words.contains(&(FORMATION_COUNT_LI_SITES[0], 0x3880_0010)));
        // a second apply on the same memory is refused: the region isn't free any more
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("isn't free"));
        let _ = std::fs::remove_dir_all(&r);
    }

    /// Bad rows are refused at load: an id past 15, 15 without 14, a role that isn't one, the
    /// wrong number of roles, a second goalkeeper; and a site that isn't the game's at apply.
    #[test]
    fn formations_csv_bad_rows_refused() {
        let r = root("formations_bad", &[("f", "formations = \"formations.csv\"\n")]);
        let bad = |csv: &str| -> String {
            std::fs::write(r.join("f/formations.csv"), csv).unwrap();
            ModSpace::load(&r, "f").unwrap_err()
        };
        assert!(bad("id,name,roles\n16,x,GK FB FB CB CB CB CM CM CM CF CF\n").contains("id must be 0-15"));
        assert!(bad("id,name,roles\n15,4-4-2,GK FB FB CB CB CB CM CM CM CF CF\n").contains("id 15 needs id 14"));
        assert!(bad("id,name,roles\n14,4-4-2,GK FB FB CB CB CB CM CM DM CF CF\n").contains("role \"DM\""));
        assert!(bad("id,name,roles\n14,4-4-2,GK FB FB CB CB CB CM CM CF CF\n").contains("needs 11 entries"));
        assert!(bad("id,name,roles\n14,4-4-2,GK GK FB CB CB CB CM CM CM CF CF\n").contains("goalkeeper"));
        assert!(bad("id,name,roles\n14,a-very-long-name,GK FB FB CB CB CB CM CM CM CF CF\n").contains("1-11 ASCII"));
        assert!(bad("id,name,roles\n14,4-4-2,GK FB FB CB CB CB CM CM CM CF CF\n14,4-3-3,GK FB FB CB CB CB CM CM CM CF CF\n").contains("id 14 twice"));
        assert!(bad("id,name\n14,x\n").contains("needs id, name and roles"));
        assert!(bad("id,name,roles\n14,Diamond,GK FB FB CB CB CB CM CM CM CF CF\n").contains("isn't a shape"));
        assert!(bad("id,name,roles,spots\n14,Diamond,GK FB FB CB CB CB CM CM CM CF CF,26 0 1 2 3 4 5 6 7 8 99\n").contains("grid spots 0-33"));
        std::fs::write(r.join("f/formations.csv"), "id,name,roles\n14,3-4-2-1,GK WB WB CB CB CB CM AM CF AM CM\n").unwrap();
        let m = ModSpace::load(&r, "f").unwrap();
        let mut mem = guest::mem::Memory::new();
        formation_image(&mut mem);
        mem.write32(guest::rec::FORMATION_NAMES_SITE.0, 0x3D60_8271);
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not lis r11,0x8270"));
        let _ = std::fs::remove_dir_all(&r);
    }


    /// `value_unit` rewrites the two `mulli ..,1000` after the value getter and the setter's
    /// `cmpwi ..,1000` for the interpreter and sets the atomic the recompiled code reads.
    #[test]
    fn value_unit_rewrites_the_three_sites() {
        let r = root("unit", &[("u", "value_unit = 5000\n"), ("bad", "value_unit = 3000\n"), ("big", "value_unit = 200000\n")]);
        assert!(ModSpace::load(&r, "bad").unwrap_err().contains("divide 1,000,000"));
        assert!(ModSpace::load(&r, "big").unwrap_err().contains("1000-100000"));
        let m = ModSpace::load(&r, "u").unwrap();
        let mut mem = guest::mem::Memory::new();
        for &pc in guest::rec::VALUE_UNIT_SITES {
            mem.write32(pc, 0x1FC3_03E8); // mulli r30,r3,1000
        }
        mem.write32(guest::rec::VALUE_FLOOR_SITE, 0x2F1F_03E8); // cmpwi cr6,r31,1000
        mem.write32(guest::rec::VALUE_CAP_LIS, 0x3D60_0262); // lis r11,0x262
        mem.write32(guest::rec::VALUE_CAP_SITE, 0x616B_5A00); // ori r11,r11,0x5a00
        for &(lis, ori) in guest::rec::OFFER_CAP_SITES.iter() {
            mem.write32(lis, 0x3D60_04C4); // lis r11,0x4c4
            mem.write32(ori, 0x616A_B400); // ori r10,r11,0xb400
        }
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["value unit GBP 5000: stored values reach GBP 327675000, the game's own valuation GBP 200000000, transfer offers GBP 400000000, values under GBP 1000000 are dropped".to_string()]);
        assert_eq!((mem.read32(guest::rec::VALUE_CAP_LIS), mem.read32(guest::rec::VALUE_CAP_SITE)), (0x3D60_0BEB, 0x616B_C200), "GBP 200m = 0x0BEBC200");
        for &(lis, ori) in guest::rec::OFFER_CAP_SITES.iter() {
            assert_eq!((mem.read32(lis), mem.read32(ori)), (0x3D60_17D7, 0x616A_8400), "GBP 400m = 0x17D78400, registers kept");
        }
        for &pc in guest::rec::VALUE_UNIT_SITES {
            assert_eq!(mem.read32(pc), 0x1FC3_1388, "mulli r30,r3,5000");
        }
        assert_eq!(mem.read32(guest::rec::VALUE_FLOOR_SITE), 0x2F1F_00C8, "cmpwi cr6,r31,200");
        assert_eq!(guest::rec::VALUE_UNIT.load(std::sync::atomic::Ordering::Relaxed), 5000);
        assert_eq!(guest::rec::value_floor(), 200);
        guest::rec::VALUE_UNIT.store(1000, std::sync::atomic::Ordering::Relaxed);
        // a word that is not the expected instruction is refused, nothing written
        mem.write32(guest::rec::VALUE_UNIT_SITES[0], 0x6000_0000);
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not a mulli by 1000"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `board_share` rewrites the one float 0.65 the transfer budget is computed from, after
    /// checking it is there; out of range or on a wrong word it is refused.
    #[test]
    fn board_share_rewrites_the_budget_float() {
        let r = root("board", &[("b", "board_share = 1.0\n"), ("low", "board_share = 0.05\n"), ("high", "board_share = 1.5\n")]);
        assert!(ModSpace::load(&r, "low").unwrap_err().contains("0.1-1.0"));
        assert!(ModSpace::load(&r, "high").unwrap_err().contains("0.1-1.0"));
        let m = ModSpace::load(&r, "b").unwrap();
        let mut mem = guest::mem::Memory::new();
        mem.write32(BOARD_SHARE_SITE, BOARD_SHARE_VANILLA);
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["board share 1: the transfer budget is 100% of the club's funds minus committed fees (vanilla 65%)".to_string()]);
        assert_eq!(mem.read32(BOARD_SHARE_SITE), 1.0f32.to_bits());
        assert_eq!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap(), vec!["board share 1: already in place".to_string()], "a raw [[patch]] of the same word first: fine");
        mem.write32(BOARD_SHARE_SITE, 0.5f32.to_bits());
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not the float 0.65"), "another value there: refused");
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `sponsor_offer_scale` multiplies the shirt offers' base table by division (the last value for the
    /// divisions below), refuses a table that isn't the game's, and bounds the values.
    #[test]
    fn sponsor_offer_scale_rewrites_the_table() {
        let r = root("offerscale", &[("s", "sponsor_offer_scale = [18.0, 5.0, 3.0]\n"), ("bad", "sponsor_offer_scale = [500.0]\n")]);
        assert!(ModSpace::load(&r, "bad").unwrap_err().contains("sponsor_offer_scale"));
        let m = ModSpace::load(&r, "s").unwrap();
        let mut mem = guest::mem::Memory::new();
        for row in 0..SPONSOR_OFFER_ROWS {
            for k in 0..16 {
                mem.write32(SPONSOR_OFFER_TABLE + 4 * (row * 16 + k), SPONSOR_OFFER_ROW0[k as usize]);
            }
        }
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0x8200_1000)).unwrap();
        assert_eq!(mem.read32(SPONSOR_OFFER_TABLE + 4 * 15), 320 * 18, "top division, top level");
        assert_eq!(mem.read32(SPONSOR_OFFER_TABLE + 4 * (16 + 15)), 320 * 5, "second division");
        assert_eq!(mem.read32(SPONSOR_OFFER_TABLE + 4 * (4 * 16 + 15)), 320 * 3, "divisions below take the last value");
        assert_eq!(mem.read32(SPONSOR_OFFER_TABLE + 4 * (5 * 16 + 15)), 320 * 18, "the next country's top division");
        assert!(notes.iter().any(|n| n.starts_with("sponsor offers")), "{notes:?}");
        // per country: Scotland's top division at a quarter of England's
        let r2 = root("offercountry", &[
            ("c", "sponsor_offer_scale = [10.0]\nsponsor_offer_country = [1.0, 0.25, 1.0, 1.0, 1.0, 1.0, 1.0, 0.5]\n"),
            ("alone", "sponsor_offer_country = [1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0]\n"),
            ("short", "sponsor_offer_scale = [10.0]\nsponsor_offer_country = [1.0, 0.25]\n"),
        ]);
        assert!(ModSpace::load(&r2, "alone").unwrap_err().contains("with sponsor_offer_scale"));
        assert!(ModSpace::load(&r2, "short").unwrap_err().contains("8 values"));
        let c = ModSpace::load(&r2, "c").unwrap();
        let mut mem2 = guest::mem::Memory::new();
        for row in 0..SPONSOR_OFFER_ROWS {
            for k in 0..16 {
                mem2.write32(SPONSOR_OFFER_TABLE + 4 * (row * 16 + k), SPONSOR_OFFER_ROW0[k as usize]);
            }
        }
        c.patch_image(&mut mem2, (0x8200_0000, 0x8200_1000)).unwrap();
        assert_eq!(mem2.read32(SPONSOR_OFFER_TABLE + 4 * 15), 3200, "England x10");
        assert_eq!(mem2.read32(SPONSOR_OFFER_TABLE + 4 * (5 * 16 + 15)), 800, "Scotland x10 x0.25");
        assert_eq!(mem2.read32(SPONSOR_OFFER_TABLE + 4 * (35 * 16 + 15)), 1600, "Portugal x10 x0.5");
        std::fs::remove_dir_all(&r2).unwrap();
        // the words a state load must restore
        let words = m.data_words(&mut mem);
        assert_eq!(words.len(), 640);
        assert_eq!(words[15], (SPONSOR_OFFER_TABLE + 60, 320 * 18));
        // a table that isn't the game's is refused, not scaled again
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0x8200_1000)).unwrap_err().contains("isn't the game's"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `ai_signing_wage_cap` rewrites the completion check's GBP 100,000 pair for the interpreter and sets
    /// the atomic the recompiled code reads; out of range or on a wrong word it is refused.
    #[test]
    fn ai_signing_wage_cap_rewrites_the_completion_ceiling() {
        let r = root("aisign", &[("w", "ai_signing_wage_cap = 1000000\n"), ("low", "ai_signing_wage_cap = 5000\n")]);
        assert!(ModSpace::load(&r, "low").unwrap_err().contains("100000-10000000"));
        let m = ModSpace::load(&r, "w").unwrap();
        let mut mem = guest::mem::Memory::new();
        let (lis, ori) = guest::rec::AI_SIGNING_WAGE_CAP_SITE;
        mem.write32(lis, 0x3C60_0001); // lis r3,1
        mem.write32(ori, 0x6063_86A0); // ori r3,r3,0x86a0
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["AI signings complete up to GBP 1000000 a week (vanilla 100,000)".to_string()]);
        assert_eq!((mem.read32(lis), mem.read32(ori)), (0x3C60_000F, 0x6063_4240), "1,000,000 = 0xF4240");
        assert_eq!(guest::rec::ai_signing_wage_cap(), 1_000_000);
        guest::rec::AI_SIGNING_WAGE_CAP.store(100_000, std::sync::atomic::Ordering::Relaxed);
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not the GBP 100,000 ceiling"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `wage_cap` rewrites the twelve GBP 250,000 pairs of the contract code for the interpreter and sets
    /// the atomic the recompiled code reads; out of range or on a wrong word it is refused.
    #[test]
    fn wage_cap_rewrites_the_offer_pairs() {
        let r = root("wagecap", &[("w", "wage_cap = 2000000\n"), ("low", "wage_cap = 1000\n")]);
        assert!(ModSpace::load(&r, "low").unwrap_err().contains("250000-10000000"));
        let m = ModSpace::load(&r, "w").unwrap();
        let mut mem = guest::mem::Memory::new();
        for &(lis, ori) in guest::rec::WAGE_OFFER_CAP_SITES.iter() {
            mem.write32(lis, 0x3D60_0003); // lis r11,3
            mem.write32(ori, 0x6163_D090); // ori r3,r11,0xd090
        }
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["wage cap GBP 2000000 a week on contract offers (vanilla 250,000)".to_string()]);
        for &(lis, ori) in guest::rec::WAGE_OFFER_CAP_SITES.iter() {
            assert_eq!((mem.read32(lis), mem.read32(ori)), (0x3D60_001E, 0x6163_8480), "2,000,000 = 0x1E8480, registers kept");
        }
        assert_eq!(guest::rec::wage_offer_cap(), 2_000_000);
        guest::rec::WAGE_OFFER_CAP.store(250_000, std::sync::atomic::Ordering::Relaxed);
        mem.write32(guest::rec::WAGE_OFFER_CAP_SITES[0].1, 0x6000_0000);
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not the GBP 250,000 wage cap"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `income_scale` is validated, reaches the machine and hooks the balance adder; only income
    /// from the listed callers is scaled, within the balance cap.
    #[test]
    fn income_scale_hooks_the_income_callers() {
        let r = root("income", &[("i", "income_scale = 4.0\n"), ("bad", "income_scale = 0\n")]);
        assert!(ModSpace::load(&r, "bad").unwrap_err().contains("0.1-100"));
        assert_eq!(ModSpace::load(&r, "i").unwrap().income_scale, Some(4.0));
        assert_eq!(crate::hooks::scaled_income(1_489_260, 4.0), 5_957_040);
        assert_eq!(crate::hooks::scaled_income(400_000_000, 4.0), 999_999_999, "the balance cap");
        assert!(guest::hooks::ADDRESSES.contains(&crate::hooks::ADD_FUNDS), "recompiled callers must reach the hook");
        for a in [crate::hooks::VALUATION, crate::hooks::VALUE_SET] {
            assert!(guest::hooks::ADDRESSES.contains(&a), "{a:#x}: recompiled callers must reach the hook");
        }
        assert_eq!(crate::hooks::stored_value(37_400, 5000, 200_000_000), 187_000_000, "Haaland");
        assert_eq!(crate::hooks::stored_value(1, 5000, 200_000_000), 10_000, "the game's floor");
        assert_eq!(crate::hooks::stored_value(65_535, 5000, 200_000_000), 200_000_000, "the cap");
        assert_eq!(crate::hooks::form_factor(80.0, 80.0), 1.0);
        assert!((crate::hooks::form_factor(85.0, 80.0) - 1.762).abs() < 0.01, "+5 points");
        assert_eq!(crate::hooks::form_factor(40.0, 80.0), 0.25, "the floor");
        assert_eq!(crate::hooks::match_form(0, 0, 60.0), 1.0, "no matches yet");
        let offs: std::collections::HashSet<i64> = (0..200).map(|i| crate::hooks::dequantise(i, 5000) as i64).collect();
        assert!(offs.len() > 30 && offs.iter().all(|o| o.abs() <= 2500 && o % 100 == 0), "{offs:?}");
        let good = crate::hooks::match_form(75, 10, 60.0);
        assert!((good - 1.03f64.powf(75.0 - 67.02)).abs() < 1e-9 && good > 1.2, "{good}");
        assert!(crate::hooks::match_form(75, 2, 60.0) < good, "fewer games count less");
        assert!(guest::hooks::ADDRESSES.contains(&crate::hooks::WAGE));
        assert!(guest::hooks::ADDRESSES.contains(&crate::hooks::STATEMENT));
        assert!(guest::hooks::ADDRESSES.contains(&crate::hooks::CONTRACT_SET));
        assert!(guest::hooks::ADDRESSES.contains(&crate::hooks::FUNDS));
        let f = r.join("funds.csv");
        std::fs::write(&f, "id,funds_m,source\n0,303.9969,budget\n8,1500,budget\n").unwrap();
        assert_eq!(read_funds(&f).unwrap(), HashMap::from([(0, 303_996_900), (8, 999_999_999)]));
        let w = r.join("wages.csv");
        std::fs::write(&w, "id,wage,years\n270,450000,7\n13,300000,\n").unwrap();
        assert_eq!(read_wages(&w).unwrap(), HashMap::from([(270, (450_000, Some(7))), (13, (300_000, None))]));
        std::fs::write(&w, "id,wage\n270,lots\n").unwrap();
        assert!(read_wages(&w).unwrap_err().contains("line 2"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn manifests() {
        let r = root("manifest", &[
            ("2025-26", "name = \"Season 2025-26\"\nversion = \"1.2\"\nbase = \"ExportPR.mng\"\n"),
            ("bare", ""),
            ("badbase", "base = \"World.bin\"\n"),
            ("typo", "nmae = \"x\"\n"),
        ]);
        std::fs::create_dir_all(r.join("not-a-mod")).unwrap();
        let m = ModSpace::load(&r, "2025-26").unwrap();
        assert_eq!((m.name.as_str(), m.version.as_str(), m.base.as_str()), ("Season 2025-26", "1.2", "ExportPR.mng"));
        assert_eq!(m.user_dir(Path::new("userdata")), Path::new("userdata/2025-26"));
        let bare = ModSpace::load(&r, "bare").unwrap();
        assert_eq!((bare.name.as_str(), bare.base.as_str()), ("bare", "Export.mng"));
        assert!(ModSpace::load(&r, "badbase").unwrap_err().contains("base must be"));
        assert!(ModSpace::load(&r, "typo").unwrap_err().contains("nmae"));
        assert!(ModSpace::load(&r, "missing").is_err());
        assert!(ModSpace::load(&r, "../2025-26").is_err());
        let listed: Vec<_> = ModSpace::list(&r).into_iter().map(|m| m.map(|m| m.id)).collect();
        assert_eq!(listed.len(), 4, "not-a-mod has no mod.toml");
        assert_eq!(listed[0], Ok("2025-26".to_string()));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// The money keys: value_unit, income_scale, values_as_stored and wage_budget_cap parse, and a
    /// value_unit or income_scale out of range is refused at load, before anything is patched.
    #[test]
    fn money_keys() {
        let r = root("money", &[
            ("fc27", "value_unit = 5000\nincome_scale = 4.0\ngate_scale = 8.0\nvalues_as_stored = true\nwage_budget_cap = 10000000\n"),
            ("plain", ""),
            ("unit_low", "value_unit = 500\n"),
            ("unit_odd", "value_unit = 3000\n"),
            ("scale_big", "income_scale = 250.0\n"),
            ("gate_big", "gate_scale = 250.0\n"),
        ]);
        let m = ModSpace::load(&r, "fc27").unwrap();
        assert_eq!((m.value_unit, m.income_scale, m.values_as_stored, m.wage_budget_cap), (Some(5000), Some(4.0), true, Some(10_000_000)));
        assert_eq!(m.gate_scale, Some(8.0));
        assert!(ModSpace::load(&r, "gate_big").unwrap_err().contains("gate_scale 250"));
        let plain = ModSpace::load(&r, "plain").unwrap();
        assert_eq!((plain.value_unit, plain.income_scale, plain.values_as_stored, plain.wage_budget_cap), (None, None, false, None));
        assert!(ModSpace::load(&r, "unit_low").unwrap_err().contains("value_unit 500"));
        assert!(ModSpace::load(&r, "unit_odd").unwrap_err().contains("divide 1,000,000"), "3000 doesn't divide GBP 1m");
        assert!(ModSpace::load(&r, "scale_big").unwrap_err().contains("income_scale 250"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// A mod's clubs.csv `record_of` reaches the database the game is served (#31 item 7): with
    /// the user's own files (LMA_GAME_DIR), the Blackburn <-> Sunderland swap puts Sunderland at
    /// id 2, inside the Premiership's id range. Silent without them.
    #[test]
    fn a_record_of_mod_moves_clubs_in_the_served_database() {
        let Ok(game) = std::env::var("LMA_GAME_DIR") else { return };
        let r = root("record_of", &[("swap", "")]);
        std::fs::create_dir_all(r.join("swap/database")).unwrap();
        std::fs::write(r.join("swap/database/clubs.csv"), "id,record_of\n2,41\n41,2\n").unwrap();
        let (bytes, _, _) = ModSpace::load(&r, "swap").unwrap().build_database(Path::new(&game)).unwrap();
        let db = lma_data::Database::parse(&bytes).unwrap();
        assert_eq!((db.club_name(2), db.club_name(41)), ("Sunderland", "Blackburn Rovers"));
        // its stadium and manager come along (the game pairs both by id, #389)
        assert_eq!((db.stadium_names[2].as_str(), db.stadium_names[41].as_str()), ("Stadium of Light", "Ewood Park"));
        assert_eq!((db.manager_surname(2), db.manager_surname(41)), ("Quinn", "Hughes"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// A sponsor_unit too small for sponsors.csv's biggest shirt deal is kept (the mod asked for it)
    /// but warned about: the deal would stop at 14 bits of units a week.
    #[test]
    fn a_small_sponsor_unit_is_warned_about() {
        let r = root("sponsorwarn", &[("s", "sponsor_unit = 10\n")]);
        std::fs::write(r.join("s/Export.mng"), lma_data::fixtures::fake_db().build().unwrap()).unwrap();
        std::fs::write(r.join("s/sponsors.csv"), "id,club,shirt_gbp,ground_gbp
0,Arsenal,50000000,20000000
").unwrap();
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false };
        let dir = std::env::temp_dir();
        let mut m = crate::machine::Machine::new(&x, &dir, &dir);
        let notes = ModSpace::load(&r, "s").unwrap().install(&mut m, &r.join("no-game-dir")).unwrap();
        assert_eq!(m.sponsor_unit, 10);
        assert!(notes.iter().any(|n| n.starts_with("WARNING: sponsor_unit 10 is too small") && n.contains("needs 59")), "{notes:?}");
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// install() with the economy files: funds.csv, sponsors.csv and wage_budgets.csv all reach the
    /// machine (their hooks with them), and sponsors.csv without a sponsor_unit picks the unit from
    /// its biggest shirt deal (here 100), so no deal is clamped.
    #[test]
    fn install_loads_the_economy_files() {
        let r = root("economy", &[("eco", "")]);
        std::fs::write(r.join("eco/Export.mng"), lma_data::fixtures::fake_db().build().unwrap()).unwrap();
        std::fs::write(r.join("eco/funds.csv"), "id,funds_m\n0,197.59\n").unwrap();
        std::fs::write(r.join("eco/sponsors.csv"), "id,club,shirt_gbp,ground_gbp\n0,Arsenal,50000000,20000000\n").unwrap();
        std::fs::write(r.join("eco/wage_budgets.csv"), "id,bill_gbp_week,budget_gbp_week\n0,3297444,4121805\n").unwrap();
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false };
        let dir = std::env::temp_dir();
        let mut m = crate::machine::Machine::new(&x, &dir, &dir);
        let notes = ModSpace::load(&r, "eco").unwrap().install(&mut m, &r.join("no-game-dir")).unwrap();
        assert_eq!(m.funds.get(&0), Some(&197_590_000));
        assert_eq!(m.sponsors.get(&0), Some(&(50_000_000, 20_000_000)));
        assert_eq!(m.wage_budgets.get(&0), Some(&4_121_805));
        assert_eq!(m.sponsor_unit, 100, "no sponsor_unit: the biggest deal picks it (50m a season = 961,538 a week = 59 units of 16,383)");
        for n in ["funds.csv: 1", "sponsors.csv: 1", "wage_budgets.csv: 1", "sponsor unit GBP 100"] {
            assert!(notes.iter().any(|x| x.starts_with(n)), "{n} missing from {notes:?}");
        }
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// install() with squad_overflow.csv: the players reach the machine with their clubs, the club
    /// lookup is hooked and the note says what overflow players don't get; a bad line is refused.
    #[test]
    fn install_loads_squad_overflow() {
        let r = root("overflow", &[("o", ""), ("bad", "")]);
        for id in ["o", "bad"] {
            std::fs::write(r.join(id).join("Export.mng"), lma_data::fixtures::fake_db().build().unwrap()).unwrap();
        }
        std::fs::write(r.join("o/squad_overflow.csv"), "id,club_id
4000,0
4001,0
4002,1
").unwrap();
        std::fs::write(r.join("bad/squad_overflow.csv"), "id,club_id
4000,Arsenal
").unwrap();
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false };
        let dir = std::env::temp_dir();
        let mut m = crate::machine::Machine::new(&x, &dir, &dir);
        let notes = ModSpace::load(&r, "o").unwrap().install(&mut m, &r.join("no-game-dir")).unwrap();
        assert_eq!(m.squad_overflow.len(), 3);
        assert_eq!(m.squad_overflow.get(&4002), Some(&1));
        assert!(notes.iter().any(|n| n.starts_with("squad_overflow.csv: 3 players beyond their club's 32")), "{notes:?}");
        let mut m = crate::machine::Machine::new(&x, &dir, &dir);
        let err = ModSpace::load(&r, "bad").unwrap().install(&mut m, &r.join("no-game-dir")).unwrap_err();
        assert!(err.contains("is not id,club_id"), "{err}");
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// A mod whose base database has a club under 11 players gets a warning note: the match
    /// engine's event picker loops forever on a fixture with such a club (#462).
    #[test]
    fn a_club_under_11_players_is_warned_about() {
        let r = root("small", &[("tiny", "")]);
        std::fs::write(r.join("tiny/Export.mng"), lma_data::fixtures::fake_db().build().unwrap()).unwrap();
        let (_, notes, names) = ModSpace::load(&r, "tiny").unwrap().build_database(&r.join("no-game-dir")).unwrap();
        assert_eq!(names[..2], ["Arsenal", "Everton"]);
        let warn = notes.iter().find(|n| n.starts_with("WARNING")).expect("a warning note");
        assert!(warn.contains("2 club(s) under 11 players") && warn.contains("Arsenal (2)") && warn.contains("Everton (1)"), "{warn}");
        assert!(!warn.contains("Wanderers"), "an empty club is not a fixture club: {warn}");
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// strings.csv replaces table strings in every language, or only the one its lang column names,
    /// and refuses an id past the table.
    #[test]
    fn strings_csv_edits_the_string_tables() {
        let r = root("strings", &[("s", ""), ("bad", "")]);
        let game = r.join("game");
        std::fs::create_dir_all(game.join("Common/GameData/Misc/Lang")).unwrap();
        for l in LANGS {
            let t = lma_data::lang::Lang { version: 1, strings: vec![b"Menu".to_vec(), b"Adidas".to_vec(), b"HSBC".to_vec()] };
            std::fs::write(game.join(format!("Common/GameData/Misc/Lang/LMA_{l}.lng")), t.build()).unwrap();
        }
        std::fs::write(r.join("s/strings.csv"), "id,text,lang\n1,Emirates,\n2,\"Etihad, Abu Dhabi\",E\n").unwrap();
        std::fs::write(r.join("bad/strings.csv"), "id,text\n9,Nope\n").unwrap();
        let (tables, count) = ModSpace::load(&r, "s").unwrap().build_strings(&game).unwrap().unwrap();
        assert_eq!(count, 6, "row 1 in five languages, row 2 in English only");
        for (l, bytes) in tables {
            let t = lma_data::lang::Lang::parse(&bytes).unwrap();
            assert_eq!(t.get(1).as_deref(), Some("Emirates"));
            assert_eq!(t.get(2).as_deref(), Some(if l == 'E' { "Etihad, Abu Dhabi" } else { "HSBC" }), "{l}");
        }
        assert!(ModSpace::load(&r, "bad").unwrap().build_strings(&game).unwrap_err().contains("line 2"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn formations_csv_appends_the_names_as_string_ids() {
        let r = root("formations_strings", &[("f", "formations = \"formations.csv\"\n"), ("v", "formations = \"formations.csv\"\n")]);
        let game = r.join("game");
        std::fs::create_dir_all(game.join("Common/GameData/Misc/Lang")).unwrap();
        for l in LANGS {
            // one language's table is longer: the appended ids go past the longest
            let mut strings = vec![b"Menu".to_vec(), b"4-4-2".to_vec(), b"4-2-4".to_vec()];
            if l == 'G' {
                strings.push(b"Extra".to_vec());
            }
            std::fs::write(game.join(format!("Common/GameData/Misc/Lang/LMA_{l}.lng")), lma_data::lang::Lang { version: 1, strings }.build()).unwrap();
        }
        std::fs::write(r.join("f/formations.csv"), "id,name,roles\n15,4-1-4-1,0 1 1 5 3 3 6 5 8 7 6\n14,3-4-2-1,GK WB WB CB CB CB CM AM CF AM CM\n").unwrap();
        let m = ModSpace::load(&r, "f").unwrap();
        let (tables, count) = m.build_strings(&game).unwrap().unwrap();
        assert_eq!(count, 0, "no strings.csv");
        assert_eq!(m.formation_string_ids.get(), Some(&vec![132, 133]), "ids past the longest table and the gap, in id order");
        for (l, bytes) in tables {
            let t = lma_data::lang::Lang::parse(&bytes).unwrap();
            assert_eq!(t.strings.len(), 134, "{l}");
            assert_eq!((t.get(132).as_deref(), t.get(133).as_deref()), (Some("3-4-2-1"), Some("4-1-4-1")), "{l}");
            assert_eq!((t.get(3).as_deref(), t.get(4).as_deref()), (Some(if l == 'G' { "Extra" } else { "" }), Some("")), "{l}: the shorter tables padded, the gap empty");
        }
        // a changed vanilla shape alone appends nothing and needs no string table
        std::fs::write(r.join("v/formations.csv"), "id,name,roles\n2,4-3-3,gk fb fb cm cb cb wf cm cm cf wf\n").unwrap();
        assert!(ModSpace::load(&r, "v").unwrap().build_strings(&game).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&r);
    }

    /// `default_spots` lays the game's own shapes out as the game does (11 of 14 rows), and refuses
    /// what isn't a shape.
    #[test]
    fn default_spots_match_the_vanilla_rows() {
        let v = |s: &str| default_spots(s).map(|a| a.to_vec());
        assert_eq!(v("4-4-2"), Some(vec![26, 9, 10, 11, 12, 13, 14, 15, 16, 18, 19]));
        assert_eq!(v("4-2-4"), Some(vec![26, 9, 10, 11, 12, 14, 15, 17, 18, 19, 20]));
        assert_eq!(v("1-2-5-2"), Some(vec![26, 27, 10, 11, 28, 3, 4, 5, 30, 18, 19]), "a sweeper, a line of five");
        assert_eq!(v("4-2-3-1 (IW)"), Some(vec![26, 9, 10, 11, 12, 14, 15, 32, 31, 33, 7]), "the text after the shape is ignored");
        assert_eq!(v("4-1-4-1"), Some(vec![26, 9, 10, 11, 12, 29, 13, 14, 15, 16, 7]));
        assert_eq!((v("4-4-3"), v("Diamond"), v("4-0-6"), v("6-4"), v("3-3-3-1-0")), (None, None, None, None, None));
    }


    /// `ai_wage_offers` reaches the hooks' switch with a note, and is off without the key.
    #[test]
    fn ai_wage_offers_turns_the_blocks_on() {
        let r = root("awo", &[("on", "ai_wage_offers = true\n"), ("off", "")]);
        assert!(ModSpace::load(&r, "on").unwrap().ai_wage_offers);
        assert!(!ModSpace::load(&r, "off").unwrap().ai_wage_offers);
        std::fs::write(r.join("on/Export.mng"), lma_data::fixtures::fake_db().build().unwrap()).unwrap();
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false };
        let dir = std::env::temp_dir();
        let mut m = crate::machine::Machine::new(&x, &dir, &dir);
        let notes = ModSpace::load(&r, "on").unwrap().install(&mut m, &r.join("no-game-dir")).unwrap();
        // the global itself isn't asserted: any test installing a mod space at the same time stores it
        // (a race on the Windows runner, #728's CI); the note says install turned it on
        assert!(notes.iter().any(|n| n.starts_with("AI wage offers: at least the player's wage + 10%")), "{notes:?}");
        crate::hooks::AI_WAGE_OFFERS.store(false, std::sync::atomic::Ordering::Relaxed); // off again for the other tests
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `subs` rewrites match setup's `addi r16,0,3` (the competitive-match limit) after checking it
    /// is there; outside 3-5 it is refused at load.
    #[test]
    fn subs_rewrites_the_match_limit() {
        let r = root("subs", &[("five", "subs = 5\n"), ("six", "subs = 6\n")]);
        assert!(ModSpace::load(&r, "six").unwrap_err().contains("3-5"));
        let m = ModSpace::load(&r, "five").unwrap();
        let mut mem = guest::mem::Memory::new();
        mem.write32(guest::rec::SUBS_SITE, 0x3A00_0003);
        mem.write32(guest::rec::SUBS_SIM_LOW, 0x3BE0_0002);
        mem.write32(guest::rec::SUBS_SIM_HIGH, 0x3BE0_0003);
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["5 substitutions in a competitive match (vanilla 3); simulated matches plan 4 or 5 when tired".to_string()]);
        assert_eq!(mem.read32(guest::rec::SUBS_SITE), 0x3A00_0005);
        assert_eq!((mem.read32(guest::rec::SUBS_SIM_LOW), mem.read32(guest::rec::SUBS_SIM_HIGH)), (0x3BE0_0004, 0x3BE0_0005));
        assert_eq!(guest::rec::subs_limit(), 5);
        guest::rec::SUBS.store(3, std::sync::atomic::Ordering::Relaxed);
        assert!(m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap_err().contains("not addi r16,0,3"), "already rewritten: refused");
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// `bench` (#530): 5-9 at load; 6-9 turn on that many reserves for the native slot lookup.
    #[test]
    fn bench_sets_the_extra_substitutes() {
        let r = root("bench", &[("nine", "bench = 9\n"), ("ten", "bench = 10\n"), ("four", "bench = 4\n")]);
        assert!(ModSpace::load(&r, "ten").unwrap_err().contains("5-9"));
        assert!(ModSpace::load(&r, "four").unwrap_err().contains("5-9"));
        let m = ModSpace::load(&r, "nine").unwrap();
        let mut mem = guest::mem::Memory::new();
        let notes = m.patch_image(&mut mem, (0x8200_0000, 0)).unwrap();
        assert_eq!(notes, vec!["9 substitutes: the 4 reserves after the matchday 16 can come on too".to_string()]);
        assert_eq!(crate::hooks::BENCH_EXTRA.swap(0, std::sync::atomic::Ordering::Relaxed), 4);
        std::fs::remove_dir_all(&r).unwrap();
    }

    /// sponsors.csv: named columns in any order, extra columns ignored, a bad number refused with
    /// its line.
    #[test]
    fn sponsors_csv_reads_per_club_deals() {
        let r = root("sponsors", &[("s", "")]);
        let p = r.join("s/sponsors.csv");
        std::fs::write(&p, "id,club,shirt_gbp,ground_gbp,source\n0,Arsenal,50000000,20000000,reported\n9,\"Man City, Etihad\",67500000,27000000,reported\n").unwrap();
        assert_eq!(read_sponsors(&p).unwrap(), HashMap::from([(0, (50_000_000, 20_000_000)), (9, (67_500_000, 27_000_000))]));
        std::fs::write(&p, "id,shirt_gbp,ground_gbp\n0,lots,1\n").unwrap();
        assert!(read_sponsors(&p).unwrap_err().contains("line 2"));
        std::fs::write(&p, "id,shirt_gbp\n0,1\n").unwrap();
        assert!(read_sponsors(&p).unwrap_err().contains("ground_gbp"));
        std::fs::remove_dir_all(&r).unwrap();
    }

    #[test]
    fn mod_argument() {
        let mut args: Vec<String> = ["run", "x.xex", "--mod", "2025-26", "100"].map(String::from).to_vec();
        assert_eq!(selected(&mut args).as_deref(), Some("2025-26"));
        assert_eq!(args, ["run", "x.xex", "100"]);
        let mut args = vec!["--mod".to_string(), "vanilla".to_string()];
        assert_eq!(selected(&mut args), None);
    }
}
