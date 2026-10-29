//! The season calendar inside `Export.mng` (#703): `Diary20.mng`, a BG2 archive of 20 seasons
//! (2006/07 .. 2025/26), each a `TWNNNN.bin` and a `diaryNNNN.bin`.
//!
//! `TWNNNN.bin` (108 bytes, 27 big-endian u32): two transfer windows of nine words each,
//! `open_day, open_dom, open_month0, close_day, close_dom, close_month0, 11, 0, flag`, days counted
//! from 1 July of the season (0 = 1 Jul, 61 = 31 Aug, 184 = 1 Jan, 214 = 31 Jan); the disc's 20
//! files are identical.
//!
//! `diaryNNNN.bin`: runs of 6-byte fixture records (`home u16 BE, away u16 BE, ff 00`, 1-based
//! clubs.csv ids, `00 00 00 00 ff 00` a blank slot) behind round headers of 0, 4, 10, 20, 24 or 58
//! bytes. A 4-byte header is `league u16 LE, count u16 LE` (league 1 = ids 0-19, the Premiership,
//! ... 23 = 413-428, the Primeira Liga: the clubs.csv id blocks). A longer header starts with a date
//! code, `season << 10 | day` (little-endian, bit 15 a flag): round 1 of 2006/07, 2007/08 and
//! 2008/09 falls on 14 Sep 2006, 6 Sep 2007 and 16 Oct 2008, all Thursdays. The other header bytes
//! (competition, round and leg codes) are still to be read off against the game's calendar screen.
//! `tools/diary_dump.py` is the reference this was ported from.

use crate::{Archive, Error, Result};

/// Seasons in the disc's diary, and the first one's starting year.
pub const SEASONS: u32 = 20;
pub const FIRST_YEAR: i32 = 2006;

/// A transfer window, in days from 1 July of its season.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    pub open_day: u32,
    pub close_day: u32,
}

/// One run of fixture records and the header before it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    pub header: Vec<u8>,
    /// The league (a 4-byte header's first field), for league rounds.
    pub league: Option<u16>,
    /// The round's date in days from 1 July, for headers that carry one.
    pub day: Option<u16>,
    /// The filled records as clubs.csv ids (0-based), home first.
    pub fixtures: Vec<(u16, u16)>,
    /// Every record slot, blank ones included.
    pub slots: usize,
}

#[derive(Clone, Debug)]
pub struct Season {
    pub index: u32,
    pub windows: [Window; 2],
    pub rounds: Vec<Round>,
}

impl Season {
    /// The season's first calendar year (2006 for index 0).
    pub fn year(&self) -> i32 {
        FIRST_YEAR + self.index as i32
    }

    /// Parses a season from its two files.
    pub fn parse(index: u32, tw: &[u8], diary: &[u8]) -> Result<Self> {
        if tw.len() < 108 {
            return Err(Error::Format(format!("TW{index:04}.bin: {} bytes, want 108", tw.len())));
        }
        let w = |k: usize| u32::from_be_bytes(tw[4 * k..4 * k + 4].try_into().unwrap());
        let windows = [Window { open_day: w(0), close_day: w(3) }, Window { open_day: w(9), close_day: w(12) }];
        Ok(Self { index, windows, rounds: rounds(index, diary) })
    }

    /// Reads a season from the database archive (`Export.mng`).
    pub fn read(export: &Archive, index: u32) -> Result<Self> {
        let inner = export.get("Diary20.mng").ok_or_else(|| Error::Format("Export.mng has no Diary20.mng".into()))?;
        let inner = Archive::parse(inner)?;
        let get = |name: String| inner.get(&name).map(<[u8]>::to_vec).ok_or_else(|| Error::Format(format!("Diary20.mng has no {name}")));
        Self::parse(index, &get(format!("TW{index:04}.bin"))?, &get(format!("diary{index:04}.bin"))?)
    }
}

/// Splits a diary into rounds: each run of records whose `ff 00` ends sit six bytes apart, with the
/// bytes since the previous run as its header. A record can hold a false `ff 00` of its own (home
/// id 255, then an away id below 256): where a run could start there or three bytes on, the start
/// whose chain of records is longer wins (a tie means the earlier one was the false one).
fn rounds(index: u32, b: &[u8]) -> Vec<Round> {
    let is_end = |i: usize| i + 1 < b.len() && b[i] == 0xFF && b[i + 1] == 0x00;
    let chain = |mut i: usize| {
        let mut n = 0;
        while is_end(i) {
            n += 1;
            i += 6;
        }
        n
    };
    let mut out = Vec::new();
    let (mut pos, mut i) = (0, 4);
    while i + 1 < b.len() {
        if !is_end(i) {
            i += 1;
            continue;
        }
        if is_end(i + 3) && chain(i + 3) >= chain(i) {
            i += 3;
        }
        let header = b[pos..i - 4].to_vec();
        let mut recs: Vec<(u16, u16)> = Vec::new();
        let mut q = i;
        while is_end(q) {
            recs.push((u16::from_be_bytes([b[q - 4], b[q - 3]]), u16::from_be_bytes([b[q - 2], b[q - 1]])));
            q += 6;
        }
        let first = (header.len() >= 2).then(|| u16::from_le_bytes([header[0], header[1]]));
        let league = first.filter(|_| header.len() == 4);
        let day = first.filter(|_| header.len() >= 10).map(|v| v & 0x7FFF).filter(|v| (v >> 10) as u32 == index && v & 0x3FF < 366).map(|v| v & 0x3FF);
        out.push(Round {
            header,
            league,
            day,
            fixtures: recs.iter().filter(|r| r.0 > 0 && r.1 > 0).map(|&(h, a)| (h - 1, a - 1)).collect(),
            slots: recs.len(),
        });
        pos = q - 4;
        i = pos + 4;
    }
    out
}

/// A day from 1 July of `year` as (year, month 1-12, day of month).
pub fn date(year: i32, day: u32) -> (i32, u32, u32) {
    let leap = |y: i32| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut months = vec![(year, 7, 31), (year, 8, 31), (year, 9, 30), (year, 10, 31), (year, 11, 30), (year, 12, 31)];
    months.extend([(year + 1, 1, 31), (year + 1, 2, if leap(year + 1) { 29 } else { 28 }), (year + 1, 3, 31), (year + 1, 4, 30), (year + 1, 5, 31), (year + 1, 6, 30)]);
    let mut d = day;
    for (y, m, len) in months {
        if d < len {
            return (y, m, d + 1);
        }
        d -= len;
    }
    (year + 1, 7, d + 1)
}

/// The diary as the game reads it (`tools/diary.py`, #735): the file is the in-memory diary, a run
/// of day records. A record: +0 u16 BE packed date (`day << 11 | (month - 1) << 7 | years << 2`,
/// `years` from the game's first year: 2006, or the mod's `base_year`), +2 u8 entry count, +3 u8
/// kind (0 competition rounds, 1 a schedule of 4-byte entries `competition, event, u16 date`, 2 the
/// season's start, 3 the end), +4 u16 the record's length, entries from +6. The records are in date
/// order; the end record is the last.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DayRecord {
    pub date: (i32, u32, u32),
    pub kind: u8,
    pub bytes: Vec<u8>,
}

fn unpack_date(w: u16, first_year: i32) -> (i32, u32, u32) {
    (first_year + ((w >> 2) & 31) as i32, ((w >> 7) & 15) as u32 + 1, (w >> 11) as u32)
}

fn pack_date((y, m, d): (i32, u32, u32), first_year: i32) -> Result<u16> {
    let years = y - first_year;
    if !(0..32).contains(&years) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return Err(Error::Format(format!("diary date {y}-{m:02}-{d:02} can't be packed from {first_year}")));
    }
    Ok((d as u16) << 11 | ((m - 1) as u16) << 7 | (years as u16) << 2)
}

/// Days since 1970-01-01 of a calendar date (proleptic Gregorian).
fn days_from_civil((y, m, d): (i32, u32, u32)) -> i64 {
    let y = if m <= 2 { y - 1 } else { y } as i64;
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    era * 146_097 + (yoe * 365 + yoe / 4 - yoe / 100 + doy) - 719_468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((yoe + era * 400 + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

/// A season file's day records (`first_year`: the year its dates count from).
pub fn day_records(diary: &[u8], first_year: i32) -> Result<Vec<DayRecord>> {
    let mut out = Vec::new();
    let mut p = 0;
    loop {
        let head = diary.get(p..p + 6).ok_or_else(|| Error::Format(format!("diary: record at {p} past the end")))?;
        let (w, kind, len) = (u16::from_be_bytes([head[0], head[1]]), head[3], u16::from_be_bytes([head[4], head[5]]) as usize);
        if (1..6).contains(&len) {
            return Err(Error::Format(format!("diary: record at {p} is {len} bytes, shorter than its head")));
        }
        let bytes = diary.get(p..p + len.max(6)).ok_or_else(|| Error::Format(format!("diary: record at {p} ({len} bytes) past the end")))?.to_vec();
        out.push(DayRecord { date: unpack_date(w, first_year), kind, bytes });
        if kind == 3 || len == 0 {
            return Ok(out);
        }
        p += len;
    }
}

/// The weekday shift (-3..=3 days) that puts a date `n` days after 1 July of `from` on the same
/// weekday `n + shift` days after 1 July of `to`.
pub fn weekday_shift(from: i32, to: i32) -> i64 {
    let s = (days_from_civil((from, 7, 1)) - days_from_civil((to, 7, 1))).rem_euclid(7);
    if s > 3 { s - 7 } else { s }
}

/// Season `index`'s diary, written for 2006 + `index`, laid on `to_year` + `index` with every
/// competition round and every scheduled date (kind 0 records, kind 1 entries) on its old weekday:
/// each keeps its days from 1 July plus `weekday_shift`, so a Saturday round stays a Saturday
/// (`base_year = 2026` alone moves 2006's Saturday 12 Aug to Wednesday 12 Aug 2026, #703). The
/// schedules' own days move with the rounds (a schedule on the season's first day stays there); the
/// season's start and the end stay on their dates; the records are sorted
/// again by date (stable). The result packs its dates from `to_year`. `weeks` moves every round and
/// scheduled date that many weeks later on top (`calendar_weeks`: 2026/27's leagues open a week after
/// the disc's, #703).
pub fn retime(diary: &[u8], index: u32, to_year: i32, weeks: u32) -> Result<Vec<u8>> {
    let (from, to) = (FIRST_YEAR + index as i32, to_year + index as i32);
    let (start_from, start_to) = (days_from_civil((from, 7, 1)), days_from_civil((to, 7, 1)));
    // the rest keep their calendar date, in the new season's years
    let kept = |(y, m, d): (i32, u32, u32)| {
        // 29 Feb in a year without one: the 28th
        let date = (y - from + to, m, if (m, d) == (2, 29) && civil_from_days(days_from_civil((y - from + to, 3, 1)) - 1).2 == 28 { 28 } else { d });
        if civil_from_days(days_from_civil(date)) == date { Ok(date) } else { Err(Error::Format(format!("diary {index}: {y}-{m:02}-{d:02} has no day in {}", date.0))) }
    };
    let mut recs = day_records(diary, FIRST_YEAR)?;
    let end = kept(recs.last().ok_or_else(|| Error::Format("diary: no records".into()))?.date)?;
    // one shift for the whole season, so no round passes another: the weekday shift plus `weeks`,
    // else without the weeks (a round in late June would leave the season: the disc's 2009/10), else
    // a week on (a negative shift on a round in the first days of July, base_year 2029: -1)
    let base = weekday_shift(from, to);
    let rounds: Vec<i64> = recs.iter().filter(|r| r.kind == 0).map(|r| days_from_civil(r.date) - start_from).collect();
    let (first, last) = (rounds.iter().min().copied().unwrap_or(0), rounds.iter().max().copied().unwrap_or(0));
    let fits = |s: i64| start_to + first + s >= start_to && start_to + last + s < days_from_civil(end);
    let shift = [base + 7 * weeks as i64, base, base + 7].into_iter().find(|&s| fits(s)).ok_or_else(|| Error::Format(format!("diary {index}: no shift keeps the rounds inside the season")))?;
    let moved = |date: (i32, u32, u32)| civil_from_days(days_from_civil(date) - start_from + start_to + shift);
    let mut keyed = Vec::with_capacity(recs.len());
    for (i, mut r) in recs.drain(..).enumerate() {
        // a schedule moves with the rounds too, so the group draw the day after the last qualifying
        // round stays after it (kept, 24 Aug came before the qualifiers' 2nd leg on 26 Aug, #703);
        // one on the season's first day stays there
        let date = match r.kind {
            0 => moved(r.date),
            1 if days_from_civil(r.date) > start_from => civil_from_days(days_from_civil(moved(r.date)).max(start_to)),
            _ => kept(r.date)?,
        };
        if r.kind != 3 && !(start_to..days_from_civil(end)).contains(&days_from_civil(date)) {
            return Err(Error::Format(format!("diary {index}: a round moves to {date:?}, outside the season")));
        }
        r.bytes[0..2].copy_from_slice(&pack_date(date, to_year)?.to_be_bytes());
        if r.kind == 1 {
            let n = r.bytes[2] as usize;
            for e in 0..n {
                let at = 6 + 4 * e + 2;
                let w = r.bytes.get(at..at + 2).ok_or_else(|| Error::Format(format!("diary {index}: schedule entry {e} past its record")))?;
                let d = moved(unpack_date(u16::from_be_bytes([w[0], w[1]]), FIRST_YEAR));
                r.bytes[at..at + 2].copy_from_slice(&pack_date(d, to_year)?.to_be_bytes());
            }
        }
        if r.kind == 3 {
            // the end record's one entry is the next season's first day
            if let Some(w) = r.bytes.get(6..8) {
                let d = kept(unpack_date(u16::from_be_bytes([w[0], w[1]]), FIRST_YEAR))?;
                r.bytes[6..8].copy_from_slice(&pack_date(d, to_year)?.to_be_bytes());
            }
        }
        keyed.push((days_from_civil(date), r.kind == 3, i, r.bytes));
    }
    keyed.sort_by_key(|k| (k.1, k.0, k.2));
    let mut out: Vec<u8> = keyed.into_iter().flat_map(|k| k.3).collect();
    out.extend_from_slice(diary.get(out.len()..).unwrap_or_default());
    Ok(out)
}

/// `Diary20.mng` with every season retimed for `to_year` (`retime`).
pub fn retime_archive(diary20: &[u8], to_year: i32, weeks: u32) -> Result<Vec<u8>> {
    let mut ar = Archive::parse(diary20)?;
    for k in 0..SEASONS {
        let name = format!("diary{k:04}.bin");
        let Some(d) = ar.get(&name) else { continue };
        let new = retime(d, k, to_year, weeks)?;
        ar.set(&name, new);
    }
    Ok(ar.build())
}

/// The European Cup in the diary (#703): competition 55 (0x37); its round ids.
pub const EUROPEAN_CUP: u8 = 55;
const GROUP_ROUND: u8 = 42;
const LAST_16: [u8; 2] = [9, 10];
const QUARTER_FINAL: u8 = 28;
/// The 2024+ league phase: 8 matchdays of 18 games (36 clubs), then a two-legged play-off of 8 ties.
pub const LEAGUE_DAYS: usize = 8;
pub const LEAGUE_GAMES: u8 = 18;
pub const PLAYOFF_TIES: u8 = 8;
/// The play-off's round ids: a two-legged pair the European Cup doesn't use (the game's round
/// kinds: 0x820ee4d8 the two-legged rounds, 0x820ee2f8 their second legs, 0x820ee5a8 the single
/// games), so its round-done code (0x8213a718) collects the winners as for any knockout round. 13/14
/// are the UEFA Cup's last-32 ids; the draw for it is the hook's (0x821394a8 draws no other id).
pub const PLAYOFF: [u8; 2] = [13, 14];

/// A day record taken apart: (days since 1970, kind, entries for kinds 0-1, the raw record for the others).
type Day = (i64, u8, Vec<Vec<u8>>, Vec<u8>);

/// A season file's European Cup in the 2024+ shape (#703 step 1), for the draw and table hooks
/// (steps 3-4) to fill: the six group days of 16 blank games become the league phase's eight days of
/// 18 (the disc's six, then two more on the same weekday six and seven weeks after the sixth), a
/// two-legged play-off (`playoff` = its two round ids) of 8 blank ties takes the last 16's old days
/// one week earlier (the week before the last 16's first leg, then that day), and the last 16 moves
/// on a week (its second leg's old day, then a week later), before the quarter-finals. Every new round gets
/// its schedule entry (the game's "this round is next" event: competition, round, the round's date)
/// the day after the round before it, as the disc does for the group days; the quarter-finals keep
/// theirs. The diary is the disc's (dates from 2006, before `retime`) and grows: the game sizes its
/// buffer from the file (the loader 0x822e5d48 writes the pointer to 0x82740400 and the size to
/// 0x82740404), and a round's u8 game count takes 18.
pub fn league_phase(diary: &[u8], index: u32, playoff: [u8; 2]) -> Result<Vec<u8>> {
    let err = |what: String| Error::Format(format!("diary {index}: {what}"));
    let recs = day_records(diary, FIRST_YEAR)?;
    let used: usize = recs.iter().map(|r| r.bytes.len()).sum();
    // a kind 0 entry is `competition, round, games, leg` + 6 bytes a game, a
    // kind 1 entry `competition, round, u16 date`; the other kinds are kept as they are
    let mut days: Vec<Day> = Vec::with_capacity(recs.len() + 8);
    for r in &recs {
        let mut entries = Vec::new();
        if r.kind <= 1 {
            let mut p = 6;
            for _ in 0..r.bytes[2] {
                let len = if r.kind == 0 { 4 + 6 * *r.bytes.get(p + 2).ok_or_else(|| err(format!("a round past its record on {:?}", r.date)))? as usize } else { 4 };
                entries.push(r.bytes.get(p..p + len).ok_or_else(|| err(format!("an entry past its record on {:?}", r.date)))?.to_vec());
                p += len;
            }
        }
        days.push((days_from_civil(r.date), r.kind, entries, r.bytes.clone()));
    }
    let rounds_of = |id: u8| days.iter().filter(|d| d.1 == 0 && d.2.iter().any(|e| e[0] == EUROPEAN_CUP && e[1] == id)).map(|d| d.0).collect::<Vec<i64>>();
    let groups = rounds_of(GROUP_ROUND);
    let (l16a, l16b, qf) = (rounds_of(LAST_16[0]), rounds_of(LAST_16[1]), rounds_of(QUARTER_FINAL));
    if groups.len() != 6 || l16a.len() != 1 || l16b.len() != 1 || qf.len() != 1 {
        return Err(err(format!("the European Cup has {} group days, {}/{} last-16 legs and {} quarter-final first legs, want 6, 1/1, 1", groups.len(), l16a.len(), l16b.len(), qf.len())));
    }
    if playoff.iter().any(|id| !rounds_of(*id).is_empty()) {
        return Err(err(format!("the play-off's round ids {playoff:?} are already in the European Cup")));
    }
    let blank = days.iter().flat_map(|d| d.2.iter()).find(|e| d_is_round(e, GROUP_ROUND)).map(|e| e[4..10].to_vec()).unwrap_or_else(|| vec![0, 0, 0, 0, 0xff, 0]);
    let mut league = groups.clone();
    league.extend([groups[5] + 42, groups[5] + 49]);
    let plan: Vec<(i64, u8, u8, u8)> = league.iter().map(|&d| (d, GROUP_ROUND, LEAGUE_GAMES, 0))
        .chain([(l16a[0] - 7, playoff[0], PLAYOFF_TIES, 0), (l16a[0], playoff[1], PLAYOFF_TIES, 1)])
        .chain([(l16b[0], LAST_16[0], 8, 0), (l16b[0] + 7, LAST_16[1], 8, 1)])
        .collect();
    let seq: Vec<i64> = plan.iter().map(|p| p.0).collect();
    if !seq.windows(2).all(|w| w[0] < w[1]) || seq[seq.len() - 1] >= qf[0] {
        return Err(err("the league phase, play-off and last 16 don't fit before the quarter-finals".into()));
    }
    // out with the old group stage and last 16, rounds and schedule entries
    let gone = |e: &Vec<u8>| e[0] == EUROPEAN_CUP && (e[1] == GROUP_ROUND || LAST_16.contains(&e[1]));
    for d in days.iter_mut().filter(|d| d.1 <= 1) {
        d.2.retain(|e| !gone(e));
    }
    let start = days_from_civil((FIRST_YEAR + index as i32, 7, 1));
    let add = |days: &mut Vec<Day>, day: i64, kind: u8, entry: Vec<u8>| {
        match days.iter_mut().find(|d| d.0 == day && d.1 == kind) {
            Some(d) => d.2.insert(0, entry),
            None => days.push((day, kind, vec![entry], Vec::new())),
        }
    };
    for (k, &(day, id, games, leg)) in plan.iter().enumerate() {
        let mut round = vec![EUROPEAN_CUP, id, games, leg];
        for _ in 0..games {
            round.extend_from_slice(&blank);
        }
        add(&mut days, day, 0, round);
        // the first group day keeps the disc's date for its entry (the day after the qualifying round)
        let at = if k == 0 { days_of_entry(&recs, GROUP_ROUND).unwrap_or(day - 1) } else { plan[k - 1].0 + 1 };
        let date = pack_date(civil_from_days(day), FIRST_YEAR)?;
        add(&mut days, at.max(start), 1, vec![EUROPEAN_CUP, id, (date >> 8) as u8, date as u8]);
    }
    days.retain(|d| d.1 > 1 || !d.2.is_empty());
    days.sort_by_key(|d| (d.1 == 3, d.0, d.1 != 2, d.1));
    let mut out = Vec::with_capacity(diary.len() + 4096);
    for (day, kind, entries, raw) in days {
        if kind > 1 {
            out.extend_from_slice(&raw);
            continue;
        }
        let body: Vec<u8> = entries.concat();
        let len = u16::try_from(6 + body.len()).map_err(|_| err(format!("a day of {} bytes", 6 + body.len())))?;
        out.extend_from_slice(&pack_date(civil_from_days(day), FIRST_YEAR)?.to_be_bytes());
        out.extend_from_slice(&[u8::try_from(entries.len()).map_err(|_| err("a day of 256 entries".into()))?, kind]);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&body);
    }
    out.extend_from_slice(diary.get(used..).unwrap_or_default());
    Ok(out)
}

/// Writes a drawn league phase into a diary `league_phase` shaped (#703, the round-42 hook): the
/// European Cup's round-42 days in date order get their games, matchday k from `days[k]` as
/// (home, away) club uids, each into the round's next blank (home u16, away u16, and the status's
/// two goal nibbles 0xF, not played, as the game's 0x82133970 writes them). The diary keeps its
/// size; the game's own copy is patched in place. Errors when the days don't match the blanks.
pub fn fill_league_phase(diary: &mut [u8], first_year: i32, days: &[Vec<(u16, u16)>]) -> Result<()> {
    let recs = day_records(diary, first_year)?;
    let mut rounds = Vec::new(); // (date, offset of the round entry)
    let mut p = 0;
    for r in &recs {
        if r.kind == 0 {
            let mut e = p + 6;
            for _ in 0..r.bytes[2] {
                let games = *diary.get(e + 2).ok_or_else(|| Error::Format("diary: a round past the end".into()))? as usize;
                if diary[e] == EUROPEAN_CUP && diary[e + 1] == GROUP_ROUND {
                    rounds.push((days_from_civil(r.date), e));
                }
                e += 4 + 6 * games;
            }
        }
        p += r.bytes.len();
    }
    rounds.sort_by_key(|r| r.0);
    if rounds.len() != days.len() {
        return Err(Error::Format(format!("diary: {} league-phase days, the draw has {}", rounds.len(), days.len())));
    }
    // every day checked before anything is written
    let mut blanks = Vec::with_capacity(rounds.len());
    for (&(_, e), games) in rounds.iter().zip(days) {
        let blank: Vec<usize> = (0..diary[e + 2] as usize).map(|j| e + 4 + 6 * j).filter(|&f| diary[f..f + 4] == [0, 0, 0, 0]).collect();
        if blank.len() < games.len() {
            return Err(Error::Format(format!("diary: a league-phase day with {} blanks for {} games", blank.len(), games.len())));
        }
        blanks.push(blank);
    }
    for (blank, games) in blanks.iter().zip(days) {
        for (&f, &(h, a)) in blank.iter().zip(games) {
            diary[f..f + 2].copy_from_slice(&h.to_be_bytes());
            diary[f + 2..f + 4].copy_from_slice(&a.to_be_bytes());
            diary[f + 4] = 0xFF;
        }
    }
    Ok(())
}

/// A league-phase game: (home uid, away uid, the score once played).
pub type LeagueGame = (u16, u16, Option<(u8, u8)>);

/// The league phase's games in a diary, in date order: (home uid, away uid, the score once played).
/// The status word's high nibble is the home goals and the next the away goals; 0xE and 0xF there
/// mean not played (`tools/diary.py`). A blank game (uid 0) is skipped.
pub fn league_phase_results(diary: &[u8], first_year: i32) -> Result<Vec<LeagueGame>> {
    let mut out = Vec::new();
    for r in day_records(diary, first_year)? {
        if r.kind != 0 {
            continue;
        }
        let mut p = 6;
        for _ in 0..r.bytes[2] {
            let h = r.bytes.get(p..p + 4).ok_or_else(|| Error::Format(format!("diary: a round past its record on {:?}", r.date)))?;
            let games = h[2] as usize;
            let body = r.bytes.get(p + 4..p + 4 + 6 * games).ok_or_else(|| Error::Format(format!("diary: games past their record on {:?}", r.date)))?;
            if h[0] == EUROPEAN_CUP && h[1] == GROUP_ROUND {
                for g in body.as_chunks::<6>().0 {
                    let (home, away) = (u16::from_be_bytes([g[0], g[1]]), u16::from_be_bytes([g[2], g[3]]));
                    if home == 0 || away == 0 {
                        continue;
                    }
                    let (hg, ag) = (g[4] >> 4, g[4] & 0xF);
                    out.push((home, away, (hg < 14 && ag < 14).then_some((hg, ag))));
                }
            }
            p += 4 + 6 * games;
        }
    }
    Ok(out)
}

fn d_is_round(e: &[u8], id: u8) -> bool {
    e.len() >= 10 && e[0] == EUROPEAN_CUP && e[1] == id
}

/// The day of the first schedule entry for the European Cup's round `id`.
fn days_of_entry(recs: &[DayRecord], id: u8) -> Option<i64> {
    recs.iter().filter(|r| r.kind == 1).find(|r| (0..r.bytes[2] as usize).any(|k| r.bytes.get(6 + 4 * k..8 + 4 * k) == Some(&[EUROPEAN_CUP, id][..]))).map(|r| days_from_civil(r.date))
}

/// `Diary20.mng` with every season's European Cup in the league-phase shape (`league_phase`).
pub fn league_phase_archive(diary20: &[u8]) -> Result<Vec<u8>> {
    let mut ar = Archive::parse(diary20)?;
    for k in 0..SEASONS {
        let name = format!("diary{k:04}.bin");
        let Some(d) = ar.get(&name) else { continue };
        let new = league_phase(d, k, PLAYOFF)?;
        ar.set(&name, new);
    }
    Ok(ar.build())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tw() -> Vec<u8> {
        let words: [u32; 27] = [0, 1, 6, 61, 31, 7, 11, 0, 0, 184, 1, 0, 214, 31, 0, 11, 0, 1, 0, 0, 0, 0, 0, 0, 151, 22, u32::MAX];
        words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }

    #[test]
    fn days_from_the_first_of_july() {
        assert_eq!(date(2006, 0), (2006, 7, 1));
        assert_eq!(date(2006, 61), (2006, 8, 31));
        assert_eq!(date(2006, 75), (2006, 9, 14), "2006/07 round 1, a Thursday");
        assert_eq!(date(2006, 184), (2007, 1, 1));
        assert_eq!(date(2007, 243), (2008, 2, 29), "a leap February");
    }

    #[test]
    fn windows_and_rounds() {
        // a dated 10-byte header (day 75 of season 1), two fixtures and a blank slot; then a league
        // round (league 6, 2 matches) behind a 4-byte header
        let mut d = Vec::new();
        d.extend_from_slice(&((1u16 << 10) | 75).to_le_bytes());
        d.extend_from_slice(&[0x01, 0x00, 0x00, 0x16, 0x32, 0x04, 0x02, 0x03]);
        for (h, a) in [(117u16, 127u16), (121, 118), (0, 0)] {
            d.extend_from_slice(&h.to_be_bytes());
            d.extend_from_slice(&a.to_be_bytes());
            d.extend_from_slice(&[0xFF, 0x00]);
        }
        d.extend_from_slice(&[0x06, 0x00, 0x02, 0x00]);
        for (h, a) in [(119u16, 120u16), (123, 124)] {
            d.extend_from_slice(&h.to_be_bytes());
            d.extend_from_slice(&a.to_be_bytes());
            d.extend_from_slice(&[0xFF, 0x00]);
        }
        let s = Season::parse(1, &tw(), &d).unwrap();
        assert_eq!(s.year(), 2007);
        assert_eq!(s.windows, [Window { open_day: 0, close_day: 61 }, Window { open_day: 184, close_day: 214 }]);
        assert_eq!(s.rounds.len(), 2);
        assert_eq!((s.rounds[0].day, s.rounds[0].league, s.rounds[0].slots), (Some(75), None, 3));
        assert_eq!(s.rounds[0].fixtures, vec![(116, 126), (120, 117)], "1-based ids to clubs.csv ids, the blank slot dropped");
        assert_eq!((s.rounds[1].day, s.rounds[1].league), (None, Some(6)));
        assert_eq!(s.rounds[1].fixtures, vec![(118, 119), (122, 123)]);
        assert!(Season::parse(0, &[0; 12], &d).is_err(), "a short TW file");
    }

    #[test]
    fn dates_and_the_weekday_shift() {
        for date in [(2006, 7, 1), (2008, 2, 29), (2026, 12, 31), (2027, 1, 1)] {
            assert_eq!(civil_from_days(days_from_civil(date)), date);
        }
        assert_eq!(days_from_civil((1970, 1, 1)), 0);
        // 1 Jul 2006 a Saturday, 1 Jul 2026 a Wednesday: three days on
        assert_eq!(weekday_shift(2006, 2026), 3);
        assert_eq!(weekday_shift(2006, 2006), 0);
        assert_eq!(weekday_shift(2026, 2006), -3);
        let w = pack_date((2027, 1, 31), 2026).unwrap();
        assert_eq!(unpack_date(w, 2026), (2027, 1, 31));
        assert!(pack_date((2025, 1, 1), 2026).is_err());
    }

    /// A season of four records: the start (1 Jul), a schedule, a Saturday round and the end.
    fn season_records() -> Vec<u8> {
        let rec = |date: (i32, u32, u32), kind: u8, entries: &[u8], n: u8| {
            let mut r = pack_date(date, 2006).unwrap().to_be_bytes().to_vec();
            r.extend_from_slice(&[n, kind]);
            r.extend_from_slice(&((6 + entries.len()) as u16).to_be_bytes());
            r.extend_from_slice(entries);
            r
        };
        let mut d = rec((2006, 7, 1), 2, &[0xff, 0xff], 1);
        let sched: Vec<u8> = [57u8, 37].into_iter().chain(pack_date((2006, 8, 19), 2006).unwrap().to_be_bytes()).collect();
        d.extend(rec((2006, 7, 1), 1, &sched, 1));
        d.extend(rec((2006, 8, 19), 0, &[1, 0, 1, 0, 0, 1, 0, 2, 0xff, 0], 1));
        d.extend(rec((2007, 6, 30), 3, &pack_date((2007, 7, 1), 2006).unwrap().to_be_bytes().into_iter().chain([0xff, 0xff]).collect::<Vec<u8>>(), 1));
        d
    }

    /// The disc's group draw is a schedule the day after the qualifiers' second leg (24 and 23 Aug
    /// 2006); retimed to 2026, it stays the day after the leg, not on 24 Aug before it (#703).
    #[test]
    fn a_schedule_after_a_round_stays_after_it() {
        let rec = |date: (i32, u32, u32), kind: u8, entries: &[u8]| {
            let mut r = pack_date(date, 2006).unwrap().to_be_bytes().to_vec();
            r.extend_from_slice(&[1, kind]);
            r.extend_from_slice(&((6 + entries.len()) as u16).to_be_bytes());
            r.extend_from_slice(entries);
            r
        };
        let mut d = rec((2006, 7, 1), 2, &[0xff, 0xff]);
        d.extend(rec((2006, 8, 23), 0, &[55, 3, 1, 1, 0, 1, 0, 2, 0xff, 0]));
        let draw: Vec<u8> = [55u8, 42].into_iter().chain(pack_date((2006, 9, 13), 2006).unwrap().to_be_bytes()).collect();
        d.extend(rec((2006, 8, 24), 1, &draw));
        d.extend(rec((2006, 9, 13), 0, &[55, 42, 1, 0, 0, 1, 0, 2, 0xff, 0]));
        d.extend(rec((2007, 6, 30), 3, &pack_date((2007, 7, 1), 2006).unwrap().to_be_bytes().into_iter().chain([0xff, 0xff]).collect::<Vec<u8>>()));
        for weeks in [0, 1] {
            let r = day_records(&retime(&d, 0, 2026, weeks).unwrap(), 2026).unwrap();
            let day = |kind: u8, n: usize| days_from_civil(r.iter().filter(|r| r.kind == kind).nth(n).unwrap().date);
            assert_eq!(day(1, 0), day(0, 0) + 1, "weeks {weeks}: the draw is the day after the qualifier");
            let named = unpack_date(u16::from_be_bytes([r.iter().find(|r| r.kind == 1).unwrap().bytes[8], r.iter().find(|r| r.kind == 1).unwrap().bytes[9]]), 2026);
            assert_eq!(days_from_civil(named), day(0, 1), "weeks {weeks}: it names the group day");
        }
    }

    #[test]
    fn a_retimed_round_keeps_its_weekday() {
        let d = season_records();
        let r = day_records(&retime(&d, 0, 2026, 0).unwrap(), 2026).unwrap();
        let got: Vec<((i32, u32, u32), u8)> = r.iter().map(|r| (r.date, r.kind)).collect();
        // Saturday 19 Aug 2006 is 49 days from 1 July: Saturday 22 Aug 2026 (49 + 3)
        assert_eq!(got, [((2026, 7, 1), 2), ((2026, 7, 1), 1), ((2026, 8, 22), 0), ((2027, 6, 30), 3)]);
        assert_eq!(unpack_date(u16::from_be_bytes([r[1].bytes[8], r[1].bytes[9]]), 2026), (2026, 8, 22), "the schedule's date moves too");
        assert_eq!(unpack_date(u16::from_be_bytes([r[3].bytes[6], r[3].bytes[7]]), 2026), (2027, 7, 1), "the next season's first day");
        assert_eq!(r[2].bytes[2..], day_records(&d, 2006).unwrap()[2].bytes[2..], "the round itself unchanged");
        assert_eq!(retime(&d, 0, 2006, 0).unwrap(), d, "vanilla: nothing moves");
        // a week later on top: Saturday 29 Aug
        let r = day_records(&retime(&d, 0, 2026, 1).unwrap(), 2026).unwrap();
        assert_eq!(r.iter().find(|r| r.kind == 0).unwrap().date, (2026, 8, 29), "calendar_weeks = 1");
    }

    #[test]
    fn a_negative_shift_that_leaves_the_season_goes_a_week_on() {
        // Saturday 1 Jul 2006, a round on the season's first day; 1 Jul 2029 is a Sunday (-1)
        let mut d = season_records();
        let at = 6 + 8 + 4;
        d[at..at + 2].copy_from_slice(&pack_date((2006, 7, 1), 2006).unwrap().to_be_bytes());
        assert_eq!(weekday_shift(2006, 2029), -1);
        let r = day_records(&retime(&d, 0, 2029, 0).unwrap(), 2029).unwrap();
        let round = r.iter().find(|r| r.kind == 0).unwrap();
        assert_eq!(round.date, (2029, 7, 7), "Saturday 7 Jul, not Saturday 30 Jun");
    }

    /// A record of `kind` on `date` (dates from 2006) with `n` entries.
    fn rec(date: (i32, u32, u32), kind: u8, entries: &[u8], n: u8) -> Vec<u8> {
        let mut r = pack_date(date, 2006).unwrap().to_be_bytes().to_vec();
        r.extend_from_slice(&[n, kind]);
        r.extend_from_slice(&((6 + entries.len()) as u16).to_be_bytes());
        r.extend_from_slice(entries);
        r
    }

    /// Season `first`/`first + 1`: the start, Saturday rounds on `rounds`, a schedule-free kind 1 on
    /// `kept`, and the end with the next season's first day.
    fn season(first: i32, rounds: &[(i32, u32, u32)], kept: Option<(i32, u32, u32)>) -> Vec<u8> {
        let mut d = rec((first, 7, 1), 2, &[0xff, 0xff], 1);
        for &r in rounds {
            d.extend(rec(r, 0, &[1, 0, 1, 0, 0, 1, 0, 2, 0xff, 0], 1));
        }
        if let Some(k) = kept {
            d.extend(rec(k, 1, &[], 0));
        }
        let next: Vec<u8> = pack_date((first + 1, 7, 1), 2006).unwrap().to_be_bytes().into_iter().chain([0xff, 0xff]).collect();
        d.extend(rec((first + 1, 6, 30), 3, &next, 1));
        d
    }

    #[test]
    fn extra_weeks_that_push_a_late_round_past_the_end_are_dropped_for_the_whole_season() {
        // Saturday 23 Jun 2007 + 3 days + a week is 3 Jul 2027, after the season's end (30 Jun)
        let d = season(2006, &[(2006, 8, 19), (2007, 6, 23)], None);
        let r = day_records(&retime(&d, 0, 2026, 1).unwrap(), 2026).unwrap();
        let rounds: Vec<(i32, u32, u32)> = r.iter().filter(|r| r.kind == 0).map(|r| r.date).collect();
        assert_eq!(rounds, [(2026, 8, 22), (2027, 6, 26)], "both rounds on the weekday shift alone, so neither passes the other");
        let weekday = |d| days_from_civil(d).rem_euclid(7);
        assert_eq!(weekday((2027, 6, 26)), weekday((2007, 6, 23)), "still a Saturday");
    }

    #[test]
    fn a_schedule_on_29_february_moves_with_the_rounds() {
        // season 1 (2007/08) laid on 2028/29 (no 29 Feb) and on 2031/32: the schedule keeps its
        // days from 1 July plus the rounds' weekday shift, like the round
        let d = season(2007, &[(2007, 8, 18)], Some((2008, 2, 29)));
        for to in [2027, 2030] {
            let r = day_records(&retime(&d, 1, to, 0).unwrap(), to).unwrap();
            let from_july = |r: &DayRecord| days_from_civil(r.date) - days_from_civil((to + 1, 7, 1));
            let (round, sched) = (r.iter().find(|r| r.kind == 0).unwrap(), r.iter().find(|r| r.kind == 1).unwrap());
            assert_eq!(from_july(sched) - from_july(round), days_from_civil((2008, 2, 29)) - days_from_civil((2007, 8, 18)), "to {to}");
        }
    }

    #[test]
    fn the_bytes_after_the_end_record_are_kept() {
        let mut d = season(2006, &[(2006, 8, 19)], None);
        d.extend_from_slice(&[0xab, 0xcd, 0, 0]);
        let out = retime(&d, 0, 2026, 0).unwrap();
        assert_eq!(out.len(), d.len());
        assert_eq!(out[out.len() - 4..], [0xab, 0xcd, 0, 0]);
    }

    #[test]
    fn civil_dates_round_trip_over_the_seasons_years() {
        for z in days_from_civil((2006, 1, 1))..days_from_civil((2060, 1, 1)) {
            assert_eq!(days_from_civil(civil_from_days(z)), z);
        }
        assert_eq!(days_from_civil((1970, 1, 1)), 0);
        assert_eq!(civil_from_days(days_from_civil((2028, 2, 29))), (2028, 2, 29));
    }

    #[test]
    fn a_record_shorter_than_its_head_is_an_error() {
        let mut d = season_records();
        d[5] = 4;
        assert!(day_records(&d, 2006).is_err());
    }

    /// The disc's 20 seasons on base years 2026-2035 with 0-1 extra weeks (`LMA_EXPORT=<Export.mng>`): every round on
    /// its old weekday, nothing lost, the dates in order.
    #[test]
    fn the_discs_seasons_retime_for_2026() {
        let Ok(path) = std::env::var("LMA_EXPORT") else { return };
        let ar = Archive::parse(&std::fs::read(path).unwrap()).unwrap();
        let diary = Archive::parse(ar.get("Diary20.mng").unwrap()).unwrap();
        let cases = (2026..=2035).flat_map(|y| (0..SEASONS).flat_map(move |k| (0..=1).map(move |w| (y, k, w))));
        for (year, k, weeks) in cases {
            let d = diary.get(&format!("diary{k:04}.bin")).unwrap();
            let new = retime(d, k, year, weeks).unwrap();
            assert_eq!(new.len(), d.len());
            let (a, b) = (day_records(d, FIRST_YEAR).unwrap(), day_records(&new, year).unwrap());
            assert_eq!(a.len(), b.len());
            let weekday = |date| days_from_civil(date).rem_euclid(7);
            let rounds = |r: &[DayRecord]| r.iter().filter(|r| r.kind == 0).map(|r| weekday(r.date)).collect::<Vec<_>>();
            let (mut wa, mut wb) = (rounds(&a), rounds(&b));
            wa.sort();
            wb.sort();
            assert_eq!(wa, wb, "{year}, season {k}, {weeks} week(s) later: the rounds' weekdays");
            assert!(b.windows(2).all(|w| days_from_civil(w[0].date) <= days_from_civil(w[1].date)), "{year}, season {k}, {weeks} week(s): in date order");
            // each competition plays its rounds in the same order (a round brought back inside the
            // season doesn't jump another)
            let order = |r: &[DayRecord]| {
                let mut v: Vec<(i64, u8, u8)> = Vec::new();
                for rec in r.iter().filter(|r| r.kind == 0) {
                    let mut p = 6;
                    for _ in 0..rec.bytes[2] {
                        let Some(h) = rec.bytes.get(p..p + 4) else { break };
                        v.push((days_from_civil(rec.date), h[0], h[1]));
                        p += 4 + 6 * h[2] as usize;
                    }
                }
                v.sort_by_key(|e| e.0);
                let mut by_comp: std::collections::BTreeMap<u8, Vec<u8>> = Default::default();
                for (_, comp, round) in v {
                    by_comp.entry(comp).or_default().push(round);
                }
                by_comp
            };
            assert_eq!(order(&a), order(&b), "{year}, season {k}, {weeks} week(s): each competition's rounds in order");
            // every schedule keeps the rounds before it (the group draw after the qualifiers, #703)
            let before = |r: &[DayRecord]| {
                let rounds: Vec<i64> = r.iter().filter(|r| r.kind == 0).map(|r| days_from_civil(r.date)).collect();
                r.iter().filter(|r| r.kind == 1).map(|s| rounds.iter().filter(|&&d| d < days_from_civil(s.date)).count()).collect::<Vec<_>>()
            };
            assert_eq!(before(&a), before(&b), "{year}, season {k}, {weeks} week(s): the rounds before each schedule");
            // only dates move: every round's body (its competitions, round ids, fixtures and blank
            // European slots, e.g. 0x37's six group days of 16 blanks) comes out byte for byte
            let bodies = |r: &[DayRecord]| {
                let mut v: Vec<Vec<u8>> = r.iter().filter(|r| r.kind == 0).map(|r| r.bytes[2..].to_vec()).collect();
                v.sort();
                v
            };
            assert_eq!(bodies(&a), bodies(&b), "{year}, season {k}, {weeks} week(s): round bodies unchanged");
        }
    }

    /// The European Cup's rounds as (date, round, games, leg, blank games), in date order, and its
    /// schedule entries as (entry day, round, round date).
    type Date = (i32, u32, u32);
    type CupRounds = Vec<(Date, u8, u8, u8, usize)>;
    fn cup(d: &[u8]) -> (CupRounds, Vec<(Date, u8, Date)>) {
        let (mut rounds, mut entries) = (Vec::new(), Vec::new());
        for r in day_records(d, FIRST_YEAR).unwrap() {
            let mut p = 6;
            for _ in 0..r.bytes[2] {
                if r.kind == 0 {
                    let h = &r.bytes[p..p + 4];
                    let blanks = (0..h[2] as usize).filter(|j| r.bytes[p + 4 + 6 * j..p + 8 + 6 * j] == [0, 0, 0, 0]).count();
                    if h[0] == EUROPEAN_CUP {
                        rounds.push((r.date, h[1], h[2], h[3], blanks));
                    }
                    p += 4 + 6 * h[2] as usize;
                } else if r.kind == 1 {
                    let e = &r.bytes[p..p + 4];
                    if e[0] == EUROPEAN_CUP {
                        entries.push((r.date, e[1], unpack_date(u16::from_be_bytes([e[2], e[3]]), FIRST_YEAR)));
                    }
                    p += 4;
                }
            }
        }
        (rounds, entries)
    }

    /// A 2006/07 European Cup as the disc has it: six group days of 16 blanks, the last 16 and the
    /// quarter-finals, each after its schedule entry, beside a league round on one of the days.
    fn european_season() -> Vec<u8> {
        let round = |id: u8, games: u8, leg: u8| {
            let mut e = vec![EUROPEAN_CUP, id, games, leg];
            for _ in 0..games {
                e.extend_from_slice(&[0, 0, 0, 0, 0xff, 0]);
            }
            e
        };
        let entry = |id: u8, date| {
            let w = pack_date(date, 2006).unwrap().to_be_bytes();
            vec![EUROPEAN_CUP, id, w[0], w[1]]
        };
        let mut d = rec((2006, 7, 1), 2, &[0xff, 0xff], 1);
        d.extend(rec((2006, 8, 24), 1, &entry(42, (2006, 9, 13)), 1));
        let groups = [(2006, 9, 13), (2006, 9, 27), (2006, 10, 18), (2006, 11, 1), (2006, 11, 22), (2006, 12, 6)];
        for (k, &g) in groups.iter().enumerate() {
            let mut e = round(42, 16, 0);
            if k == 2 {
                e.extend_from_slice(&[7, 43, 1, 0, 0, 0, 0, 0, 0xff, 0]); // another league's midweek slot
            }
            d.extend(rec(g, 0, &e, 1 + (k == 2) as u8));
            if k < 5 {
                d.extend(rec(civil_from_days(days_from_civil(g) + 1), 1, &entry(42, groups[k + 1]), 1));
            }
        }
        d.extend(rec((2006, 12, 15), 1, &entry(9, (2007, 2, 21)), 1));
        d.extend(rec((2007, 2, 21), 0, &round(9, 8, 0), 1));
        d.extend(rec((2007, 2, 22), 1, &entry(10, (2007, 3, 7)), 1));
        d.extend(rec((2007, 3, 7), 0, &round(10, 8, 1), 1));
        d.extend(rec((2007, 3, 16), 1, &entry(28, (2007, 3, 28)), 1));
        d.extend(rec((2007, 3, 28), 0, &round(28, 4, 0), 1));
        let next: Vec<u8> = pack_date((2007, 7, 1), 2006).unwrap().to_be_bytes().into_iter().chain([0xff, 0xff]).collect();
        d.extend(rec((2007, 6, 30), 3, &next, 1));
        d
    }

    #[test]
    fn the_league_phase_takes_eight_days_of_eighteen_then_a_play_off() {
        let d = european_season();
        let out = league_phase(&d, 0, PLAYOFF).unwrap();
        let (rounds, entries) = cup(&out);
        let league: Vec<_> = rounds.iter().filter(|r| r.1 == 42).map(|r| (r.0, r.2, r.4)).collect();
        assert_eq!(league.len(), LEAGUE_DAYS);
        assert!(league.iter().all(|l| (l.1, l.2) == (18, 18)), "18 blank games a day");
        assert_eq!(league[6].0, (2007, 1, 17), "six weeks after the sixth day, a Wednesday");
        assert_eq!(league[7].0, (2007, 1, 24));
        let ko: Vec<_> = rounds.iter().filter(|r| r.1 != 42).map(|r| (r.0, r.1, r.2, r.3)).collect();
        assert_eq!(ko, [((2007, 2, 14), 13, 8, 0), ((2007, 2, 21), 14, 8, 1), ((2007, 3, 7), 9, 8, 0), ((2007, 3, 14), 10, 8, 1), ((2007, 3, 28), 28, 4, 0)]);
        // a schedule entry before each round, naming its date; the disc's first and the quarter-final's kept
        for r in &rounds {
            let e = entries.iter().find(|e| e.1 == r.1 && e.2 == r.0).unwrap_or_else(|| panic!("no entry for round {} on {:?}", r.1, r.0));
            assert!(days_from_civil(e.0) < days_from_civil(r.0));
        }
        assert_eq!(entries.iter().find(|e| e.2 == (2006, 9, 13)).unwrap().0, (2006, 8, 24));
        assert_eq!(entries.iter().find(|e| e.1 == 13).unwrap().0, (2007, 1, 25), "the day after the league phase");
        assert_eq!(entries.iter().find(|e| e.1 == 28).unwrap().0, (2007, 3, 16));
        // the other league's slot is still on its day, and the records stay in date order
        let recs = day_records(&out, FIRST_YEAR).unwrap();
        let oct = recs.iter().find(|r| r.date == (2006, 10, 18) && r.kind == 0).unwrap();
        assert_eq!(oct.bytes[2], 2);
        assert!(recs.windows(2).all(|w| days_from_civil(w[0].date) <= days_from_civil(w[1].date)));
        assert_eq!(recs.last().unwrap().kind, 3);
        // retimed for 2026 like the disc's own diary
        let r = day_records(&retime(&out, 0, 2026, 1).unwrap(), 2026).unwrap();
        assert_eq!(r.iter().filter(|r| r.kind == 0).count(), recs.iter().filter(|r| r.kind == 0).count());
    }

    #[test]
    fn a_drawn_league_fills_the_blanks_in_date_order() {
        let mut d = league_phase(&european_season(), 0, PLAYOFF).unwrap();
        let len = d.len();
        let days: Vec<Vec<(u16, u16)>> = (0..LEAGUE_DAYS as u16).map(|k| (0..18u16).map(|g| (100 * (k + 1) + 2 * g + 1, 100 * (k + 1) + 2 * g + 2)).collect()).collect();
        fill_league_phase(&mut d, FIRST_YEAR, &days).unwrap();
        assert_eq!(d.len(), len, "same size");
        let recs = day_records(&d, FIRST_YEAR).unwrap();
        let mut seen = Vec::new();
        for r in recs.iter().filter(|r| r.kind == 0) {
            let mut p = 6;
            for _ in 0..r.bytes[2] {
                let h = &r.bytes[p..p + 4];
                if h[0] == EUROPEAN_CUP && h[1] == 42 {
                    let first = &r.bytes[p + 4..p + 10];
                    seen.push((r.date, u16::from_be_bytes([first[0], first[1]]), u16::from_be_bytes([first[2], first[3]]), first[4]));
                }
                p += 4 + 6 * h[2] as usize;
            }
        }
        assert_eq!(seen.len(), LEAGUE_DAYS);
        assert_eq!(seen[0], ((2006, 9, 13), 101, 102, 0xFF), "matchday 1 on the first day, not played");
        assert_eq!(seen[7], ((2007, 1, 24), 801, 802, 0xFF));
        // a draw of the wrong shape is refused
        assert!(fill_league_phase(&mut d.clone(), FIRST_YEAR, &days[..7]).is_err());
        let more: Vec<Vec<(u16, u16)>> = (0..LEAGUE_DAYS).map(|_| vec![(1, 2); 19]).collect();
        let mut fresh = league_phase(&european_season(), 0, PLAYOFF).unwrap();
        assert!(fill_league_phase(&mut fresh, FIRST_YEAR, &more).is_err(), "19 games for 18 blanks");
        assert_eq!(fresh, league_phase(&european_season(), 0, PLAYOFF).unwrap(), "nothing written on an error");
    }

    #[test]
    fn the_league_phase_results_come_back_with_their_scores() {
        let mut d = league_phase(&european_season(), 0, PLAYOFF).unwrap();
        let days: Vec<Vec<(u16, u16)>> = (0..LEAGUE_DAYS as u16).map(|k| (0..18u16).map(|g| (100 * (k + 1) + 2 * g + 1, 100 * (k + 1) + 2 * g + 2)).collect()).collect();
        fill_league_phase(&mut d, FIRST_YEAR, &days).unwrap();
        let r = league_phase_results(&d, FIRST_YEAR).unwrap();
        assert_eq!(r.len(), 144);
        assert!(r.iter().all(|g| g.2.is_none()), "nothing played yet");
        assert_eq!((r[0].0, r[0].1), (101, 102));
        // the first game ends 3-1
        let at = d.windows(4).position(|w| w == [0, 101, 0, 102]).unwrap();
        d[at + 4] = 0x31;
        let r = league_phase_results(&d, FIRST_YEAR).unwrap();
        assert_eq!(r[0].2, Some((3, 1)));
        assert_eq!(r.iter().filter(|g| g.2.is_some()).count(), 1);
    }

    #[test]
    fn the_league_phase_refuses_what_it_cant_place() {
        let d = european_season();
        assert!(league_phase(&d, 0, [9, 30]).is_err(), "a play-off id the cup already has");
        let once = league_phase(&d, 0, PLAYOFF).unwrap();
        assert!(league_phase(&once, 0, PLAYOFF).is_err(), "eight group days: already rewritten");
    }

    /// The disc's 20 seasons in the league-phase shape (`LMA_EXPORT=<Export.mng>`): the European
    /// Cup's new rounds, every other competition's rounds and entries byte for byte, then retimed.
    #[test]
    fn the_discs_seasons_take_the_league_phase() {
        let Ok(path) = std::env::var("LMA_EXPORT") else { return };
        let ar = Archive::parse(&std::fs::read(path).unwrap()).unwrap();
        let diary = Archive::parse(ar.get("Diary20.mng").unwrap()).unwrap();
        let others = |d: &[u8]| {
            let mut v: Vec<(i64, u8, Vec<u8>)> = Vec::new();
            for r in day_records(d, FIRST_YEAR).unwrap() {
                let mut p = 6;
                for _ in 0..if r.kind <= 1 { r.bytes[2] } else { 0 } {
                    let len = if r.kind == 0 { 4 + 6 * r.bytes[p + 2] as usize } else { 4 };
                    if r.bytes[p] != EUROPEAN_CUP {
                        v.push((days_from_civil(r.date), r.kind, r.bytes[p..p + len].to_vec()));
                    }
                    p += len;
                }
                if r.kind > 1 {
                    v.push((days_from_civil(r.date), r.kind, r.bytes.clone()));
                }
            }
            v.sort();
            v
        };
        for k in 0..SEASONS {
            let d = diary.get(&format!("diary{k:04}.bin")).unwrap();
            let out = league_phase(d, k, PLAYOFF).unwrap_or_else(|e| panic!("season {k}: {e}"));
            let (rounds, _) = cup(&out);
            assert_eq!(rounds.iter().filter(|r| r.1 == 42 && r.2 == 18 && r.4 == 18).count(), LEAGUE_DAYS, "season {k}");
            let ids: Vec<u8> = rounds.iter().map(|r| r.1).filter(|&id| id != 42).collect();
            assert_eq!(ids[..2], [2, 3], "season {k}: qualifying first");
            assert_eq!(ids[2..6], [13, 14, 9, 10], "season {k}: play-off, then the last 16");
            assert_eq!(others(d), others(&out), "season {k}: the other competitions untouched");
            retime(&out, k, 2026, 1).unwrap_or_else(|e| panic!("season {k} retimed: {e}"));
        }
    }

    #[test]
    fn a_record_with_a_false_end_inside_it() {
        // 1-based home id 255 (00 ff) and away 0x0011: "00 ff 00 11 ff 00" has a false end at +1
        let mut d = vec![0x06, 0x00, 0x01, 0x00];
        d.extend_from_slice(&[0x00, 0xFF, 0x00, 0x11, 0xFF, 0x00]);
        d.extend_from_slice(&[0x06, 0x00, 0x02, 0x00]);
        d.extend_from_slice(&[0x00, 0x02, 0x00, 0xFF, 0xFF, 0x00]);
        d.extend_from_slice(&[0x00, 0xFF, 0x00, 0x03, 0xFF, 0x00]);
        let r = rounds(0, &d);
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!((r[0].league, r[0].fixtures.clone()), (Some(6), vec![(254, 16)]));
        assert_eq!((r[1].league, r[1].fixtures.clone()), (Some(6), vec![(1, 254), (254, 2)]));
    }
}
