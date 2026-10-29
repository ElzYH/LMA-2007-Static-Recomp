//! The game's numbers a mod may change by name (#870, the maintainer 28 Oct: "the whole game should be easy to
//! mod"). Each entry is one instruction whose immediate the recompiler reads from `TUNE` instead of
//! baking it in; mod.toml's `[tune]` section sets them by name (`runtime::mods`), and the interpreter
//! sees the same value through the rewritten word. Adding a number is one line in `TUNABLES`, then
//! regenerate the parts. With nothing set every slot holds the game's own value, so vanilla runs and
//! their fingerprints are unchanged.
//!
//! Supported instructions: `mulli` (7), `subfic` (8), `cmplwi` (10), `cmpwi` (11), `addi`/`li` (14), `ori` (24).
//! A `Table` entry is a run of the game's data words (a table the code indexes, e.g. the wage
//! demand's percentages by club tier); a mod sets it as an array, written over the words at load.
//! A `Bytes` entry is the same for a table of unsigned bytes.
//! Before adding a data entry, run `tools/tune_audit.py` (the Ghidra export): every Table, Bytes and F32
//! entry should have one reader, or its size runs into another table / its literal is shared.

use std::sync::atomic::{AtomicI32, Ordering::Relaxed};

/// One moddable number: `name` in mod.toml's `[tune]`, where it is (`kind`, `site`), the range a mod
/// may set and what it does.
pub struct Tunable {
    pub name: &'static str,
    pub kind: Kind,
    pub site: u32,
    /// More instructions that carry the same number, as (site, vanilla word): one rule the game
    /// checks in several places (`Imm` only).
    pub also: &'static [(u32, u32)],
    pub min: i32,
    pub max: i32,
    pub what: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The immediate of the instruction at `site`, whose vanilla word is this (checked before a mod
    /// rewrites it; the recompiler loads the value from `TUNE`).
    Imm(u32),
    /// This many signed 32-bit data words from `site` (the code reads them from memory, so a mod's
    /// values are simply written there).
    Table(u32),
    /// This many unsigned data bytes from `site`, like `Table`.
    Bytes(u32),
    /// A 32-bit float in the data at `site`, read by this entry's code only (check the readers before
    /// adding one: a literal the compiler shares, like 100.0, would change every reader). `min`/`max`
    /// are in hundredths.
    F32,
}

impl Tunable {
    /// The vanilla instruction word of an `Imm` entry (0 for a table).
    pub const fn word(&self) -> u32 {
        match self.kind {
            Kind::Imm(w) => w,
            Kind::Table(_) | Kind::Bytes(_) | Kind::F32 => 0,
        }
    }
}

pub const TUNABLES: &[Tunable] = &[
    // personal terms 0x82187de0: a player aged 25-30 (contract kind below 0xc0, 0x8211d688 0,
    // 0x8211c540 above 0) adds this % to his demand and refuses (6) an offer below it; at modern
    // wages 35% is GBP 100k a week on a 300k star (Isak asked ~405k, #867)
    Tunable { name: "prime_wage_premium", kind: Kind::Imm(0x1D7F_0023), site: 0x8218_83d8, also: &[], min: 0, max: 100, what: "the % a player aged 25-30 adds to his wage demand (vanilla 35)" },
    // the premium's age window: `cmpwi age,24; ble` skips it, then `cmpwi age,31; bge` (ages 25-30)
    Tunable { name: "prime_wage_age_after", kind: Kind::Imm(0x2F03_0018), site: 0x8218_8390, also: &[], min: 0, max: 60, what: "players older than this pay the prime wage premium (vanilla 24)" },
    Tunable { name: "prime_wage_age_before", kind: Kind::Imm(0x2F03_001F), site: 0x8218_83a0, also: &[], min: 0, max: 60, what: "players younger than this pay the prime wage premium (vanilla 31)" },
    // `offer + 100 < demand` refuses (8, or 9 when the selling club's own check fails) before any
    // age premium; the premium's own check (6) has no margin
    Tunable { name: "wage_refuse_margin", kind: Kind::Imm(0x396B_0064), site: 0x8218_8354, also: &[], min: 0, max: 32767, what: "a player refuses when his demand is more than this above the offered wage (vanilla 100)" },
    // personal terms: his demand + this % of his current wage, indexed by the buying club's tier minus
    // his club's tier (clubs.csv wealth), -15..14 (30 words, index 0 at 0x8200b244; the status table follows,
    // so a difference of 15 reads its first word): a weaker buyer pays more, a bigger one gets him cheaper
    Tunable { name: "demand_tier_pct", kind: Kind::Table(30), site: 0x8200_b208, also: &[], min: -100, max: 200, what: "the % on his wage demand by buyer tier minus his club's tier, -15..14" },
    // then + this % of that, by the buying club's league status minus his club's, -6..5 (12 words,
    // index 0 at 0x8200b298)
    // the sale check 0x821889c0 (answer 2, and 8 counting the players already leaving): a club
    // won't sell while it has this many players or fewer
    Tunable { name: "sale_min_squad", kind: Kind::Imm(0x2F03_0010), site: 0x8218_8a64, also: &[(0x8218_8ae4, 0x2F0B_0010)], min: 0, max: 31, what: "a club won't sell while it has this many players or fewer (vanilla 16)" },
    // ... or this many or fewer that pass 0x82120758 (kind 0; by the name of the count, the players
    // available to play)
    Tunable { name: "sale_min_available", kind: Kind::Imm(0x2F03_000B), site: 0x8218_8a7c, also: &[(0x8218_8b00, 0x2F0B_000B)], min: 0, max: 31, what: "a club won't sell while it has this many available players or fewer (vanilla 11)" },
    // the sale check (answer 5): a buyer with this many players or more can't sign; the squad array
    // holds 32, so only lower
    Tunable { name: "squad_max", kind: Kind::Imm(0x2F03_0020), site: 0x8218_8b98, also: &[], min: 1, max: 32, what: "a club with this many players can't sign another (vanilla 32, the squad array's size)" },
    // the AI's transfer search 0x8218c430: with no position to strengthen it fills its squad up to
    // this + its tier / 4
    Tunable { name: "squad_fill_base", kind: Kind::Imm(0x3BEB_0012), site: 0x8218_c5fc, also: &[], min: 0, max: 28, what: "the AI buys until it has this + its tier / 4 players (vanilla 18)" },
    // the AI's bid 0x82189088: base / 100 x a random % in [low, high), the low minus 3 x a 4-bit
    // field of the deal (+0xc bits 11-14); capped at twice the valuation and at least the best open
    // offer. By the player's status (0x8211c4e0): 5, else the first bid, else a raise; status 3's
    // first bid. Keep each low below its high.
    Tunable { name: "bid_pct_top_low", kind: Kind::Imm(0x23CA_00A0), site: 0x8218_914c, also: &[], min: 1, max: 400, what: "the AI's first bid for a status-5 player: the % range's low end (vanilla 160)" },
    Tunable { name: "bid_pct_top_high", kind: Kind::Imm(0x3960_00AA), site: 0x8218_9148, also: &[], min: 1, max: 400, what: "... its high end (vanilla 170)" },
    Tunable { name: "bid_pct_low", kind: Kind::Imm(0x23CA_006E), site: 0x8218_9158, also: &[], min: 1, max: 400, what: "the AI's first bid: the % range's low end (vanilla 110)" },
    Tunable { name: "bid_pct_high", kind: Kind::Imm(0x3960_0078), site: 0x8218_9154, also: &[], min: 1, max: 400, what: "... its high end (vanilla 120)" },
    Tunable { name: "bid_raise_pct_low", kind: Kind::Imm(0x3BC0_0069), site: 0x8218_9160, also: &[], min: 1, max: 400, what: "the AI's raised bid: the % range's low end (vanilla 105)" },
    Tunable { name: "bid_raise_pct_high", kind: Kind::Imm(0x3960_0073), site: 0x8218_9164, also: &[], min: 1, max: 400, what: "... its high end (vanilla 115)" },
    Tunable { name: "bid_pct_status3_low", kind: Kind::Imm(0x23CB_0050), site: 0x8218_9108, also: &[], min: 1, max: 400, what: "the AI's first bid for a status-3 player: the % range's low end (vanilla 80)" },
    Tunable { name: "bid_pct_status3_high", kind: Kind::Imm(0x23FE_0064), site: 0x8218_910c, also: &[], min: 1, max: 400, what: "... its high end (vanilla 100)" },
    // the asking price 0x82188f38: for a player of status 5 the club asks its own price or this x
    // the valuation, whichever is more (the float at 0x8200b2c8 has its three readers here only)
    Tunable { name: "asking_price_top_factor", kind: Kind::F32, site: 0x8200_b2c8, also: &[], min: 50, max: 1000, what: "a status-5 player's asking price: at least this x his valuation (vanilla 1.8)" },
    // a deal's worth to the seller 0x8217c410 (fee + instalments + an exchange player's value; the
    // sell-on's -0.25 at 0x8200ae90 and the 5.0 at 0x82005364 are shared literals, so not here):
    // instalments count at -(this) of their total (the game stores the factor negated)
    Tunable { name: "deal_instalments_factor", kind: Kind::F32, site: 0x8200_ae94, also: &[], min: -200, max: 0, what: "a deal's instalments count at minus this of their total (vanilla -0.9: 90%)" },
    // an exchange player counts at this x his valuation, by his status (0x8211c4e0, jump table at
    // 0x8217c584): 5, 1 or 3, 2/4/6, anything else
    Tunable { name: "exchange_value_status5", kind: Kind::F32, site: 0x8200_ae84, also: &[], min: 0, max: 500, what: "an exchange player of status 5 counts at this x his valuation (vanilla 1.5)" },
    Tunable { name: "exchange_value_status1_3", kind: Kind::F32, site: 0x8200_ae80, also: &[], min: 0, max: 500, what: "an exchange player of status 1 or 3: this x his valuation (vanilla 0.8)" },
    Tunable { name: "exchange_value_status2_4_6", kind: Kind::F32, site: 0x8200_ae7c, also: &[], min: 0, max: 500, what: "an exchange player of status 2, 4 or 6: this x his valuation (vanilla 0)" },
    Tunable { name: "exchange_value_other", kind: Kind::F32, site: 0x8200_ae88, also: &[], min: 0, max: 500, what: "an exchange player of any other status: this x his valuation (vanilla 0.75)" },
    // the board's funds figure 0x820eaa10 (FUNDS: a club is in trouble at -figure/2): the club's
    // wealth tier (clubs.csv) picks a word of this table, x 10, then x its nation's byte (0x820033ac;
    // England 150), or the two hard-coded cases below
    Tunable { name: "funds_tier", kind: Kind::Table(16), site: 0x8270_3118, also: &[], min: 0, max: 1_000_000, what: "the board's funds figure by wealth tier 0-15 (x 10 x the nation's byte)" },
    Tunable { name: "funds_nation_factor", kind: Kind::Bytes(52), site: 0x8200_33ac, also: &[], min: 0, max: 255, what: "the board's funds figure: the factor by the club's nation group 0-51 (51 when unknown), x the tier word x 10" },
    Tunable { name: "funds_chelsea_factor", kind: Kind::Imm(0x1C7E_00C8), site: 0x820e_ab38, also: &[], min: 0, max: 30_000, what: "the club with uid 0x1716 (Chelsea) uses this instead of its nation's byte (vanilla 200)" },
    Tunable { name: "funds_cup_club_factor", kind: Kind::Imm(0x1C7E_0064), site: 0x820e_ab18, also: &[], min: 0, max: 30_000, what: "a club of country type 0xe in competition 0xb/0xc uses this instead of its nation's byte (vanilla 100)" },
    // the board's verdict 0x822bd6b8: the club's level on its division's 5-level expectation ladder
    // (0x82702a2c; 0x820eb4e8, more is worse) against the board's target level (0x820eb5e8)
    Tunable { name: "board_verdict_levels_short", kind: Kind::Imm(0x3963_0002), site: 0x822b_d71c, also: &[], min: 0, max: 5, what: "the board's worst verdict (0x13) needs the club this many levels short of its target and in debt past half its funds figure (vanilla 2)" },
    // league prize money 0x820ec3d8 (PrizeMoney_LeaguePrizeMoneyAllocate): a word per final league
    // position (row) and division (column), one table per territory (the club's 0x820e5730); territories
    // 2 and 3 get none
    Tunable { name: "league_prize_territory0", kind: Kind::Table(120), site: 0x8200_4288, also: &[], min: 0, max: 100_000_000, what: "league prize money, territory 0: 24 positions x 5 divisions, position-major" },
    Tunable { name: "league_prize_territory1", kind: Kind::Table(48), site: 0x8200_4468, also: &[], min: 0, max: 100_000_000, what: "league prize money, territory 1: 12 positions x 4 divisions" },
    Tunable { name: "league_prize_territory4", kind: Kind::Table(80), site: 0x8200_4668, also: &[], min: 0, max: 100_000_000, what: "league prize money, territory 4: 20 positions x 4 divisions" },
    Tunable { name: "league_prize_territory5", kind: Kind::Table(40), site: 0x8200_4528, also: &[], min: 0, max: 100_000_000, what: "league prize money, territory 5: 20 positions x 2 divisions" },
    Tunable { name: "league_prize_territory6", kind: Kind::Table(40), site: 0x8200_45c8, also: &[], min: 0, max: 100_000_000, what: "league prize money, territory 6: 20 positions x 2 divisions" },
    // cup prize money 0x820ec548 / 0x820ecae0 / 0x820ec798: per competition id (0x22-0x39) a table
    // of rows [round, win, loss, draw, paid to both clubs], ended by a round 0 (not part of the
    // entry); keep each row's first number, the round it pays for
    Tunable { name: "cup_prize_22", kind: Kind::Table(70), site: 0x8200_36a0, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x22: 14 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_23", kind: Kind::Table(55), site: 0x8200_37e0, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x23: 11 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_28", kind: Kind::Table(75), site: 0x8200_38d0, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x28: 15 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_29", kind: Kind::Table(75), site: 0x8200_3a10, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x29: 15 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2a", kind: Kind::Table(70), site: 0x8200_3ed8, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2a: 14 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2b", kind: Kind::Table(5), site: 0x8200_4004, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2b: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2c", kind: Kind::Table(35), site: 0x8200_4030, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2c: 7 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2d", kind: Kind::Table(5), site: 0x8200_40d0, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2d: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2e", kind: Kind::Table(55), site: 0x8200_3b50, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2e: 11 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2f", kind: Kind::Table(60), site: 0x8200_3c40, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x2f: 12 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_30", kind: Kind::Table(5), site: 0x8200_3d44, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x30: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_31", kind: Kind::Table(55), site: 0x8200_3d70, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x31: 11 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_32", kind: Kind::Table(25), site: 0x8200_3e60, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x32: 5 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_33", kind: Kind::Table(5), site: 0x8200_4238, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x33: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_34", kind: Kind::Table(5), site: 0x8200_4260, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x34: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_37", kind: Kind::Table(25), site: 0x8200_4198, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x37: 5 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_38", kind: Kind::Table(35), site: 0x8200_40f8, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x38: 7 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_39", kind: Kind::Table(5), site: 0x8200_4210, also: &[], min: 0, max: 100_000_000, what: "cup prize money, competition 0x39: 1 rows of [round, win, loss, draw, both clubs]" },
    Tunable { name: "cup_prize_2f_division_15_fee", kind: Kind::Imm(0x3B80_1388), site: 0x820e_cbf0, also: &[(0x820e_cc04, 0x3BC0_1388)], min: 0, max: 32_767, what: "competition 0x2f, rounds below 0x1b: a club in division 15 gets this as its both-clubs amount (home and away; vanilla 5000)" },
    Tunable { name: "demand_status_pct", kind: Kind::Table(12), site: 0x8200_b280, also: &[], min: -100, max: 200, what: "the % on his wage demand by buyer status minus his club's status, -6..5" },
    // player development 0x82196c70 (each skill, in turn): ages split into 5 bands at 21/26/31/34;
    // a skill grows with chance growth x potential (0x8211a068) / 100, raised by (1 + gap/10) when it is
    // below potential x the band's factor (1.25, 1.1, 1, 0.75, 0.5), and drops with chance decline %;
    // both halved when 0x820ddaf8 is 0; the skill stays in 1..100
    Tunable { name: "development_by_age", kind: Kind::Table(10), site: 0x8200_bbe4, also: &[], min: 0, max: 100, what: "[growth, decline %] per age band (under 21, 21-25, 26-30, 31-33, 34+); vanilla 35,20 30,20 25,20 10,30 5,50" },
    Tunable { name: "development_band1_age", kind: Kind::Imm(0x2F03_0015), site: 0x8219_6c98, also: &[], min: 0, max: 60, what: "the youngest age in development band 1 (vanilla 21)" },
    Tunable { name: "development_band2_age", kind: Kind::Imm(0x2F03_001A), site: 0x8219_6cb4, also: &[], min: 0, max: 60, what: "the youngest age in development band 2 (vanilla 26)" },
    Tunable { name: "development_band3_age", kind: Kind::Imm(0x2F03_001F), site: 0x8219_6ccc, also: &[], min: 0, max: 60, what: "the youngest age in development band 3 (vanilla 31)" },
    Tunable { name: "development_band4_age", kind: Kind::Imm(0x2F03_0022), site: 0x8219_6ce0, also: &[], min: 0, max: 60, what: "the youngest age in development band 4 (vanilla 34)" },
    // after a match (0x822251a8, each of the 16 in a squad), 0x82196a28 moves a player under 24
    // (`cmpwi age,24` at 0x82196a58) towards his potential: a row [from, to, bonus] whose range holds his
    // match rating (slot word 0x83, 30..100 for a player who played, 0 otherwise; the same value
    // Player_SeasonRatingUpdate 0x8211bd48 takes) adds bonus x the gap
    // to potential / 30 to each skill's progress; 100 progress is +1 skill (0x8200bc10, 6 rows)
    Tunable { name: "young_match_growth", kind: Kind::Table(18), site: 0x8200_bc10, also: &[], min: 0, max: 100, what: "rows [from, to, bonus] by match rating 0..100 for players under 24; vanilla bonus 0,10,20,30,40,50 for 0-50..90-100" },
    Tunable { name: "young_match_growth_age", kind: Kind::Imm(0x2F03_0018), site: 0x8219_6a58, also: &[], min: 0, max: 60, what: "players younger than this develop from their matches (vanilla 24)" },
    // the team screen's row labels 0x8220c640: rows 11..15 read "SUB" (string 0x207), from 16 the
    // reserves' (`cmpwi r30,16; bge` at 0x8220c928); the mod key `bench` sets it to 11 + bench (#530)
    Tunable { name: "sub_label_rows", kind: Kind::Imm(0x2F1E_0010), site: 0x8220_c928, also: &[], min: 16, max: 20, what: "team screen rows below this (from row 12) are labelled as substitutes (vanilla 16; bench = 9 sets 20)" },
];

const N: usize = TUNABLES.len();

/// An instruction's immediate as the instruction reads it: signed for `mulli`, `subfic`, `cmpwi` and `addi`,
/// unsigned for `cmplwi` and `ori`.
pub const fn imm(word: u32) -> i32 {
    match word >> 26 {
        10 | 24 => (word & 0xFFFF) as i32,
        _ => (word & 0xFFFF) as i16 as i32,
    }
}

/// The current value of each tunable; the game's own until a mod sets one.
/// (Tables live in guest memory and have no slot: theirs stays 0.)
pub static TUNE: [AtomicI32; N] = {
    let mut a = [const { AtomicI32::new(0) }; N];
    let mut i = 0;
    while i < N {
        a[i] = AtomicI32::new(imm(TUNABLES[i].word()));
        i += 1;
    }
    a
};

/// Tunable `i`'s value (what the recompiled code calls).
#[inline(always)]
pub fn tune(i: usize) -> i64 {
    TUNE[i].load(Relaxed) as i64
}

/// The tunable at `site` whose vanilla word is `word` (the recompiler's lookup).
pub fn at(site: u32, word: u32) -> Option<usize> {
    TUNABLES.iter().position(|t| (t.site == site && t.kind == Kind::Imm(word)) || t.also.contains(&(site, word)))
}

/// An `Imm` entry's instructions: (site, vanilla word), the first one and `also`.
pub fn sites(t: &Tunable) -> Vec<(u32, u32)> {
    std::iter::once((t.site, t.word())).chain(t.also.iter().copied()).collect()
}

/// The tunable called `name`.
pub fn by_name(name: &str) -> Option<usize> {
    TUNABLES.iter().position(|t| t.name == name)
}

/// The word with its immediate set to `v` (`v` within the tunable's range, so it fits).
pub fn with_imm(word: u32, v: i32) -> u32 {
    (word & 0xFFFF_0000) | (v as u32 & 0xFFFF)
}

/// Every slot back to the game's own value (a mod space switch, tests).
pub fn reset() {
    for (i, t) in TUNABLES.iter().enumerate() {
        TUNE[i].store(imm(t.word()), Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tunable_is_a_supported_instruction_with_its_vanilla_value_in_range() {
        for (i, t) in TUNABLES.iter().enumerate() {
            assert_eq!(by_name(t.name), Some(i), "{}: names are unique", t.name);
            assert!(t.min <= t.max && t.site % 4 == 0, "{}", t.name);
            match t.kind {
                Kind::Imm(w) => {
                    for &(_, aw) in t.also {
                        assert_eq!((aw >> 26, imm(aw)), (w >> 26, imm(w)), "{}: every site the same instruction and number", t.name);
                    }
                    assert!(matches!(w >> 26, 7 | 8 | 10 | 11 | 14 | 24), "{}: opcode {} is not supported", t.name, w >> 26);
                    assert!((t.min..=t.max).contains(&imm(w)), "{}: vanilla {} outside {}..={}", t.name, imm(w), t.min, t.max);
                    let (lo, hi) = if matches!(w >> 26, 10 | 24) { (0, 0xFFFF) } else { (-0x8000, 0x7FFF) };
                    assert!(t.min >= lo && t.max <= hi, "{}: the range must fit the immediate", t.name);
                    assert_eq!(tune(i), imm(w) as i64, "{}: starts at the game's value", t.name);
                }
                Kind::Table(n) => assert!(n > 0 && t.also.is_empty(), "{}", t.name),
                Kind::Bytes(n) => assert!(n > 0 && t.also.is_empty() && t.min >= 0 && t.max <= 255, "{}", t.name),
                Kind::F32 => assert!(t.also.is_empty(), "{}", t.name),
            }
            let span = |u: &Tunable| -> Vec<(u32, u32)> {
                match u.kind {
                    Kind::Table(n) => vec![(u.site, u.site + 4 * n)],
                    Kind::Bytes(n) => vec![(u.site, u.site + n)],
                    Kind::F32 => vec![(u.site, u.site + 4)],
                    Kind::Imm(_) => sites(u).into_iter().map(|(a, _)| (a, a + 4)).collect(),
                }
            };
            for (j, u) in TUNABLES.iter().enumerate() {
                for (a0, a1) in span(t) {
                    for (b0, b1) in span(u) {
                        assert!(a1 <= b0 || b1 <= a0 || (i == j && a0 == b0), "{} and {} overlap", t.name, u.name);
                    }
                }
            }
        }
    }

    #[test]
    fn mods_md_lists_every_tunable() {
        // MODS.md's table is `lma-cli tune-list`'s output; rerun it after changing TUNABLES
        let md = include_str!("../../../../MODS.md");
        for t in TUNABLES {
            assert!(md.contains(&format!("| `{}` |", t.name)), "MODS.md: no row for {} (paste `lma-cli tune-list`)", t.name);
        }
    }

    #[test]
    fn immediates_and_rewritten_words() {
        assert_eq!(imm(0x1D7F_0023), 35);
        assert_eq!(imm(0x2C03_FFFF), -1, "cmpwi is signed");
        assert_eq!(imm(0x2803_FFFF), 0xFFFF, "cmplwi is unsigned");
        assert_eq!(with_imm(0x1D7F_0023, 15), 0x1D7F_000F);
        assert_eq!(with_imm(0x3860_0005, -2), 0x3860_FFFE);
    }
}
