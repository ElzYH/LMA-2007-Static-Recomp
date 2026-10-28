//! Needs the user's own game files: set LMA_GAME_DIR to the extracted game folder, and
//! LMA_PS2_EXPORT to the PS2 disc's `Export.mng` for the PS2 database (#3).
use lma_data::Database;

fn game_file(name: &str) -> Option<std::path::PathBuf> {
    let dir = std::env::var("LMA_GAME_DIR").ok()?;
    let p = std::path::Path::new(&dir).join("Common/GameData/Misc").join(name);
    p.exists().then_some(p)
}

/// The untouched database: `Export.mng.orig` where lma_edit.py installed an edit, else `Export.mng`.
fn vanilla() -> Option<std::path::PathBuf> {
    game_file("Export.mng.orig").or_else(|| game_file("Export.mng"))
}

#[test]
fn rebuilds_byte_identical() {
    for (name, path) in [("Export.mng", vanilla()), ("ExportPR.mng", game_file("ExportPR.mng"))] {
        let Some(p) = path else { eprintln!("skipping {name}: set LMA_GAME_DIR"); continue };
        let orig = std::fs::read(&p).unwrap();
        let mut db = Database::parse(&orig).unwrap();
        assert_eq!(db.build().unwrap(), orig, "{name} changed on rebuild");
    }
}

#[test]
fn known_players_decode() {
    let Some(p) = vanilla() else { return };
    let db = Database::open(p).unwrap();
    let henry = db.player(9);
    assert_eq!(henry.display_name(), "Thierry Henry");
    assert_eq!((henry.age, henry.squad_number, henry.height_cm), (28, 14, 188));
    assert_eq!(henry.ratings[11], 100); // pace
    assert_eq!(db.club_name(0), "Arsenal");
    assert_eq!(db.manager_surname(0), "Wenger");
    assert_eq!(db.stands(0), Some([13000, 17000, 13000, 17000]));
    assert_eq!(db.club_wealth(0), 15);
}

#[test]
fn edits_survive_rebuild() {
    let Some(p) = vanilla() else { return };
    let mut db = Database::open(p).unwrap();
    db.set_name(9, "Bukayo", "Saka").unwrap();
    db.set_field(9, lma_data::db::AGE, 23).unwrap();
    let bytes = db.build().unwrap();
    let db2 = Database::parse(&bytes).unwrap();
    assert_eq!(db2.player(9).display_name(), "Bukayo Saka");
    assert_eq!(db2.player(9).age, 23);
}

/// clubs.csv record_of (#31 item 7): swapping Blackburn (2) and Sunderland (41) moves exactly
/// those two clubs, records and names, as the Mac's in-game proof did by hand; a second swap back
/// gives the disc's file again.
#[test]
fn record_of_swaps_two_clubs_and_back() {
    let Some(p) = vanilla() else { return };
    let orig = std::fs::read(&p).unwrap();
    let mut db = Database::parse(&orig).unwrap();
    db.move_clubs(&[(2, 41), (41, 2)]).unwrap();
    let swapped = Database::parse(&db.build().unwrap()).unwrap();
    assert_eq!((swapped.club_name(2), swapped.club_name(41)), ("Sunderland", "Blackburn Rovers"));
    let before = Database::parse(&orig).unwrap();
    assert_eq!((swapped.squad(2), swapped.squad(41)), (before.squad(41), before.squad(2)));
    assert_eq!((swapped.club_wealth(2), swapped.club_division(2)), (before.club_wealth(41), before.club_division(41)));
    // the game pairs stadiums with clubs by id (#389): Sunderland takes the Stadium of Light along
    assert_eq!((swapped.stadium_names[2].as_str(), swapped.stands(2)), (before.stadium_names[41].as_str(), before.stands(41)));
    assert_eq!((swapped.manager_surname(2), swapped.manager_surname(41)), (before.manager_surname(41), before.manager_surname(2)), "and the managers");
    for ci in (0..before.club_count()).filter(|&c| c != 2 && c != 41) {
        assert_eq!((swapped.club_name(ci), swapped.squad(ci)), (before.club_name(ci), before.squad(ci)), "club {ci} moved");
    }
    db.move_clubs(&[(2, 41), (41, 2)]).unwrap();
    assert_eq!(db.build().unwrap(), orig, "two swaps are no change");
}

#[test]
fn csv_export_then_import_is_byte_identical() {
    for (name, path) in [("Export.mng", vanilla()), ("ExportPR.mng", game_file("ExportPR.mng"))] {
        let Some(p) = path else { continue };
        let orig = std::fs::read(&p).unwrap();
        let dir = std::env::temp_dir().join(format!("lma-csv-roundtrip-{name}-{}", std::process::id()));
        lma_data::edit::export(&Database::parse(&orig).unwrap(), &dir).unwrap();
        let mut db = Database::parse(&orig).unwrap();
        assert!(lma_data::edit::apply(&mut db, &dir).unwrap().is_empty(), "{name}: name pools grew");
        assert_eq!(db.build().unwrap(), orig, "{name} changed after a CSV round trip");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// The PS2's database (`MG2`, little-endian, #3), from LMA_PS2_EXPORT.
fn ps2() -> Option<std::path::PathBuf> {
    let p = std::path::PathBuf::from(std::env::var("LMA_PS2_EXPORT").ok()?);
    p.exists().then_some(p)
}

#[test]
fn ps2_rebuilds_byte_identical_and_decodes() {
    let Some(p) = ps2() else { eprintln!("skipping the PS2 database: set LMA_PS2_EXPORT"); return };
    let orig = std::fs::read(&p).unwrap();
    let mut db = Database::parse(&orig).unwrap();
    assert_eq!(db.endian(), lma_data::Endian::Little);
    assert_eq!(db.build().unwrap(), orig, "the PS2 database changed on rebuild");
    // the same player as on the 360, through the PS2's own bit layout and name pools
    let henry = db.player(9);
    assert_eq!(henry.display_name(), "Thierry Henry");
    assert_eq!((henry.age, henry.squad_number, henry.height_cm), (28, 14, 188));
    assert_eq!(henry.ratings[11], 100); // pace
    assert_eq!(db.player(0).display_name(), "Jens Lehmann");
    assert_eq!(db.club_name(0), "Arsenal");
    assert_eq!(db.squad(0)[0], 0, "Lehmann leads Arsenal's squad list");
    assert_eq!(db.manager_surname(0), "Wenger");
    assert_eq!(db.stands(0), Some([13000, 17000, 13000, 17000]));
    assert_eq!(db.club_wealth(0), 15);
}

#[test]
fn ps2_edits_and_csv_round_trip() {
    let Some(p) = ps2() else { return };
    let orig = std::fs::read(&p).unwrap();
    let mut db = Database::parse(&orig).unwrap();
    db.set_name(9, "Bukayo", "Saka").unwrap();
    db.set_field(9, lma_data::db::AGE, 23).unwrap();
    db.set_field(9, lma_data::db::HEIGHT, 178).unwrap();
    db.set_club_wealth(0, 7).unwrap();
    let mut back = Database::parse(&db.build().unwrap()).unwrap();
    assert_eq!(back.player(9).display_name(), "Bukayo Saka");
    assert_eq!((back.player(9).age, back.player(9).height_cm), (23, 178));
    assert_eq!(back.player(9).ratings, db.player(9).ratings, "neighbouring fields untouched");
    assert_eq!(back.club_wealth(0), 7);
    assert_eq!(back.squad(0), db.squad(0));
    let _ = back.build().unwrap();

    let dir = std::env::temp_dir().join(format!("lma-csv-roundtrip-ps2-{}", std::process::id()));
    lma_data::edit::export(&Database::parse(&orig).unwrap(), &dir).unwrap();
    let nations = std::fs::read_to_string(dir.join("nations.csv")).unwrap();
    assert!(nations.contains("
11,England,") && nations.contains("
12,France,"), "the PS2's own nation ids");
    let mut db = Database::parse(&orig).unwrap();
    assert!(lma_data::edit::apply(&mut db, &dir).unwrap().is_empty(), "PS2: name pools grew");
    assert_eq!(db.build().unwrap(), orig, "the PS2 database changed after a CSV round trip");
    std::fs::remove_dir_all(&dir).unwrap();
}

/// A players.csv that leaves a club under 11 players is refused (#31: fc27final3's 1-player
/// squads hung careers in the match engine's event picker); 11 left is fine.
#[test]
fn an_apply_never_leaves_a_club_under_eleven() {
    let Some(p) = vanilla() else { return };
    let mut db = Database::open(&p).unwrap();
    let arsenal = db.squad(0);
    let dir = std::env::temp_dir().join(format!("lma-squad-floor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let csv = |keep: usize| {
        let mut s = String::from("id,club_id\n");
        for pid in &arsenal[keep..] {
            s += &format!("{pid},\n");
        }
        s
    };
    std::fs::write(dir.join("players.csv"), csv(10)).unwrap();
    let err = lma_data::edit::apply(&mut db, &dir).unwrap_err().to_string();
    assert!(err.contains("club 0 (Arsenal): 10 players left"), "{err}");
    let mut db = Database::open(&p).unwrap();
    std::fs::write(dir.join("players.csv"), csv(11)).unwrap();
    lma_data::edit::apply(&mut db, &dir).unwrap();
    assert_eq!(db.squad(0).len(), 11);
    std::fs::remove_dir_all(&dir).unwrap();
}
