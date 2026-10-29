//! The European Cup's 2024+ league phase draw (#703 step 3): 36 clubs in 4 pots of 9, each club
//! meeting two clubs of every pot (its own included), one at home and one away, never a club of its
//! own nation and at most two of any other nation, played over 8 matchdays of 18 games in which
//! every club plays once.
//!
//! The draw is native (the game's group draw, 0x82134480, writes 8 groups of 4); the runtime hook
//! writes the result into the diary's blank league-phase games (`diary::league_phase`). It is
//! deterministic in its seed, so a deterministic run draws the same league every time.

use std::collections::HashMap;

/// Clubs in the league phase, pots, and clubs a pot.
pub const CLUBS: usize = 36;
pub const POTS: usize = 4;
pub const POT: usize = 9;
/// Matchdays, and games a matchday.
pub const DAYS: usize = 8;
pub const GAMES: usize = CLUBS / 2;
/// Opponents a club may have from any one other nation.
pub const SAME_NATION_CAP: usize = 2;

/// A game: (home, away), as indices into the draw's club list (pot k = indices 9k..9k+9).
pub type Game = (usize, usize);

/// xorshift64*: small, and the same on every machine.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            v.swap(i, self.below(i + 1));
        }
    }
}

/// The league phase for 36 clubs given by nation (`nations[k]`, pot `k / 9`): 8 matchdays of 18
/// games, or None when no draw keeps the nation rules (after a bounded search).
pub fn draw(nations: &[u16; CLUBS], seed: u64) -> Option<Vec<Vec<Game>>> {
    let mut rng = Rng::new(seed);
    for _ in 0..2000 {
        let Some(games) = opponents(nations, &mut rng) else { continue };
        for _ in 0..50 {
            if let Some(days) = schedule(&games, &mut rng) {
                return Some(days);
            }
        }
    }
    None
}

/// The 144 games: each club two opponents per pot, one home and one away, within the nation rules.
/// Piece by piece (each pot's own games, then each pair of pots), every piece drawn until it fits
/// the opponents' nations so far.
fn opponents(nations: &[u16; CLUBS], rng: &mut Rng) -> Option<Vec<Game>> {
    let mut count: HashMap<(usize, u16), usize> = HashMap::new();
    let mut games = Vec::with_capacity(CLUBS * DAYS / 2);
    // adds `new` to the counts if every game in it keeps the rules
    let take = |count: &mut HashMap<(usize, u16), usize>, new: &[Game]| {
        let mut c = count.clone();
        for &(h, a) in new {
            for (x, o) in [(h, a), (a, h)] {
                if nations[x] == nations[o] {
                    return false;
                }
                let n = c.entry((x, nations[o])).or_insert(0);
                *n += 1;
                if *n > SAME_NATION_CAP {
                    return false;
                }
            }
        }
        *count = c;
        true
    };
    for p in 0..POTS {
        // within the pot: a 9-cycle, each club home to the next
        let mut order: Vec<usize> = (p * POT..(p + 1) * POT).collect();
        let piece = (0..500).find_map(|_| {
            rng.shuffle(&mut order);
            let new: Vec<Game> = (0..POT).map(|k| (order[k], order[(k + 1) % POT])).collect();
            take(&mut count, &new).then_some(new)
        })?;
        games.extend(piece);
    }
    for p in 0..POTS {
        for q in p + 1..POTS {
            // against a later pot: two matchings, s (pot p at home) and t (pot q at home), with no
            // club meeting the same opponent twice
            let mut s: Vec<usize> = (0..POT).collect();
            let mut t: Vec<usize> = (0..POT).collect();
            let piece = (0..2000).find_map(|_| {
                rng.shuffle(&mut s);
                rng.shuffle(&mut t);
                if (0..POT).any(|x| s[x] == t[x]) {
                    return None;
                }
                let new: Vec<Game> = (0..POT).map(|x| (p * POT + x, q * POT + s[x])).chain((0..POT).map(|x| (q * POT + t[x], p * POT + x))).collect();
                take(&mut count, &new).then_some(new)
            })?;
            games.extend(piece);
        }
    }
    Some(games)
}

/// The games split into 8 matchdays in which every club plays once (a perfect matching a day).
fn schedule(games: &[Game], rng: &mut Rng) -> Option<Vec<Vec<Game>>> {
    let mut left: Vec<Game> = games.to_vec();
    let mut days = Vec::with_capacity(DAYS);
    for _ in 0..DAYS {
        rng.shuffle(&mut left);
        let mut used = [false; CLUBS];
        let mut pick = Vec::with_capacity(GAMES);
        let mut steps = 0;
        if !matching(&left, &mut used, &mut pick, &mut steps) {
            return None;
        }
        let mut day: Vec<Game> = pick.iter().map(|&i| left[i]).collect();
        pick.sort_unstable_by(|a, b| b.cmp(a));
        for i in pick {
            left.swap_remove(i);
        }
        day.sort_unstable();
        days.push(day);
    }
    Some(days)
}

/// A perfect matching from `games` (indices into it in `pick`), by backtracking from the club with
/// the fewest choices; gives up after a bounded number of steps.
fn matching(games: &[Game], used: &mut [bool; CLUBS], pick: &mut Vec<usize>, steps: &mut u32) -> bool {
    if pick.len() == GAMES {
        return true;
    }
    *steps += 1;
    if *steps > 20_000 {
        return false;
    }
    let choices = |c: usize, used: &[bool; CLUBS]| -> Vec<usize> { (0..games.len()).filter(|&i| (games[i].0 == c || games[i].1 == c) && !used[games[i].0] && !used[games[i].1]).collect() };
    let Some(club) = (0..CLUBS).filter(|&c| !used[c]).min_by_key(|&c| choices(c, used).len()) else { return true };
    let options = choices(club, used);
    for i in options {
        let (h, a) = games[i];
        used[h] = true;
        used[a] = true;
        pick.push(i);
        if matching(games, used, pick, steps) {
            return true;
        }
        pick.pop();
        used[h] = false;
        used[a] = false;
    }
    false
}

/// A played game: (home, away, home goals, away goals), clubs as in the draw.
pub type Score = (usize, usize, u8, u8);

/// A club's line in the league phase's table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub club: usize,
    pub played: u8,
    pub won: u8,
    pub drawn: u8,
    pub lost: u8,
    pub scored: u16,
    pub conceded: u16,
    pub away_scored: u16,
    pub away_won: u8,
}

impl Row {
    pub fn points(&self) -> u16 {
        3 * self.won as u16 + self.drawn as u16
    }

    pub fn difference(&self) -> i32 {
        self.scored as i32 - self.conceded as i32
    }
}

/// The 36-club table from the games played so far, in order: points, goal difference, goals scored,
/// away goals scored, wins, away wins (UEFA's league-phase order), then the club's index, which is
/// its seeding (pot order follows the coefficient), for UEFA's coefficient tie-break.
pub fn table(games: &[Score]) -> Vec<Row> {
    let mut rows: Vec<Row> = (0..CLUBS).map(|club| Row { club, ..Row::default() }).collect();
    for &(h, a, hg, ag) in games {
        for (c, f, g, home) in [(h, hg, ag, true), (a, ag, hg, false)] {
            let r = &mut rows[c];
            r.played += 1;
            r.scored += f as u16;
            r.conceded += g as u16;
            if !home {
                r.away_scored += f as u16;
            }
            match f.cmp(&g) {
                std::cmp::Ordering::Greater => {
                    r.won += 1;
                    r.away_won += !home as u8;
                }
                std::cmp::Ordering::Equal => r.drawn += 1,
                std::cmp::Ordering::Less => r.lost += 1,
            }
        }
    }
    rows.sort_by_key(|r| (std::cmp::Reverse((r.points(), r.difference(), r.scored, r.away_scored, r.won, r.away_won)), r.club));
    rows
}

/// A knockout tie: `first` is at home in the first leg, `second` (the seeded club) in the second.
/// `bracket` 0-3 says which pair of the top 8 its winner meets in the last 16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tie {
    pub first: usize,
    pub second: usize,
    pub bracket: usize,
}

/// The play-off from the final table's clubs (`order[0]` first): 9th-16th are seeded against
/// 17th-24th by pairs of places, 15th/16th v 17th/18th (bracket 0), 13th/14th v 19th/20th (1),
/// 11th/12th v 21st/22nd (2), 9th/10th v 23rd/24th (3), each pair drawn; the seeded club is at home
/// in the second leg.
pub fn playoff(order: &[usize], seed: u64) -> Vec<Tie> {
    let mut rng = Rng::new(seed);
    let mut ties = Vec::with_capacity(8);
    for bracket in 0..4 {
        let seeded = [order[14 - 2 * bracket], order[15 - 2 * bracket]];
        let mut unseeded = [order[16 + 2 * bracket], order[17 + 2 * bracket]];
        rng.shuffle(&mut unseeded);
        ties.extend(seeded.iter().zip(unseeded).map(|(&s, u)| Tie { first: u, second: s, bracket }));
    }
    ties
}

/// The last 16: each play-off bracket's two winners drawn against a pair of the top 8, bracket 0
/// against 1st/2nd, 1 against 3rd/4th, 2 against 5th/6th, 3 against 7th/8th; the top-8 club is at
/// home in the second leg. `winners[k]` is the winner of `ties[k]`.
pub fn last_16(order: &[usize], ties: &[Tie], winners: &[usize], seed: u64) -> Vec<Tie> {
    let mut rng = Rng::new(seed);
    let mut out = Vec::with_capacity(8);
    for bracket in 0..4 {
        let top = [order[2 * bracket], order[2 * bracket + 1]];
        let mut from: Vec<usize> = ties.iter().zip(winners).filter(|(t, _)| t.bracket == bracket).map(|(_, &w)| w).collect();
        rng.shuffle(&mut from);
        out.extend(top.iter().zip(from).map(|(&t, w)| Tie { first: w, second: t, bracket }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rule of the league phase on a drawn result.
    fn check(nations: &[u16; CLUBS], days: &[Vec<Game>]) {
        assert_eq!(days.len(), DAYS);
        let mut met = vec![Vec::new(); CLUBS];
        let mut home = [[0usize; POTS]; CLUBS];
        let mut away = [[0usize; POTS]; CLUBS];
        for (d, day) in days.iter().enumerate() {
            assert_eq!(day.len(), GAMES, "matchday {d}");
            let mut plays = [0; CLUBS];
            for &(h, a) in day {
                plays[h] += 1;
                plays[a] += 1;
                met[h].push(a);
                met[a].push(h);
                home[h][a / POT] += 1;
                away[a][h / POT] += 1;
                assert_ne!(nations[h], nations[a], "matchday {d}: {h} v {a} of one nation");
            }
            assert!(plays.iter().all(|&n| n == 1), "matchday {d}: every club once");
        }
        for c in 0..CLUBS {
            let mut o = met[c].clone();
            o.sort_unstable();
            o.dedup();
            assert_eq!(o.len(), DAYS, "club {c}: 8 different opponents");
            assert!((0..POTS).all(|p| home[c][p] == 1 && away[c][p] == 1), "club {c}: one home and one away a pot");
            let mut by_nation = std::collections::HashMap::new();
            for &x in &met[c] {
                *by_nation.entry(nations[x]).or_insert(0) += 1;
            }
            assert!(by_nation.values().all(|&n| n <= SAME_NATION_CAP), "club {c}: at most 2 of a nation");
        }
    }

    /// A 2025/26-like field: 6 English, 4 each of Spain, Italy and Germany, 3 French, 2 Portuguese and
    /// Dutch, the rest one a nation, spread over the pots.
    fn realistic() -> [u16; CLUBS] {
        [1, 2, 3, 4, 1, 2, 3, 5, 6, 1, 2, 3, 4, 5, 6, 7, 8, 9, 1, 4, 3, 2, 10, 11, 12, 13, 14, 1, 4, 15, 16, 17, 18, 19, 20, 1]
    }

    #[test]
    fn a_league_phase_keeps_every_rule() {
        let nations = realistic();
        for seed in 0..30 {
            let days = draw(&nations, seed).unwrap_or_else(|| panic!("seed {seed}: no draw"));
            check(&nations, &days);
        }
    }

    #[test]
    fn the_draw_is_deterministic_in_its_seed() {
        let nations = realistic();
        assert_eq!(draw(&nations, 7), draw(&nations, 7));
        assert_ne!(draw(&nations, 7), draw(&nations, 8));
    }

    #[test]
    fn one_nation_everywhere_has_no_draw() {
        assert_eq!(draw(&[3; CLUBS], 1), None);
    }

    #[test]
    fn nine_nations_of_four_one_a_pot() {
        let nations: [u16; CLUBS] = std::array::from_fn(|k| (k % POT) as u16);
        check(&nations, &draw(&nations, 42).unwrap());
    }

    #[test]
    fn the_table_orders_by_points_then_the_tie_breaks() {
        // 0 beats 1 at home 2-0; 2 beats 3 away 0-2: same points, difference and goals, 2's goals came away
        let t = table(&[(0, 1, 2, 0), (3, 2, 0, 2), (4, 5, 1, 1)]);
        assert_eq!((t[0].club, t[1].club), (2, 0), "away goals");
        assert_eq!((t[0].points(), t[0].away_won), (3, 1));
        // 4 and 5 drew 1-1 at 4's: one point each, above every club without one; 5's goal was away
        assert_eq!((t[2].club, t[3].club), (5, 4));
        assert_eq!(t[2].points(), 1);
        assert_eq!(t[35].club, 3, "the losers last, the bigger defeat at the bottom");
        assert_eq!(t.len(), CLUBS);
        // goal difference before goals scored
        let t = table(&[(6, 7, 3, 2), (8, 9, 1, 0)]);
        assert_eq!((t[0].club, t[1].club), (6, 8), "+1 each, 6 scored more");
    }

    #[test]
    fn the_play_off_and_the_last_16_follow_the_brackets() {
        let order: Vec<usize> = (0..CLUBS).rev().collect(); // club 35 top, club 0 bottom
        let ties = playoff(&order, 3);
        assert_eq!(ties.len(), 8);
        for t in &ties {
            let (s, u) = (order.iter().position(|&c| c == t.second).unwrap(), order.iter().position(|&c| c == t.first).unwrap());
            assert!((8..16).contains(&s) && (16..24).contains(&u), "seeded 9-16 at home second, 17-24 first");
            assert_eq!(t.bracket, (15 - s) / 2, "place {s} in bracket {}", t.bracket);
            assert_eq!((u - 16) / 2, t.bracket);
        }
        // the seeded clubs all go through
        let winners: Vec<usize> = ties.iter().map(|t| t.second).collect();
        let l16 = last_16(&order, &ties, &winners, 3);
        assert_eq!(l16.len(), 8);
        for t in &l16 {
            let top = order.iter().position(|&c| c == t.second).unwrap();
            assert!(top < 8 && top / 2 == t.bracket);
            let from = ties.iter().find(|x| x.second == t.first).unwrap();
            assert_eq!(from.bracket, t.bracket, "1st/2nd meet the winners of 15th/16th v 17th/18th");
        }
        let mut met: Vec<usize> = l16.iter().flat_map(|t| [t.first, t.second]).collect();
        met.sort_unstable();
        met.dedup();
        assert_eq!(met.len(), 16, "every club once");
        assert_eq!(playoff(&order, 3), ties, "deterministic");
    }

}
