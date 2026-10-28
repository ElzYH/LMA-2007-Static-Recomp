//! End to end through `lma_import::import` with a Transfermarkt file (#31, #380): the level
//! follows market value, Transfermarkt's line becomes the main position with the source's group
//! second, nobody crosses the goalkeeper line (nor matches across it on a surname word alone),
//! and heights come from Transfermarkt.
//! Fake data only.
use std::collections::HashMap;
use std::path::PathBuf;

const RATINGS: [&str; 17] = [
    "tackling", "marking", "passing", "technique", "dribbling", "shot_power", "shot_accuracy", "heading", "positioning", "runs",
    "creativity", "pace", "intelligence", "temperament", "stamina", "strength", "fitness",
];

fn dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lma_import_tm_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A vanilla base: one club with eight slots, levels 30..100 spread over the four groups.
fn base(d: &PathBuf) {
    let mut p = format!("id,club_id,forename,surname,age,position,nation_id,{},value,skin,sides\n", RATINGS.join(","));
    let groups = ["GK", "DEF", "MID", "ATT"];
    for id in 0..40 {
        let level = 30 + (id % 10) * 7; // 30..93 in each group
        let club = if id < 8 { "0" } else { "" };
        let r = vec![level.to_string(); 17].join(",");
        p += &format!("{id},{club},Old,Player{id},25,{},0,{r},45000,3,7\n", groups[id % 4]);
    }
    std::fs::write(d.join("players.csv"), p).unwrap();
    // "AFC" normalises to "" (LMA has one): an empty alias must not land on it
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,AFC\n").unwrap();
    std::fs::write(d.join("nations.csv"), "nation_id,name\n0,England\n").unwrap();
}

fn mapping() -> lma_import::Config {
    toml::from_str(&mapping_toml()).unwrap()
}

fn mapping_toml() -> String {
    let ratings: String = RATINGS.iter().map(|r| format!("{r} = [\"ovr\"]\n")).collect();
    format!(
        r#"season_start = "2026-07-01"
min_squad = 8
[columns]
forename = "first_name"
surname = "last_name"
common_name = "common_name"
birthdate = "birthdate"
club = "club"
nationality = "nationality"
position = "position"
alt_positions = "alternate_positions"
overall = "overall_rating"
[ratings]
{ratings}
[gk_ratings]
{ratings}
[positions]
GK = "GK"
CB = "DEF"
CM = "MID"
RW = "MID/ATT"
RM = "MID"
ST = "ATT"
[sides]
RW = "R"
RM = "R"
ST = "C"
CM = "C"
CB = "C"
GK = "LRC"
[transfermarkt]
last_name = "last_name"
name = "name"
birth_date = "date_of_birth"
value = "market_value_in_eur"
sub_position = "sub_position"
height_cm = "height_in_cm"
[transfermarkt.positions]
"Goalkeeper" = "GK"
"Centre-Back" = "DEF"
"Central Midfield" = "MID"
"Centre-Forward" = "ATT"
"Right Winger" = "MID/ATT"
[club_aliases]
"Arsenal Tula" = ""
"#
    )
}

/// One cell of the written players.csv: `column` of the row with this surname.
fn cell(csv: &str, surname: &str, column: &str) -> String {
    let mut lines = csv.trim_start_matches('\u{feff}').lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |n: &str| head.iter().position(|h| *h == n).unwrap();
    let row = lines.map(|l| l.split(',').collect::<Vec<_>>()).find(|c| c.len() == head.len() && c[col("surname")] == surname).unwrap();
    row[col(column)].to_string()
}

/// The written players.csv rows by surname: (forename, position, position2, height, mean rating,
/// value).
fn rows(csv: &str) -> HashMap<String, (String, String, String, u32, f64, u32)> {
    let mut lines = csv.trim_start_matches('\u{feff}').lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |n: &str| head.iter().position(|h| *h == n).unwrap();
    let ratings: Vec<usize> = RATINGS.iter().map(|r| col(r)).collect();
    lines
        .map(|l| l.split(',').collect::<Vec<_>>())
        .filter(|c| c.len() == head.len())
        .map(|c| {
            let mean = ratings.iter().map(|&i| c[i].parse::<f64>().unwrap()).sum::<f64>() / 17.0;
            (c[col("surname")].to_string(), (c[col("forename")].to_string(), c[col("position")].to_string(), c[col("position2")].to_string(), c[col("height_cm")].parse().unwrap(), mean, c[col("value")].parse().unwrap()))
        })
        .collect()
}

#[test]
fn transfermarkt_sets_level_line_and_heights() {
    let d = dir("e2e");
    base(&d);
    // FC: everyone rated 60, so only Transfermarkt can tell them apart
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Royal,Gunner,,1999-01-01,Royal Arsenal,England,ST,,60,60\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,60,60\n\
        Squad,Backup,,2002-03-03,Arsenal,England,ST,,60,60\n\
        Deep,Lying,,2003-04-04,Arsenal,England,ST,,60,60\n\
        David,Raya Martin,,2004-05-05,Arsenal,England,CB,,60,60\n\
        Bukayo,Saka,,2001-09-05,Arsenal,England,RW,RM,60,60\n\
        Pedro,Gonzalez Lopez,Pedri,2002-11-25,Arsenal,England,CM,,60,60\n\
        Morgan,Rogers,,2002-07-26,Arsenal,England,CM,RW,60,60\n\
        Other,Namesake,,2000-06-06,Arsenal Tula,England,ST,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let tm = "last_name,name,date_of_birth,market_value_in_eur,sub_position,height_in_cm\n\
        One,Keeper One,2000-01-01 00:00:00,5000000,Centre-Back,191\n\
        Striker,Star Striker,2001-02-02 00:00:00,150000000,Centre-Forward,183\n\
        Backup,Squad Backup,2002-03-03 00:00:00,2000000,Centre-Forward,179\n\
        Lying,Deep Lying,2003-04-04 00:00:00,30000000,Central Midfield,180\n\
        Raya,David Raya,2004-05-05 00:00:00,40000000,Centre-Back,186\n\
        Saka,Bukayo Saka,2001-09-05 00:00:00,120000000,Right Winger,178\n";
    std::fs::write(d.join("tm.csv"), tm).unwrap();
    let lma_import::Imported { players: csv, clubs, report, .. } = lma_import::import(&d, &d.join("roster.csv"), None, Some(&d.join("tm.csv")), &mapping()).unwrap();
    assert!(clubs.is_none(), "no ladders: no division changes");
    let r = rows(&csv);
    assert_eq!((report.tm_matched, report.tm_unmatched, report.tm_other_line), (6, 2, 0));
    // a club aliased to "" stays out, although its name word-matches LMA's Arsenal; and a word
    // match never shares a club with the exact name, even when it comes first in the file
    assert_eq!(
        report.clubs_unmatched,
        vec![("Arsenal Tula".to_string(), 1), ("Royal Arsenal (LMA's Arsenal is Arsenal's)".to_string(), 1)]
    );
    assert!(!r.contains_key("Namesake") && !r.contains_key("Gunner"));
    // the goalkeeper stays a goalkeeper although Transfermarkt calls him a centre-back
    assert_eq!((r["One"].1.as_str(), r["One"].2.as_str()), ("GK", ""));
    // Transfermarkt's line first, the source's group second
    assert_eq!((r["Lying"].1.as_str(), r["Lying"].2.as_str()), ("MID", "ATT"));
    assert_eq!(r["Striker"].1, "ATT");
    // a winger is a midfielder who also plays up front, as vanilla files Robben and Giggs (#31
    // item 3: an ATT winger left the wide-midfield slots to central midfielders)
    assert_eq!((r["Saka"].1.as_str(), r["Saka"].2.as_str()), ("MID", "ATT"));
    // and plays on the right (RW + RM); a goalkeeper on any side; FC's striker centrally
    assert_eq!((cell(&csv, "Saka", "sides"), cell(&csv, "One", "sides"), cell(&csv, "Striker", "sides")), ("2".into(), "7".into(), "1".into()));
    // an alternate's second line counts: a central midfielder who plays on the wing also plays up
    // front (Rogers: CM, alternate RW = MID/ATT), on the right and in the centre
    assert_eq!((r["Rogers"].1.as_str(), r["Rogers"].2.as_str()), ("MID", "ATT"));
    assert_eq!(cell(&csv, "Rogers", "sides"), "3");
    // a single-name player's forename is cleared ("-"): a blank cell keeps the 2006 record's
    assert_eq!(r["Pedri"].0, "-");
    // the level follows market value: same FC ratings, the dearer striker rates higher
    let (star, squad) = (r["Striker"].4, r["Backup"].4);
    assert!(star > squad + 10.0, "star {star} vs squad {squad}");
    // known-as name (#31 item 4: "David" / "Raya", not the legal "Raya Martin") and height
    assert_eq!((r["Raya"].0.as_str(), r["Raya"].3), ("David", 186));
    // the stored value comes from the market value (#31 item 1), never the replaced record's 45000
    assert_eq!((r["Striker"].5, r["Backup"].5), (32000, 425));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A surname word alone never matches across the goalkeeper line (#608): FM's goalkeeper "Amigo"
/// (Alejandro Fernandez Sainz-Pardo) was born on Enzo Fernandez's birthday and took his EUR 80m
/// (fc27final24: GBP 78m at ratings ~37); a keeper Transfermarkt files as an outfielder still
/// matches on a shared forename. A dear player with low ratings lands in the report.
#[test]
fn no_surname_match_across_the_goalkeeper_line() {
    let d = dir("gkline");
    base(&d);
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Alejandro,Fernandez Sainz-Pardo,Amigo,2001-01-17,Arsenal,England,GK,,60,60\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Cheap,Dear,,1998-08-08,Arsenal,England,ST,,30,30\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let tm = "last_name,name,date_of_birth,market_value_in_eur,sub_position,height_in_cm\n\
        Fernandez,Enzo Fernandez,2001-01-17 00:00:00,80000000,Central Midfield,178\n\
        One,Keeper One,2000-01-01 00:00:00,5000000,Centre-Back,191\n\
        Dear,Cheap Dear,1998-08-08 00:00:00,25000000,Centre-Forward,180\n";
    std::fs::write(d.join("tm.csv"), tm).unwrap();
    let cfg: lma_import::Config = toml::from_str(&mapping_toml().replace("[transfermarkt]\n", "[transfermarkt]\noverall_weight = 1.0\n")).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), None, Some(&d.join("tm.csv")), &cfg).unwrap();
    let r = rows(&out.players);
    assert_eq!((out.report.tm_matched, out.report.tm_unmatched, out.report.tm_other_line), (2, 1, 1));
    assert_eq!((r["Amigo"].1.as_str(), r["Amigo"].5), ("GK", 0));
    assert_eq!(r["One"].1, "GK");
    assert!(r["One"].5 > 0, "Keeper One keeps his Transfermarkt value");
    assert_eq!(out.report.value_outliers.len(), 1, "{:?}", out.report.value_outliers);
    assert!(out.report.value_outliers[0].starts_with("Cheap Dear (Arsenal): EUR 25.0m"), "{:?}", out.report.value_outliers);
    std::fs::remove_dir_all(&d).unwrap();
}

/// Skin tone (#31 item 5): the attribute source's per-player tone wins over the nation default
/// (vanilla's English players are all 3 here); a player without one keeps the default.
#[test]
fn fm_skin_tone_wins_over_the_nation_default() {
    let d = dir("fmskin");
    base(&d);
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Bukayo,Saka,,2001-09-05,Arsenal,England,ST,,60,60\n\
        Kai,Havertz,,1999-06-11,Arsenal,England,ST,,60,60\n\
        No,Tone,,2000-01-01,Arsenal,England,ST,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let fm = "first_name,last_name,birth_date,ovr,skin_tone\n\
        Bukayo,Saka,2001-09-05,70,19\n\
        Kai,Havertz,1999-06-11,70,3\n\
        No,Tone,2000-01-01,70,\n";
    std::fs::write(d.join("fm.csv"), fm).unwrap();
    let ratings: String = RATINGS.iter().map(|r| format!("{r} = [\"ovr\"]\n")).collect();
    let toml = mapping_toml()
        + &format!(
            "[fm]\nfirst_name = \"first_name\"\nlast_name = \"last_name\"\nbirth_date = \"birth_date\"\n[fm.ratings]\n{ratings}[fm.gk_ratings]\n{ratings}[fm.skin]\ncolumn = \"skin_tone\"\nlma = [0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 3, 3, 3, 3, 3, 2, 2, 2, 2, 2]\n"
        );
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), Some(&d.join("fm.csv")), None, &cfg).unwrap();
    assert_eq!((out.report.fm_matched, out.report.fm_skin), (3, 2));
    let mut lines = out.players.trim_start_matches('\u{feff}').lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |n: &str| head.iter().position(|h| *h == n).unwrap();
    let skin: HashMap<String, String> = lines
        .map(|l| l.split(',').collect::<Vec<_>>())
        .filter(|c| c.len() == head.len())
        .map(|c| (c[col("surname")].to_string(), c[col("skin")].to_string()))
        .collect();
    assert_eq!((skin["Saka"].as_str(), skin["Havertz"].as_str(), skin["Tone"].as_str()), ("2", "0", "3"));
    std::fs::remove_dir_all(&d).unwrap();
}

/// The FM export as a roster (#31, the maintainer: "world is 2026 only"): it fills a club the roster lacks,
/// leaves out a women's section of the same name (shorter squad) and an FM-only player at a roster
/// club, names FM's nation ids through the players both have, and puts its free agents (older
/// uids only) into the records no squad uses.
#[test]
fn fm_fills_clubs_the_roster_lacks_and_the_free_agents() {
    let d = dir("fill");
    let r = vec!["50"; 17].join(",");
    let mut p = format!("id,club_id,forename,surname,age,position,nation_id,{},skin\n", RATINGS.join(","));
    for id in 0..40 {
        let club = match id {
            0..=17 => "0",
            18..=35 => "1",
            _ => "",
        };
        p += &format!("{id},{club},Old,Player{id},25,MID,0,{r},0\n");
    }
    std::fs::write(d.join("players.csv"), p).unwrap();
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Celtic\n").unwrap();
    std::fs::write(d.join("nations.csv"), "nation_id,name\n0,England\n").unwrap();
    std::fs::write(d.join("roster.csv"), "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\nBukayo,Saka,,2001-09-05,Arsenal,England,RW,,60,60\n").unwrap();
    let mut fm = String::from("uid,first_name,last_name,birth_date,ovr,club_name,club_uid,nation_id,natural_positions,ability_current,height_cm\n");
    fm += "1,Bukayo,Saka,2001-09-05,80,Arsenal,602,139,AMR,180,178\n";
    fm += "2,Extra,Gunner,2000-01-01,70,Arsenal,602,139,MC,150,180\n";
    for k in 0..18 {
        fm += &format!("{},Hoop,Man{k},1999-01-{:02},60,Celtic,900,139,DC,120,182\n", 10 + k, k + 1);
        fm += &format!("{},Hoop,Woman{k},1999-02-{:02},60,Celtic,2000000900,139,DC,120,166\n", 2000000100 + k, k + 1);
    }
    fm += "50,Free,AgentOne,1990-03-01,55,,,139,STC,110,181\n";
    fm += "51,Free,AgentTwo,1991-03-01,55,,,139,DC,90,183\n";
    fm += "2000000051,Free,AgentNew,1992-03-01,55,,,139,MC,100,168\n";
    // a free agent with no uid can't be told from the newer (women's) database: left out
    fm += ",Free,AgentNoUid,1993-03-01,55,,,139,MC,100,181\n";
    std::fs::write(d.join("fm.csv"), fm).unwrap();
    let ratings: String = RATINGS.iter().map(|r| format!("{r} = [\"ovr\"]\n")).collect();
    let toml = mapping_toml()
        + &format!(
            "[fm]\nfirst_name = \"first_name\"\nlast_name = \"last_name\"\nbirth_date = \"birth_date\"\nheight_cm = \"height_cm\"\n[fm.ratings]\n{ratings}[fm.gk_ratings]\n{ratings}\
             [fm.fill]\nclub = \"club_name\"\nclub_uid = \"club_uid\"\nnation = \"nation_id\"\npositions = \"natural_positions\"\nability = \"ability_current\"\n\
             min_club_height_cm = 174.0\nuid = \"uid\"\nfree_agent_uid_below = 2000000000\n[fm.fill.labels]\nDC = \"CB\"\nMC = \"CM\"\nAMR = \"RW\"\nSTC = \"ST\"\n"
        );
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), Some(&d.join("fm.csv")), None, &cfg).unwrap();
    assert_eq!((out.report.fill_clubs, out.report.fill_players, out.report.free_agents), (1, 18, 2));
    let csv = &out.players;
    assert_eq!(cell(csv, "Man0", "club_id"), "1");
    assert_eq!(cell(csv, "Man0", "nation_id"), "0", "FM's 139 is England: Saka is both");
    assert_eq!((cell(csv, "AgentOne", "club_id"), cell(csv, "AgentTwo", "club_id")), (String::new(), String::new()));
    for absent in ["Woman0", "Gunner", "AgentNew", "AgentNoUid"] {
        assert!(!csv.contains(&format!(",{absent},")), "{absent} written");
    }
    // each club's FM uid (badge packs are keyed by it): the roster club by its players', the
    // filled club its own
    let fm_uids: Vec<String> = out.clubs_fm.unwrap().trim_start_matches('\u{feff}').lines().map(String::from).collect();
    assert_eq!(fm_uids, ["id,name_E,fm_uid", "0,Arsenal,602", "1,Celtic,900"]);
    std::fs::remove_dir_all(&d).unwrap();
}

/// Decision 1 phase B (#31): LMA clubs with a squad and no source (outside the leagues: country
/// 15) are taken over by FM clubs LMA lacks; the same club under another spelling first (shared
/// distinctive word, same nation), never an FM club the roster's players play for (FC 27's
/// Arsenal is FM's "Gunners FC" here), and the new name lands in clubs.csv.
#[test]
fn clubs_with_no_source_are_taken_over() {
    let d = dir("takeover");
    let r = vec!["50"; 17].join(",");
    let mut p = format!("id,club_id,forename,surname,age,position,nation_id,{}\n", RATINGS.join(","));
    for id in 0..60 {
        let (club, nation) = match id {
            0..=17 => ("0", 0),
            18..=35 => ("1", 1), // a Serbian club, rich
            36..=53 => ("2", 0), // an English non-league club
            _ => ("", 0),
        };
        p += &format!("{id},{club},Old,Player{id},25,MID,{nation},{r}\n");
    }
    std::fs::write(d.join("players.csv"), p).unwrap();
    std::fs::write(d.join("clubs.csv"), "id,name_E,country,wealth\n0,Arsenal,0,15\n1,Crvena Zvezda,15,9\n2,Old Etonians Town,15,3\n").unwrap();
    std::fs::write(d.join("nations.csv"), "nation_id,name\n0,England\n1,Serbia\n").unwrap();
    std::fs::write(d.join("roster.csv"), "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\nBukayo,Saka,,2001-09-05,Arsenal,England,RW,,60,60\nNemanja,Serb,,1995-05-05,Arsenal,Serbia,CB,,60,60\nDeclan,Rice,,1999-01-14,Arsenal,England,CM,,60,60\n").unwrap();
    let mut fm = String::from("uid,first_name,last_name,birth_date,ovr,club_name,club_uid,nation_id,natural_positions,ability_current,height_cm\n");
    // FC 27's Arsenal under FM's name: never a takeover
    fm += "1,Bukayo,Saka,2001-09-05,80,Gunners FC,602,139,AMR,180,178\n2,Nemanja,Serb,1995-05-05,80,Gunners FC,602,200,DC,180,188\n3,Declan,Rice,1999-01-14,80,Gunners FC,602,139,MC,180,185\n";
    for k in 0..18 {
        fm += &format!("{},Gun,Extra{k},1998-03-{:02},60,Gunners FC,602,139,MC,150,182\n", 10 + k, k + 1);
        // the stronger English club, and Red Star under FM's spelling (weaker, Serbian)
        fm += &format!("{},Strong,Eng{k},1997-04-{:02},60,Strong United,700,139,DC,160,182\n", 100 + k, k + 1);
        fm += &format!("{},Zvezda,Man{k},1996-05-{:02},60,FK Crvena zvezda Beograd,800,200,DC,120,183\n", 200 + k, k + 1);
    }
    std::fs::write(d.join("fm.csv"), fm).unwrap();
    let ratings: String = RATINGS.iter().map(|r| format!("{r} = [\"ovr\"]\n")).collect();
    let toml = mapping_toml()
        + &format!(
            "[fm]\nfirst_name = \"first_name\"\nlast_name = \"last_name\"\nbirth_date = \"birth_date\"\nheight_cm = \"height_cm\"\n[fm.ratings]\n{ratings}[fm.gk_ratings]\n{ratings}\
             [fm.fill]\nclub = \"club_name\"\nclub_uid = \"club_uid\"\nnation = \"nation_id\"\npositions = \"natural_positions\"\nability = \"ability_current\"\n\
             [fm.fill.labels]\nDC = \"CB\"\nMC = \"CM\"\nAMR = \"RW\"\n"
        );
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), Some(&d.join("fm.csv")), None, &cfg).unwrap();
    // the richer Serbian club finds its FM spelling although Strong United is stronger
    assert_eq!(out.report.takeovers, vec![("Crvena Zvezda".to_string(), "FK Crvena zvezda Beograd".to_string()), ("Old Etonians Town".to_string(), "Strong United".to_string())]);
    assert_eq!((cell(&out.players, "Man0", "club_id"), cell(&out.players, "Eng0", "club_id")), ("1".into(), "2".into()));
    assert!(!out.players.contains(",Extra0,"), "FM's Arsenal took nothing over");
    let clubs = out.clubs.unwrap();
    assert!(clubs.contains("\r\n1,,,,FK Crvena zvezda Beograd,"), "{clubs}");
    // a transfer into a taken-over club, named as FM names it (the maintainer's Martinelli to Al Hilal is
    // this case: Al Hilal is FM's, on another club's record); a club no squad has is reported
    std::fs::write(d.join("transfers.csv"), "player,club\nBukayo Saka,Strong United\nDeclan Rice,Nowhere Rovers\n").unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { fm: Some(&d.join("fm.csv")), transfers: Some(&d.join("transfers.csv")), ..Default::default() }, &cfg).unwrap();
    assert_eq!(cell(&out.players, "Saka", "club_id"), "2", "Saka at Strong United's record");
    assert_eq!(cell(&out.players, "Rice", "club_id"), "0", "Rice stays");
    assert_eq!((out.report.transfers.len(), out.report.transfers_unmatched.len()), (1, 1));
    std::fs::remove_dir_all(&d).unwrap();
}

/// The rest of decision 1 (#31): a roster club LMA lacks ("Newtown", Premier) takes its league's
/// place; the LMA club it pushes out (B, no source) drops to the pool onto the id of the poorest
/// retired club outside the leagues (R), whose record is Newtown's shell; the clubs no source
/// covers (B, S) play with the FM free agents, the richer first; no record keeps a 2006 player.
#[test]
fn new_clubs_take_their_league_and_free_agents_fill_the_rest() {
    let d = dir("newclubs");
    let r = vec!["50"; 17].join(",");
    let mut p = format!("id,club_id,forename,surname,age,position,nation_id,{}\n", RATINGS.join(","));
    for id in 0..60 {
        let club = if id < 48 { (id / 8).to_string() } else { String::new() };
        p += &format!("{id},{club},Old,Player{id},25,MID,0,{r}\n");
    }
    std::fs::write(d.join("players.csv"), p).unwrap();
    // A, B top flight; C, D second; R (poor) and S (richer) outside the leagues
    std::fs::write(d.join("clubs.csv"), "id,name_E,country,division,wealth\n0,A,0,0,9\n1,B,0,0,8\n2,C,0,19,5\n3,D,0,19,5\n4,R,15,12,1\n5,S,15,12,5\n").unwrap();
    std::fs::write(d.join("nations.csv"), "nation_id,name\n0,England\n").unwrap();
    let mut roster = String::from("first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr,league\n");
    for (club, league) in [("A", "Premier"), ("Newtown", "Premier"), ("C", "Champ"), ("D", "Champ")] {
        for k in 0..8 {
            roster += &format!("{club}x,{club}{k},,1990-0{}-{:02},{club},England,CM,,60,60,{league}\n", 1 + k % 9, 10 + k);
        }
    }
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let mut fm = String::from("uid,first_name,last_name,birth_date,ovr,club_name,club_uid,nation_id,natural_positions,ability_current,height_cm\n");
    // 16 for the two squads, 12 for the unattached records
    for k in 0..28 {
        fm += &format!("{},Free,Agent{k},1995-03-{:02},55,,,139,MC,{},180\n", 500 + k, k + 1, 120 - k);
    }
    std::fs::write(d.join("fm.csv"), fm).unwrap();
    let ratings: String = RATINGS.iter().map(|r| format!("{r} = [\"ovr\"]\n")).collect();
    let toml = mapping_toml().replace("overall = \"overall_rating\"\n", "overall = \"overall_rating\"\nleague = \"league\"\n")
        + &format!(
            "[fm]\nfirst_name = \"first_name\"\nlast_name = \"last_name\"\nbirth_date = \"birth_date\"\n[fm.ratings]\n{ratings}[fm.gk_ratings]\n{ratings}\
             [fm.fill]\nclub = \"club_name\"\nnation = \"nation_id\"\npositions = \"natural_positions\"\nability = \"ability_current\"\n[fm.fill.labels]\nMC = \"CM\"\n\
             [[ladders]]\ncountry = 0\ndivisions = [0, 19]\nleagues = [\"Premier\", \"Champ\"]\n"
        );
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), Some(&d.join("fm.csv")), None, &cfg).unwrap();
    assert_eq!(out.report.new_clubs, vec![("Newtown".to_string(), "Premier".to_string(), "B".to_string())]);
    let clubs = out.clubs.unwrap();
    // Newtown at B's id with R's record; B to R's id in the pool
    assert!(clubs.contains("\r\n1,4,0,0,Newtown,"), "{clubs}");
    assert!(clubs.contains("\r\n4,1,15,12,"), "{clubs}");
    assert_eq!(cell(&out.players, "Newtown0", "club_id"), "1");
    // B (pushed out, no source; now at R's id 4) and S both play with free agents, the best to B
    assert_eq!(out.report.free_agent_squads, vec!["B".to_string(), "S".to_string()]);
    assert_eq!((cell(&out.players, "Agent0", "club_id"), cell(&out.players, "Agent15", "club_id")), ("4".into(), "5".into()));
    assert_eq!(out.report.records_kept, 0);
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn mod_toml_carries_base_year_and_the_copied_patches() {
    // --patches reads another mod's mod.toml: its [[patch]] tables, nothing else
    let earlier = "name = \"k\"\nversion = \"2026\"\nbase = \"Export.mng\"\nbase_year = 2026\n\n[[patch]]\naddr = \"0x82000018\"\nfrom = 1\nto = 2\n\n[[patch]]\naddr = \"0x8200001c\"\nfrom = 3\nto = 4\n";
    let patches = lma_import::mod_patches(earlier).unwrap();
    assert_eq!(patches.len(), 2);
    let mut cfg: lma_import::Config = toml::from_str(&mapping_toml()).unwrap();
    cfg.base_year = Some(2026);
    let out = lma_import::mod_toml("fc27i", &cfg, &patches);
    assert!(out.starts_with("name = \"fc27i\"\n") && out.contains("base_year = 2026\n"), "{out}");
    // what it writes reads back as the same patches
    let again = lma_import::mod_patches(&out).unwrap();
    assert_eq!(again.iter().map(|p| (p.addr.as_str(), p.from, p.to)).collect::<Vec<_>>(), vec![("0x82000018", 1, 2), ("0x8200001c", 3, 4)]);
    assert!(lma_import::mod_patches("[[patch]]\naddr = \"0x82000018\"\nfrom = 1\n").is_err(), "a patch without `to`");
    assert!(lma_import::mod_patches("name = \"x\"\n").unwrap().is_empty());
}

/// Stadiums (#31): a club takes the stadium of the Transfermarkt club most of its matched players
/// are at (three at least), its seats spread like the record's stands; a club whose vote is
/// too thin keeps its own.
#[test]
fn transfermarkt_clubs_give_the_stadium() {
    let d = dir("stadium");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E,stadium,cap_north,cap_east,cap_south,cap_west\n0,Arsenal,Highbury,10000,10000,10000,8000\n1,AFC,,,,,\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,60,60\n\
        Squad,Backup,,2002-03-03,Arsenal,England,ST,,60,60\n\
        Deep,Lying,,2003-04-04,Arsenal,England,ST,,60,60\n\
        David,Raya,,2004-05-05,Arsenal,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let tm = "player_id,last_name,name,date_of_birth,market_value_in_eur,current_club_id\n\
        1,One,Keeper One,2000-01-01,5000000,11\n\
        2,Striker,Star Striker,2001-02-02,150000000,11\n\
        3,Backup,Squad Backup,2002-03-03,2000000,11\n\
        4,Lying,Deep Lying,2003-04-04,30000000,999\n";
    std::fs::write(d.join("tm.csv"), tm).unwrap();
    std::fs::write(d.join("tm_clubs.csv"), "club_id,name,stadium_name,stadium_seats\n11,Arsenal FC,Emirates Stadium,60704\n999,Elsewhere,Far Away Park,100\n").unwrap();
    let toml = mapping_toml().replace("[transfermarkt.positions]", "club = \"current_club_id\"\n[transfermarkt.stadiums]\nid = \"club_id\"\nname = \"stadium_name\"\nseats = \"stadium_seats\"\n[transfermarkt.positions]");
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let import = |clubs_file: Option<&std::path::Path>| lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_clubs: clubs_file, ..Default::default() }, &cfg).unwrap();
    let out = import(Some(&d.join("tm_clubs.csv")));
    assert_eq!(out.report.stadiums, 1);
    assert!(out.report.stadiums_kept.is_empty());
    let clubs = out.clubs.expect("a stadium to write");
    let mut lines = clubs.trim_start_matches('\u{feff}').lines();
    let head: Vec<&str> = lines.next().unwrap().split(',').collect();
    let row: Vec<&str> = lines.next().unwrap().split(',').collect();
    let col = |n: &str| row[head.iter().position(|h| *h == n).unwrap()];
    // 3 of 4 votes for club 11; 60,704 seats as 10:10:10:8
    assert_eq!((col("id"), col("stadium")), ("0", "Emirates Stadium"));
    assert_eq!([col("cap_north"), col("cap_east"), col("cap_south"), col("cap_west")], ["15977", "15974", "15974", "12779"]);
    // without the clubs file nothing changes
    assert!(import(None).clubs.is_none());
    // shirt numbers: the keeper's latest at club 11 (not an older one, nor a newer national-team
    // game); the striker's 9 is at another club; the rest take the lowest left
    std::fs::write(
        d.join("lineups.csv"),
        "\"game_lineups_id\",\"date\",\"player_id\",\"club_id\",\"player_name\",\"number\"\n\
         \"a\",\"2025-05-01\",\"1\",\"11\",\"Keeper, One\",\"13\"\n\
         \"b\",\"2026-08-20\",\"1\",\"11\",\"Keeper One\",\"22\"\n\
         \"c\",\"2026-09-01\",\"1\",\"3299\",\"Keeper One\",\"1\"\n\
         \"d\",\"2026-08-20\",\"2\",\"999\",\"Star Striker\",\"9\"\n",
    )
    .unwrap();
    let toml = toml.replace("club = \"current_club_id\"\n", "club = \"current_club_id\"\nid = \"player_id\"\n").replace(
        "[transfermarkt.positions]",
        "[transfermarkt.lineups]\nplayer = \"player_id\"\nclub = \"club_id\"\nnumber = \"number\"\ndate = \"date\"\n[transfermarkt.positions]",
    );
    let numbered: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_lineups: Some(&d.join("lineups.csv")), ..Default::default() }, &numbered).unwrap();
    assert_eq!(out.report.shirt_numbers, 1);
    assert_eq!(cell(&out.players, "One", "squad_number"), "22");
    let others: Vec<String> = ["Striker", "Backup", "Lying", "Raya"].iter().map(|n| cell(&out.players, n, "squad_number")).collect();
    let mut sorted = others.clone();
    sorted.sort();
    assert_eq!(sorted, ["1", "2", "3", "4"], "{others:?}");
    // a club its players don't vote for takes the club of its name, when only one has it
    std::fs::write(d.join("tm_clubs.csv"), "club_id,name,stadium_name,stadium_seats\n12,Arsenal FC,Old Ground,1000\n999,Elsewhere,Far Away Park,100\n").unwrap();
    let toml = toml.replace("seats = \"stadium_seats\"\n", "seats = \"stadium_seats\"\nclub = \"name\"\n");
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_clubs: Some(&d.join("tm_clubs.csv")), ..Default::default() }, &cfg).unwrap();
    assert!(out.clubs.unwrap().contains(",Old Ground,"), "Arsenal by name");
    // two clubs of that name: no guess
    std::fs::write(d.join("tm_clubs.csv"), "club_id,name,stadium_name,stadium_seats\n12,Arsenal FC,Old Ground,1000\n13,Arsenal,Other Ground,900\n").unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_clubs: Some(&d.join("tm_clubs.csv")), ..Default::default() }, &cfg).unwrap();
    assert!(out.clubs.is_none());
    // and the report names the club that keeps its stadium
    assert_eq!(out.report.stadiums_kept, vec!["Arsenal (Highbury)".to_string()]);
    // unless its ground was replaced: today's name, the stands as they were
    let mut renamed: lma_import::Config = toml::from_str(&toml).unwrap();
    renamed.stadium_renames.insert("Arsenal|Highbury".into(), "Emirates Stadium".into());
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_clubs: Some(&d.join("tm_clubs.csv")), ..Default::default() }, &renamed).unwrap();
    assert!(out.report.stadiums_kept.is_empty());
    assert!(out.clubs.unwrap().contains(",Emirates Stadium,10000,10000,10000,8000"));
    std::fs::remove_dir_all(&d).unwrap();
}

/// An alias "#id" picks one of two LMA clubs of the same name (2006's A-League Newcastle United
/// Jets beside England's); one naming no club is an error.
#[test]
fn an_id_alias_picks_one_of_two_namesakes() {
    let d = dir("idalias");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Arsenal\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Jet,One,,2000-01-01,Newcastle Jets,England,ST,,60,60\n\
        Jet,Two,,2000-01-02,Newcastle Jets,England,CM,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let toml = mapping_toml().replace("[club_aliases]\n", "[club_aliases]\n\"Newcastle Jets\" = \"#1\"\n");
    let cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    let out = lma_import::import(&d, &d.join("roster.csv"), None, None, &cfg).unwrap();
    assert_eq!(cell(&out.players, "One", "club_id"), "1");
    let bad: lma_import::Config = toml::from_str(&mapping_toml().replace("[club_aliases]\n", "[club_aliases]\n\"Newcastle Jets\" = \"#7\"\n")).unwrap();
    assert!(lma_import::import(&d, &d.join("roster.csv"), None, None, &bad).err().unwrap().contains("names no LMA club"));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A transfers file moves a squad player to another club after everything else (the maintainer:
/// Martinelli to Al Hilal); a line naming no one is reported, not applied.
#[test]
fn a_transfers_file_moves_a_player() {
    let d = dir("transfers");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,60,60\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Blue,Back,,2000-01-03,Chelsea,England,CB,,60,60\n\
        Gabriel,Martinelli Silva,Gabriel Martinelli,2001-06-18,Arsenal,England,ST,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(d.join("transfers.csv"), "player,club\nGabriel Martinelli,Chelsea\nStar Striker,Arsenal\nNobody Atall,Chelsea\n").unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { transfers: Some(&d.join("transfers.csv")), ..Default::default() }, &mapping()).unwrap();
    assert_eq!(cell(&out.players, "Gabriel Martinelli", "club_id"), "1");
    assert_eq!(cell(&out.players, "Striker", "club_id"), "0");
    assert_eq!(out.report.transfers, vec!["Gabriel Martinelli: Arsenal -> Chelsea".to_string(), "Star Striker: already at Arsenal".to_string()]);
    assert_eq!(out.report.transfers_unmatched.len(), 1);
    std::fs::remove_dir_all(&d).unwrap();
}

/// League histories (#31): the base's champions move down by the years base_year adds to 2006
/// (a historic club keeps its bare uid), and a Transfermarkt season whose champion is one of our
/// clubs goes on its row with its league record; a champion that isn't ours is reported.
#[test]
fn league_history_shifts_and_takes_transfermarkt_champions() {
    let d = dir("history");
    base(&d);
    std::fs::write(d.join("history.csv"), "country,tier,row,club_id,club,uid,won,drawn,lost,for,against,points\n0,0,0,0,Arsenal,5800,26,12,0,73,26,90\n0,0,1,,,8003,20,4,6,56,23,44\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,60,60\n\
        Squad,Backup,,2002-03-03,Arsenal,England,ST,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let tm = "last_name,name,date_of_birth,market_value_in_eur,current_club_id\n\
        One,Keeper One,2000-01-01,5000000,11\n\
        Striker,Star Striker,2001-02-02,150000000,11\n\
        Backup,Squad Backup,2002-03-03,2000000,11\n";
    std::fs::write(d.join("tm.csv"), tm).unwrap();
    // 2007/08 (row 0 for base year 2008): club 11 wins twice and draws once; 2006/07 (row 1):
    // club 99, not ours, wins it
    std::fs::write(
        d.join("games.csv"),
        "competition_id,season,date,home_club_id,away_club_id,home_club_goals,away_club_goals\n\
         GB1,2007,2007-08-11,11,12,2,0\nGB1,2007,2007-08-18,12,11,1,3\nGB1,2007,2007-08-25,11,13,1,1\nGB1,2006,2006-08-19,99,12,1,0\nXX1,2007,2007-08-11,13,12,9,0\n",
    )
    .unwrap();
    let toml = mapping_toml().replace("[transfermarkt.positions]", "club = \"current_club_id\"\n[transfermarkt.history]\ncompetition = \"competition_id\"\nseason = \"season\"\ndate = \"date\"\nhome_club = \"home_club_id\"\naway_club = \"away_club_id\"\nhome_goals = \"home_club_goals\"\naway_goals = \"away_club_goals\"\n[transfermarkt.history.leagues]\nGB1 = [0, 0]\n[transfermarkt.positions]");
    let mut cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    cfg.base_year = Some(2008);
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_games: Some(&d.join("games.csv")), ..Default::default() }, &cfg).unwrap();
    let h = out.history.expect("a history.csv");
    let lines: Vec<&str> = h.trim_start_matches('\u{feff}').lines().collect();
    assert_eq!(
        lines,
        [
            "country,tier,row,club_id,uid,won,drawn,lost,for,against,points",
            "0,0,0,0,,2,1,0,6,2,7",
            "0,0,2,0,,26,12,0,73,26,90",
            "0,0,3,,8003,20,4,6,56,23,44",
        ]
    );
    assert_eq!((out.report.history_seasons, out.report.history_unmatched.len()), (1, 1));
    std::fs::remove_dir_all(&d).unwrap();
}

/// Cup finals (the maintainer, 12 Oct: the Champions League final shown was 2006's): the base's finals move
/// down by the years base_year adds; a Transfermarkt final between two of our clubs goes on its row,
/// with its score in play when the events show a shoot-out (Transfermarkt's goals count the kicks);
/// one with a side not ours is reported; `--cup-extra` fills the rows still empty.
#[test]
fn cup_finals_shift_take_transfermarkt_finals_and_fill_from_a_list() {
    let d = dir("cups");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    std::fs::write(
        d.join("cups.csv"),
        "cup,row,winner_id,winner,winner_uid,runner_up_id,runner_up,runner_up_uid,winner_goals,runner_up_goals\n\
         european_cup,0,1,Chelsea,,0,Arsenal,,2,1\n\
         european_cup,1,,,8003,0,Arsenal,,1,0\n",
    )
    .unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,60,60\n\
        Squad,Backup,,2002-03-03,Arsenal,England,ST,,60,60\n\
        Blue,Back,,2000-04-04,Chelsea,England,CB,,60,60\n\
        Blue,Wing,,2000-05-05,Chelsea,England,LW,,60,60\n\
        Blue,Nine,,2000-06-06,Chelsea,England,ST,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let tm = "last_name,name,date_of_birth,market_value_in_eur,current_club_id\n\
        One,Keeper One,2000-01-01,5000000,11\n\
        Striker,Star Striker,2001-02-02,150000000,11\n\
        Backup,Squad Backup,2002-03-03,2000000,11\n\
        Back,Blue Back,2000-04-04,5000000,12\n\
        Wing,Blue Wing,2000-05-05,5000000,12\n\
        Nine,Blue Nine,2000-06-06,5000000,12\n";
    std::fs::write(d.join("tm.csv"), tm).unwrap();
    // 2007/08's Champions League final (row 0 for base year 2008): 1-1, 4-3 on penalties, which
    // Transfermarkt writes 5-4; 2006/07's FA Cup final: a club not ours wins it
    std::fs::write(
        d.join("games.csv"),
        "game_id,competition_id,season,round,date,home_club_id,away_club_id,home_club_goals,away_club_goals,home_club_name,away_club_name\n\
         100,CL,2007,Final,2008-05-21,11,12,5,4,Arsenal FC,Chelsea FC\n\
         101,FAC,2006,Final,2007-05-19,99,12,1,0,Nowhere FC,Chelsea FC\n\
         102,CL,2007,Semi-Finals,2008-04-29,11,13,3,0,Arsenal FC,Elsewhere\n",
    )
    .unwrap();
    std::fs::write(
        d.join("events.csv"),
        "game_event_id,date,game_id,minute,type,club_id,club_name,player_id,description\n\
         a,2008-05-21,100,20,Goals,11,Arsenal FC,1,\"1:0, Right-footed shot\"\n\
         b,2008-05-21,100,70,Goals,12,Chelsea FC,2,\"1:1, Header\"\n\
         c,2008-05-21,100,120,Shootout,11,Arsenal FC,1,\n\
         d,2008-01-01,555,10,Goals,12,Chelsea FC,2,\n",
    )
    .unwrap();
    std::fs::write(
        d.join("extra.csv"),
        "competition,season,winner,runner_up,winner_goals,runner_up_goals\n\
         CL,2006,Arsenal,Chelsea,2,0\n\
         fa_cup,2006,Chelsea,Arsenal,3,1\n\
         CL,2007,Chelsea,Arsenal,9,9\n\
         league_cup,2006,#1,#0,2,2\n",
    )
    .unwrap();
    let toml = mapping_toml().replace("[transfermarkt.positions]", "club = \"current_club_id\"\n[transfermarkt.history]\ncompetition = \"competition_id\"\nseason = \"season\"\ndate = \"date\"\nhome_club = \"home_club_id\"\naway_club = \"away_club_id\"\nhome_goals = \"home_club_goals\"\naway_goals = \"away_club_goals\"\nhome_club_name = \"home_club_name\"\naway_club_name = \"away_club_name\"\nround = \"round\"\ngame_id = \"game_id\"\n[transfermarkt.history.leagues]\nGB1 = [0, 0]\n[transfermarkt.history.cups]\nCL = \"european_cup\"\nFAC = \"fa_cup\"\n[transfermarkt.positions]");
    let mut cfg: lma_import::Config = toml::from_str(&toml).unwrap();
    cfg.base_year = Some(2008);
    let sources = lma_import::Sources { tm: Some(&d.join("tm.csv")), tm_games: Some(&d.join("games.csv")), tm_events: Some(&d.join("events.csv")), cup_extra: Some(&d.join("extra.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &cfg).unwrap();
    let c = out.cups.expect("a cups.csv");
    let lines: Vec<&str> = c.trim_start_matches('\u{feff}').lines().collect();
    assert_eq!(
        lines,
        [
            "cup,row,winner_id,winner_uid,runner_up_id,runner_up_uid,winner_goals,runner_up_goals",
            "european_cup,0,0,,1,,1,1",
            "european_cup,1,0,,1,,2,0",
            "european_cup,2,1,,0,,2,1",
            "european_cup,3,,8003,0,,1,0",
            "fa_cup,1,1,,0,,3,1",
            "league_cup,1,1,,0,,2,2",
        ]
    );
    let r = &out.report;
    assert_eq!((r.cup_finals, r.cup_shootouts, r.cup_extra, r.cup_unmatched.len()), (1, 1, 3, 1), "{:?}", r.cup_unmatched);
    std::fs::remove_dir_all(&d).unwrap();
}

/// Live Transfermarkt squads (the maintainer, 10 Oct): a player found in another of our clubs' live squad
/// (by birth date and a name word; the club by its name) moves there with the live shirt number;
/// one found at a club that isn't ours leaves our squads; one on no live squad stays. The
/// managers list sets a club's manager (surname) and reports a name matching none.
#[test]
fn live_squads_move_players_and_a_managers_list_sets_managers() {
    let d = dir("live");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Mover,Moves,,2000-01-01,Arsenal,England,ST,,60,60\n\
        Leaver,Leaves,,2000-02-02,Arsenal,England,ST,,60,60\n\
        Stayer,Stays,,2000-03-03,Arsenal,England,CM,,60,60\n\
        Blue,Back,,2000-04-04,Chelsea,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,631,Chelsea FC,1,Mover Moves,2000-01-01,23,,50000000\n\
         XX1,999,Elsewhere United,2,Leaver Leaves,2000-02-02,9,,1000000\n",
    )
    .unwrap();
    std::fs::write(d.join("managers.csv"), "club,manager\nChelsea,Enzo Maresca\nNowhere Athletic,Some One\n").unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), managers: Some(&d.join("managers.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert_eq!(cell(&out.players, "Moves", "club_id"), "1", "Mover at Chelsea, his live club");
    assert_eq!(cell(&out.players, "Moves", "squad_number"), "23", "with the live shirt number");
    assert!(!out.players.contains(",Leaves,"), "Leaver is at a club that isn't ours: out of our squads");
    assert_eq!(cell(&out.players, "Stays", "club_id"), "0", "Stayer is on no live squad: stays");
    assert_eq!((out.report.live_matched, out.report.live_moves.len(), out.report.live_elsewhere), (2, 1, 1));
    let clubs = out.clubs.expect("a clubs.csv with the manager");
    assert!(clubs.lines().any(|l| l.starts_with("1,") && l.contains(",Maresca,")), "{clubs}");
    assert_eq!(out.report.listed_managers_unmatched.len(), 1);
    std::fs::remove_dir_all(&d).unwrap();
}

/// A managers list whose later row finds only a club an earlier row set ("Arsenal FC", Arsenal de
/// Sarandí in the 22 Oct list, its record taken over): the first row's Arteta stays and the later
/// row is reported (fc27final32 gave Arsenal "Lisa" at 95); a `reputation` column sets the
/// manager's reputation.
#[test]
fn a_managers_list_skips_clubs_an_earlier_row_set_and_sets_reputations() {
    let d = dir("managers_namesake");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Bukayo,Saka,,2001-09-05,Arsenal,England,RW,,60,60\n\
        Blue,Back,,2000-04-04,Chelsea,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(d.join("managers.csv"), "club,manager,reputation\nArsenal,Mikel Arteta,80\nChelsea,Xabi Alonso,88\nArsenal FC,Fabián Lisa,\n").unwrap();
    let sources = lma_import::Sources { managers: Some(&d.join("managers.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    let clubs = out.clubs.expect("a clubs.csv with the managers");
    let row = |id: &str| clubs.lines().map(|l| l.trim_start_matches('\u{feff}')).find(|l| l.starts_with(&format!("{id},"))).unwrap_or_default().to_string();
    assert!(row("0").contains(",Arteta,80"), "Arsenal keeps Arteta: {clubs}");
    assert!(row("1").contains(",Alonso,88"), "{clubs}");
    assert!(!clubs.contains(",Lisa,"), "{clubs}");
    assert_eq!(out.report.listed_managers_unmatched, ["Arsenal FC (its club already set by an earlier row)"]);
    assert_eq!((out.report.listed_managers, out.report.manager_reps_listed), (2, 2));
    std::fs::write(d.join("managers.csv"), "club,manager,reputation\nChelsea,Xabi Alonso,100\n").unwrap();
    assert!(lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).is_err(), "reputations are 0-99");
    std::fs::remove_dir_all(&d).unwrap();
}

/// FC 27 lists some players at two clubs (a loanee at both ends): the live squad claims one row,
/// the one already at the live club, and the other leaves our squads, so he isn't there twice.
#[test]
fn a_player_listed_at_two_clubs_ends_up_once_at_his_live_club() {
    let d = dir("live_dupes");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Twice,Listed,,2000-01-01,Arsenal,England,ST,,60,60\n\
        Twice,Listed,,2000-01-01,Chelsea,England,ST,,60,60\n\
        Other,Gunner,,2000-02-02,Arsenal,England,CM,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,631,Chelsea FC,1,Twice Listed,2000-01-01,23,Arsenal FC,50000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert_eq!(out.players.matches(",Listed,").count(), 1, "{}", out.players);
    assert_eq!(cell(&out.players, "Listed", "club_id"), "1", "at Chelsea, his live club");
    assert_eq!((out.report.live_matched, out.report.live_duplicates, out.report.live_moves.len()), (1, 1, 0));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A row moving to its live club where that club already has him under a birth date a few days
/// off (an FM fill row against FC 27's): merged into that row, which takes the live number.
#[test]
fn a_live_move_onto_the_same_player_merges_into_him() {
    let d = dir("live_merge");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Twice,Listed,,2000-01-01,Arsenal,England,ST,,60,60\n\
        Twice,Listed,,2000-01-05,Chelsea,England,ST,,60,60\n\
        Other,Gunner,,2000-02-02,Arsenal,England,CM,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,631,Chelsea FC,1,Twice Listed,2000-01-01,23,,50000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert_eq!(out.players.matches(",Listed,").count(), 1, "{}", out.players);
    assert_eq!(cell(&out.players, "Listed", "club_id"), "1");
    assert_eq!(cell(&out.players, "Listed", "squad_number"), "23", "the live number reaches the row kept");
    assert_eq!((out.report.live_duplicates, out.report.live_moves.len()), (1, 0));
    std::fs::remove_dir_all(&d).unwrap();
}

/// A live club whose name matches none of ours is the club most of its found players are at
/// (3 or more): "Gunners Football Club" is our Arsenal, so its players stay and its signing moves
/// in, instead of all of them leaving our squads as gone elsewhere.
#[test]
fn a_live_club_is_matched_by_its_players_votes() {
    let d = dir("live_votes");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        One,Gunner,,2000-01-01,Arsenal,England,ST,,60,60\n\
        Two,Gunner,,2000-01-02,Arsenal,England,CM,,60,60\n\
        Three,Gunner,,2000-01-03,Arsenal,England,CB,,60,60\n\
        New,Signing,,2000-01-04,Chelsea,England,GK,,60,60\n\
        Blue,Stays,,2000-01-05,Chelsea,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,11,Gunners Football Club,1,One Gunner,2000-01-01,9,,1000000\n\
         GB1,11,Gunners Football Club,2,Two Gunner,2000-01-02,8,,1000000\n\
         GB1,11,Gunners Football Club,3,Three Gunner,2000-01-03,5,,1000000\n\
         GB1,11,Gunners Football Club,4,New Signing,2000-01-04,1,,1000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert_eq!(out.players.matches(",Gunner,").count(), 3, "{}", out.players);
    assert_eq!(cell(&out.players, "Signing", "club_id"), "0", "the signing moves to Arsenal");
    assert_eq!((out.report.live_elsewhere, out.report.live_clubs_voted.len()), (0, 1));
    assert!(out.report.live_clubs_unmatched.is_empty(), "{:?}", out.report.live_clubs_unmatched);
    std::fs::remove_dir_all(&d).unwrap();
}

/// A name word alone doesn't make a live club ours: its found players must mostly be at that club.
/// "Arsenal Tula" shares a word with our Arsenal, but its two found players are Chelsea's, so it
/// stays a club that isn't ours and they leave our squads (not moved to Arsenal).
#[test]
fn a_name_word_match_needs_its_players() {
    let d = dir("live_word");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Gun,Stays,,2000-01-01,Arsenal,England,ST,,60,60\n\
        Blue,Goes,,2000-01-02,Chelsea,England,CM,,60,60\n\
        Blue,Also,,2000-01-03,Chelsea,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         RU1,99,Arsenal Tula,2,Blue Goes,2000-01-02,8,,1000000\n\
         RU1,99,Arsenal Tula,3,Blue Also,2000-01-03,5,,1000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert!(!out.players.contains(",Goes,") && !out.players.contains(",Also,"), "{}", out.players);
    assert_eq!(out.report.live_moves.len(), 0, "nobody moved to Arsenal");
    assert!(out.report.live_clubs_by_word.is_empty());
    std::fs::remove_dir_all(&d).unwrap();
}

/// `squad_overflow` (#538): a real squad bigger than `max_squad` keeps its best `max_squad` in the
/// squad array and the rest in spare records with no club, listed for squad_overflow.csv; without
/// it the extra players aren't written.
#[test]
fn squad_overflow_keeps_the_players_beyond_max_squad() {
    let d = dir("overflow");
    base(&d);
    let mut roster = String::from("first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n");
    for k in 0..13 {
        let pos = ["GK", "CB", "CM", "ST"][k % 4];
        roster += &format!("Real,Squad{k},,2000-01-{:02},Arsenal,England,{pos},,{},{}\n", k + 1, 80 - k, 80 - k);
    }
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    let with = |extra: &str| -> lma_import::Config { toml::from_str(&format!("max_squad = 10\nreal_squads = true\n{extra}{}", mapping_toml())).unwrap() };
    let out = lma_import::import(&d, &d.join("roster.csv"), None, None, &with("squad_overflow = true\n")).unwrap();
    for k in 0..10 {
        assert_eq!(cell(&out.players, &format!("Squad{k}"), "club_id"), "0", "Squad{k} is one of the best 10");
    }
    for k in 10..13 {
        assert_eq!(cell(&out.players, &format!("Squad{k}"), "club_id"), "", "Squad{k} is beyond the 10");
        assert_eq!(cell(&out.players, &format!("Squad{k}"), "squad_slot"), "");
    }
    assert_eq!(out.report.squad_overflow.len(), 3);
    assert!(out.report.squad_overflow.iter().all(|&(_, club)| club == 0));
    let csv = lma_import::squad_overflow_csv(&out.report.squad_overflow);
    assert!(csv.starts_with("id,club_id\n") && csv.lines().count() == 4, "{csv}");
    let out = lma_import::import(&d, &d.join("roster.csv"), None, None, &with("")).unwrap();
    assert!(out.report.squad_overflow.is_empty());
    assert!(!out.players.contains("Squad12"), "without squad_overflow the squad stops at max_squad");
    std::fs::remove_dir_all(&d).unwrap();
}

/// `live_add_missing` (the maintainer, 12 Oct: Max Dowman): a live squad player at one of our clubs whom the
/// roster lacks is added there from the live row; a roster player on the live squad isn't added
/// twice, and without the flag nobody is added.
#[test]
fn live_squads_add_players_the_roster_lacks() {
    let d = dir("live_add");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Star,Striker,,2001-02-02,Arsenal,England,ST,,70,70\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,11,Arsenal,9,Star Striker,2001-02-02,7,,80000000\n\
         GB1,11,Arsenal,1187629,Max Dowman,2009-12-31,56,,50000000\n\
         GB1,11,Arsenal,77,Starr Striker,2001-05-05,9,,1000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let with: lma_import::Config = toml::from_str(&format!("live_add_missing = true\n{}", mapping_toml())).unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &with).unwrap();
    assert_eq!(cell(&out.players, "Dowman", "club_id"), "0", "Dowman added at Arsenal");
    assert_eq!(cell(&out.players, "Dowman", "squad_number"), "56");
    assert_eq!(out.report.live_added.len(), 1, "{:?}", out.report.live_added);
    assert_eq!(out.report.live_add_skipped.len(), 1, "same surname and birth year as a roster player: a spelling, not a new one");
    assert_eq!(out.players.matches(",Striker,").count(), 1, "a roster player on the live squad isn't added again");
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &mapping()).unwrap();
    assert!(!out.players.contains("Dowman") && out.report.live_added.is_empty());
    std::fs::remove_dir_all(&d).unwrap();
}

/// `live_add_missing`'s other guards (#571): a live player born the same day as a roster player at
/// his club is that player under his known-as name (the first fc27 re-import added "Vinicius Junior"
/// beside "Vini Jr."), and one valued under `live_add_min_value_eur` isn't added.
#[test]
fn live_squads_skip_known_as_names_and_cheap_players() {
    let d = dir("live_add_guards");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Vinicius,Junior,Vini Jr.,2000-07-12,Arsenal,Brazil,LW,,85,85\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("live.csv"),
        "comp,club_id,club,player_id,name,birth_date,number,loan_from,market_value_eur\n\
         GB1,11,Arsenal,371998,Vinicius Paixao de Oliveira,2000-07-12,7,,150000000\n\
         GB1,11,Arsenal,5001,Young Reserve,2008-03-03,62,,500000\n\
         GB1,11,Arsenal,5002,Real Prospect,2008-04-04,63,,3000000\n",
    )
    .unwrap();
    let sources = lma_import::Sources { tm_live: Some(&d.join("live.csv")), ..Default::default() };
    let with: lma_import::Config = toml::from_str(&format!("live_add_missing = true\nlive_add_min_value_eur = 1000000\n{}", mapping_toml())).unwrap();
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &with).unwrap();
    assert_eq!(out.report.live_added.len(), 1, "only the prospect: {:?}", out.report.live_added);
    assert_eq!(cell(&out.players, "Prospect", "club_id"), "0");
    assert!(out.report.live_add_skipped.iter().any(|s| s.starts_with("Vinicius Paixao")), "{:?}", out.report.live_add_skipped);
    assert!(!out.players.contains("Paixao") && !out.players.contains("Reserve"));
    std::fs::remove_dir_all(&d).unwrap();
}

/// `--history-extra` (the maintainer, 12 Oct: no seasons between 2006/07 and 2011/12): its champions fill the
/// rows no other source has, by club name (a club with no source squad by the name the game shows);
/// a row the base already has stays; an unknown club is reported.
#[test]
fn history_extra_fills_the_empty_seasons() {
    let d = dir("history_extra");
    base(&d);
    std::fs::write(d.join("clubs.csv"), "id,name_E\n0,Arsenal\n1,Chelsea\n2,Gretna\n").unwrap();
    // the base's last season, 2005/06 (row 0 before the shift)
    std::fs::write(d.join("history.csv"), "country,tier,row,club_id,club,uid,won,drawn,lost,for,against,points\n0,0,0,1,Chelsea,,29,4,5,72,22,91\n").unwrap();
    let roster = "first_name,last_name,common_name,birthdate,club,nationality,position,alternate_positions,overall_rating,ovr\n\
        Keeper,One,,2000-01-01,Arsenal,England,GK,,60,60\n\
        Blue,Back,,2000-04-04,Chelsea,England,CB,,60,60\n";
    std::fs::write(d.join("roster.csv"), roster).unwrap();
    std::fs::write(
        d.join("extra.csv"),
        "country,tier,season,club,won,drawn,lost,for,against,points\n\
         0,0,2009,Chelsea,27,5,6,103,32,86\n\
         0,0,2010,Arsenal,23,11,4,78,37,80\n\
         0,0,2008,Gretna,20,5,5,60,30,65\n\
         0,0,2005,Arsenal,1,1,1,1,1,4\n\
         0,0,2011,Nowhere Town,28,5,5,93,29,89\n",
    )
    .unwrap();
    let cfg: lma_import::Config = toml::from_str(&format!("base_year = 2026\n{}", mapping_toml())).unwrap();
    let sources = lma_import::Sources { history_extra: Some(&d.join("extra.csv")), ..Default::default() };
    let out = lma_import::import_with(&d, &d.join("roster.csv"), &sources, &cfg).unwrap();
    let h = out.history.expect("a history.csv");
    // 2009/10 is 16 seasons before 2026/27 (row 16), 2010/11 row 15; the base's 2005/06 sits at row 20
    assert!(h.lines().any(|l| l == "0,0,16,1,,27,5,6,103,32,86"), "{h}");
    assert!(h.lines().any(|l| l == "0,0,15,0,,23,11,4,78,37,80"), "{h}");
    assert!(h.lines().any(|l| l == "0,0,17,2,,20,5,5,60,30,65"), "Gretna has no squad in any source: {h}");
    assert!(h.lines().any(|l| l.starts_with("0,0,20,1,")), "the base's row stays: {h}");
    assert_eq!(out.report.history_extra, 3);
    assert_eq!(out.report.history_unmatched.len(), 1, "{:?}", out.report.history_unmatched);
    std::fs::remove_dir_all(&d).unwrap();
}
