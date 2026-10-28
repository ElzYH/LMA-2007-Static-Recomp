//! The shipped mapping (`mappings/fc27.toml`, #31) parses, and its tables agree with each other:
//! a misplaced line (a position under `[sides]`) or a label one table has and another lacks
//! would otherwise only show in a full import.
use lma_import::LMA_RATINGS;

const GROUPS: [&str; 4] = ["GK", "DEF", "MID", "ATT"];

/// A "main/second" mapping's groups are LMA's (second never a goalkeeper).
fn valid(mapped: &str) -> bool {
    let mut parts = mapped.split('/').map(str::trim);
    let main = parts.next().unwrap_or("");
    let second = parts.next();
    GROUPS.contains(&main) && second.is_none_or(|s| s != "GK" && GROUPS.contains(&s)) && parts.next().is_none()
}

#[test]
fn fc27_mapping_parses_and_its_tables_agree() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mappings/fc27.toml");
    let cfg: lma_import::Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    for (label, mapped) in &cfg.positions {
        assert!(valid(mapped), "[positions] {label} = {mapped:?}");
        assert!(cfg.sides.contains_key(label), "[sides] has no {label}");
    }
    for (label, sides) in &cfg.sides {
        assert!(cfg.positions.contains_key(label), "[sides] {label} isn't a position");
        assert!(!sides.is_empty() && sides.chars().all(|c| "LRC".contains(c)), "[sides] {label} = {sides:?}");
    }
    for table in [&cfg.ratings, &cfg.gk_ratings] {
        for r in LMA_RATINGS {
            assert!(table.contains_key(r), "a ratings table has no {r}");
        }
    }
    let tm = cfg.transfermarkt.as_ref().expect("[transfermarkt]");
    for (label, mapped) in &tm.positions {
        assert!(valid(mapped), "[transfermarkt.positions] {label} = {mapped:?}");
    }
    assert!((0.0..=1.0).contains(&tm.overall_weight));
    let fm = cfg.fm.as_ref().expect("[fm]");
    for table in [&fm.ratings, &fm.gk_ratings] {
        for r in LMA_RATINGS {
            assert!(table.contains_key(r), "an [fm] ratings table has no {r}");
        }
    }
    let skin = fm.skin.as_ref().expect("[fm.skin]");
    assert!(skin.lma.len() == 20 && skin.lma.iter().all(|v| *v <= 3), "[fm.skin] lma: {:?}", skin.lma);
    // FM's position labels land on positions the mapping knows
    let fill = fm.fill.as_ref().expect("[fm.fill]");
    for (label, fc) in &fill.labels {
        assert!(cfg.positions.contains_key(fc), "[fm.fill.labels] {label} = {fc:?}, not a position");
    }
    for ladder in &cfg.ladders {
        assert_eq!(ladder.divisions.len(), ladder.leagues.len(), "a ladder of country {} has a league per division", ladder.country);
    }
    // the 2026/27 start (#390; on since 10 Oct)
    assert_eq!(cfg.base_year, Some(2026));
    // stadium renames are keyed "club|stadium" (two clubs share Belle Vue), to a name the game
    // can hold (39 characters)
    for (key, new) in &cfg.stadium_renames {
        let parts: Vec<&str> = key.split('|').collect();
        assert!(parts.len() == 2 && parts.iter().all(|p| !p.trim().is_empty()), "[stadium_renames] {key:?} is not \"club|stadium\"");
        assert!(!new.trim().is_empty() && new.chars().count() <= 39, "[stadium_renames] {key} = {new:?}");
    }
    // the shipped mapping carries the sources' columns the CLI options need
    let tm = cfg.transfermarkt.as_ref().expect("[transfermarkt]");
    assert!(tm.club.is_some() && tm.stadiums.is_some(), "--tm-clubs needs [transfermarkt] club and [transfermarkt.stadiums]");
    assert!(tm.id.is_some() && tm.lineups.is_some(), "--tm-lineups needs [transfermarkt] id and [transfermarkt.lineups]");
    // the league histories: every mapped competition fills its own league slot (World.bin has
    // 8 countries x 5 tiers), the eight top flights
    let history = tm.history.as_ref().expect("[transfermarkt.history]");
    let slots: std::collections::BTreeSet<[usize; 2]> = history.leagues.values().copied().collect();
    assert_eq!(slots.len(), history.leagues.len(), "two competitions fill one league: {:?}", history.leagues);
    assert!(slots.iter().all(|&[c, t]| c < lma_data::db::HISTORY_COUNTRIES && t < lma_data::db::HISTORY_TIERS), "{slots:?}");
    assert_eq!(slots, (0..8).map(|c| [c, 0]).collect(), "the eight top flights");
}
