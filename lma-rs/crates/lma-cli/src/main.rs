//! Command-line tools for the LMA 2007 port.
//!
//!   lma-cli xex-info <default.xex> [--dump-image out.bin]
//!   lma-cli player <Export.mng> <id>
//!   lma-cli db-export <Export.mng> <csv_dir>            players/clubs/nations.csv
//!   lma-cli db-import <Export.mng> <csv_dir> <out.mng>  apply CSVs (only what changes)
//!   lma-cli scan-code <default.xex>
//!   lma-cli imports <default.xex>
//!   lma-cli run <default.xex> [max_instructions] [--mod <name>]   (or LMA_MOD=<name>)
//!   lma-cli compare <default.xex> <state> (--write-ref <dir> | --ref <dir>) [...]   (see compare.rs)

mod compare;
#[cfg(feature = "xma-ffmpeg")]
mod xma_check;

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mod_id = runtime::mods::selected(&mut args);
    let res = match args.first().map(String::as_str) {
        Some("xex-info") if args.len() >= 2 => xex_info(&args[1], args.get(3).filter(|_| args[2] == "--dump-image")),
        Some("player") if args.len() == 3 => player(&args[1], &args[2]),
        Some("diary") if (2..=3).contains(&args.len()) => diary(&args[1], args.get(2).map(String::as_str)),
        Some("db-export") if args.len() == 3 => db_export(&args[1], &args[2]),
        Some("db-import") if args.len() == 4 => db_import(&args[1], &args[2], &args[3]),
        Some("squad-import") if args.len() >= 6 && args.len() % 2 == 0 => squad_import(&args[1..]),
        Some("scan-code") if args.len() == 2 => scan_code(&args[1]),
        #[cfg(feature = "xma-ffmpeg")]
        Some("xma-check") if args.len() >= 4 && args[1] == "--cache" => xma_check::check_cache(&args[2], &args[3], args.get(4).map(|s| s.as_str())),
        #[cfg(feature = "xma-ffmpeg")]
        Some("audio-cache") if (3..=4).contains(&args.len()) => xma_check::build_cache(&args[1], &args[2], args.get(3).map(|s| s.as_str())),
        #[cfg(feature = "xma-ffmpeg")]
        Some("xma-check") if args.len() >= 2 => xma_check::main(&args[1], args.get(2).map(|s| s.as_str()), args.get(3).and_then(|s| s.parse().ok())),
        Some("archive") if args.len() >= 3 && (args[2] == "ls" || (args[2] == "extract" && args.len() == 4)) => archive(&args[1], &args[2], args.get(3).map(String::as_str)),
        Some("textures") if args.len() == 3 => textures(&args[1], &args[2]),
        Some("strings") if (2..=3).contains(&args.len()) => strings(&args[1], args.get(2).map(String::as_str)),
        Some("sounds") if (4..=5).contains(&args.len()) => sounds(&args[1], &args[2], &args[3], args.get(4).map(String::as_str)),
        Some("imports") if args.len() == 2 => imports(&args[1]),
        Some("disasm-shader") if args.len() == 2 => disasm_shader(&args[1]),
        Some("compare") if args.len() >= 3 => compare::main(&args[1], &args[2], &args[3..]),
        Some("state-mem") if args.len() == 4 => state_mem(&args[1], &args[2], &args[3]),
        Some("overall-check") if args.len() == 3 => overall_check(&args[1], &args[2]),
        Some("wgsl") if args.len() >= 2 => wgsl(&args[1..]),
        Some("run") if args.len() >= 2 => run(&args[1], args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000_000), mod_id),
        _ => Err("usage: lma-cli xex-info <default.xex> [--dump-image out.bin] | player <Export.mng> <id> | diary <Export.mng> [season 0-19] | db-export <Export.mng> <csv_dir> | db-import <Export.mng> <csv_dir> <out.mng> | archive <file.mng> ls | archive <file.mng> extract <dir> | textures <file.mng> <out_dir> | strings <LMA_E.lng or the Lang dir> [out.csv] | sounds <bank.xwb or Sound.mng> <cache_dir> <out_dir> [bank.xsb] | run <default.xex> [max_instructions] [--mod <name>] | compare <default.xex> <state> (--write-ref <dir> | --ref <dir>) | state-mem <default.xex> <state> <out.bin>".into()),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

type Res = Result<(), Box<dyn std::error::Error>>;

fn xex_info(path: &str, dump: Option<&String>) -> Res {
    let x = xex::Xex::parse(&std::fs::read(path)?)?;
    println!("image base  {:#010x}", x.image_base);
    println!("entry point {:#010x}", x.entry_point);
    println!("image size  {:#x}{}", x.image.len(), if x.used_devkit_key { " (devkit key)" } else { "" });
    println!("sections:");
    for (name, va, size, ch) in x.sections() {
        println!("  {name:8} {va:#010x} {size:#9x} flags {ch:#010x}");
    }
    let imports = x.imports();
    for lib in &x.libraries {
        let n = imports.iter().filter(|i| i.library == lib.name).count();
        let funcs = imports.iter().filter(|i| i.library == lib.name && i.kind == 1).count();
        println!("import {:16} v{:#x}: {n} records, {funcs} functions", lib.name, lib.version);
    }
    if let Some(out) = dump {
        std::fs::write(out, &x.image)?;
        println!("wrote {out}");
    }
    Ok(())
}

/// A save state's guest memory as one sparse file: every saved page at its offset in the flat
/// 4 GB guest space (holes read as zeros), for scanning game data with mmap.
/// `overall-check <default.xex> <state>`: every player's overall from the game's own function
/// (0x8211f4c8, interpreted on the state's memory) against `runtime::hooks::player_overall_of`, the
/// native one LMA_NATIVE_OVERALL=1 installs (#39). Prints the mismatches and a count.
fn overall_check(xex: &str, state: &str) -> Res {
    let x = xex::Xex::parse(&std::fs::read(xex)?)?;
    let game_dir = std::path::Path::new(xex).parent().unwrap_or(std::path::Path::new("."));
    let mut m = runtime::machine::Machine::new(&x, game_dir, &std::env::current_dir()?.join("userdata"));
    m.load_state(std::path::Path::new(state))?;
    let table = m.mem.read32(0x8273_D6AC);
    let count = if table == 0 { 0 } else { m.mem.read32(table + 4) };
    let (sp, ret) = (m.cpu.gpr[1].wrapping_sub(0x4000), 0x8200_0000u32);
    let (mut same, mut differ) = (0u32, 0u32);
    let mut roles = [0u32; 10];
    // the ids past the table too (the game reads low memory for them)
    for p in (0..count + 2).chain([u32::MAX]) {
        let native = runtime::hooks::player_overall_of(&mut m.mem, p);
        roles[runtime::hooks::player_role(&mut m.mem, p).min(9) as usize] += 1;
        m.cpu.gpr[1] = sp;
        m.cpu.gpr[3] = p as i32 as i64 as u64;
        m.cpu.lr = ret as u64;
        m.cpu.pc = 0x8211_F4C8;
        let mut steps = 0;
        while m.cpu.pc != ret && steps < 100_000 {
            m.cpu.step(&mut m.mem).map_err(|e| format!("player {p}: {e:?} at {:#x}", m.cpu.pc))?;
            steps += 1;
        }
        let game = m.cpu.gpr[3] as u32;
        // and the role function on its own
        m.cpu.gpr[1] = sp;
        m.cpu.gpr[3] = p as i32 as i64 as u64;
        m.cpu.lr = ret as u64;
        m.cpu.pc = 0x8211_BE88;
        while m.cpu.pc != ret {
            m.cpu.step(&mut m.mem).map_err(|e| format!("player {p} role: {e:?} at {:#x}", m.cpu.pc))?;
        }
        let (game_role, role) = (m.cpu.gpr[3] as u32, runtime::hooks::player_role(&mut m.mem, p));
        if native == Some(game) && role == game_role {
            same += 1;
        } else {
            differ += 1;
            if differ <= 20 {
                println!("player {p}: game {game} (role {game_role}), native {native:?} (role {role})");
            }
        }
    }
    println!("{count} players (+3 out of range): {same} equal, {differ} differ; roles {roles:?}");
    if differ > 0 {
        return Err("the native overall differs from the game's".into());
    }
    Ok(())
}

fn state_mem(xex: &str, state: &str, out: &str) -> Res {
    use std::io::{Seek, SeekFrom, Write};
    let x = xex::Xex::parse(&std::fs::read(xex)?)?;
    let game_dir = std::path::Path::new(xex).parent().unwrap_or(std::path::Path::new("."));
    let mut m = runtime::machine::Machine::new(&x, game_dir, &std::env::current_dir()?.join("userdata"));
    m.load_state(std::path::Path::new(state))?;
    let pages = m.mem.snapshot_pages();
    let mut f = std::fs::File::create(out)?;
    let mut bytes = 0u64;
    for (i, b) in &pages {
        f.seek(SeekFrom::Start(*i as u64 * b.len() as u64))?;
        f.write_all(b)?;
        bytes += b.len() as u64;
    }
    f.set_len(1 << 32)?;
    println!("wrote {out}: {} pages, {} MB", pages.len(), bytes >> 20);
    Ok(())
}

/// `Machine::run` until `target` instructions: in real time it also returns after 50 ms with
/// every thread idle (so a window stays responsive), which lma-cli must not take as the end.
fn run_until(m: &mut runtime::machine::Machine, target: u64) -> runtime::machine::Exit {
    loop {
        let exit = m.run(target);
        if !matches!(exit, runtime::machine::Exit::Limit) || m.clock >= target {
            return exit;
        }
    }
}

/// `LMA_NET=host:<port>` / `LMA_NET=join:<addr:port>` (#695, the online-play slice): two lma-cli
/// processes run the same state in lock-step over one TCP connection. Every `LMA_NET_STEP` million
/// instructions (default 5, about a frame) each side sends the buttons its own `LMA_INPUT` script
/// holds for the next step and waits for the other's; the host drives port 1 (controller 1) and
/// the joiner port 2 on both machines, so both compute the same world. At the end each prints
/// `net fingerprint <hash>` over the guest's RAM: equal on both sides when the lock-step held.
/// Deterministic mode only (`LMA_DETERMINISTIC=1`): real-time pacing would differ per machine.
fn net_lockstep(m: &mut runtime::machine::Machine, net: &str, presses: &[(u64, u16)], end: u64) -> Result<runtime::machine::Exit, Box<dyn std::error::Error>> {
    use std::io::{Read, Write};
    if !m.deterministic {
        return Err("LMA_NET needs LMA_DETERMINISTIC=1 (the two sides must pace alike)".into());
    }
    let (host, addr) = match net.split_once(':') {
        Some(("host", port)) => (true, format!("0.0.0.0:{port}")),
        Some(("join", addr)) => (false, addr.to_string()),
        _ => return Err("LMA_NET: want host:<port> or join:<addr:port>".into()),
    };
    let mut peer = if host {
        let listener = std::net::TcpListener::bind(&addr)?;
        eprintln!("net: host listening on {addr}");
        listener.accept()?.0
    } else {
        eprintln!("net: joining {addr}");
        std::net::TcpStream::connect(&addr)?
    };
    peer.set_nodelay(true)?;
    let step = std::env::var("LMA_NET_STEP").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(5) * 1_000_000;
    let (own_port, other_port) = if host { (0usize, 1usize) } else { (1, 0) };
    // both controllers are plugged in on both sides
    m.input.set(0, runtime::input::Pad::default());
    m.input.set(1, runtime::input::Pad::default());
    // hello: build and the state's clock must match
    let hello = format!("{} {}\n", runtime::BUILD, m.clock);
    peer.write_all(hello.as_bytes())?;
    let mut theirs = Vec::new();
    let mut b = [0u8; 1];
    while peer.read(&mut b)? == 1 && b[0] != b'\n' {
        theirs.push(b[0]);
    }
    let theirs = String::from_utf8_lossy(&theirs).to_string();
    if theirs != hello.trim_end() {
        return Err(format!("net: the other side runs {theirs:?}, this side {:?}", hello.trim_end()).into());
    }
    eprintln!("net: fingerprint {:016x} before the first step", ram_fingerprint(m));
    // as the plain path: events before the state's clock are skipped (replaying them would bump
    // the XInput packet number), so a script written for a resumed run doesn't start with a button held
    let presses: Vec<(u64, u16)> = presses.iter().copied().filter(|p| p.0 >= m.clock).collect();
    let mut own: u16 = 0;
    let mut next = 0usize;
    let mut steps = 0u64;
    let mut exit = runtime::machine::Exit::Limit;
    while m.clock < end {
        let target = (m.clock + step).min(end);
        // the buttons this side's script holds at the step's start (the last event at or before it)
        while next < presses.len() && presses[next].0 <= m.clock {
            own = presses[next].1;
            next += 1;
        }
        let mut frame = [0u8; 10];
        frame[..8].copy_from_slice(&m.clock.to_be_bytes());
        frame[8..].copy_from_slice(&own.to_be_bytes());
        peer.write_all(&frame)?;
        let mut got = [0u8; 10];
        if let Err(e) = peer.read_exact(&mut got) {
            // the other side ended (its limit, an error of its own, a closed window): not a desync
            return Err(format!("net: the other side ended at step {steps} (this side at {}): {e}", m.clock).into());
        }
        let (their_clock, their_buttons) = (u64::from_be_bytes(got[..8].try_into().unwrap()), u16::from_be_bytes(got[8..].try_into().unwrap()));
        if their_clock != m.clock {
            return Err(format!("net: desync, the other side is at {their_clock}, this side at {}", m.clock).into());
        }
        m.input.set(own_port, runtime::input::Pad { buttons: own, ..Default::default() });
        m.input.set(other_port, runtime::input::Pad { buttons: their_buttons, ..Default::default() });
        exit = run_until(m, target);
        steps += 1;
        if !matches!(exit, runtime::machine::Exit::Limit) {
            break;
        }
    }
    eprintln!("net: {steps} steps of {} M in lock-step, {} as {}", step / 1_000_000, if host { "host" } else { "joiner" }, if host { "port 1" } else { "port 2" });
    println!("net fingerprint {:016x} at {}", ram_fingerprint(m), m.clock);
    Ok(exit)
}

/// A fingerprint of the guest's virtual RAM (the image, its data and the heap at 0x8...; 0xA... is
/// physical memory, mostly the GPU's, which never moved in a headless test): FNV-1a over 1 MB reads.
/// `LMA_FINGERPRINT=1` prints it after a plain run too (the control for a lock-step test).
fn ram_fingerprint(m: &mut runtime::machine::Machine) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    let mut buf = vec![0u8; 1 << 20];
    for a in (0x8000_0000u32..0x9000_0000).step_by(1 << 20) {
        m.mem.read(a, &mut buf);
        for &x in &buf {
            h = (h ^ x as u64).wrapping_mul(0x100000001b3);
        }
    }
    h
}

/// `run_until` for a replay: also returns true when the clock stopped moving for ~5 s of host time
/// (`Machine::run` gives back every 50 ms while every guest thread waits), so a replay whose held
/// time nothing advances any more doesn't spin forever (#296).
fn run_until_or_stuck(m: &mut runtime::machine::Machine, target: u64) -> (runtime::machine::Exit, bool) {
    let clock = m.clock;
    until_or_stuck(target, clock, |t| {
        let exit = m.run(t);
        (exit, m.clock)
    })
}

/// Runs `step(target)` (which returns the exit and the clock reached) until the clock reaches
/// `target` or the run ends otherwise; (exit, true) once 100 `Limit` returns in a row left the
/// clock where it was.
fn until_or_stuck(target: u64, clock: u64, mut step: impl FnMut(u64) -> (runtime::machine::Exit, u64)) -> (runtime::machine::Exit, bool) {
    let (mut last, mut idle) = (clock, 0);
    loop {
        let (exit, clock) = step(target);
        if !matches!(exit, runtime::machine::Exit::Limit) || clock >= target {
            return (exit, false);
        }
        if clock == last {
            idle += 1;
            if idle >= 100 {
                return (exit, true);
            }
        } else {
            (last, idle) = (clock, 0);
        }
    }
}

/// `strings <LMA_<L>.lng | Lang dir> [out.csv]` (the asset converter, docs/NATIVE_GAME.md): the game's
/// string table(s) as CSV in the mods' `strings.csv` shape, `id,text,lang` (the language letter from
/// the file name: E, F, G, I, S), every string of every table, so a mod or the native text system
/// starts from the full export. One file: that language; a directory: every `LMA_?.lng` in it, in
/// letter order. Without `out.csv` the CSV goes to stdout.
fn strings(path: &str, out: Option<&str>) -> Res {
    let p = std::path::Path::new(path);
    let mut files: Vec<std::path::PathBuf> = if p.is_dir() {
        std::fs::read_dir(p)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|f| f.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.len() == 9 && n.starts_with("LMA_") && n.ends_with(".lng")))
            .collect()
    } else {
        vec![p.to_path_buf()]
    };
    files.sort();
    if files.is_empty() {
        return Err(format!("{path}: no LMA_?.lng files").into());
    }
    let mut csv = String::new();
    lma_data::csv::write_row(&mut csv, &["id", "text", "lang"]);
    let (mut tables, mut rows) = (0, 0);
    for f in &files {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let lang = name.strip_prefix("LMA_").and_then(|n| n.strip_suffix(".lng")).unwrap_or("?");
        let t = lma_data::lang::Lang::parse(&std::fs::read(f)?).map_err(|e| format!("{}: {e}", f.display()))?;
        for i in 0..t.strings.len() {
            lma_data::csv::write_row(&mut csv, &[i.to_string(), t.get(i).unwrap_or_default(), lang.to_string()]);
            rows += 1;
        }
        tables += 1;
    }
    match out {
        Some(o) => {
            std::fs::write(o, csv)?;
            println!("{rows} strings from {tables} table(s) written to {o}");
        }
        None => print!("{csv}"),
    }
    Ok(())
}

/// `sounds <bank.xwb | Sound.mng> <cache_dir> <out_dir> [bank.xsb]` (the asset converter,
/// docs/NATIVE_GAME.md): every wave of the file's wave bank(s), from the decoded-audio cache
/// (`runtime::xact_cache`, `userdata/audio`, written at install: no decoding here), as
/// `<out_dir>/<bank>/<wave name or index>.wav` (16-bit PCM, the cache's channels and rate); with the
/// sound bank, `<out_dir>/<bank>/cues.json`: cue name, sound, wave, looping, volume, pitch. A wave the
/// install couldn't decode is listed, not written. Never into the repo.
fn sounds(path: &str, cache: &str, out: &str, xsb: Option<&str>) -> Res {
    let bytes = std::fs::read(path)?;
    let banks = lma_data::xact::wave_banks(&bytes)?;
    if banks.is_empty() {
        return Err(format!("{path}: no wave bank").into());
    }
    let (mut written, mut missing) = (0usize, Vec::new());
    for (entry, data) in &banks {
        let bank = lma_data::xact::WaveBank::parse(data)?;
        let name = if bank.name.is_empty() { entry.trim_end_matches(".xwb").to_string() } else { bank.name.clone() };
        let dir = std::path::Path::new(out).join(&name);
        std::fs::create_dir_all(&dir)?;
        let cached = runtime::xact_cache::Cached::open(std::path::Path::new(cache), &bank, data)
            .ok_or_else(|| format!("{name}: no complete cache in {cache} (lma-cli audio-cache builds it)"))?;
        for (i, w) in bank.waves.iter().enumerate() {
            let Some(pcm) = cached.wave(i) else {
                missing.push(format!("{name}/{i}"));
                continue;
            };
            let stem = w.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| format!("{i:04}"));
            // a wave name is a file name here: one plain component, as `archive extract` and `textures` require
            if std::path::Path::new(&stem).components().count() != 1 || std::path::Path::new(&stem).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                return Err(format!("{name}/{i}: a wave name that leaves the output folder ({stem:?})").into());
            }
            write_wav(&dir.join(format!("{stem}.wav")), &pcm)?;
            written += 1;
        }
    }
    if let Some(xsb) = xsb {
        let sb = lma_data::xact::SoundBank::parse(&std::fs::read(xsb)?)?;
        let mut j = String::from("[\n");
        for c in &sb.cues {
            let snd = sb.sounds.get(c.sound);
            let wave = snd.and_then(|s| s.wave).map_or("null".to_string(), |(b, w)| {
                format!("{{\"bank\": {}, \"index\": {w}}}", json_str(&sb.wave_banks.get(b as usize).cloned().unwrap_or_default()))
            });
            j.push_str(&format!("  {{\"cue\": {}, \"sound\": {}, \"wave\": {wave}, \"looping\": {}, \"volume\": {}, \"pitch\": {}}},\n",
                json_str(&c.name), c.sound, snd.is_some_and(|s| s.looping), snd.map_or(0, |s| s.volume), snd.map_or(0, |s| s.pitch)));
        }
        if j.ends_with(",\n") {
            j.truncate(j.len() - 2);
            j.push('\n');
        }
        j.push_str("]\n");
        let name = if sb.name.is_empty() { "sounds".to_string() } else { sb.name.clone() };
        let dir = std::path::Path::new(out).join(&name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("cues.json"), j)?;
        println!("{} cues written to {}", sb.cues.len(), dir.join("cues.json").display());
    }
    for m in &missing {
        eprintln!("not in the cache (decode it live): {m}");
    }
    println!("{written} waves written under {out}; {} not decoded at install", missing.len());
    Ok(())
}

/// `s` as a JSON string literal: quotes, backslashes and control characters escaped the JSON way
/// (`\uXXXX`), everything else (accents included) as UTF-8.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A 16-bit PCM WAV file: the RIFF header and the interleaved samples.
fn write_wav(path: &std::path::Path, pcm: &runtime::xact::Pcm) -> std::io::Result<()> {
    let data_len = (pcm.samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&(pcm.channels as u16).to_le_bytes());
    out.extend_from_slice(&pcm.rate.to_le_bytes());
    out.extend_from_slice(&(pcm.rate * pcm.channels * 2).to_le_bytes());
    out.extend_from_slice(&((pcm.channels * 2) as u16).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in &pcm.samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)
}

/// `textures <file.mng> <out_dir>` (the asset converter, docs/NATIVE_GAME.md): every DDS and TIM2
/// leaf of a BG2 archive and the archives nested in it, decoded to RGBA8 (`lma_data::textures`)
/// and written as `<out_dir>/<archive path>/<name>.png` (level 0 only); other leaves are skipped
/// and counted. Never into the repo: the pictures are the player's.
fn textures(path: &str, out: &str) -> Res {
    let ar = lma_data::Archive::parse(&std::fs::read(path)?)?;
    let dir = std::path::Path::new(out);
    let (mut written, mut skipped, mut failed) = (0usize, 0usize, Vec::new());
    let mut err = None;
    ar.walk(&mut |p, d| {
        if err.is_some() {
            return;
        }
        if !lma_data::textures::is_texture(d) {
            skipped += 1;
            return;
        }
        if std::path::Path::new(p).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            err = Some(format!("{p}: an entry name that leaves the output folder"));
            return;
        }
        let t = match lma_data::textures::decode(d) {
            Ok(t) => t,
            Err(e) => {
                failed.push(format!("{p}: {e}"));
                return;
            }
        };
        let to = dir.join(format!("{p}.png"));
        let r = to.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| {
            let mut enc = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(&to)?), t.width, t.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().map_err(std::io::Error::other)?;
            w.write_image_data(&t.data).map_err(std::io::Error::other)
        });
        match r {
            Ok(()) => written += 1,
            Err(e) => err = Some(format!("{}: {e}", to.display())),
        }
    });
    if let Some(e) = err {
        return Err(e.into());
    }
    for f in &failed {
        eprintln!("skipped: {f}");
    }
    println!("{written} textures written under {}; {skipped} other files; {} not decodable", dir.display(), failed.len());
    Ok(())
}

/// `archive <file.mng> ls | extract <dir>` (#873, the asset converter's first step): every leaf file
/// of a BG2 archive and the archives nested in it (`outer.mng/inner.mng/file.dds`). `ls` prints
/// size and path, then a count per extension; `extract` writes the leaves under `<dir>` with that
/// path (never into the repo: the disc's files are the player's).
fn archive(path: &str, _what: &str, out: Option<&str>) -> Res {
    let ar = lma_data::Archive::parse(&std::fs::read(path)?)?;
    let mut by_ext: std::collections::BTreeMap<String, (usize, u64)> = Default::default();
    let (mut files, mut bytes) = (0usize, 0u64);
    let dir = out.map(std::path::Path::new);
    let mut err = None;
    ar.walk(&mut |p, d| {
        let ext = match p.rsplit_once('.') {
            Some((_, e)) if !e.contains('/') => e.to_ascii_lowercase(),
            _ => "(none)".to_string(),
        };
        let e = by_ext.entry(ext).or_default();
        e.0 += 1;
        e.1 += d.len() as u64;
        files += 1;
        bytes += d.len() as u64;
        match dir {
            None => println!("{:>10} {p}", d.len()),
            Some(dir) if err.is_none() => {
                // a table name that leaves the output folder (`..`, an absolute path) is refused, as the
                // disc reader refuses such names (#834); the entry's bytes are the player's, so hygiene only
                if std::path::Path::new(p).components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
                    err = Some(format!("{p}: an entry name that leaves the output folder"));
                    return;
                }
                let to = dir.join(p);
                let r = to.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&to, d));
                if let Err(e) = r {
                    err = Some(format!("{}: {e}", to.display()));
                }
            }
            _ => {}
        }
    });
    if let Some(e) = err {
        return Err(e.into());
    }
    match dir {
        None => println!("# {files} files in {path} ({bytes} bytes)"),
        Some(dir) => println!("{files} files ({bytes} bytes) written under {}", dir.display()),
    }
    for (ext, (n, b)) in &by_ext {
        println!("# {n:>7} .{ext:<8} {b} bytes");
    }
    Ok(())
}

/// The game code's `.text` range (start, size) for the mod space's code patches.
fn text_section(x: &xex::Xex) -> (u32, u32) {
    x.sections().into_iter().find(|s| s.0 == ".text").map_or((x.image_base, x.image.len() as u32), |s| (s.1, s.2))
}

/// `diary <Export.mng> [season]` (#703): a season's calendar as CSV (the transfer windows as comment
/// lines, then every round with its league, date and fixtures as clubs.csv ids); all 20 without a season.
fn diary(path: &str, season: Option<&str>) -> Res {
    let ar = lma_data::Archive::parse(&std::fs::read(path)?)?;
    let seasons: Vec<u32> = match season {
        Some(s) => vec![s.parse()?],
        None => (0..lma_data::diary::SEASONS).collect(),
    };
    println!("season,round,league,date,slots,home,away");
    for k in seasons {
        let s = lma_data::diary::Season::read(&ar, k)?;
        for (n, w) in s.windows.iter().enumerate() {
            let (o, c) = (lma_data::diary::date(s.year(), w.open_day), lma_data::diary::date(s.year(), w.close_day));
            println!("# {}/{:02} transfer window {}: {}-{:02}-{:02} to {}-{:02}-{:02}", s.year(), (s.year() + 1) % 100, n + 1, o.0, o.1, o.2, c.0, c.1, c.2);
        }
        for (i, r) in s.rounds.iter().enumerate() {
            let date = r.day.map(|d| lma_data::diary::date(s.year(), d as u32)).map_or(String::new(), |(y, m, d)| format!("{y}-{m:02}-{d:02}"));
            let league = r.league.map_or(String::new(), |l| l.to_string());
            if r.fixtures.is_empty() {
                println!("{k},{i},{league},{date},{},,", r.slots);
            }
            for (h, a) in &r.fixtures {
                println!("{k},{i},{league},{date},{},{h},{a}", r.slots);
            }
        }
    }
    Ok(())
}

fn player(path: &str, id: &str) -> Res {
    let db = lma_data::Database::open(path)?;
    let p = db.player(id.parse()?);
    let owners = db.owners();
    let club = owners[p.id].map_or("free agent".to_string(), |c| db.club_name(c).to_string());
    println!("{} ({}) #{} {} age {} {}cm, {club}", p.display_name(), p.id, p.squad_number, p.position.label(), p.age, p.height_cm);
    for (f, v) in lma_data::db::RATINGS.iter().zip(p.ratings) {
        println!("  {:14} {v}", f.name);
    }
    Ok(())
}

fn db_export(mng: &str, dir: &str) -> Res {
    let db = lma_data::Database::open(mng)?;
    lma_data::edit::export(&db, std::path::Path::new(dir))?;
    println!("exported {} players, {} clubs to {dir}", db.player_count(), db.club_count());
    Ok(())
}

fn db_import(mng: &str, dir: &str, out: &str) -> Res {
    let mut db = lma_data::Database::open(mng)?;
    for note in lma_data::edit::apply(&mut db, std::path::Path::new(dir))? {
        println!("note: {note}");
    }
    let bytes = db.build()?;
    std::fs::write(out, &bytes)?;
    println!("wrote {out} ({} bytes)", bytes.len());
    Ok(())
}

/// Functions from .pdata as (start, length in bytes).
fn functions(x: &xex::Xex) -> Vec<(u32, u32)> {
    let Some((_, va, size, _)) = x.sections().into_iter().find(|s| s.0 == ".pdata") else { return vec![] };
    (0..size / 8)
        .filter_map(|i| {
            let start = x.read_u32(va + i * 8)?;
            let packed = x.read_u32(va + i * 8 + 4)?;
            (start != 0).then_some((start, ((packed >> 8) & 0x3F_FFFF) * 4))
        })
        .collect()
}

fn scan_code(path: &str) -> Res {
    let x = xex::Xex::parse(&std::fs::read(path)?)?;
    let funcs = functions(&x);
    let mut counts: std::collections::BTreeMap<&str, u64> = Default::default();
    let mut unknown: std::collections::BTreeMap<(u32, u32), (u64, u32)> = Default::default();
    let mut total = 0u64;
    for &(start, len) in &funcs {
        for a in (start..start + len).step_by(4) {
            let Some(w) = x.read_u32(a) else { continue };
            total += 1;
            match ppc::lookup(w) {
                Some(m) => *counts.entry(m).or_default() += 1,
                None => {
                    let e = unknown.entry((ppc::opcd(w), ppc::xo10(w))).or_insert((0, w));
                    e.0 += 1;
                }
            }
        }
    }
    let known: u64 = counts.values().sum();
    println!("{} functions, {total} instructions, {known} recognised ({:.3}%), {} distinct mnemonics",
        funcs.len(), known as f64 * 100.0 / total as f64, counts.len());
    let mut by: Vec<_> = counts.into_iter().collect();
    by.sort_by(|a, b| b.1.cmp(&a.1));
    for (m, n) in by.iter() {
        println!("  {m:12} {n}");
    }
    let mut un: Vec<_> = unknown.into_iter().collect();
    un.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!("unrecognised forms ({}):", un.len());
    for ((op, xo), (n, w)) in un.iter().take(25) {
        println!("  opcd {op:2} xo {xo:4}  x{n:<6} e.g. {w:#010x}");
    }
    Ok(())
}

fn imports(path: &str) -> Res {
    let x = xex::Xex::parse(&std::fs::read(path)?)?;
    for i in x.imports() {
        let (name, var) = runtime::exports::lookup(&i.library, i.ordinal).unwrap_or(("<unknown>", false));
        if i.kind == 1 || var {
            let kind = if var { " (variable)" } else { "" };
            println!("{:12} {:3} {:#010x} {name}{kind}", i.library, i.ordinal, i.address);
        }
    }
    Ok(())
}

fn run(path: &str, limit: u64, mod_id: Option<String>) -> Res {
    let x = xex::Xex::parse(&std::fs::read(path)?)?;
    let game_dir = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("."));
    let mut userdata = std::env::current_dir()?.join("userdata");
    // a replay runs on a copy of the user folder the recording started with
    if let Ok(rec) = std::env::var("LMA_REPLAY") {
        match runtime::journal::replay_userdata(&rec)? {
            Some(dir) => userdata = dir,
            None => eprintln!("replay: WARNING {rec}.userdata missing (older recording): using ./userdata, exact only if it holds the same files"),
        }
    }
    let space = mod_id.map(|id| runtime::mods::ModSpace::load(std::path::Path::new("mods"), &id)).transpose()?;
    let user_dir = space.as_ref().map_or(userdata.clone(), |s| s.user_dir(&userdata));
    let mut m = runtime::machine::Machine::new(&x, game_dir, &user_dir);
    if let Some(s) = &space {
        let mut notes = s.install(&mut m, game_dir)?;
        notes.extend(s.patch_image(&mut m.mem, text_section(&x))?);
        m.after_load_writes = s.data_words(&mut m.mem);
        eprintln!("mod {}: {} {} (base {}), saves in {}", s.id, s.name, s.version, s.base, user_dir.display());
        for n in notes {
            eprintln!("  {n}");
        }
    }
    if std::env::var("LMA_INTERP").is_err() {
        let n = m.install_blocks(game_blocks::PARTS);
        eprintln!("recompiled blocks: {n} (build {})", runtime::BUILD);
        eprintln!("time: {} (LMA_DETERMINISTIC=1 for instruction-count time)", if m.deterministic { "deterministic" } else { "real-time" });
        if std::env::var("LMA_INTERP_STATS").is_ok() {
            runtime::rec::INTERP_STATS_ON.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if std::env::var("LMA_MISSES").is_ok() {
            m.misses = Some(Default::default());
        }
    }
    let t = std::time::Instant::now();
    let profile = std::env::var("LMA_PROFILE").is_ok();
    let func_profile = std::env::var("LMA_FUNC_PROFILE").ok();
    // LMA_CALL_TARGETS=1 (#171 / #145): count indirect calls by (site, target), printed at the end
    if std::env::var("LMA_CALL_TARGETS").as_deref() == Ok("1") {
        guest::rec::CALL_TARGETS_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if func_profile.is_some() {
        m.pc_samples = Some(Default::default());
    }
    m.break_at = std::env::var("LMA_BREAK").ok().and_then(|v| u32::from_str_radix(&v, 16).ok());
    m.reg_watch = std::env::var("LMA_REGWATCH").ok().and_then(|v| v.parse().ok()).map(|r| (r, 5_000_000));
    if let Ok(list) = std::env::var("LMA_FRAMES") {
        m.gpu.dump_frames = list.split(',').filter_map(|v| v.parse().ok()).collect();
    }
    m.gpu.render = std::env::var("LMA_NORENDER").is_err();
    // LMA_PRESENT_ALL=1 (#863): present every frame as lma-app does, so the render thread throttles the game per frame
    m.gpu.present_all = std::env::var_os("LMA_PRESENT_ALL").is_some();
    // #864: headless runs skip every resolve read-back unless LMA_SKIP_LAND says otherwise (lma-app lands them)
    runtime::gpu_wgpu::SKIP_LAND_DEFAULT.store(true, std::sync::atomic::Ordering::Relaxed);
    // #876: headless runs draw the tiled scene once unless LMA_SINGLE_PASS says otherwise (lma-app replays)
    runtime::gpu::SINGLE_PASS_DEFAULT.store(true, std::sync::atomic::Ordering::Relaxed);
    m.gpu.render_every = std::env::var("LMA_RENDER_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    m.gpu.capture_frame = std::env::var("LMA_CAPTURE").ok().and_then(|v| v.parse().ok());
    m.watch = std::env::var("LMA_WATCH").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()); // 0x optional, as lma-app
    // LMA_RWATCH=<hex>: guest loads of that word, reported with the next check point's pc (feature rwatch)
    #[cfg(feature = "rwatch")]
    if let Some(a) = std::env::var("LMA_RWATCH").ok().and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()) {
        guest::mem::RWATCH.store(a, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some((x, y)) = std::env::var("LMA_PIXEL").ok().and_then(|v| v.split_once(',').map(|(a, b)| (a.parse().ok(), b.parse().ok()))).and_then(|(a, b)| a.zip(b)) {
        *runtime::raster::PIXEL_TRACE.lock().unwrap() = Some((x, y, Vec::new()));
    }
    // LMA_BACKEND=wgpu renders pixels on the GPU (#6); installed before LMA_LOAD restores EDRAM
    if std::env::var("LMA_BACKEND").as_deref() == Ok("wgpu") {
        let gpu = runtime::gpu_wgpu::WgpuBackend::new()?;
        eprintln!("backend: wgpu on {}", gpu.adapter_name());
        m.gpu.renderer = if std::env::var("LMA_RENDER_THREAD").as_deref() == Ok("0") { Box::new(gpu) } else { Box::new(runtime::threaded::threaded(gpu, &m.mem)) };
    }
    // LMA_LOAD=state resumes a save state; LMA_SAVE=state writes one when the run ends
    if let Ok(path) = std::env::var("LMA_LOAD") {
        let t = std::time::Instant::now();
        m.load_state(std::path::Path::new(&path))?;
        eprintln!("loaded {path} at {} instructions in {:.2?}", m.clock, t.elapsed());
    }
    // LMA_POKE="addr:hexbytes,...": write guest memory once the state is loaded (runtime::machine::parse_poke)
    m.apply_pokes()?;
    // LMA_REPLAY=path replays an lma-app LMA_RECORD session exactly, from path.state (#108). Use
    // LMA_BACKEND=wgpu like lma-app: resolves land in guest memory.
    if let Ok(path) = std::env::var("LMA_REPLAY") {
        m.load_state(std::path::Path::new(&format!("{path}.state")))?;
        m.journal = Some(runtime::journal::Journal::replay(&format!("{path}.journal"))?);
        m.deterministic = false;
        eprintln!("replaying {path} from {} instructions", m.clock);
        if let Some(w) = m.journal.as_ref().and_then(|j| j.build_warning()) {
            eprintln!("replay: WARNING {w}");
        }
    }
    // LMA_INPUT="at:buttons:len,..." presses controller buttons (hex mask, runtime::input::button)
    // from `at` for `len` million instructions, e.g. "3600:10:100" = START at 3.6B for 0.1B.
    // an item may end in `:port` (1-4) to press another controller's buttons (#693)
    let presses_all = runtime::input::parse_script_ports(&std::env::var("LMA_INPUT").unwrap_or_default())?; // (clock, port, buttons from then on)
    let presses: Vec<(u64, u16)> = presses_all.iter().filter(|e| e.1 == 0).map(|&(at, _, b)| (at, b)).collect();
    m.input.set(0, runtime::input::Pad::default()); // controller 1 plugged in from boot
    // and every other controller the script presses (as LMA_PADS=n would)
    for port in presses_all.iter().map(|e| e.1).filter(|&p| p > 0).collect::<std::collections::BTreeSet<_>>() {
        m.input.set(port, runtime::input::Pad::default());
    }
    let end = if profile { limit.saturating_sub(20_000_000) } else { limit };
    // LMA_FRAME_PROFILE=1: where both threads' time went per guest frame, printed at the end (#6)
    runtime::prof::ON.store(std::env::var("LMA_FRAME_PROFILE").as_deref() == Ok("1"), std::sync::atomic::Ordering::Relaxed);
    let prof_start = (runtime::prof::snapshot(), m.gpu.stats.frames, std::time::Instant::now());
    let mut exit = runtime::machine::Exit::Limit;
    let mut replay_pad = (m.gpu.render, runtime::input::Pad::default());
    // after a state load the instruction clock repeats: LMA_REPLAY_AFTER_LOADS=k stops at the
    // limit only once k recorded loads have been replayed
    let after_loads: u32 = std::env::var("LMA_REPLAY_AFTER_LOADS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut loads = 0u32;
    while m.journal.as_ref().is_some_and(|j| j.is_replay()) {
        while let Some(ui) = m.journal.as_mut().unwrap().replay_ui(m.clock) {
            match ui {
                runtime::journal::Ui::Pad(render, pad) => replay_pad = (render, pad),
                runtime::journal::Ui::Load(n) => {
                    let path = format!("{}.load{n}.state", std::env::var("LMA_REPLAY").unwrap_or_default());
                    m.load_state(std::path::Path::new(&path))?;
                    loads += 1;
                    eprintln!("replay: loaded {path} ({} instructions)", m.clock);
                }
            }
        }
        // like lma-app, which sets them before every run (a loaded state brings its own pad)
        m.gpu.render = replay_pad.0;
        m.input.set(0, replay_pad.1);
        let clock = m.clock;
        let j = m.journal.as_mut().unwrap();
        if j.played_out(clock) {
            // played out, and past the end mark of the last save or the stop (the recorded time
            // holds up to it, #296): the rest of the run is ordinary real time
            eprintln!("replay: finished at {} instructions, {} divergences", m.clock, j.diverged);
            m.end_journal();
            break;
        }
        let stop = if loads >= after_loads { end } else { u64::MAX };
        let next = j.next_pad_clock().or(j.end_clock().map(|c| c + 1)).unwrap_or(stop).min(stop);
        let stuck;
        (exit, stuck) = run_until_or_stuck(&mut m, next);
        if stuck {
            // every guest thread waits on a time the replay holds still and no event is left to
            // move it: the replay has diverged (#298 hung 44 min here); carry on live
            eprintln!("replay: no progress at {} instructions for 5 s (time held, no event reached): diverged, carrying on in real time", m.clock);
            m.end_journal();
            break;
        }
        if !matches!(exit, runtime::machine::Exit::Limit) || (m.clock >= end && loads >= after_loads) {
            eprintln!("replay: {} divergences so far", m.journal.as_ref().unwrap().diverged);
            break;
        }
    }
    // presses before the start (a loaded state's clock) are past: the state brings its own pad, and
    // replaying them would bump the XInput packet number the game sees (a resumed run then
    // differed from a straight one)
    let start = m.clock;
    if let Ok(net) = std::env::var("LMA_NET") {
        // LMA_NET=host:<port> | join:<addr:port> (#695): two processes in deterministic lock-step
        exit = net_lockstep(&mut m, &net, &presses, end)?;
    } else {
        for (at, port, buttons) in presses_all.iter().copied().filter(|p| p.0 >= start && p.0 < end) {
            exit = run_until(&mut m, at);
            if !matches!(exit, runtime::machine::Exit::Limit) {
                break;
            }
            m.input.set(port, runtime::input::Pad { buttons, ..Default::default() });
            if port == 0 {
                eprintln!("input: buttons {buttons:#06x} at {}", m.clock);
            } else {
                eprintln!("input: controller {} buttons {buttons:#06x} at {}", port + 1, m.clock);
            }
        }
        if matches!(exit, runtime::machine::Exit::Limit) {
            exit = run_until(&mut m, end);
        }
        if std::env::var_os("LMA_FINGERPRINT").is_some() {
            println!("fingerprint {:016x} at {}", ram_fingerprint(&mut m), m.clock);
        }
    }
    if runtime::prof::ON.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{}", runtime::prof::report(&prof_start.0, &runtime::prof::snapshot(), m.gpu.stats.frames - prof_start.1, prof_start.2.elapsed().as_secs_f64()));
    }
    if profile {
        m.pc_samples = Some(Default::default());
        exit = run_until(&mut m, limit);
        let mut hot: Vec<_> = m.pc_samples.take().unwrap().into_iter().collect();
        hot.sort_by(|a, b| b.1.cmp(&a.1));
        println!("hot pcs (last 20M instructions):");
        for ((th, pc), n) in hot.iter().take(25) {
            let w = m.mem.read32(*pc);
            println!("  t{th} {pc:#010x} x{n:<6} {:08x} {}", w, ppc::lookup(w).unwrap_or("?"));
        }
    }
    let native = runtime::hooks::NATIVE_CALLS.load(std::sync::atomic::Ordering::Relaxed);
    if native > 0 {
        eprintln!("native blocks: {native} overalls");
    }
    if !m.hook_calls.is_empty() {
        let mut v: Vec<_> = m.hook_calls.iter().collect();
        v.sort();
        let calls: Vec<String> = v.iter().map(|(a, n)| format!("{a:#010x} x{n}")).collect();
        eprintln!("hooks: {}", calls.join(", "));
    }
    if let (Some(out), Some(samples)) = (&func_profile, m.pc_samples.take()) {
        // map sampled PCs to their .pdata function and write "start samples" sorted by heat
        let funcs = functions(&x);
        let mut starts: Vec<u32> = funcs.iter().map(|f| f.0).collect();
        starts.sort();
        let mut heat: std::collections::HashMap<u32, u64> = Default::default();
        let mut per_thread: std::collections::BTreeMap<usize, u64> = Default::default();
        for (&(th, _), &n) in &samples {
            *per_thread.entry(th).or_default() += n;
        }
        let all: u64 = per_thread.values().sum();
        // per thread: its share and its hottest functions (real work, spin-waits or idle loops)
        let mut by_thread_func: std::collections::HashMap<(usize, u32), u64> = Default::default();
        for (&(th, pc), &n) in &samples {
            let i = starts.partition_point(|&s| s <= pc);
            if i > 0 {
                *by_thread_func.entry((th, starts[i - 1])).or_default() += n;
            }
        }
        for (th, n) in &per_thread {
            let mut top: Vec<_> = by_thread_func.iter().filter(|((t, _), _)| t == th).map(|((_, f), c)| (*f, *c)).collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            let tops: Vec<String> = top.iter().take(5).map(|(f, c)| format!("{f:#010x} {:.0}%", *c as f64 * 100.0 / *n as f64)).collect();
            println!("thread {th}: {:.1}% of samples; top {}", *n as f64 * 100.0 / all.max(1) as f64, tops.join(", "));
        }
        for ((_, pc), n) in samples {
            let i = starts.partition_point(|&s| s <= pc);
            if i > 0 {
                *heat.entry(starts[i - 1]).or_default() += n;
            }
        }
        let mut v: Vec<_> = heat.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = v.iter().map(|x| x.1).sum();
        let lines: Vec<String> = v.iter().map(|(f, n)| format!("{f:#010x} {n}")).collect();
        std::fs::write(out, lines.join("
"))?;
        let mut acc = 0u64;
        for (k, (_, n)) in v.iter().enumerate() {
            acc += n;
            if [10, 100, 500, 1000, 2000].contains(&(k + 1)) {
                println!("top {} functions cover {:.1}% of samples", k + 1, acc as f64 * 100.0 / total as f64);
            }
        }
    }
    let secs = t.elapsed().as_secs_f64();
    std::fs::write("last_run.log", m.log.join("
"))?;
    let skip = m.log.len().saturating_sub(60);
    if skip > 0 {
        println!("... {skip} earlier kernel calls");
    }
    for l in &m.log[skip..] {
        println!("{l}");
    }
    println!("exit: {exit:?}");
    // a trap names the guard; the callers come from the back chain (#816: one twi guard in a helper
    // with a savegpr prologue, 11 call sites: lr already points inside the helper and the arguments
    // sit in r23..r31); callers[0] is the call site of the trapping function (runtime::backchain).
    if let runtime::machine::Exit::Stopped { thread, stop: guest::cpu::Stop::Trap { .. } } = &exit {
        let (gpr, lr) = { let c = if *thread == m.cur { &m.cpu } else { &m.threads[*thread].cpu }; (c.gpr, c.lr as u32) };
        let sp = gpr[1] as u32;
        let callers = runtime::backchain::back_chain(&mut m.mem, sp, 6);
        let words = |lo: usize, hi: usize| gpr[lo..=hi].iter().map(|r| *r as u32).collect::<Vec<_>>();
        println!("  lr {lr:#010x}, callers {callers:#x?}, sp {sp:#010x}");
        println!("  r3..r10 {:#x?}, r23..r31 {:#x?}", words(3, 10), words(23, 31));
        println!("  frame words sp+0x40..sp+0x5c {:#x?}", (0..8).map(|i| m.mem.read32(sp + 0x40 + 4 * i)).collect::<Vec<_>>());
    }
    if m.audio.enabled {
        println!("audio: client {:x?}, {} callbacks, {} frames submitted, {} not silent; XMA: {} context updates", m.audio.client, m.audio.calls, m.audio.frames, m.audio.loud, m.audio.xma_updates);
        if let Some(s) = runtime::native_audio::summary() {
            println!("{s}");
        }
        println!("  channel peaks (FL FR C LFE BL BR): {:?}, {} samples NaN or infinite", m.audio.ch_peak, m.audio.non_finite);
        let e = m.audio.errors();
        println!("  invariants: {} stale ring blocks, {} loop jumps, {} context resets, {} decode errors, {} frames dropped after stalls (latest callback {} ms late), {} silent callbacks filled", e[0], e[1], e[2], e[3], e[5], m.audio.late_max / (runtime::threads::CLOCK_HZ / 1000), m.audio.silent_fills);
        for (i, c) in m.audio.xma_hw.iter().enumerate().filter(|(_, c)| c.packets > 0) {
            println!("  xma {i}: {} packets -> {} samples, peak {}{}", c.packets, c.samples, c.peak, if c.decoder.is_none() { " (no decoder)" } else { "" });
        }
        if let (Some(path), Some(c)) = (std::env::var_os("LMA_AUDIO_WAV"), &m.audio.capture) {
            std::fs::write(&path, runtime::audio::wav(c))?;
            println!("audio: {} s written to {}", c.len() / runtime::audio::RATE as usize, path.to_string_lossy());
        }
    }
    println!("{} instructions in {secs:.2}s ({:.1} M/s)", m.clock, m.clock as f64 / secs / 1e6);
    if let Some((_, _, lines)) = runtime::raster::PIXEL_TRACE.lock().unwrap().take() {
        std::fs::write("pixel_trace.txt", lines.join("
"))?;
        eprintln!("pixel trace: {} fragments -> pixel_trace.txt", lines.len());
    }
    // LMA_FIND="value/mask,..." (hex): list guest addresses whose big-endian u32 matches
    if let Ok(spec) = std::env::var("LMA_FIND") {
        let pages = m.mem.snapshot_pages();
        for item in spec.split(',') {
            let (v, mask) = item.split_once('/').unwrap_or((item, "ffffffff"));
            let (v, mask) = (u32::from_str_radix(v, 16)?, u32::from_str_radix(mask, 16)?);
            let mut hits = Vec::new();
            for (page, bytes) in &pages {
                for (i, w) in bytes.chunks_exact(4).enumerate() {
                    if u32::from_be_bytes(w.try_into().unwrap()) & mask == v & mask {
                        hits.push((page << 16) + 4 * i as u32);
                    }
                }
            }
            // command buffers (0xAA000000..0xAB000000) repeat every constant; LMA_FIND_ALL keeps them
            if std::env::var("LMA_FIND_ALL").is_err() {
                hits.retain(|&a| !(0xAA00_0000..0xAB00_0000).contains(&a));
            }
            let shown: Vec<String> = hits.iter().take(40).map(|a| format!("{a:#010x}")).collect();
            println!("find {v:#010x}/{mask:#x}: {} hits {}", hits.len(), shown.join(" "));
        }
    }
    if let Ok(path) = std::env::var("LMA_SAVE") {
        let t = std::time::Instant::now();
        let n = m.save_state(std::path::Path::new(&path))?;
        eprintln!("saved {path}: {:.1} MB in {:.2?}", n as f64 / 1e6, t.elapsed());
    }
    // LMA_INTERP_STATS=1: instructions run through rec::interp, by mnemonic (what to translate next)
    if let Some(stats) = runtime::rec::INTERP_STATS.lock().unwrap().take() {
        let mut by_name: std::collections::HashMap<&str, u64> = Default::default();
        for (w, n) in stats {
            *by_name.entry(ppc::lookup(w).unwrap_or("?")).or_default() += n;
        }
        let mut v: Vec<_> = by_name.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        let total: u64 = v.iter().map(|x| x.1).sum();
        println!("interpreted instructions: {total}");
        for (name, n) in v.iter().take(40) {
            println!("  {name:12} {n:>12} {:5.1}%", 100.0 * *n as f64 / total.max(1) as f64);
        }
    }
    if let Some(h) = &m.misses {
        let mut v: Vec<_> = h.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        let lines: Vec<String> = v.iter().map(|(pc, n)| format!("{pc:08x} {n}")).collect();
        std::fs::write("misses.txt", lines.join("
")).ok();
    }
    println!("host time: code {:.2?}, kernel HLE {:.2?}, gpu {:.2?}; {} dispatches outside recompiled code", m.timing[0], m.timing[1], m.timing[2], m.interp_steps);
    let rs = m.gpu.renderer.stats();
    let rt = rs.timing;
    println!("renderer: vertex {:.2?}, pixels {:.2?} (texture decode {:.2?}), resolve {:.2?}, present {:.2?}; {} pixels drawn", rt[0], rt[1], rt[4], rt[2], rt[3], rs.pixels_drawn);
    for (i, th) in m.threads.iter().enumerate() {
        let pc = if i == m.cur { m.cpu.pc } else { th.cpu.pc };
        let ran = m.thread_instr.get(i).copied().unwrap_or(0);
        let share = 100.0 * ran as f64 / m.thread_instr.iter().sum::<u64>().max(1) as f64;
        println!("  thread {i} {:12} pc {pc:#010x} {:?}, {:.1} M instr ({share:.1}%)", th.name, th.state, ran as f64 / 1e6);
    }
    println!("distinct kernel functions called: {}", m.calls.len());
    println!("gpu: {:?} rptr {:#x} blocked {} pending interrupts {:?}{}", m.gpu.stats, m.gpu.rptr, m.gpu.blocked, m.gpu.pending_interrupts.iter().take(8).collect::<Vec<_>>(), if m.gpu.pass_dropped > 0 { format!(" single-pass dropped {}", m.gpu.pass_dropped) } else { String::new() });
    if let Some((n, avg, low, worst)) = runtime::gpu::frame_stats(&m.gpu.swap_times) {
        println!("frames: {n} frame times, {avg:.1} fps average, {low:.1} fps 1% low, worst {worst:.1} ms");
        // LMA_FRAME_GAPS=1: the three longest swap intervals, with their frame index and offset from the first swap
        if std::env::var_os("LMA_FRAME_GAPS").is_some() {
            let st = &m.gpu.swap_times;
            let mut gaps: Vec<(f64, usize)> = st.windows(2).enumerate().map(|(i, w)| ((w[1] - w[0]).as_secs_f64(), i)).collect();
            gaps.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (g, i) in gaps.iter().take(3) {
                println!("  gap {:.1} ms after swap {i} (+{:.2} s from the first swap)", g * 1000.0, (st[*i] - st[0]).as_secs_f64());
            }
        }
    }
    // LMA_PEEK="addr:len,addr:len" dumps guest memory after the run (hex addresses)
    if let Ok(spec) = std::env::var("LMA_PEEK") {
        for item in spec.split(',') {
            let (a, l) = item.split_once(':').unwrap_or((item, "40"));
            let (a, l) = (u32::from_str_radix(a, 16)?, u32::from_str_radix(l, 16)?);
            for row in (0..l).step_by(16) {
                let mut b = [0u8; 16];
                m.mem.read(a + row, &mut b);
                let hex: Vec<String> = b.chunks(4).map(|c| c.iter().map(|x| format!("{x:02x}")).collect()).collect();
                println!("  {:#010x}: {}", a + row, hex.join(" "));
            }
        }
    }
    if let Some(counts) = guest::rec::CALL_TARGETS.lock().unwrap().take() {
        let total: u64 = counts.values().sum();
        let mut v: Vec<_> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        println!("indirect calls: {total} at {} (site, target) pairs; the top 40:", v.len());
        for ((site, target), n) in v.iter().take(40) {
            println!("  site {site:#010x} -> {target:#010x}  {n} ({:.1}%)", 100.0 * *n as f64 / total.max(1) as f64);
        }
    }
    if std::env::var("LMA_REGS").is_ok() {
        let t: usize = std::env::var("LMA_REGS").unwrap().parse().unwrap_or(m.cur);
        let c = if t == m.cur { &m.cpu } else { &m.threads[t].cpu };
        println!("  thread {t} pc {:#010x}", c.pc);
        for i in (0..32).step_by(4) {
            println!("  r{i:<2} {:016x} {:016x} {:016x} {:016x}", c.gpr[i], c.gpr[i + 1], c.gpr[i + 2], c.gpr[i + 3]);
        }
        println!("  lr {:#x} ctr {:#x} cr {:08x}", c.lr, c.ctr, c.cr_word());
        for v in 0..8 {
            let hex: String = c.vr[v].iter().map(|b| format!("{b:02x}")).collect();
            println!("  v{v} {hex}");
        }
    }
    for (n, w, h, px) in &m.gpu.frames_out {
        std::fs::create_dir_all("frames")?;
        let path = format!("frames/frame_{n:05}.png");
        let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        let mut enc = png::Encoder::new(file, *w, *h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(px)?;
        println!("wrote {path}");
    }
    for (name, w, h, px) in &m.gpu.renderer.take_textures() {
        std::fs::create_dir_all("frames")?;
        let path = format!("frames/{name}.png");
        let file = std::io::BufWriter::new(std::fs::File::create(&path)?);
        let mut enc = png::Encoder::new(file, *w, *h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header()?.write_image_data(px)?;
        println!("wrote {path}");
    }
    println!("renderer: {} pixels drawn, {} resolves", rs.pixels_drawn, rs.resolves);
    if !m.gpu.capture.is_empty() {
        std::fs::create_dir_all("capture")?;
        std::fs::write("capture/frame.txt", m.gpu.capture.join("
"))?;
        for (label, code) in &m.gpu.captured_shaders {
            let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_be_bytes()).collect();
            std::fs::write(format!("capture/{label}.bin"), bytes)?;
        }
        println!("captured {} lines, {} shaders -> capture/", m.gpu.capture.len(), m.gpu.captured_shaders.len());
    }
    for l in &m.gpu.log {
        println!("  gpu: {l}");
    }
    Ok(())
}

fn disasm_shader(path: &str) -> Res {
    let bytes = std::fs::read(path)?;
    let code: Vec<u32> = bytes.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
    print!("{}", runtime::shader::Shader::new(code).disassemble());
    Ok(())
}

/// Write an RGBA8 image as PNG, creating the folder.
fn write_png(path: &std::path::Path, w: u32, h: u32, px: &[u8]) -> Res {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut enc = png::Encoder::new(std::io::BufWriter::new(std::fs::File::create(path)?), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(px)?;
    Ok(())
}

/// Read a PNG written by `write_png` as (width, height, RGBA8).
fn read_png(path: &std::path::Path) -> Result<(u32, u32, Vec<u8>), Box<dyn std::error::Error>> {
    let mut dec = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path)?));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    buf.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba {
        return Err(format!("{}: expected RGBA, got {:?}", path.display(), info.color_type).into());
    }
    Ok((info.width, info.height, buf))
}

/// `wgsl <vs_*.bin|ps_*.bin>... [--print]`: translate captured shaders to WGSL and validate them
/// with naga. The stage comes from the file name prefix (vs_ / ps_).
fn wgsl(args: &[String]) -> Res {
    let print = args.iter().any(|a| a == "--print");
    let (mut ok, mut bad) = (0, 0);
    for path in args.iter().filter(|a| !a.starts_with("--")) {
        let bytes = std::fs::read(path)?;
        let code: Vec<u32> = bytes.chunks(4).map(|c| u32::from_be_bytes(c.try_into().unwrap())).collect();
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let stage = if name.starts_with("ps") { runtime::shader_wgsl::Stage::Pixel } else { runtime::shader_wgsl::Stage::Vertex };
        let t = runtime::shader_wgsl::translate(&runtime::shader::Shader::new(code), stage);
        if print {
            println!("// ---- {name}\n{}", t.wgsl);
        }
        let result = naga::front::wgsl::parse_str(&t.wgsl).map_err(|e| e.emit_to_string(&t.wgsl)).and_then(|module| {
            naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
                .validate(&module)
                .map(|_| ())
                .map_err(|e| format!("{e:?}"))
        });
        match result {
            Ok(()) => {
                ok += 1;
                println!("ok    {name} ({} lines, textures {:?})", t.wgsl.lines().count(), t.textures);
            }
            Err(e) => {
                bad += 1;
                println!("FAIL  {name}: {}", e.lines().take(12).collect::<Vec<_>>().join("\n      "));
            }
        }
    }
    println!("{ok} valid, {bad} failed");
    if bad > 0 { Err("some shaders failed to validate".into()) } else { Ok(()) }
}

/// `squad-import <db-export dir> <roster.csv> <mapping.toml> <mods dir> <mod name> [--fm <csv>] [--tm <csv>] [--staff <csv>] [--tm-clubs <csv>] [--tm-lineups <csv>] [--tm-games <csv>] [--transfers <csv>] [--patches <toml>] [--from <mod dir>]`: build a
/// mod space from a real-world roster (see `lma-import` and `mappings/fc27.toml`).
fn squad_import(a: &[String]) -> Res {
    let (base, roster, mapping, mods, name) = (&a[0], &a[1], &a[2], &a[3], &a[4]);
    let cfg: lma_import::Config = toml::from_str(&std::fs::read_to_string(mapping)?).map_err(|e| format!("{mapping}: {e}"))?;
    // optional sources after the five arguments: --fm <attributes csv>, --tm <Transfermarkt players
    // csv>, --staff <FM staff csv> (managers), --tm-clubs <Transfermarkt clubs csv> (stadiums),
    // --tm-lineups <Transfermarkt game lineups csv> (shirt numbers), --tm-games <Transfermarkt games
    // csv> (league histories), --transfers <player,club csv>
    // (moves the sources lack), --patches <toml>
    let opt = |flag: &str| a[5..].chunks(2).find(|c| c[0] == flag).and_then(|c| c.get(1)).map(std::path::Path::new);
    if let Some(bad) = a[5..].chunks(2).find(|c| !["--fm", "--tm", "--staff", "--tm-clubs", "--tm-lineups", "--tm-games", "--transfers", "--managers", "--tm-live", "--patches", "--budgets", "--weights", "--history-extra", "--tm-events", "--cup-extra", "--from"].contains(&c[0].as_str())) {
        return Err(format!("squad-import: unknown option {:?}", bad[0]).into());
    }
    // --patches <toml>: the [[patch]] tables to copy into mod.toml (e.g. an earlier mod's)
    let patches = match opt("--patches") {
        Some(p) => lma_import::mod_patches(&std::fs::read_to_string(p)?).map_err(|e| format!("{}: {e}", p.display()))?,
        None => Vec::new(),
    };
    let lma_import::Imported { players, clubs, clubs_fm, history, cups, last_tables, funds_targets, wages, report } = lma_import::import_with(
        std::path::Path::new(base),
        std::path::Path::new(roster),
        &lma_import::Sources { fm: opt("--fm"), tm: opt("--tm"), staff: opt("--staff"), tm_clubs: opt("--tm-clubs"), tm_lineups: opt("--tm-lineups"), transfers: opt("--transfers"), tm_games: opt("--tm-games"), managers: opt("--managers"), tm_live: opt("--tm-live"), budgets: opt("--budgets"), weights: opt("--weights"), history_extra: opt("--history-extra"), tm_events: opt("--tm-events"), cup_extra: opt("--cup-extra") },
        &cfg,
    )?;
    let dir = std::path::Path::new(mods).join(name);
    std::fs::create_dir_all(dir.join("database"))?;
    std::fs::write(dir.join("database").join("players.csv"), &players)?;
    // division changes (#31 item 7); a previous import's file goes when nothing moves now
    let clubs_csv = dir.join("database").join("clubs.csv");
    match clubs {
        Some(c) => std::fs::write(&clubs_csv, c)?,
        None if clubs_csv.exists() => std::fs::remove_file(&clubs_csv)?,
        None => {}
    }
    let mut mod_toml = lma_import::mod_toml(name, &cfg, &patches);
    // --from <mod dir>: the earlier mod's hand-kept mod.toml keys and patches, and its badges/
    if let Some(prev) = opt("--from") {
        let from = prev.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        mod_toml = lma_import::carry_mod_toml(&mod_toml, &std::fs::read_to_string(prev.join("mod.toml"))?, &from)?;
        let badges = prev.join("badges");
        if badges.is_dir() && !dir.join("badges").exists() {
            std::fs::create_dir_all(dir.join("badges"))?;
            let mut n = 0;
            for e in std::fs::read_dir(&badges)? {
                let e = e?;
                if e.file_type()?.is_file() {
                    std::fs::copy(e.path(), dir.join("badges").join(e.file_name()))?;
                    n += 1;
                }
            }
            eprintln!("badges/: {n} files copied from {from}");
        }
        // and its hand-kept files the import doesn't write (#607: fc27final27's funds.csv and
        // sponsors.csv were missing from every fresh import; the runtime reads both). They are
        // keyed by club id and ids follow the re-seat, so each row is carried by the club's vanilla
        // record through the two mods' clubs.csv; a club the new mod lacks is dropped and listed
        let prev_clubs = std::fs::read_to_string(prev.join("database").join("clubs.csv"))?;
        let new_clubs = std::fs::read_to_string(dir.join("database").join("clubs.csv"))?;
        for file in lma_import::HAND_KEPT_FILES {
            let src = prev.join(file);
            if src.is_file() && !dir.join(file).exists() {
                let carried = lma_import::carry_by_club(&std::fs::read_to_string(&src)?, &prev_clubs, &new_clubs).map_err(|e| format!("{file}: {e}"))?;
                std::fs::write(dir.join(file), carried.text)?;
                eprintln!("{file}: {} rows carried from {from} by club record ({} moved to a new id){}", carried.kept, carried.moved, if carried.dropped.is_empty() { String::new() } else { format!(", {} dropped (no such club now): {}", carried.dropped.len(), carried.dropped.join(", ")) });
            }
        }
        // the season the mod starts in, from its base_year (2026 = the 2026/27 season; rc1's menu said 2025/26 for a 2026/27 game)
        let season = cfg.base_year.map_or("2026/27".to_string(), |y| format!("{y}/{:02}", (y + 1) % 100));
        eprintln!("menu.txt line: {name}|{season} FC 27 ({name})|<description>");
    }
    std::fs::write(dir.join("mod.toml"), mod_toml)?;
    // the league histories (a base exported before history.csv existed has none to carry)
    let history_csv = dir.join("database").join("history.csv");
    match history {
        Some(h) => std::fs::write(&history_csv, h)?,
        None if history_csv.exists() => std::fs::remove_file(&history_csv)?,
        None => {}
    }
    // and the cup finals (a base exported before cups.csv existed has none)
    let cups_csv = dir.join("database").join("cups.csv");
    match cups {
        Some(c) => std::fs::write(&cups_csv, c)?,
        None if cups_csv.exists() => std::fs::remove_file(&cups_csv)?,
        None => {}
    }
    // each club's FM uid (badge packs are keyed by it), beside the database, not in it
    if let Some(t) = &funds_targets {
        std::fs::write(dir.join("funds_targets.csv"), t)?;
    }
    // and last season's final tables (a base exported before last_tables.csv existed has none)
    let last_csv = dir.join("database").join("last_tables.csv");
    match last_tables {
        Some(c) => std::fs::write(&last_csv, c)?,
        None if last_csv.exists() => std::fs::remove_file(&last_csv)?,
        None => {}
    }
    // players beyond their club's 32 (runtime::mods: hooks::player_club); a previous import's file
    // goes when there are none now
    let overflow_csv = dir.join("squad_overflow.csv");
    if !report.squad_overflow.is_empty() {
        std::fs::write(&overflow_csv, lma_import::squad_overflow_csv(&report.squad_overflow))?;
    } else if overflow_csv.exists() {
        std::fs::remove_file(&overflow_csv)?;
    }
    // real weekly wages (runtime::mods reads wages.csv next to mod.toml)
    if let Some(w) = &wages {
        std::fs::write(dir.join("wages.csv"), w)?;
        // each club's least wage budget: its real bill plus headroom (runtime::mods, wage_budgets.csv)
        if let Some(h) = cfg.wage_headroom {
            std::fs::write(dir.join("wage_budgets.csv"), lma_import::wage_budgets_csv(&players, w, h, &report.squad_overflow)?)?;
        }
    }
    // the mapping's game strings (today's competition names), merged into the mod's strings.csv
    let strings_path = dir.join("strings.csv");
    let existing = std::fs::read_to_string(&strings_path).ok();
    if let Some(s) = lma_import::strings_csv(existing.as_deref(), &cfg.strings)? {
        std::fs::write(&strings_path, s)?;
    }
    let fm_csv = dir.join("clubs_fm.csv");
    match clubs_fm {
        Some(c) => std::fs::write(&fm_csv, c)?,
        None if fm_csv.exists() => std::fs::remove_file(&fm_csv)?,
        None => {}
    }
    let text = report.text();
    std::fs::write(dir.join("import-report.txt"), &text)?;
    println!("{}", text.lines().take(3).collect::<Vec<_>>().join("
"));
    println!("wrote {} (full report: import-report.txt)", dir.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_str_escapes_the_json_way() {
        assert_eq!(super::json_str("PLAIN_01"), "\"PLAIN_01\"");
        assert_eq!(super::json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(super::json_str("\u{1}x\u{e9}"), "\"\\u0001x\u{e9}\"", "control chars as \\uXXXX, accents as UTF-8");
    }

    /// `write_wav`: a canonical 44-byte PCM header (RIFF size, fmt chunk, byte rate, block align) and
    /// the samples interleaved little-endian.
    #[test]
    fn write_wav_writes_a_canonical_pcm_header() {
        let dir = std::env::temp_dir().join(format!("lma_wav_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.wav");
        let pcm = runtime::xact::Pcm { channels: 2, rate: 44_100, samples: vec![1, -1, 300] };
        super::write_wav(&path, &pcm).unwrap();
        let d = std::fs::read(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(d.len(), 44 + 6);
        assert_eq!(&d[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&d[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes(d[20..22].try_into().unwrap()), 1, "PCM");
        assert_eq!(u16::from_le_bytes(d[22..24].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(d[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u32::from_le_bytes(d[28..32].try_into().unwrap()), 44_100 * 4, "byte rate");
        assert_eq!(u16::from_le_bytes(d[32..34].try_into().unwrap()), 4, "block align");
        assert_eq!(u16::from_le_bytes(d[34..36].try_into().unwrap()), 16);
        assert_eq!(&d[36..40], b"data");
        assert_eq!(u32::from_le_bytes(d[40..44].try_into().unwrap()), 6);
        assert_eq!(&d[44..], &[1, 0, 0xFF, 0xFF, 0x2C, 0x01]);
    }

    use super::until_or_stuck;
    use runtime::machine::Exit;

    #[test]
    fn a_replay_that_stops_advancing_is_stuck_after_100_idle_returns() {
        let mut calls = 0;
        let (exit, stuck) = until_or_stuck(1000, 10, |_| {
            calls += 1;
            (Exit::Limit, 10)
        });
        assert!(matches!(exit, Exit::Limit) && stuck);
        assert_eq!(calls, 100);
    }

    #[test]
    fn slow_progress_is_not_stuck_and_the_target_ends_the_run() {
        // the clock moves one instruction every 60 returns: never 100 idle ones in a row
        let mut calls = 0u64;
        let (exit, stuck) = until_or_stuck(13, 10, |_| {
            calls += 1;
            (Exit::Limit, 10 + calls / 60)
        });
        assert!(matches!(exit, Exit::Limit) && !stuck);
        assert_eq!(calls, 180);
    }

    #[test]
    fn another_exit_ends_the_run_at_once() {
        let mut calls = 0;
        let (exit, stuck) = until_or_stuck(1000, 10, |_| {
            calls += 1;
            (Exit::Deadlock, 10)
        });
        assert!(matches!(exit, Exit::Deadlock) && !stuck);
        assert_eq!(calls, 1);
    }
}


