//! Test fixtures: a tiny hand-built database other crates' tests can load without the user's
//! game files. Compiled for this crate's tests and for dependants with the `test-fixtures` feature.
use crate::db::{AGE, FORENAME, HEIGHT, LANGS, POSITION, RATINGS, SURNAME};
use crate::Database;

fn pool(names: &[&str]) -> Vec<u8> {
    let mut offs = Vec::new();
    let mut text = Vec::new();
    for n in names {
        offs.extend(((4 * names.len() + text.len()) as u32).to_be_bytes());
        text.extend(n.bytes());
        text.push(0);
    }
    offs.extend(text);
    offs
}

fn table(magic: u32, rs: usize, n: usize) -> Vec<u8> {
    let mut t: Vec<u8> = [magic, rs as u32, n as u32].iter().flat_map(|v| v.to_be_bytes()).collect();
    t.resize(12 + rs * n, 0);
    t
}

fn fixed(names: &[&str], size: usize) -> Vec<u8> {
    names.iter().flat_map(|n| {
        let mut b = n.as_bytes().to_vec();
        b.resize(size, 0);
        b
    }).collect()
}

/// A tiny database: Arsenal (Henry, Cole), Everton (Moyes Jr), a placeholder club, one free agent.
pub fn fake_db() -> Database {
    fake_db_with(4)
}

/// `fake_db` with `players` player records (the first four as above, the rest blank), for
/// tests that need a club with a full squad.
pub fn fake_db_with(players: usize) -> Database {
    assert!(players >= 4);
    let clubs = ["Arsenal", "Everton", "Wanderers"];
    let mut files = vec![
        ("Forename.bin".to_string(), pool(&["", "Thierry", "Ashley"])),
        ("Surnames.bin".into(), pool(&["Henry", "Cole", "Wenger", "Moyes", "Free"])),
        ("Players.bin".into(), table(0x4C3E_B25B, 40, players)),
        ("Clubs.bin".into(), table(0x5ABE_B540, 84, 3)),
        ("StadName.bin".into(), fixed(&["Highbury", "Goodison Park"], 40)),
        ("Stadiums.bin".into(), vec![0; 2 * 32]),
        // one per club, then two unattached (the job market)
        ("Managers.bin".into(), vec![0; 5 * 36]),
    ];
    for l in LANGS {
        files.push((format!("ClubNam{l}.bin"), fixed(&clubs, 26)));
        files.push((format!("ShortNm{l}.bin"), fixed(&["ARS", "EVE", "WAN"], 8)));
    }
    // BG2 archive: header, 0x108-byte entries, data at 0x800-aligned offsets
    let data_start = (0x14 + files.len() * 0x108).next_multiple_of(0x800);
    let mut head = b"BG2\0".to_vec();
    let mut body = Vec::new();
    let mut entries = Vec::new();
    for (name, data) in &files {
        let mut e = name.as_bytes().to_vec();
        e.resize(0x100, 0);
        e.extend((body.len() as u32).to_be_bytes());
        e.extend((data.len() as u32).to_be_bytes());
        entries.extend(e);
        body.extend(data);
        body.resize(body.len().next_multiple_of(0x800), 0);
    }
    for v in [0x14, entries.len(), data_start, body.len()] {
        head.extend((v as u32).to_be_bytes());
    }
    head.extend(entries);
    head.resize(data_start, 0);
    head.extend(body);
    let mut db = Database::parse(&head).unwrap();
    for (pid, (fore, sur, pos)) in [(1, 0, 3), (2, 1, 1), (0, 3, 2), (0, 4, 0)].into_iter().enumerate() {
        db.set_field(pid, FORENAME, fore).unwrap();
        db.set_field(pid, SURNAME, sur).unwrap();
        db.set_field(pid, POSITION, pos).unwrap();
        db.set_field(pid, AGE, 25).unwrap();
        db.set_field(pid, HEIGHT, 180).unwrap();
        for f in RATINGS {
            db.set_field(pid, f, 50 + pid as u32).unwrap();
        }
    }
    db.set_squad(0, &[0, 1]).unwrap();
    db.set_squad(1, &[2]).unwrap();
    db.set_stands(0, [13000, 17000, 13000, 17000]);
    db.set_manager_surname(0, "Wenger").unwrap();
    db.set_club_wealth(0, 15).unwrap();
    Database::parse(&db.build().unwrap()).unwrap()
}
