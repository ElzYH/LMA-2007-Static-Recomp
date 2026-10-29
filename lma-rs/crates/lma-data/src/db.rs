//! The club/player database inside `Export.mng` / `ExportPR.mng`.
//!
//! Field positions were reverse-engineered from the retail disc and checked in game
//! (squad screen, training skills summary, stadium screen, team select funds).
//!
//! The PS2's database (#3) has the same files and record sizes, little-endian: tables have their
//! own magic, bit fields are LSB-first (a field's position counts from the low bit of each
//! little-endian word, nearly always the same number as on the 360: `Field::ps2`), club and
//! stadium names are packed NUL-terminated strings rather than fixed slots, and manager records
//! are 24 bytes with the surname in bits 0-14. Verified against the 360's database by matching
//! ~19,500 players by name: every rating, position, age, height and squad number agrees; nation
//! ids sit at the same bits but are numbered differently (England is 14 on the 360, 11 on the PS2).

use crate::archive::Archive;
use crate::{bits, text, Endian, Error, Result};
use std::collections::HashMap;

pub const LANGS: [char; 5] = ['E', 'F', 'G', 'I', 'S'];
/// Table magics: the 360's two, and the PS2's (read little-endian).
const TABLE_MAGICS: [u32; 3] = [0x4C3E_B25B, 0x5ABE_B540, 0x394F_AD09];
const SQUAD_OFF: usize = 18;
pub const SQUAD_MAX: usize = 32;
const CLUBNAME_LEN: usize = 26;
const SHORTNAME_LEN: usize = 8;
const STADNAME_LEN: usize = 40;
const STADIUM_RS: usize = 32;
/// One season of a league history: the champion's uid (0: none) and its record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistoryRow {
    pub uid: u16,
    pub won: u8,
    pub drawn: u8,
    pub lost: u8,
    pub goals_for: u8,
    pub goals_against: u8,
    pub points: u8,
}

impl HistoryRow {
    fn parse(b: &[u8]) -> Self {
        HistoryRow { uid: u16::from_be_bytes([b[0], b[1]]), won: b[2], drawn: b[3], lost: b[4], goals_for: b[5], goals_against: b[6], points: b[7] }
    }
    fn bytes(&self) -> [u8; 8] {
        let u = self.uid.to_be_bytes();
        [u[0], u[1], self.won, self.drawn, self.lost, self.goals_for, self.goals_against, self.points]
    }
}

/// One cup final in a cup history: the winner's and the runner-up's uids (0: no final that season)
/// and the score (in play: the game keeps no shoot-out).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CupRow {
    pub winner: u16,
    pub runner_up: u16,
    pub winner_goals: u8,
    pub runner_up_goals: u8,
}

impl CupRow {
    fn parse(b: &[u8]) -> Self {
        CupRow { winner: u16::from_be_bytes([b[0], b[1]]), runner_up: u16::from_be_bytes([b[2], b[3]]), winner_goals: b[4], runner_up_goals: b[5] }
    }
    fn bytes(&self) -> [u8; 8] {
        let (w, r) = (self.winner.to_be_bytes(), self.runner_up.to_be_bytes());
        [w[0], w[1], r[0], r[1], self.winner_goals, self.runner_up_goals, 0, 0]
    }
}

/// World.bin's cup histories (the maintainer, 12 Oct: the finals were still 2006's): each cup is `CUP_ROWS`
/// eight-byte rows at its offset, newest season first (row 0 the season ending in the base year),
/// an empty row a season without a final. The game has these seven; the league histories follow at
/// `HISTORY_AT`, and a 0x70-byte table of other data sits between the Super Cup and the FA Cup.
pub const CUPS: [(&str, usize); 7] = [
    ("european_cup", 0x70),
    ("uefa_cup", 0x520),
    ("super_cup", 0x9d0),
    ("fa_cup", 0xef0),
    ("league_cup", 0x13a0),
    ("charity_shield", 0x1850),
    ("league_trophy", 0x1d00),
];
pub const CUP_ROWS: usize = 150;

/// World.bin's loan table: this many eight-byte slots, ending one of `LOAN_TAILS` bytes before
/// the end of the file (`Database::loans`): 32 in Export.mng, 0 in ExportPR.mng (whose World.bin
/// is 32 bytes shorter).
const LOAN_SLOTS: usize = 700;
const LOAN_TAILS: [usize; 2] = [32, 0];
const LOAN_FREE: [u8; 8] = [0xFF, 0xFF, 0, 0, 0, 0, 0, 0];
/// World.bin's league histories (#31): country `c`'s tier `t` (0 = top) is `HISTORY_ROWS`
/// eight-byte rows at `HISTORY_AT + c * HISTORY_COUNTRY + t * HISTORY_TIER`, newest season first.
const HISTORY_AT: usize = 0x21b0;
const HISTORY_COUNTRY: usize = 0x2aa0;
const HISTORY_TIER: usize = 1200;
pub const HISTORY_ROWS: usize = 150;
/// The countries with leagues (England .. Portugal) and the tiers each has room for.
pub const HISTORY_COUNTRIES: usize = 8;
pub const HISTORY_TIERS: usize = 5;

/// Last season's final tables in World.bin: each league's clubs by final position (u16 uids),
/// the leagues one after another in `LAST_TABLE_NATIONS` order, top tier first. The
/// game builds a season's European entrants from them at a career's start (0x8213a918 ->
/// 0x82138b50 -> 0x821333d0 -> 0x820e1a98, World DB +0x36c64) as it does from the final tables
/// at a season's end, so on the disc a new career's cups have 2005/06's qualifiers: Chelsea,
/// Manchester United, Liverpool, Arsenal (#607).
pub const LAST_TABLE_AT: usize = 0x17190;
/// The nations of `LAST_TABLE_SIZES`, in the block's order (found from the disc's 2005/06 champions
/// at each league's first position: Chelsea, Celtic, Barcelona, Inter, Lyon, Bayern, PSV, Porto).
pub const LAST_TABLE_NATIONS: [&str; 8] = ["England", "Scotland", "Spain", "Italy", "France", "Germany", "Netherlands", "Portugal"];
/// Each nation's league sizes, top tier first (445 clubs in all).
pub const LAST_TABLE_SIZES: [&[usize]; 8] = [&[20, 24, 24, 24, 24], &[12, 10, 10, 10], &[20, 22], &[20, 22], &[20, 20, 20], &[18, 18, 19, 18], &[18, 20], &[16, 16]];
const STANDS_OFF: usize = 16;

/// What differs between the Xbox 360's database and the PS2's (#3), picked from the archive.
#[derive(Clone, Copy, Debug)]
struct Layout {
    e: Endian,
    /// Club and stadium names packed back to back (the PS2) instead of fixed-width slots.
    packed_names: bool,
    manager_rs: usize,
    /// Manager surname id: (bit position, width) in the platform's bit order.
    mgr_surname: (usize, usize),
}

impl Layout {
    fn of(e: Endian) -> Self {
        match e {
            Endian::Big => Layout { e, packed_names: false, manager_rs: 36, mgr_surname: (11, 15) },
            Endian::Little => Layout { e, packed_names: true, manager_rs: 24, mgr_surname: (0, 15) },
        }
    }

    fn get(&self, rec: &[u8], pos: usize, width: usize) -> u32 {
        match self.e {
            Endian::Big => bits::get(rec, pos, width),
            Endian::Little => bits::get_le(rec, pos, width),
        }
    }

    fn set(&self, rec: &mut [u8], pos: usize, width: usize, val: u32) {
        match self.e {
            Endian::Big => bits::set(rec, pos, width, val),
            Endian::Little => bits::set_le(rec, pos, width, val),
        }
    }

    fn field_pos(&self, f: Field) -> usize {
        if self.e == Endian::Big { f.pos } else { f.ps2 }
    }
}

/// A bit field of a player record: (name, bit position, width, value offset), and its position
/// in the PS2's record (LSB-first, #3).
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub pos: usize,
    pub width: usize,
    pub add: u32,
    pub ps2: usize,
}

const fn f(name: &'static str, pos: usize, width: usize, add: u32) -> Field {
    Field { name, pos, width, add, ps2: pos }
}

/// A field whose PS2 position isn't the 360's.
const fn f2(name: &'static str, pos: usize, ps2: usize, width: usize, add: u32) -> Field {
    Field { name, pos, width, add, ps2 }
}

pub const SURNAME: Field = f2("surname_id", 3, 0, 15, 0);
pub const FORENAME: Field = f("forename_id", 18, 14, 0);
pub const POSITION: Field = f("position", 60, 2, 0);
pub const POSITION2: Field = f("position2", 62, 2, 0);
pub const NATION: Field = f("nation_id", 110, 8, 0);
pub const HEIGHT: Field = f2("height_cm", 129, 128, 6, 150);
pub const AGE: Field = f("age", 183, 5, 16);
pub const SQUAD_NUMBER: Field = f("squad_number", 192, 7, 0);
/// Skin tone (#31): 0 light, 1 olive, 2 and 3 dark (vanilla: Terry 0, Ronaldinho 1, Drogba 2,
/// Henry 3). Found on the 360's record only: export and apply skip it on the PS2's.
pub const SKIN: Field = f("skin", 276, 2, 0);
/// Hair type (#697): bits 270-274, 0-31, the match's hair model (`+0x20 >> 13 & 0x1f` of the runtime
/// record, read by the appearance loader 0x82232288 with the colour: Hair.mng's textures are
/// `type * 14 + colour`). Which id is which of Hair.mng's 31 styles (bld/lng/man/med/sht) is still to
/// be pinned on the live app; vanilla's commonest is 8. (Until 22 Oct this was read at 268, 4 bits,
/// straddling the colour and the type.) An imported player keeps his 2006 slot's unless it is
/// written. 360 only, like `SKIN`.
pub const HAIR: Field = f("hair", 270, 5, 0);
/// Hair colour (#697): bits 266-269, 0-13 (the 14 colours of HairDiffuseCol/FacialHairCol, and the
/// eyebrows'). From vanilla's well-known players: 0 black (Henry, Drogba, Ronaldinho, Ballack,
/// Lampard), 1 dark brown (Terry, Carragher, Cech, Owen), 2 brown (Gerrard), 3 dark blonde (Beckham,
/// Bentley, Robben), 4 light brown (Crouch, Duff, Savage), 5 ginger (Scholes, Riise), 10 blonde
/// (Nedved, Lehmann); 6-9 and 11-13 are rare (greys, dyes). (Until 22 Oct this was read at 272,
/// 3 bits: the type's low bits.) 360 only.
pub const HAIR_COLOUR: Field = f("hair_colour", 266, 4, 0);
/// The face the match draws (#697): one 4-bit field, bits 275-278, that holds `SKIN` as its middle
/// two bits. The appearance loader (0x82232288) reads it from the runtime record (`+0x20 >> 9`, a
/// copy of these bits; all 27,290 records agree) and `0x82010ca0` maps it to Face.mng's sets:
/// 0-2 white, 3-5 black, 6-8 brown, 9-11 oriental (9 ori1, 10 ori3, 11 ori2), 12-14 medium, 15 black.
/// Writing `skin` alone keeps bits 275 and 278 from the record's earlier player, so an imported
/// player's face set was half his and half the 2006 player's (29% of fc27final30 drawn from
/// another set). Applied after `skin`, so a `face` column wins. 360 only, like `SKIN`.
pub const FACE: Field = f("face", 275, 4, 0);
/// Face ids of each tone set (`FACE`), lightest to darkest: white, medium, brown, black, oriental.
pub const FACE_SETS: [[u32; 3]; 5] = [[0, 1, 2], [12, 13, 14], [6, 7, 8], [3, 4, 5], [9, 11, 10]];
/// A manager's reputation in his Managers.bin record (360): (bit, width), 0-99. Found by comparing
/// the top clubs' 2006 managers (80-99) with League Two's and the Conference's (around 30).
pub const MGR_REPUTATION: (usize, usize) = (70, 7);
/// Market value in £1000s (#31 item 1): a big-endian u16 at record byte 20 (vanilla: Henry 45000,
/// Gerrard 30000, Kompany 8000, the game showing £8.0m), 0 for over half the players. The value
/// the game displays mixes it with one of its own. 360 only, like `SKIN`.
pub const VALUE: Field = f("value", 160, 16, 0);
/// Sides a player plays on (#31 item 3), one bit each: 4 left, 2 right, 1 centre; 7 any (every
/// goalkeeper). Vanilla: Ashley Cole 4, Gary Neville 2, Terry 1, Giggs 4, Beckham 2, Lennon 6. With
/// `position` / `position2` (the lines) it is the whole of LMA's position. 360 only.
pub const SIDES: Field = f("sides", 221, 3, 0);
/// Preferred foot (#31 item 3): 1 right, 2 left, 3 both (vanilla: Gerrard 1, Messi and Giggs 2,
/// Rooney and Robben 3). 360 only.
pub const FOOT: Field = f("foot", 156, 2, 0);

/// The 17 ratings in the order the squad screen lists them. Goalkeepers reuse slots:
/// reach=tackling, awareness=marking, distribution=passing, handling=technique,
/// reflexes=shot_accuracy, aerial=heading, agility=pace.
pub const RATINGS: [Field; 17] = [
    f("tackling", 32, 7, 0),
    f("marking", 39, 7, 0),
    f("passing", 46, 7, 0),
    f("technique", 53, 7, 0),
    f("dribbling", 135, 7, 0),
    f("shot_power", 231, 7, 0),
    f("shot_accuracy", 64, 7, 0),
    f("heading", 71, 7, 0),
    f("positioning", 176, 7, 0),
    f("runs", 149, 7, 0),
    f("creativity", 142, 7, 0),
    f("pace", 85, 7, 0),
    f("intelligence", 103, 7, 0),
    f("temperament", 78, 7, 0),
    f("stamina", 205, 7, 0),
    f("strength", 224, 7, 0),
    f("fitness", 96, 7, 0),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Gk,
    Def,
    Mid,
    Att,
}

impl Position {
    pub fn from_code(c: u32) -> Self {
        [Self::Gk, Self::Def, Self::Mid, Self::Att][c as usize & 3]
    }
    pub fn label(self) -> &'static str {
        ["GK", "DEF", "MID", "ATT"][self as usize]
    }
}

/// The surname ids the game shows: past about 30,000 (ids 31,689 and up in fc27final5, the strings
/// loaded whole) a player is shown as "Smith" (slot 0), so the pool stops growing here and reuses
/// the slots nothing uses (#31, 10 Oct: FC Chelyabinsk and Steaua all "Smith" in a match).
pub const SURNAMES_SHOWN: usize = 24_000;

/// Forename.bin / Surnames.bin: u32 offsets then NUL-terminated strings.
/// Existing indices never move; new names are appended, and once the pool reaches the size its
/// id field can hold (`capacity`), they take the slots of names nothing uses any more (#31: a
/// 2026 import brings ~15,000 new surnames, the field holds 32,768).
#[derive(Clone, Debug)]
pub struct NamePool {
    pub names: Vec<String>,
    orig: Vec<u8>,
    dirty: bool,
    e: Endian,
    lookup: HashMap<String, u32>,
    pub(crate) capacity: usize,
    /// Slots free for reuse, found when the pool fills and again when they run out
    /// (`Database::name_index`).
    free: Option<std::collections::BTreeSet<u32>>,
}

impl NamePool {
    fn parse(d: &[u8], e: Endian) -> Self {
        let n = e.u32(d, 0) as usize / 4;
        let names: Vec<String> = (0..n).map(|i| text::cstr(d, e.u32(d, i * 4) as usize)).collect();
        let mut lookup = HashMap::new();
        for (i, s) in names.iter().enumerate() {
            lookup.entry(s.clone()).or_insert(i as u32);
        }
        Self { names, orig: d.to_vec(), dirty: false, e, lookup, capacity: usize::MAX, free: None }
    }

    /// Full: `s` is new and no slot is left (or none found free yet).
    fn needs_free_slots(&self, s: &str) -> bool {
        !self.lookup.contains_key(s) && self.names.len() >= self.capacity && self.free.as_ref().is_none_or(|f| f.is_empty())
    }

    pub fn index_of(&mut self, s: &str) -> Result<u32> {
        if let Some(&i) = self.lookup.get(s) {
            if let Some(free) = &mut self.free {
                free.remove(&i);
            }
            return Ok(i);
        }
        text::encode(s).ok_or_else(|| Error::Value(format!("'{s}' has characters the game can't show")))?;
        self.dirty = true;
        if self.names.len() < self.capacity {
            self.names.push(s.to_string());
            self.lookup.insert(s.to_string(), self.names.len() as u32 - 1);
            return Ok(self.names.len() as u32 - 1);
        }
        let slot = self
            .free
            .as_mut()
            .and_then(|f| f.pop_first())
            .ok_or_else(|| Error::Value(format!("name pool full: {} names, every one in use", self.names.len())))?;
        let old = std::mem::replace(&mut self.names[slot as usize], s.to_string());
        if self.lookup.get(&old) == Some(&slot) {
            self.lookup.remove(&old);
        }
        self.lookup.insert(s.to_string(), slot);
        Ok(slot)
    }

    /// True once a name has been appended (the pool is then rebuilt on save).
    pub fn grew(&self) -> bool {
        self.dirty
    }

    fn build(&self) -> Vec<u8> {
        if !self.dirty {
            return self.orig.clone();
        }
        let mut offs = Vec::new();
        let mut pool = Vec::new();
        let base = 4 * self.names.len();
        for s in &self.names {
            offs.extend_from_slice(&self.e.u32_bytes((base + pool.len()) as u32));
            pool.extend(text::encode(s).expect("checked on insert"));
            pool.push(0);
        }
        offs.extend(pool);
        offs
    }
}

struct Table {
    magic: u32,
    rs: usize,
    recs: Vec<Vec<u8>>,
    e: Endian,
}

impl Table {
    fn parse(d: &[u8], e: Endian) -> Result<Self> {
        let magic = e.u32(d, 0);
        if !TABLE_MAGICS.contains(&magic) {
            return Err(Error::Format(format!("bad table magic {magic:#x}")));
        }
        let rs = e.u32(d, 4) as usize;
        let n = e.u32(d, 8) as usize;
        let recs = (0..n).map(|i| d[12 + i * rs..12 + (i + 1) * rs].to_vec()).collect();
        Ok(Self { magic, rs, recs, e })
    }

    fn build(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.rs * self.recs.len());
        for v in [self.magic, self.rs as u32, self.recs.len() as u32] {
            out.extend_from_slice(&self.e.u32_bytes(v));
        }
        for r in &self.recs {
            out.extend_from_slice(r);
        }
        out
    }
}

fn fixed_strings(d: &[u8], size: usize) -> Vec<String> {
    d.chunks(size).map(|c| text::cstr(c, 0)).collect()
}

/// NUL-terminated strings back to back (the PS2's club and stadium names, #3).
fn packed_strings(d: &[u8]) -> Vec<String> {
    let body = d.strip_suffix(&[0]).unwrap_or(d);
    if body.is_empty() && d.is_empty() {
        return Vec::new();
    }
    body.split(|&b| b == 0).map(text::decode).collect()
}

/// `packed_strings` back to bytes; `max` is the longest name the game's slots hold (as on the 360).
fn pack_packed(names: &[String], max: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for s in names {
        let b = text::encode(s).ok_or_else(|| Error::Value(format!("'{s}': unsupported characters")))?;
        if b.len() > max {
            return Err(Error::Value(format!("'{s}' is longer than {max} characters")));
        }
        out.extend(b);
        out.push(0);
    }
    Ok(out)
}

fn pack_fixed(names: &[String], size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(names.len() * size);
    for s in names {
        let mut b = text::encode(s).ok_or_else(|| Error::Value(format!("'{s}': unsupported characters")))?;
        if b.len() > size - 1 {
            return Err(Error::Value(format!("'{s}' is longer than {} characters", size - 1)));
        }
        b.resize(size, 0);
        out.extend(b);
    }
    Ok(out)
}

/// A decoded player, convenient for UIs. Writes go through [`Database::set_field`].
#[derive(Clone, Debug)]
pub struct Player {
    pub id: usize,
    pub forename: String,
    pub surname: String,
    pub position: Position,
    pub position2: Option<Position>,
    pub age: u32,
    pub height_cm: u32,
    pub nation_id: u32,
    pub squad_number: u32,
    pub ratings: [u32; 17],
}

impl Player {
    pub fn display_name(&self) -> String {
        let f = self.forename.trim();
        if f.is_empty() { self.surname.trim().to_string() } else { format!("{f} {}", self.surname.trim()) }
    }

    /// Mean of the ratings that matter for the player's role (keepers: their 7 GK slots
    /// plus the shared ones; outfield: everything but the keeper-only meaning).
    pub fn overall(&self) -> u32 {
        let r = &self.ratings;
        let idx: &[usize] = match self.position {
            Position::Gk => &[0, 1, 2, 3, 6, 7, 11, 12, 13, 14, 15],
            _ => &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
        };
        idx.iter().map(|&i| r[i]).sum::<u32>() / idx.len() as u32
    }
}

pub struct Database {
    ar: Archive,
    layout: Layout,
    pub forenames: NamePool,
    pub surnames: NamePool,
    players: Table,
    clubs: Table,
    pub club_names: Vec<Vec<String>>,  // [lang][club]
    pub short_names: Vec<Vec<String>>, // [lang][club]
    pub stadium_names: Vec<String>,
    stadiums: Vec<Vec<u8>>,
    managers: Vec<Vec<u8>>,
}

impl Database {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let ar = Archive::parse(bytes)?;
        let layout = Layout::of(ar.endian);
        let e = ar.endian;
        let file = |n: &str| ar.get(n).ok_or_else(|| Error::Format(format!("{n} missing from archive")));
        let names = |d: &[u8], size: usize| if layout.packed_names { packed_strings(d) } else { fixed_strings(d, size) };
        let mut club_names = Vec::new();
        let mut short_names = Vec::new();
        for l in LANGS {
            club_names.push(names(file(&format!("ClubNam{l}.bin"))?, CLUBNAME_LEN));
            short_names.push(fixed_strings(file(&format!("ShortNm{l}.bin"))?, SHORTNAME_LEN));
        }
        Ok(Self {
            forenames: NamePool { capacity: 1 << FORENAME.width, ..NamePool::parse(file("Forename.bin")?, e) },
            surnames: NamePool { capacity: SURNAMES_SHOWN, ..NamePool::parse(file("Surnames.bin")?, e) },
            players: Table::parse(file("Players.bin")?, e)?,
            clubs: Table::parse(file("Clubs.bin")?, e)?,
            stadium_names: names(file("StadName.bin")?, STADNAME_LEN),
            stadiums: file("Stadiums.bin")?.chunks(STADIUM_RS).map(<[u8]>::to_vec).collect(),
            managers: file("Managers.bin")?.chunks(layout.manager_rs).map(<[u8]>::to_vec).collect(),
            club_names,
            short_names,
            layout,
            ar,
        })
    }

    /// The database's byte order: big for the Xbox 360's, little for the PS2's (#3).
    pub fn endian(&self) -> Endian {
        self.layout.e
    }

    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::parse(&std::fs::read(path)?)
    }

    // ---- players -------------------------------------------------------
    pub fn player_count(&self) -> usize {
        self.players.recs.len()
    }

    pub fn field(&self, pid: usize, f: Field) -> u32 {
        self.layout.get(&self.players.recs[pid], self.layout.field_pos(f), f.width) + f.add
    }

    pub fn set_field(&mut self, pid: usize, f: Field, value: u32) -> Result<()> {
        let raw = value
            .checked_sub(f.add)
            .filter(|v| f.width == 32 || *v < (1 << f.width))
            .ok_or_else(|| Error::Value(format!("{} = {value} out of range", f.name)))?;
        let layout = self.layout;
        layout.set(&mut self.players.recs[pid], layout.field_pos(f), f.width, raw);
        Ok(())
    }

    pub fn player(&self, pid: usize) -> Player {
        let g = |f| self.field(pid, f);
        let p2 = g(POSITION2);
        Player {
            id: pid,
            forename: self.forenames.names[g(FORENAME) as usize].clone(),
            surname: self.surnames.names[g(SURNAME) as usize].clone(),
            position: Position::from_code(g(POSITION)),
            position2: (p2 != 0).then(|| Position::from_code(p2)),
            age: g(AGE),
            height_cm: g(HEIGHT),
            nation_id: g(NATION),
            squad_number: g(SQUAD_NUMBER),
            ratings: RATINGS.map(|f| self.field(pid, f)),
        }
    }

    /// A name's index in its pool. When the pool is full (its id field's size), the slots that
    /// no player (and, for surnames, no manager) uses are found once and reused.
    fn name_index(&mut self, surname: bool, s: &str) -> Result<u32> {
        let full = if surname { self.surnames.needs_free_slots(s) } else { self.forenames.needs_free_slots(s) };
        if full {
            let mut used = std::collections::HashSet::new();
            for pid in 0..self.player_count() {
                used.insert(self.field(pid, if surname { SURNAME } else { FORENAME }));
            }
            if surname {
                let (pos, width) = self.layout.mgr_surname;
                for m in &self.managers {
                    used.insert(self.layout.get(m, pos, width));
                }
            }
            let pool = if surname { &mut self.surnames } else { &mut self.forenames };
            // slot 0 is kept (the empty name, a single-name player's forename)
            // only slots under the capacity: a base whose pool is already longer (a mod's own
            // Export.mng) must not hand out a surname id the game shows as "Smith"
            let cap = pool.capacity.min(pool.names.len()) as u32;
            pool.free = Some((1..cap).filter(|i| !used.contains(i)).collect());
        }
        if surname { self.surnames.index_of(s) } else { self.forenames.index_of(s) }
    }

    pub fn set_name(&mut self, pid: usize, forename: &str, surname: &str) -> Result<()> {
        let fi = self.name_index(false, forename)?;
        let si = self.name_index(true, surname)?;
        self.set_field(pid, FORENAME, fi)?;
        self.set_field(pid, SURNAME, si)
    }

    // ---- clubs ---------------------------------------------------------
    pub fn club_count(&self) -> usize {
        self.clubs.recs.len()
    }

    /// Historic/special clubs (Wanderers, World Cup Team, ...) have an all-zero squad list.
    pub fn is_placeholder(&self, ci: usize) -> bool {
        self.clubs.recs[ci][SQUAD_OFF..SQUAD_OFF + 2 * SQUAD_MAX].iter().all(|&b| b == 0)
    }

    pub fn squad(&self, ci: usize) -> Vec<usize> {
        let r = &self.clubs.recs[ci];
        (0..SQUAD_MAX)
            .map(|k| self.layout.e.u16(r, SQUAD_OFF + 2 * k))
            .filter(|&id| id != 0xFFFF)
            .map(usize::from)
            .collect()
    }

    pub fn set_squad(&mut self, ci: usize, ids: &[usize]) -> Result<()> {
        if ids.len() > SQUAD_MAX {
            return Err(Error::Value(format!("club {ci}: {} players, max {SQUAD_MAX}", ids.len())));
        }
        let e = self.layout.e;
        let r = &mut self.clubs.recs[ci];
        for k in 0..SQUAD_MAX {
            let v = ids.get(k).map_or(0xFFFF, |&id| id as u16);
            r[SQUAD_OFF + 2 * k..SQUAD_OFF + 2 * k + 2].copy_from_slice(&e.u16_bytes(v));
        }
        Ok(())
    }

    /// Club that owns each player (first listing wins), skipping placeholder clubs.
    pub fn owners(&self) -> Vec<Option<usize>> {
        let mut own = vec![None; self.player_count()];
        for ci in (0..self.club_count()).filter(|&c| !self.is_placeholder(c)) {
            for pid in self.squad(ci) {
                own[pid].get_or_insert(ci);
            }
        }
        own
    }

    /// Starting-funds tier, 0-15 (verified: 15 = £30m, 7 = £2.25m): the record's first 4 bits
    /// (the PS2's: its low 4, #3).
    pub fn club_wealth(&self, ci: usize) -> u8 {
        self.layout.get(&self.clubs.recs[ci], 0, 4) as u8
    }

    pub fn set_club_wealth(&mut self, ci: usize, tier: u8) -> Result<()> {
        if tier > 15 {
            return Err(Error::Value(format!("wealth {tier} must be 0-15")));
        }
        let layout = self.layout;
        layout.set(&mut self.clubs.recs[ci], 0, 4, tier as u32);
        Ok(())
    }

    /// Country of the club's league (byte 3 of the record): 0 England, 1 Scotland, 2 Italy,
    /// 3 Spain, 4 Germany, 5 France, 6 Netherlands, 7 Portugal, 15 none (the rest of the world
    /// and the non-league pool).
    pub fn club_country(&self, ci: usize) -> u8 {
        self.clubs.recs[ci][3]
    }

    pub fn set_club_country(&mut self, ci: usize, country: u8) -> Result<()> {
        if country > 15 {
            return Err(Error::Value(format!("country {country} must be 0-15")));
        }
        self.clubs.recs[ci][3] = country;
        Ok(())
    }

    /// Division within the country (byte 7 >> 2; the low two bits are kept): England 0
    /// Premiership, 19 Championship, 20 League One, 21 League Two, 11 Conference; Scotland 0 SPL,
    /// 1-3 Divisions 1-3; Italy 4 Serie A, 5 Serie B; Spain 6 Primera, 7 Segunda; Germany 9
    /// Bundesliga, 10 2. Bundesliga, 17-18 Regionalliga; France 1 Ligue 1, 2 Ligue 2, 8 National;
    /// Netherlands 13 Eredivisie, 14 Eerste Divisie; Portugal 15, 16; 12 = the non-league pool.
    pub fn club_division(&self, ci: usize) -> u8 {
        self.clubs.recs[ci][7] >> 2
    }

    pub fn set_club_division(&mut self, ci: usize, division: u8) -> Result<()> {
        if division > 63 {
            return Err(Error::Value(format!("division {division} must be 0-63")));
        }
        let b = &mut self.clubs.recs[ci][7];
        *b = (division << 2) | (*b & 3);
        Ok(())
    }

    pub fn club_name(&self, ci: usize) -> &str {
        &self.club_names[0][ci]
    }

    /// Moves whole clubs between ids (#31 item 7). The game's leagues are id ranges (England:
    /// 0-19 the Premiership, 20-43, 44-67, 68-91, 92-115; the Mac's Blackburn/Sunderland swap,
    /// Agent sync 6096955231), so a club changes league by changing id. Each `(to, from)` gives
    /// id `to` the club that was at `from`: its 84-byte record (squad, wealth, country/division
    /// byte, kits; its stadium and own-id fields then name `to`, `renumber_club`), its name and
    /// short name in every language, its stadium (record and name) and its manager: the game
    /// pairs both with clubs by id (the Mac's check on #389: with only the record moved,
    /// Sunderland at id 2 played at Ewood Park, and Blackburn at 41 had the Stadium of Light and
    /// Quinn). The `to` ids must be the `from` ids rearranged.
    pub fn move_clubs(&mut self, moves: &[(usize, usize)]) -> Result<()> {
        let n = self.club_count();
        let (mut to, mut from): (Vec<usize>, Vec<usize>) = moves.iter().copied().unzip();
        to.sort();
        from.sort();
        if let Some(bad) = to.iter().chain(&from).find(|&&c| c >= n) {
            return Err(Error::Value(format!("club id {bad} out of range (0..{})", n - 1)));
        }
        if to.windows(2).any(|w| w[0] == w[1]) || from.windows(2).any(|w| w[0] == w[1]) || to != from {
            return Err(Error::Value("record_of must move clubs among the ids it names, each club once (a permutation)".into()));
        }
        // a stadium moves only between clubs that both have one (the historic sides past
        // stadium_count() have none, and are never moved)
        if let Some(&(t, f)) = moves.iter().find(|&&(t, f)| (t < self.stadium_count()) != (f < self.stadium_count())) {
            return Err(Error::Value(format!("record_of {t} <- {f}: only one of the two clubs has a stadium record")));
        }
        let (recs, names, shorts) = (self.clubs.recs.clone(), self.club_names.clone(), self.short_names.clone());
        let (stadiums, stadium_names, managers) = (self.stadiums.clone(), self.stadium_names.clone(), self.managers.clone());
        for &(t, f) in moves {
            if t < managers.len() && f < managers.len() {
                self.managers[t] = managers[f].clone();
            }
            self.clubs.recs[t] = recs[f].clone();
            for l in 0..LANGS.len() {
                self.club_names[l][t] = names[l][f].clone();
                self.short_names[l][t] = shorts[l][f].clone();
            }
            if f < stadiums.len() {
                self.stadiums[t] = stadiums[f].clone();
            }
            self.renumber_club(t, f);
            if f < stadium_names.len() && t < stadium_names.len() {
                self.stadium_names[t] = stadium_names[f].clone();
            }
        }
        Ok(())
    }

    /// The ids a club record holds (360), all its own row in vanilla (1,136 real clubs): the word at
    /// +4 has its stadium in bits 21-31 (`0x820e4620` reads it) and its own id in bits 8-20, and
    /// bytes 10-11 repeat the own id.
    pub fn club_ids(&self, ci: usize) -> Option<(u32, u32, u16)> {
        let (e, r) = (self.layout.e, self.clubs.recs.get(ci)?);
        let w = e.u32(r, 4);
        (e == Endian::Big).then(|| (w >> 21, (w >> 8) & 0x1FFF, e.u16(r, 10)))
    }

    /// The name slot a stadium record points at (360): bits 1-11 of the word at +8 (`0x8212e070`
    /// reads it), its own row in vanilla (all 1,141).
    pub fn stadium_name_slot(&self, si: usize) -> Option<u32> {
        let (e, r) = (self.layout.e, self.stadiums.get(si)?);
        (e == Endian::Big).then(|| (e.u32(r, 8) >> 1) & 0x7FF)
    }

    /// A club moved from row `f` to `t` (`move_clubs`) points at `t` wherever its records named `f`:
    /// the club's stadium and own id, and its stadium record's name slot. Copied as they were, a
    /// venue went club -> stadium `f` (now another club's) -> name slot of that club's old row (#618:
    /// fc27final23's Forest at home at York's ground, Palace at Ipswich's).
    fn renumber_club(&mut self, t: usize, f: usize) {
        let e = self.layout.e;
        if e != Endian::Big || t > 0x7FF || f > 0x7FF {
            return;
        }
        let (t32, f32) = (t as u32, f as u32);
        let r = &mut self.clubs.recs[t];
        let mut w = e.u32(r, 4);
        if w >> 21 == f32 {
            w = (w & 0x001F_FFFF) | (t32 << 21);
        }
        if (w >> 8) & 0x1FFF == f32 {
            w = (w & !(0x1FFF << 8)) | (t32 << 8);
        }
        r[4..8].copy_from_slice(&e.u32_bytes(w));
        if e.u16(r, 10) == f as u16 {
            r[10..12].copy_from_slice(&e.u16_bytes(t as u16));
        }
        if let Some(r) = self.stadiums.get_mut(t) {
            let w = e.u32(r, 8);
            if (w >> 1) & 0x7FF == f32 {
                r[8..12].copy_from_slice(&e.u32_bytes((w & !(0x7FF << 1)) | (t32 << 1)));
            }
        }
    }

    // ---- stadiums / managers ------------------------------------------
    /// Clubs 0..stadium_count() have a stadium record.
    pub fn stadium_count(&self) -> usize {
        self.stadiums.len()
    }

    pub fn stands(&self, ci: usize) -> Option<[u16; 4]> {
        let r = self.stadiums.get(ci)?;
        Some(std::array::from_fn(|k| self.layout.e.u16(r, STANDS_OFF + 2 * k)))
    }

    pub fn set_stands(&mut self, ci: usize, caps: [u16; 4]) {
        let e = self.layout.e;
        let r = &mut self.stadiums[ci];
        for (k, c) in caps.iter().enumerate() {
            r[STANDS_OFF + 2 * k..STANDS_OFF + 2 * k + 2].copy_from_slice(&e.u16_bytes(*c));
        }
    }

    /// Manager records: 0..club_count() are the clubs' own (record i = club i's manager), the
    /// rest are unattached, the managers out of work the game hires from (135 in the 2006 game).
    pub fn manager_count(&self) -> usize {
        self.managers.len()
    }

    pub fn manager_surname(&self, i: usize) -> &str {
        let (p, w) = self.layout.mgr_surname;
        &self.surnames.names[self.layout.get(&self.managers[i], p, w) as usize]
    }

    pub fn set_manager_surname(&mut self, i: usize, name: &str) -> Result<()> {
        let si = self.name_index(true, name)?;
        let layout = self.layout;
        let (p, w) = layout.mgr_surname;
        layout.set(&mut self.managers[i], p, w, si);
        Ok(())
    }

    /// A manager's reputation, 0-99 (the stars the game shows: half a star per 10; vanilla Wenger,
    /// Mourinho and Benitez 99, Ferguson 90, Redknapp 70, League Two around 30). The 360's record
    /// only: None on the PS2's, whose layout isn't known.
    pub fn manager_reputation(&self, i: usize) -> Option<u32> {
        (self.layout.e == Endian::Big).then(|| bits::get(&self.managers[i], MGR_REPUTATION.0, MGR_REPUTATION.1))
    }

    pub fn set_manager_reputation(&mut self, i: usize, v: u32) -> Result<()> {
        if self.layout.e != Endian::Big {
            return Err(Error::Value("manager reputation: the PS2's manager record layout isn't known".into()));
        }
        if v > 99 {
            return Err(Error::Value(format!("manager reputation {v} must be 0-99")));
        }
        bits::set(&mut self.managers[i], MGR_REPUTATION.0, MGR_REPUTATION.1, v);
        Ok(())
    }

    // ---- loans (World.bin) ---------------------------------------------
    /// The players out on loan at the start (#31, the maintainer's fc27s play-test: Havertz and Madueke
    /// showed "L", greyed, on Gilbert's and Muamba's 2006 records). World.bin ends with a table
    /// of `LOAN_SLOTS` eight-byte slots (and 32 bytes of something else): (player id, u16, u16, 0) sorted by player id, then free
    /// slots (0xFFFF, 0, 0, 0); vanilla has 281 loans (Glen Johnson, Cisse, Kirkland, Howard,
    /// Pique, ...). The 360's only: None on the PS2, without World.bin, or when its end is no
    /// loan table in either known place (a database laid out otherwise is left alone).
    pub fn loans(&self) -> Result<Option<Vec<usize>>> {
        Ok(self.loan_table().map(|(_, t)| t.into_iter().map(|s| s.0).collect()))
    }

    /// Drop the loans of these players (records now holding other people); returns how many.
    pub fn clear_loans(&mut self, players: &std::collections::HashSet<usize>) -> Result<usize> {
        let Some((at, table)) = self.loan_table() else { return Ok(0) };
        let kept: Vec<(usize, [u8; 8])> = table.iter().filter(|s| !players.contains(&s.0)).copied().collect();
        let cleared = table.len() - kept.len();
        if cleared > 0 {
            let mut world = self.ar.get("World.bin").unwrap().to_vec();
            for k in 0..LOAN_SLOTS {
                let slot = kept.get(k).map_or(LOAN_FREE, |s| s.1);
                world[at + 8 * k..at + 8 * k + 8].copy_from_slice(&slot);
            }
            self.ar.set("World.bin", world);
        }
        Ok(cleared)
    }

    /// The loan table's offset and its occupied slots as (player id, raw slot), at the first of
    /// `LOAN_TAILS` where every slot checks: ids ascending and in range, the free slots all at
    /// the end, a free slot somewhere (700 loans would be no table at all).
    fn loan_table(&self) -> Option<(usize, Vec<(usize, [u8; 8])>)> {
        let world = self.ar.get("World.bin").filter(|_| self.layout.e == Endian::Big)?;
        LOAN_TAILS.iter().find_map(|&tail| {
            let at = world.len().checked_sub(tail + LOAN_SLOTS * 8)?;
            let mut out: Vec<(usize, [u8; 8])> = Vec::new();
            let mut free = false;
            for k in 0..LOAN_SLOTS {
                let slot: [u8; 8] = world[at + 8 * k..at + 8 * k + 8].try_into().unwrap();
                if slot == LOAN_FREE {
                    free = true;
                    continue;
                }
                let pid = u16::from_be_bytes([slot[0], slot[1]]) as usize;
                if free || slot[6..] != [0, 0] || pid >= self.player_count() || out.last().is_some_and(|l| l.0 >= pid) {
                    return None;
                }
                out.push((pid, slot));
            }
            free.then_some((at, out))
        })
    }

    // ---- league histories (World.bin) ------------------------------------
    /// A league's champions, newest season first (`HISTORY_AT`): the Records > League Champions
    /// screen shows row `i` as the season ending in base year - `i` (2005/06 first on the disc:
    /// Chelsea 29-4-5 72-22 91). An empty row is a season without one (the war years). None on
    /// the PS2 or without World.bin.
    pub fn league_history(&self, country: usize, tier: usize) -> Option<Vec<HistoryRow>> {
        let at = self.history_at(country, tier)?;
        let w = self.ar.get("World.bin")?;
        Some((0..HISTORY_ROWS).map(|i| HistoryRow::parse(&w[at + 8 * i..at + 8 * i + 8])).collect())
    }

    /// Replace a league's history with `rows` (newest first; the rest of its rows emptied).
    pub fn set_league_history(&mut self, country: usize, tier: usize, rows: &[HistoryRow]) -> Result<()> {
        let at = self.history_at(country, tier).ok_or_else(|| Error::Value(format!("no league history for country {country} tier {tier}")))?;
        if rows.len() > HISTORY_ROWS {
            return Err(Error::Value(format!("country {country} tier {tier}: {} history rows, at most {HISTORY_ROWS}", rows.len())));
        }
        let mut w = self.ar.get("World.bin").unwrap().to_vec();
        for i in 0..HISTORY_ROWS {
            w[at + 8 * i..at + 8 * i + 8].copy_from_slice(&rows.get(i).copied().unwrap_or_default().bytes());
        }
        self.ar.set("World.bin", w);
        Ok(())
    }

    /// A league's last final table (`LAST_TABLE_AT`): its clubs' uids by position. None on the PS2,
    /// without World.bin or for a league `LAST_TABLE_SIZES` doesn't have.
    pub fn last_table(&self, country: usize, tier: usize) -> Option<Vec<u16>> {
        let (at, n) = self.last_table_at(country, tier)?;
        let w = self.ar.get("World.bin")?;
        Some((0..n).map(|k| u16::from_be_bytes([w[at + 2 * k], w[at + 2 * k + 1]])).collect())
    }

    /// Replace a league's last final table with `uids` by position (exactly the league's size).
    pub fn set_last_table(&mut self, country: usize, tier: usize, uids: &[u16]) -> Result<()> {
        let (at, n) = self.last_table_at(country, tier).ok_or_else(|| Error::Value(format!("no last final table for country {country} tier {tier}")))?;
        if uids.len() != n {
            return Err(Error::Value(format!("country {country} tier {tier}: {} clubs in the last final table, the league has {n}", uids.len())));
        }
        let mut w = self.ar.get("World.bin").unwrap().to_vec();
        for (k, u) in uids.iter().enumerate() {
            w[at + 2 * k..at + 2 * k + 2].copy_from_slice(&u.to_be_bytes());
        }
        self.ar.set("World.bin", w);
        Ok(())
    }

    fn last_table_at(&self, country: usize, tier: usize) -> Option<(usize, usize)> {
        let n = *LAST_TABLE_SIZES.get(country)?.get(tier)?;
        let before: usize = LAST_TABLE_SIZES[..country].iter().flat_map(|t| t.iter()).sum::<usize>() + LAST_TABLE_SIZES[country][..tier].iter().sum::<usize>();
        let at = LAST_TABLE_AT + 2 * before;
        let ok = self.layout.e == Endian::Big;
        ok.then_some((at, n)).filter(|&(at, n)| self.ar.get("World.bin").is_some_and(|w| w.len() >= at + 2 * n))
    }

    /// A cup's finals, newest season first (`CUPS`). None on the PS2 or without World.bin.
    pub fn cup_history(&self, cup: usize) -> Option<Vec<CupRow>> {
        let at = self.cup_at(cup)?;
        let w = self.ar.get("World.bin")?;
        Some((0..CUP_ROWS).map(|i| CupRow::parse(&w[at + 8 * i..at + 8 * i + 8])).collect())
    }

    /// Replace a cup's finals with `rows` (newest first; the rest of its rows emptied).
    pub fn set_cup_history(&mut self, cup: usize, rows: &[CupRow]) -> Result<()> {
        let at = self.cup_at(cup).ok_or_else(|| Error::Value(format!("no cup history {cup}")))?;
        if rows.len() > CUP_ROWS {
            return Err(Error::Value(format!("cup {}: {} rows, at most {CUP_ROWS}", CUPS[cup].0, rows.len())));
        }
        let mut w = self.ar.get("World.bin").unwrap().to_vec();
        for i in 0..CUP_ROWS {
            w[at + 8 * i..at + 8 * i + 8].copy_from_slice(&rows.get(i).copied().unwrap_or_default().bytes());
        }
        self.ar.set("World.bin", w);
        Ok(())
    }

    fn cup_at(&self, cup: usize) -> Option<usize> {
        let at = CUPS.get(cup)?.1;
        let ok = self.layout.e == Endian::Big;
        ok.then_some(at).filter(|&at| self.ar.get("World.bin").is_some_and(|w| w.len() >= at + 8 * CUP_ROWS))
    }

    fn history_at(&self, country: usize, tier: usize) -> Option<usize> {
        let at = HISTORY_AT + country * HISTORY_COUNTRY + tier * HISTORY_TIER;
        let ok = self.layout.e == Endian::Big && country < HISTORY_COUNTRIES && tier < HISTORY_TIERS;
        ok.then_some(at).filter(|&at| self.ar.get("World.bin").is_some_and(|w| w.len() >= at + 8 * HISTORY_ROWS))
    }

    /// The club a uid names (ClubRmap.bin: uid -> club record index, u16 each), or None for an
    /// uid of a club the database no longer has (history rows name some).
    pub fn club_of_uid(&self, uid: u16) -> Option<usize> {
        let m = self.ar.get("ClubRmap.bin")?;
        let i = u16::from_be_bytes(m.get(2 * uid as usize..2 * uid as usize + 2)?.try_into().ok()?) as usize;
        (i < self.club_count()).then_some(i)
    }

    /// A club's uid (ClubRmap's inverse). Every club has one uid but club 0, which every unused
    /// uid also maps to: its own is the one the league histories and cup finals use for it (Arsenal's
    /// 5800).
    pub fn uid_of_club(&self, club: usize) -> Option<u16> {
        let m = self.ar.get("ClubRmap.bin")?;
        let all: Vec<u16> = (0..m.len() / 2).filter(|&u| u16::from_be_bytes([m[2 * u], m[2 * u + 1]]) as usize == club).map(|u| u as u16).collect();
        match all.as_slice() {
            [one] => Some(*one),
            [] => None,
            many => {
                let mut seen: HashMap<u16, usize> = HashMap::new();
                for c in 0..HISTORY_COUNTRIES {
                    for t in 0..HISTORY_TIERS {
                        for r in self.league_history(c, t).unwrap_or_default() {
                            if r.uid != 0 && many.contains(&r.uid) {
                                *seen.entry(r.uid).or_default() += 1;
                            }
                        }
                    }
                }
                // and the cup finals
                for c in 0..CUPS.len() {
                    for r in self.cup_history(c).unwrap_or_default() {
                        for uid in [r.winner, r.runner_up] {
                            if uid != 0 && many.contains(&uid) {
                                *seen.entry(uid).or_default() += 1;
                            }
                        }
                    }
                }
                seen.into_iter().max_by_key(|&(u, n)| (n, std::cmp::Reverse(u))).map(|(u, _)| u)
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn put_file(&mut self, name: &str, data: Vec<u8>) {
        self.ar.set(name, data);
    }

    // ---- write ---------------------------------------------------------
    pub fn build(&mut self) -> Result<Vec<u8>> {
        let mut files: Vec<(String, Vec<u8>)> = vec![
            ("Forename.bin".into(), self.forenames.build()),
            ("Surnames.bin".into(), self.surnames.build()),
            ("Players.bin".into(), self.players.build()),
            ("Clubs.bin".into(), self.clubs.build()),
            ("StadName.bin".into(), self.pack_names(&self.stadium_names, STADNAME_LEN)?),
            ("Stadiums.bin".into(), self.stadiums.concat()),
            ("Managers.bin".into(), self.managers.concat()),
        ];
        for (li, l) in LANGS.iter().enumerate() {
            files.push((format!("ClubNam{l}.bin"), self.pack_names(&self.club_names[li], CLUBNAME_LEN)?));
            files.push((format!("ShortNm{l}.bin"), pack_fixed(&self.short_names[li], SHORTNAME_LEN)?));
        }
        for (n, d) in files {
            self.ar.set(&n, d);
        }
        Ok(self.ar.build())
    }

    /// Club or stadium names as the platform stores them: fixed `size`-byte slots on the 360,
    /// packed on the PS2 (with the 360's length limit).
    fn pack_names(&self, names: &[String], size: usize) -> Result<Vec<u8>> {
        if self.layout.packed_names { pack_packed(names, size - 1) } else { pack_fixed(names, size) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name pool as the game stores it: u32 offsets, then NUL-terminated strings.
    fn pool(names: &[&str]) -> Vec<u8> {
        let mut offs = Vec::new();
        let mut strs = Vec::new();
        for s in names {
            offs.extend_from_slice(&((4 * names.len() + strs.len()) as u32).to_be_bytes());
            strs.extend_from_slice(s.as_bytes());
            strs.push(0);
        }
        offs.extend(strs);
        offs
    }

    /// A moved club's record names its new row as its stadium and own id, and its stadium record
    /// the new name slot (#618); their other bits stay.
    #[test]
    fn moved_clubs_point_at_their_new_rows() {
        let mut db = crate::fixtures::fake_db();
        for ci in 0..2usize {
            let w = (ci as u32) << 21 | (ci as u32) << 8 | 0xd0;
            db.clubs.recs[ci][4..8].copy_from_slice(&w.to_be_bytes());
            db.clubs.recs[ci][10..12].copy_from_slice(&(ci as u16).to_be_bytes());
            db.clubs.recs[ci][20] = 0x70 + ci as u8;
            db.stadiums[ci][8..12].copy_from_slice(&((ci as u32) << 1 | 0x8000_0001).to_be_bytes());
        }
        assert_eq!((db.club_ids(1), db.stadium_name_slot(1)), (Some((1, 1, 1)), Some(1)));
        db.move_clubs(&[(0, 1), (1, 0)]).unwrap();
        assert_eq!((db.club_ids(0), db.club_ids(1)), (Some((0, 0, 0)), Some((1, 1, 1))), "stadium and own id follow the move");
        assert_eq!((db.stadium_name_slot(0), db.stadium_name_slot(1)), (Some(0), Some(1)), "and the stadium's name slot");
        assert_eq!((db.clubs.recs[0][7], db.clubs.recs[0][20], db.clubs.recs[1][20]), (0xd0, 0x71, 0x70), "the rest of each record moved as it was");
        assert_eq!(db.stadiums[0][8] & 0x80, 0x80, "and the stadium record's other bits");
    }

    #[test]
    fn name_pool_keeps_indices_and_appends() {
        let bytes = pool(&["Thierry", "Robert"]);
        let mut p = NamePool::parse(&bytes, Endian::Big);
        assert_eq!(p.names, vec!["Thierry", "Robert"]);
        assert_eq!(p.index_of("Robert").unwrap(), 1, "an existing name keeps its index");
        assert!(!p.grew());
        assert_eq!(p.build(), bytes, "unchanged: byte-identical");
        assert_eq!(p.index_of("Cesc").unwrap(), 2, "a new name is appended");
        assert!(p.grew());
        let back = NamePool::parse(&p.build(), Endian::Big);
        assert_eq!(back.names, vec!["Thierry", "Robert", "Cesc"]);
    }

    /// A pool capped below its id field (`SURNAMES_SHOWN`, #455): a new name takes a free slot under
    /// the cap and never an id the game would show as "Smith"; with no free slot left it is an error,
    /// not an append past the cap.
    #[test]
    fn capped_pool_reuses_slots_under_the_cap() {
        let bytes = pool(&["", "Smith", "Pershin", "Unused"]);
        let mut p = NamePool { capacity: 3, ..NamePool::parse(&bytes, Endian::Big) };
        assert!(p.needs_free_slots("Pedri"), "full: 4 names, cap 3, no free set yet");
        assert!(!p.needs_free_slots("Pershin"), "an existing name needs no slot");
        let cap = p.capacity.min(p.names.len()) as u32;
        p.free = Some((1..cap).filter(|&i| i != 1 && i != 2).collect()); // slot 3 ("Unused") is past the cap
        assert!(p.index_of("Pedri").is_err(), "no slot under the cap: an error, never id 3 or 4");
        p.free = Some([2].into_iter().collect());
        assert_eq!(p.index_of("Pedri").unwrap(), 2, "the free slot under the cap");
        assert_eq!(p.names[2], "Pedri");
        assert!(p.lookup.get("Pershin").is_none(), "the replaced name is gone from the lookup");
        assert_eq!(p.index_of("Pedri").unwrap(), 2, "and found again without a slot");
        assert!(p.index_of("Yamal").is_err(), "the pool is full again");
    }

    #[test]
    fn fixed_width_strings() {
        let names = vec!["Arsenal".to_string(), "Reading".to_string()];
        let packed = pack_fixed(&names, 16).unwrap();
        assert_eq!(packed.len(), 32);
        assert_eq!(fixed_strings(&packed, 16), names);
        assert!(pack_fixed(&["Wolverhampton Wanderers".to_string()], 16).is_err(), "too long for the slot");
    }

    #[test]
    fn tables_round_trip() {
        let mut bytes = Vec::new();
        for v in [TABLE_MAGICS[0], 4, 2] {
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let t = Table::parse(&bytes, Endian::Big).unwrap();
        assert_eq!(t.recs, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        assert_eq!(t.build(), bytes);
        bytes[0] ^= 0xFF;
        assert!(Table::parse(&bytes, Endian::Big).is_err(), "unknown magic");
    }

    /// The PS2's tables: their own magic, little-endian header (#3).
    #[test]
    fn little_endian_tables_round_trip() {
        let mut bytes = Vec::new();
        for v in [TABLE_MAGICS[2], 4, 2] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let t = Table::parse(&bytes, Endian::Little).unwrap();
        assert_eq!(t.recs, vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        assert_eq!(t.build(), bytes);
        assert!(Table::parse(&bytes, Endian::Big).is_err(), "read big-endian it's no table");
    }

    /// The PS2's club and stadium names: NUL-terminated strings back to back (#3).
    #[test]
    fn packed_strings_round_trip() {
        let bytes = b"Arsenal\0Aston Villa\0Tom\x9a\0".to_vec();
        let names = packed_strings(&bytes);
        assert_eq!(names, vec!["Arsenal", "Aston Villa", "Tom\u{161}"]);
        assert_eq!(pack_packed(&names, 25).unwrap(), bytes);
        assert!(pack_packed(&["Wolverhampton Wanderers".to_string()], 16).is_err(), "longer than the 360's slot");
        assert!(packed_strings(&[]).is_empty());
    }

    #[test]
    fn positions() {
        assert_eq!(Position::from_code(0).label(), "GK");
        assert_eq!(Position::from_code(3).label(), "ATT");
        assert_eq!(Position::from_code(5), Position::Def, "two bits");
    }
}
