//! Club divisions for the source's season (#31 item 7): every club of a country's league ladder is
//! re-seated from the source's league, keeping each division's size, because the game's fixtures
//! assume the 2005/06 sizes (England 20/24/24/24 and the Conference's 24).

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

/// One country's leagues, top to bottom, as `clubs.csv` numbers them (lma-data's `country` and
/// `division`), with the source league that fills each ("" for one the source doesn't have) and,
/// optionally, the live Transfermarkt competition code that fills each (`comps`, #699: "SC2" for
/// Scotland's second tier, which FC 27 lacks; live membership over the roster's where both name a club).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ladder {
    pub country: u32,
    pub divisions: Vec<u32>,
    pub leagues: Vec<String>,
    #[serde(default)]
    pub comps: Vec<String>,
}

/// An LMA club as the base export has it.
#[derive(Clone, Debug)]
pub struct Slot {
    pub id: u32,
    pub name: String,
    pub country: u32,
    pub division: u32,
    /// In a league at all (the historic sides, FA XI and Old Etonians, have no squad and no
    /// league data, yet read as country 0 division 0).
    pub playing: bool,
}

/// A club's new (country, division), and lines for the import report.
pub struct Reseat {
    pub moves: Vec<(u32, u32, u32)>,
    pub notes: Vec<String>,
    /// New clubs (`reseat`'s `new`) that won a place: (index, country, division).
    pub placed: Vec<(usize, u32, u32)>,
}

/// A place's claimant: an LMA club, or the source's club LMA lacks (by index in `new`).
#[derive(Clone, Copy)]
enum Claimant<'a> {
    Club(&'a Slot),
    New(usize),
}

/// Re-seats every ladder's clubs. A club the source places in one of the ladder's leagues takes a
/// place in that division; the others keep their order; each division keeps its size, so the
/// clubs pushed out of the bottom go to `overflow` (the non-league pool). A place the source's
/// league can't fill (a bigger block than the real league, a club LMA lacks) goes to a club the
/// source has in no league that plays in that division or above (it keeps its place: #699, Nancy
/// and Nantes were Ligue 1's fillers while the source had them in Ligue 2), else to the best of
/// the division below. `league_of` is each matched club's source league, `comp_of` its live
/// competition (over the league where a ladder names `comps`); `new` the leagues of the source's
/// clubs LMA lacks (Bromley, Girona): they claim places in their league after the LMA clubs the
/// source has there.
pub fn reseat(slots: &[Slot], ladders: &[Ladder], overflow: (u32, u32), league_of: &HashMap<u32, String>, comp_of: &HashMap<u32, String>, new: &[String]) -> Reseat {
    let mut out = Reseat { moves: Vec::new(), notes: Vec::new(), placed: Vec::new() };
    for ladder in ladders {
        let level_of = |c: &Slot| -> Option<usize> {
            let by_comp = comp_of.get(&c.id).and_then(|comp| ladder.comps.iter().position(|l| !l.is_empty() && l == comp));
            by_comp.or_else(|| {
                let league = league_of.get(&c.id)?;
                ladder.leagues.iter().position(|l| !l.is_empty() && l == league)
            })
        };
        let source_league = |c: &Slot| comp_of.get(&c.id).or_else(|| league_of.get(&c.id)).map_or("", |s| s.as_str());
        let here = |c: &Slot| c.playing && c.country == ladder.country && ladder.divisions.contains(&c.division);
        let sizes: Vec<usize> = ladder.divisions.iter().map(|d| slots.iter().filter(|c| here(c) && c.division == *d).count()).collect();
        // the ladder's clubs, and pool clubs the source has risen into it
        let clubs: Vec<&Slot> = slots
            .iter()
            .filter(|c| here(c) || (c.playing && (c.country, c.division) == overflow && level_of(c).is_some()))
            .collect();
        for c in slots.iter().filter(|c| c.playing && !here(c) && (c.country, c.division) != overflow && level_of(c).is_some()) {
            out.notes.push(format!("{} stays in its own country's leagues (the source has it in {})", c.name, source_league(c)));
        }
        let current = |c: &Slot| if c.country == ladder.country { ladder.divisions.iter().position(|d| *d == c.division).unwrap_or(usize::MAX) } else { usize::MAX };
        let new_level = |k: usize| ladder.leagues.iter().position(|l| !l.is_empty() && *l == new[k]);
        let mut claimants: Vec<Claimant> = clubs.into_iter().map(Claimant::Club).collect();
        claimants.extend((0..new.len()).filter(|&k| new_level(k).is_some()).map(Claimant::New));
        // by league; an LMA club before a new one; then where it plays now, and its id
        let key = |c: &Claimant| match c {
            Claimant::Club(s) => (level_of(s).unwrap_or(usize::MAX), current(s), s.id as u64),
            Claimant::New(k) => (new_level(*k).unwrap_or(usize::MAX), usize::MAX, u32::MAX as u64 + *k as u64),
        };
        claimants.sort_by_key(key);
        let level = |c: &Claimant| match c {
            Claimant::Club(s) => level_of(s),
            Claimant::New(k) => new_level(*k),
        };
        let mut rest = claimants;
        for (k, (&division, &size)) in ladder.divisions.iter().zip(&sizes).enumerate() {
            let sourced = ladder.leagues.get(k).is_some_and(|l| !l.is_empty()) || ladder.comps.get(k).is_some_and(|l| !l.is_empty());
            let taken = if sourced {
                // the source's clubs for this division; spare places: a club the source has in no
                // league keeps its place here (or rises from above), then the division below
                let mut t = take_where(&mut rest, size, |c| level(c) == Some(k));
                let spare = size - t.len();
                t.extend(take_where(&mut rest, spare, |c| matches!(c, Claimant::Club(s) if level_of(s).is_none() && current(s) <= k)));
                let spare = size - t.len();
                t.extend(take_where(&mut rest, spare, |_| true));
                t
            } else {
                take_where(&mut rest, size, |_| true)
            };
            let mut fallback = Vec::new();
            for c in taken {
                if sourced && level(&c) != Some(k) {
                    fallback.push(match c {
                        Claimant::Club(s) => s.name.clone(),
                        Claimant::New(n) => format!("new club {n}"),
                    });
                }
                match c {
                    Claimant::Club(c) if (c.country, c.division) != (ladder.country, division) => out.moves.push((c.id, ladder.country, division)),
                    Claimant::New(n) => out.placed.push((n, ladder.country, division)),
                    _ => {}
                }
            }
            if !fallback.is_empty() {
                out.notes.push(format!(
                    "country {} division {division}: {} place(s) the source's league couldn't fill: {}",
                    ladder.country,
                    fallback.len(),
                    fallback.join(", ")
                ));
            }
        }
        // pushed out of the bottom: LMA clubs to the pool; a new club simply isn't placed
        for c in rest {
            if let Claimant::Club(c) = c {
                if (c.country, c.division) != overflow {
                    out.moves.push((c.id, overflow.0, overflow.1));
                }
            }
        }
    }
    out
}

/// The game's league and cup code count on the id blocks: after the re-seat every league division
/// a ladder names must still hold the clubs vanilla gives it, each at an id of its own block, and
/// the cup draws read those counts (#735: the Portuguese League Cup draws 28 of Liga 2 + Primeira
/// and spun when the clubs' divisions held fewer). `moves` are `id_moves`' (to id, from id,
/// country, division). Returns one line per problem; empty when every block holds.
pub fn check_blocks(slots: &[Slot], ladders: &[Ladder], moves: &[(u32, u32, u32, u32)]) -> Vec<String> {
    use std::collections::BTreeMap;
    let league = |c: u32, d: u32| ladders.iter().any(|l| l.country == c && l.divisions.contains(&d));
    let mut now: HashMap<u32, (u32, u32)> = slots.iter().map(|s| (s.id, (s.country, s.division))).collect();
    for &(to, _, country, division) in moves {
        now.insert(to, (country, division));
    }
    let mut want: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    let mut have: BTreeMap<(u32, u32), usize> = BTreeMap::new();
    let mut problems = Vec::new();
    for s in slots.iter().filter(|s| league(s.country, s.division)) {
        *want.entry((s.country, s.division)).or_default() += 1;
        let at = now[&s.id];
        if at != (s.country, s.division) {
            problems.push(format!("id {} is in {}/{}, its block is {}/{}", s.id, at.0, at.1, s.country, s.division));
        }
    }
    for &(c, d) in now.values().filter(|&&(c, d)| league(c, d)) {
        *have.entry((c, d)).or_default() += 1;
    }
    for (&(c, d), &n) in &want {
        let h = have.get(&(c, d)).copied().unwrap_or(0);
        if h != n {
            problems.push(format!("country {c} division {d}: {h} clubs, the game's block holds {n} (its cup draws need them)"));
        }
    }
    problems
}

/// Removes and returns, in order, up to `n` claimants `pred` accepts.
fn take_where<'a>(rest: &mut Vec<Claimant<'a>>, n: usize, pred: impl Fn(&Claimant<'a>) -> bool) -> Vec<Claimant<'a>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() && out.len() < n {
        if pred(&rest[i]) {
            out.push(rest.remove(i));
        } else {
            i += 1;
        }
    }
    out
}

/// The game's leagues are id ranges (the Mac's proof, Agent sync 6096955231): a division is the
/// ids of the clubs in it, and a club changes division by taking an id there. Clubs that stay
/// keep their ids; in each division the arriving clubs take the leaving clubs' ids, in id order.
/// Returns `(to, from, country, division)` for every club that moves: id `to` takes the whole
/// club from id `from` (clubs.csv `record_of`), now in that country and division.
///
/// A new club placed by the re-seat (`placed`) takes a leaving id too, with the record of one of
/// `retirees` (2006 clubs with no source, outside the leagues) as its shell: every new club in a
/// league pushes one more LMA club into `overflow`, and those take the retirees' ids. Also
/// returns `(new club index, its id, its shell's old id)`.
pub fn id_moves(slots: &[Slot], moves: &[(u32, u32, u32)], placed: &[(usize, u32, u32)], overflow: (u32, u32), retirees: &[u32]) -> (Vec<(u32, u32, u32, u32)>, Vec<(usize, u32, u32)>) {
    let dest: HashMap<u32, (u32, u32)> = moves.iter().map(|&(id, c, d)| (id, (c, d))).collect();
    let mut leaving: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    let mut arriving: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    for s in slots {
        if let Some(&to) = dest.get(&s.id) {
            leaving.entry((s.country, s.division)).or_default().push(s.id);
            arriving.entry(to).or_default().push(s.id);
        }
    }
    let mut new_in: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for &(k, c, d) in placed {
        new_in.entry((c, d)).or_default().push(k);
    }
    let mut shells = retirees.iter().copied().take(placed.len());
    leaving.entry(overflow).or_default().extend(retirees.iter().copied().take(placed.len()));
    let mut keys: Vec<(u32, u32)> = arriving.keys().chain(new_in.keys()).copied().collect();
    keys.sort();
    keys.dedup();
    let (mut out, mut new_at) = (Vec::new(), Vec::new());
    for key in keys {
        let mut free = leaving.remove(&key).unwrap_or_default();
        free.sort();
        let mut ids = arriving.remove(&key).unwrap_or_default();
        ids.sort();
        let news = new_in.remove(&key).unwrap_or_default();
        // equal counts: the re-seat keeps every division's size, and each new club adds a retiree
        let mut free = free.into_iter();
        // arrivals first in the zip: it takes from its first iterator before the second, and a
        // free id must not be lost when there are no old arrivals left
        for (from, to) in ids.into_iter().zip(free.by_ref()) {
            out.push((to, from, key.0, key.1));
        }
        for (to, k) in free.zip(news) {
            let Some(shell) = shells.next() else { break };
            out.push((to, shell, key.0, key.1));
            new_at.push((k, to, shell));
        }
    }
    out.sort();
    (out, new_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(id: u32, name: &str, country: u32, division: u32) -> Slot {
        Slot { id, name: name.into(), country, division, playing: true }
    }

    fn ladder() -> Ladder {
        Ladder { country: 0, divisions: vec![0, 19, 11], leagues: vec!["Premier".into(), "Championship".into(), String::new()], comps: Vec::new() }
    }

    #[test]
    fn the_blocks_check_names_a_club_off_its_block() {
        let slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11), slot(9, "P", 15, 12)];
        // a re-seat's moves: C up to B's id, B down to C's: every block still holds
        let ok = [(1, 2, 0, 0), (2, 1, 0, 19)];
        assert!(check_blocks(&slots, &[ladder()], &ok).is_empty());
        // a pool club's record put at a league id with the pool's division: the league is a club short
        let bad = [(3, 9, 15, 12)];
        let p = check_blocks(&slots, &[ladder()], &bad);
        assert_eq!(p.len(), 2, "{p:?}");
        assert!(p[0].contains("id 3 is in 15/12") && p[1].contains("division 19: 1 clubs"), "{p:?}");
    }

    #[test]
    fn promotion_and_relegation_keep_the_sizes() {
        // two-club divisions: A, B top; C, D second; E, F third (the source has no third league)
        let slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11)];
        let league_of: HashMap<u32, String> = [(0, "Premier"), (1, "Championship"), (2, "Premier"), (3, "Championship")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let r = reseat(&slots, &[ladder()], (15, 12), &league_of, &HashMap::new(), &[]);
        // B down, C up; the third division is untouched
        assert_eq!(r.moves, vec![(2, 0, 0), (1, 0, 19)]);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        // as ids: C takes B's id in the top flight, B takes C's
        assert_eq!(id_moves(&slots, &r.moves, &r.placed, (15, 12), &[]).0, vec![(1, 2, 0, 0), (2, 1, 0, 19)]);
    }

    #[test]
    fn a_missing_clubs_place_goes_to_a_club_in_no_source_league_and_the_bottom_drops_to_the_pool() {
        // the source's top league has only A in LMA; its second league has C, D and a pool club P;
        // B (top today) is in none of the source's leagues
        let mut slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11)];
        slots.push(slot(6, "P", 15, 12));
        slots.push(Slot { playing: false, ..slot(7, "Old Etonians", 0, 0) });
        let league_of: HashMap<u32, String> = [(0, "Premier"), (2, "Championship"), (3, "Championship"), (6, "Championship")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let r = reseat(&slots, &[ladder()], (15, 12), &league_of, &HashMap::new(), &[]);
        // top: A and B (it keeps its place: #699); second: C, D; third: P (the source's club
        // over E and F, which it has in no league), E; F out
        assert_eq!(r.moves, vec![(6, 0, 11), (5, 15, 12)]);
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
        assert!(r.notes[0].contains("division 0: 1 place(s)") && r.notes[0].ends_with(": B"), "{:?}", r.notes);
    }

    #[test]
    fn a_missing_clubs_place_is_filled_from_below_when_no_unplaced_club_plays_there() {
        // the source relegates B; A is the top league's only other LMA club; E and F (third
        // division, no source league) don't rise over the source's second-league clubs
        let slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11)];
        let league_of: HashMap<u32, String> = [(0, "Premier"), (1, "Championship"), (2, "Championship"), (3, "Championship")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let r = reseat(&slots, &[ladder()], (15, 12), &league_of, &HashMap::new(), &[]);
        // top: A, B (the best of the division below: it plays top today); nothing else moves
        assert!(r.moves.is_empty(), "{:?}", r.moves);
        assert_eq!(r.notes.len(), 1, "{:?}", r.notes);
        assert!(r.notes[0].ends_with(": B"), "{:?}", r.notes);
    }

    /// `comps` (#699): the live competition places a club over the roster's league, and fills a
    /// division the roster has no league for.
    #[test]
    fn a_live_competition_places_a_club_over_the_roster_league() {
        let ladder = Ladder { country: 0, divisions: vec![0, 19, 11], leagues: vec!["Premier".into(), String::new(), String::new()], comps: vec!["X1".into(), "X2".into(), String::new()] };
        let slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11)];
        // the roster has B top, the live competitions have it second and E second
        let league_of: HashMap<u32, String> = [(0, "Premier"), (1, "Premier")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let comp_of: HashMap<u32, String> = [(1, "X2"), (2, "X1"), (4, "X2")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let r = reseat(&slots, &[ladder], (15, 12), &league_of, &comp_of, &[]);
        // top: A, C; second: B, E; third: D, F
        assert_eq!(r.moves, vec![(2, 0, 0), (1, 0, 19), (4, 0, 19), (3, 0, 11)]);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// A club the source has in the top league but LMA lacks (N) takes the place C would have
    /// filled from below: C stays in its own league, the bottom club (F) drops to the pool, onto
    /// the id of a retired club (R, outside the leagues), whose record is N's shell at C's old
    /// place... at B's: the top division's leaving id.
    #[test]
    fn a_new_club_takes_its_place_and_a_retired_clubs_record() {
        let mut slots = vec![slot(0, "A", 0, 0), slot(1, "B", 0, 0), slot(2, "C", 0, 19), slot(3, "D", 0, 19), slot(4, "E", 0, 11), slot(5, "F", 0, 11)];
        slots.push(slot(9, "R", 15, 12));
        let league_of: HashMap<u32, String> = [(0, "Premier"), (2, "Championship"), (3, "Championship")].into_iter().map(|(i, l)| (i, l.to_string())).collect();
        let r = reseat(&slots, &[ladder()], (15, 12), &league_of, &HashMap::new(), &["Premier".to_string()]);
        // top: A, N; second: C, D; third: B, E; F out
        assert_eq!(r.moves, vec![(1, 0, 11), (5, 15, 12)]);
        assert_eq!(r.placed, vec![(0, 0, 0)]);
        assert!(r.notes.is_empty(), "{:?}", r.notes);
        let (moves, new_at) = id_moves(&slots, &r.moves, &r.placed, (15, 12), &[9]);
        // B to F's id in the third division, F to R's in the pool, N at B's with R's record
        assert_eq!(moves, vec![(1, 9, 0, 0), (5, 1, 0, 11), (9, 5, 15, 12)]);
        assert_eq!(new_at, vec![(0, 1, 9)]);
    }
}
