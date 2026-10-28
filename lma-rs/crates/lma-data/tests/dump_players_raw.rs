//! Exploration helper (#31 item 5, skin tone): with `LMA_EXPORT=<Export.mng>` and
//! `LMA_RAW_OUT=<file>`, writes every player's whole record (hex) with his nation id, so the
//! appearance fields can be found among the record's unmapped bits by comparing nations. Silent
//! without the variables (no game data in the repo).
use lma_data::db::NATION;
use lma_data::Database;
use std::fmt::Write as _;

#[test]
fn dump_players_raw() {
    let (Ok(path), Ok(out)) = (std::env::var("LMA_EXPORT"), std::env::var("LMA_RAW_OUT")) else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    let (name, players) = ar.entries.iter().find(|(n, _)| n.to_ascii_lowercase().starts_with("player")).expect("no players table");
    let rs = u32::from_be_bytes(players[4..8].try_into().unwrap()) as usize;
    let n = u32::from_be_bytes(players[8..12].try_into().unwrap()) as usize;
    println!("{name}: {n} records of {rs} bytes");
    let db = Database::open(&path).unwrap();
    assert_eq!(db.player_count(), n);
    let mut text = format!("# {name} {n} records of {rs} bytes\nid,nation_id,record\n");
    for pid in 0..n {
        let rec = &players[12 + pid * rs..12 + (pid + 1) * rs];
        let _ = write!(text, "{pid},{},", db.field(pid, NATION));
        for b in rec {
            let _ = write!(text, "{b:02x}");
        }
        text.push('\n');
    }
    std::fs::write(&out, text).unwrap();
}
