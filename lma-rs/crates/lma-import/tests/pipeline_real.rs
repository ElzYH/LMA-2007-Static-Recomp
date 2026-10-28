//! The whole pipeline on the user's own database (LMA_GAME_DIR, skipped without it): db-export,
//! squad-import with the shipped mapping and a small roster, db-import, rebuild. Checks what only
//! the real files show: the mod applies, the squad lands, the replaced records' 2006 loans go,
//! and the league histories move down by the base year with the clubs' titles kept.
use lma_data::Database;

fn export_mng() -> Option<std::path::PathBuf> {
    let dir = std::env::var("LMA_GAME_DIR").ok()?;
    let misc = std::path::Path::new(&dir).join("Common/GameData/Misc");
    [misc.join("Export.mng.orig"), misc.join("Export.mng")].into_iter().find(|p| p.exists())
}

#[test]
fn export_import_apply_rebuild() {
    let Some(mng) = export_mng() else { return };
    let orig = std::fs::read(&mng).unwrap();
    let db = Database::parse(&orig).unwrap();
    let tmp = std::env::temp_dir().join(format!("lma_pipeline_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let base = tmp.join("base");
    lma_data::edit::export(&db, &base).unwrap();
    assert!(base.join("history.csv").exists(), "the export carries the league histories");
    // Arsenal (club 0) gets a new squad; 2006's Arsenal had Gilbert and Muamba out on loan
    let loans_before = db.loans().unwrap().unwrap();
    assert!(loans_before.contains(&22) && loans_before.contains(&27));
    let mapping = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../mappings/fc27.toml");
    let mut cfg: lma_import::Config = toml::from_str(&std::fs::read_to_string(mapping).unwrap()).unwrap();
    // one club's roster: no league re-seat (it would move every club the roster lacks)
    cfg.ladders.clear();
    // every column the mapping's ratings read, at 70, and its filter column
    let mut cols: Vec<String> = cfg
        .ratings
        .values()
        .chain(cfg.gk_ratings.values())
        .flat_map(|s| match s {
            lma_import::Source::Columns(c) => c.clone(),
            lma_import::Source::Constant(_) => vec![],
        })
        .collect();
    cols.sort();
    cols.dedup();
    let filter = (cfg.filter_column.clone().unwrap_or_default(), cfg.filter_value.clone().unwrap_or_default());
    let mut roster = format!("first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,height_cm,preferred_foot,weak_foot,{},{}\n", filter.0, cols.join(","));
    // 28 players: Arsenal's records 0..27 all take one, Gilbert's (22) and Muamba's (27) included
    let positions = ["GK", "CB", "CB", "LB", "RB", "CDM", "CM", "CAM", "LW", "RW", "ST", "ST", "GK", "CB", "CM", "ST", "LM", "RM", "GK", "CB", "LB", "RB", "CM", "CDM", "CAM", "LW", "RW", "ST"];
    for (k, pos) in positions.iter().enumerate() {
        let ratings = vec!["70"; cols.len()].join(",");
        roster += &format!("Test{k},Player{k},,{}-0{}-1{},Arsenal,England,{pos},,{},180,Right,3,{},{ratings}\n", 1995 + k % 8, 1 + k % 9, k % 10, 70 + k % 15, filter.1);
    }
    std::fs::write(tmp.join("roster.csv"), roster).unwrap();
    let out = lma_import::import(&base, &tmp.join("roster.csv"), None, None, &cfg).unwrap();
    let mod_db = tmp.join("mod");
    std::fs::create_dir_all(&mod_db).unwrap();
    std::fs::write(mod_db.join("players.csv"), &out.players).unwrap();
    if let Some(c) = &out.clubs {
        std::fs::write(mod_db.join("clubs.csv"), c).unwrap();
    }
    std::fs::write(mod_db.join("history.csv"), out.history.expect("a history.csv from the base's")).unwrap();
    let mut db = Database::parse(&orig).unwrap();
    lma_data::edit::apply(&mut db, &mod_db).unwrap();
    let db = Database::parse(&db.build().unwrap()).unwrap();
    // the new squad, and its replaced records' loans gone
    let ours: Vec<usize> = db.squad(0).into_iter().filter(|&p| db.player(p).surname.starts_with("Player")).collect();
    assert!(ours.len() >= 11, "{} of the roster in Arsenal's squad", ours.len());
    let loans_after = db.loans().unwrap().unwrap();
    assert!(ours.iter().all(|p| !loans_after.contains(p)), "a replaced record kept its 2006 loan");
    assert!(ours.contains(&22) && ours.contains(&27), "Gilbert's and Muamba's records replaced: {ours:?}");
    assert_eq!(loans_after.len(), loans_before.len() - 2, "their two loans gone, every other kept");
    // the histories: 2005/06's champion (Chelsea) moved down by base_year - 2006 rows
    let shift = cfg.base_year.map_or(0, |y| y - 2006) as usize;
    let before = Database::parse(&orig).unwrap().league_history(0, 0).unwrap();
    let after = db.league_history(0, 0).unwrap();
    assert_eq!(after[shift], before[0], "Chelsea's 2005/06 row at row {shift}");
    assert_eq!(db.club_of_uid(after[shift].uid).map(|c| db.club_names[0][c].clone()).as_deref(), Some("Chelsea"));
    assert!(after[..shift].iter().all(|r| r.uid == 0), "no Transfermarkt games given: the new seasons blank");
    std::fs::remove_dir_all(&tmp).unwrap();
}
