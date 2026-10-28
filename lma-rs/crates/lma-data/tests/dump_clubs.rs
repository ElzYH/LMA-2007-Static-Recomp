//! Exploration helper (#31, the 2026/27 start): with `LMA_EXPORT=<Export.mng>` prints the
//! archive's files and the raw club records of clubs in known 2006/07 divisions, to find the
//! division/competition field. Silent without the variable (no game data in the repo).
use lma_data::Database;

#[test]
fn dump_clubs() {
    let Ok(path) = std::env::var("LMA_EXPORT") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    for (name, data) in &ar.entries {
        let magic = if data.len() >= 12 { u32::from_be_bytes([data[0], data[1], data[2], data[3]]) } else { 0 };
        let rs = if data.len() >= 12 { u32::from_be_bytes([data[4], data[5], data[6], data[7]]) } else { 0 };
        let n = if data.len() >= 12 { u32::from_be_bytes([data[8], data[9], data[10], data[11]]) } else { 0 };
        println!("file {name:14} {:8} bytes  magic {magic:#010x} rs {rs} n {n}", data.len());
    }
    let db = Database::open(&path).unwrap();
    let want = ["Arsenal", "Chelsea", "Sunderland", "Leeds United", "Birmingham City", "Nottingham Forest", "Scunthorpe United", "Hereford United", "Celtic", "Rangers", "Barcelona", "Real Madrid", "Juventus", "Bayern Munich", "Lyon", "Ajax"];
    let clubs = ar.get("Clubs.bin").unwrap();
    let rs = u32::from_be_bytes([clubs[4], clubs[5], clubs[6], clubs[7]]) as usize;
    for ci in 0..db.club_count() {
        let name = db.club_name(ci);
        if want.iter().any(|w| name == *w) {
            let rec = &clubs[12 + ci * rs..12 + (ci + 1) * rs];
            println!("club {ci:4} {name:20} wealth {:2} {}", db.club_wealth(ci), rec.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
        }
    }
}

/// Groups every club by the (byte 3, byte 7) pair of its record (country and division candidates)
/// and prints the group sizes with sample names; then the heads of World.bin and Diary20.mng.
#[test]
fn group_clubs() {
    let Ok(path) = std::env::var("LMA_EXPORT") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    let db = Database::open(&path).unwrap();
    let clubs = ar.get("Clubs.bin").unwrap();
    let rs = u32::from_be_bytes([clubs[4], clubs[5], clubs[6], clubs[7]]) as usize;
    let mut groups: std::collections::BTreeMap<(u8, u8), Vec<String>> = std::collections::BTreeMap::new();
    for ci in 0..db.club_count() {
        if db.is_placeholder(ci) {
            continue;
        }
        let rec = &clubs[12 + ci * rs..12 + (ci + 1) * rs];
        groups.entry((rec[3], rec[7])).or_default().push(db.club_name(ci).to_string());
    }
    for ((c, d), names) in &groups {
        println!("group country {c:3} div {d:#04x} ({:2}): {} clubs: {}", d >> 2, names.len(), names.iter().take(6).cloned().collect::<Vec<_>>().join(", "));
    }
    let world = ar.get("World.bin").unwrap();
    println!("World.bin head: {}", world[..96].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
    // 16-bit big-endian values that look like years or day counts
    let mut years = Vec::new();
    for i in (0..world.len() - 1).step_by(1) {
        let v = u16::from_be_bytes([world[i], world[i + 1]]);
        if (1990..=2030).contains(&v) {
            years.push((i, v));
        }
    }
    println!("World.bin 16-bit values 1990..2030: {:?}", &years[..years.len().min(20)]);
    if let Some(diary) = ar.get("Diary20.mng") {
        match lma_data::archive::Archive::parse(diary) {
            Ok(d) => {
                for (n, data) in &d.entries {
                    println!("diary entry {n:14} {} bytes head {}", data.len(), data[..data.len().min(24)].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
                }
            }
            Err(e) => println!("Diary20.mng is not an archive: {e:?}; head {}", diary[..48].iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")),
        }
    }
}

/// The season diaries' encoding: TW blocks as u32 lists, the first rows of two diaries, the
/// offsets where consecutive diaries differ, and World.bin's leading words and ASCII runs.
#[test]
fn dump_diary() {
    let Ok(path) = std::env::var("LMA_EXPORT") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    let diary = lma_data::archive::Archive::parse(ar.get("Diary20.mng").unwrap()).unwrap();
    let get = |n: &str| diary.get(n).unwrap();
    for n in ["TW0000.bin", "TW0001.bin", "TW0019.bin"] {
        let d = get(n);
        println!("{n}: {:?}", d.chunks(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<_>>());
    }
    for n in ["diary0000.bin", "diary0001.bin", "diary0019.bin"] {
        let d = get(n);
        println!("{n} ({} bytes), first 12 rows of 8:", d.len());
        for row in d[..96].chunks(8) {
            println!("   {}", row.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" "));
        }
    }
    let (a, b) = (get("diary0000.bin"), get("diary0001.bin"));
    let diffs: Vec<usize> = (0..a.len().min(b.len())).filter(|&i| a[i] != b[i]).collect();
    println!("diary0000 vs diary0001: {} differing bytes of {}, first offsets {:?}", diffs.len(), a.len().min(b.len()), &diffs[..diffs.len().min(24)]);
    // stride guess: for each candidate record size, how often byte 0 of a record repeats the first record's byte 0
    for rs in [4usize, 6, 8, 10, 12, 16, 20, 24, 32] {
        let same = (0..a.len() / rs).filter(|&k| a[k * rs] == a[0]).count();
        println!("   stride {rs:2}: byte 0 equals the first record's in {same}/{} records", a.len() / rs);
    }
    let w = ar.get("World.bin").unwrap();
    println!("World.bin words: {:?}", w[12..12 + 64].chunks(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect::<Vec<_>>());
    let mut runs = Vec::new();
    let mut i = 0;
    while i < w.len() {
        let j = (i..w.len()).take_while(|&k| w[k].is_ascii_graphic() || w[k] == b' ').count();
        if j >= 4 {
            runs.push((i, String::from_utf8_lossy(&w[i..i + j]).to_string()));
        }
        i += j.max(1);
    }
    println!("World.bin ASCII runs ({}): {:?}", runs.len(), &runs[..runs.len().min(40)]);
}

/// Are the diaries lists of 4-byte records (day, season bits, event, competition)? Counts the
/// records whose season bits match the diary's index and the (event, competition) codes.
#[test]
fn diary_records() {
    let Ok(path) = std::env::var("LMA_EXPORT") else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    let diary = lma_data::archive::Archive::parse(ar.get("Diary20.mng").unwrap()).unwrap();
    for (season, name) in [(0u8, "diary0000.bin"), (1, "diary0001.bin"), (19, "diary0019.bin")] {
        let d = diary.get(name).unwrap();
        let recs: Vec<&[u8]> = d.chunks_exact(4).collect();
        let matching = recs.iter().filter(|r| (r[1] & 0x7c) >> 2 == season && r[1] & 0x80 != 0).count();
        let zero = recs.iter().filter(|r| r.iter().all(|&b| b == 0)).count();
        let ff = recs.iter().filter(|r| **r == [0xff, 0, 0, 0] || **r == [0, 0, 0xff, 0]).count();
        let mut codes: std::collections::BTreeMap<(u8, u8), usize> = std::collections::BTreeMap::new();
        let mut days: std::collections::BTreeMap<u8, usize> = std::collections::BTreeMap::new();
        for r in &recs {
            if (r[1] & 0x7c) >> 2 == season && r[1] & 0x80 != 0 {
                *codes.entry((r[2], r[3])).or_default() += 1;
                *days.entry(r[3]).or_default() += 1;
            }
        }
        println!("{name}: {} records, {matching} with season {season} and bit 7, {zero} zero, {ff} ff-markers; {} distinct (event, comp) codes; comps: {:?}", recs.len(), codes.len(), days);
        let mut top: Vec<_> = codes.iter().collect();
        top.sort_by_key(|(_, n)| std::cmp::Reverse(**n));
        println!("   top codes: {:?}", &top[..top.len().min(16)]);
        // the day field: (b0 | (b1 & 3) << 8) for the matching records, min/max
        let dvals: Vec<u32> = recs.iter().filter(|r| (r[1] & 0x7c) >> 2 == season && r[1] & 0x80 != 0).map(|r| r[0] as u32 | ((r[1] & 3) as u32) << 8).collect();
        println!("   day field min {:?} max {:?}, first 16: {:?}", dvals.iter().min(), dvals.iter().max(), &dvals[..dvals.len().min(16)]);
    }
}

/// Raw player records (hex) for the ids in `LMA_PIDS` (comma list), to search the unmapped bits
/// for value / wage / contract fields against values seen in game (#31 items 1 and 8).
#[test]
fn dump_player_records() {
    let (Ok(path), Ok(pids)) = (std::env::var("LMA_EXPORT"), std::env::var("LMA_PIDS")) else { return };
    let bytes = std::fs::read(&path).unwrap();
    let ar = lma_data::archive::Archive::parse(&bytes).unwrap();
    let players = ar.get("Players.bin").unwrap();
    let rs = u32::from_be_bytes([players[4], players[5], players[6], players[7]]) as usize;
    for pid in pids.split(',').filter_map(|s| s.trim().parse::<usize>().ok()) {
        let rec = &players[12 + pid * rs..12 + (pid + 1) * rs];
        println!("record {pid} {}", rec.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(""));
    }
}
