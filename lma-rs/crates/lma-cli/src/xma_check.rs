//! `lma-cli xma-check <game dir> [name filter] [max clips]` (#199): every XMA clip in the game's wave
//! banks, decoded two ways from the same raw bytes and compared sample by sample:
//! - ours: the runtime's XMA hardware model (`runtime::xma::Channel`, one context per stream, as
//!   the game sets them up: stream k starts at packet k, input buffer handed back when spent, a
//!   31-block output ring drained like the game drains it);
//! - reference: the `ffmpeg` command line decoding the clip as one multi-stream XMA2 file.
//!
//! A clip whose streams differ beyond a tight tolerance is a bug in the hardware model (packet
//! routing, stream skips, the ring) or the decoder setup. Needs the game files (never in the repo)
//! and `ffmpeg` on PATH; without them it reports what's missing and checks nothing.

use runtime::xma::{self, Channel, Context, Mem, PACKET};
use std::path::{Path, PathBuf};

/// One wave-bank entry: format and its raw data.
struct Clip {
    bank: String,
    index: usize,
    channels: u32,
    rate: u32,
    /// Samples per channel, from the bank's entry.
    duration: usize,
    data: Vec<u8>,
}

/// The XMA entries of an XACT wave bank (version 39, big-endian, as on this disc).
fn bank_clips(path: &Path) -> Vec<Clip> {
    let Ok(d) = std::fs::read(path) else { return Vec::new() };
    if d.len() < 64 || &d[..4] != b"DNBW" {
        return Vec::new();
    }
    let be = |o: usize| u32::from_be_bytes(d[o..o + 4].try_into().unwrap());
    let seg = |i: usize| (be(8 + 8 * i) as usize, be(12 + 8 * i) as usize);
    let ((bank, _), (meta, _), (wave, _)) = (seg(0), seg(1), seg(4));
    let (flags, count, meta_size) = (be(bank), be(bank + 4) as usize, be(bank + 72) as usize);
    let compact = flags & 0x2_0000 != 0;
    let name = path.strip_prefix(path.ancestors().nth(2).unwrap_or(Path::new(""))).unwrap_or(path).display().to_string();
    let mut out = Vec::new();
    for i in 0..count {
        let e = meta + i * meta_size;
        let fmt = if compact { be(bank + 84) } else { be(e + 4) };
        let (tag, channels, rate) = (fmt & 3, (fmt >> 2) & 7, (fmt >> 5) & 0x3_ffff);
        let (off, len) = (be(e + 8) as usize, be(e + 12) as usize);
        if tag != 1 || wave + off + len > d.len() || len < PACKET {
            continue;
        }
        let duration = if compact { 0 } else { (be(e) >> 4) as usize };
        out.push(Clip { bank: name.clone(), index: i, channels, rate, duration, data: d[wave + off..wave + off + len].to_vec() });
    }
    out
}

/// Flat guest memory for the hardware model.
struct Ram(Vec<u8>);
impl Mem for Ram {
    fn read(&mut self, phys: u32, buf: &mut [u8]) {
        buf.copy_from_slice(&self.0[phys as usize..phys as usize + buf.len()]);
    }
    fn write(&mut self, phys: u32, buf: &[u8]) {
        self.0[phys as usize..phys as usize + buf.len()].copy_from_slice(buf);
    }
}

fn rate_code(hz: u32) -> u32 {
    match hz {
        24_000 => 0,
        32_000 => 1,
        44_100 => 2,
        _ => 3,
    }
}

/// Our decode: stream k of the clip through the hardware model, interleaved samples.
fn ours(clip: &Clip, stream: u32, stream_channels: u32) -> Vec<i16> {
    const BASE: u32 = 0x1000;
    let ring = BASE + clip.data.len() as u32 + 0x1000;
    let blocks = 31u32;
    let mut ram = Ram(vec![0; (ring + blocks * xma::BLOCK as u32 + 0x1000) as usize]);
    ram.0[BASE as usize..BASE as usize + clip.data.len()].copy_from_slice(&clip.data);
    let mut ctx = Context {
        input_packets: [(clip.data.len() / PACKET) as u32, 0],
        input_valid: [true, false],
        input_ptr: [BASE, 0],
        input_read_offset: stream * PACKET as u32 * 8 + 32,
        output_blocks: blocks,
        output_valid: true,
        output_ptr: ring,
        stereo: stream_channels == 2,
        sample_rate: rate_code(clip.rate),
        ..Default::default()
    };
    let mut ch = Channel { enabled: true, ..Default::default() };
    ch.ensure_decoder((stream_channels, clip.rate), xma::decoder);
    let mut out = Vec::new();
    let mut idle = 0;
    for _ in 0..1_000_000 {
        let progressed = ch.work(&mut ctx, &mut ram);
        // the game reads everything written so far and hands the ring back
        let (r, w, cap) = (ctx.output_read as usize, ctx.output_write as usize, blocks as usize);
        let n = if !ctx.output_valid && r == w { cap } else { (w + cap - r) % cap };
        for k in 0..n {
            let o = ring as usize + ((r + k) % cap) * xma::BLOCK;
            out.extend(ram.0[o..o + xma::BLOCK].chunks(2).map(|b| i16::from_be_bytes([b[0], b[1]])));
        }
        ctx.output_read = ctx.output_write;
        ctx.output_valid = true;
        // a few idle runs at the end, as the game keeps kicking: the drain comes on the second
        idle = if !progressed && n == 0 { idle + 1 } else { 0 };
        if idle >= 3 {
            break;
        }
    }
    out
}

/// The reference: the whole clip through the `ffmpeg` command line, interleaved samples.
fn reference(clip: &Clip, tmp: &Path) -> Option<Vec<i16>> {
    let streams = clip.channels.div_ceil(2) as u16;
    let mask: u32 = match clip.channels {
        1 => 4,
        2 => 3,
        n => (1 << n) - 1,
    };
    let mut ext = Vec::new();
    for v in [streams as u32] {
        ext.extend_from_slice(&(v as u16).to_le_bytes());
    }
    ext.extend_from_slice(&mask.to_le_bytes());
    for v in [0u32, clip.data.len() as u32, 0, 0, 0, 0] {
        ext.extend_from_slice(&v.to_le_bytes());
    }
    ext.extend_from_slice(&[0, 4]);
    ext.extend_from_slice(&1u16.to_le_bytes());
    let mut fmt = Vec::new();
    for v in [0x166u16, clip.channels as u16] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    fmt.extend_from_slice(&clip.rate.to_le_bytes());
    fmt.extend_from_slice(&(clip.rate * clip.channels * 2).to_le_bytes());
    for v in [(2 * clip.channels) as u16, 16, ext.len() as u16] {
        fmt.extend_from_slice(&v.to_le_bytes());
    }
    fmt.extend_from_slice(&ext);
    let mut body = b"WAVEfmt ".to_vec();
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(clip.data.len() as u32).to_le_bytes());
    body.extend_from_slice(&clip.data);
    let mut file = b"RIFF".to_vec();
    file.extend_from_slice(&(body.len() as u32).to_le_bytes());
    file.extend_from_slice(&body);
    std::fs::write(tmp, file).ok()?;
    let out = std::process::Command::new("ffmpeg").args(["-hide_banner", "-v", "quiet", "-i"]).arg(tmp).args(["-f", "s16le", "-acodec", "pcm_s16le", "-"]).output().ok()?;
    Some(out.stdout.chunks(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
}

/// The reference missing whole frames that ours has (the ffmpeg command line drops a short last
/// packet): identical up to some sample, then identical again with ours shifted by 512-sample
/// frames, and ours closer to the clip's stated duration. Returns the samples dropped.
fn reference_dropped(ours: &[i16], oc: usize, reference: &[i16], rc: usize, first: usize, duration: usize) -> Option<usize> {
    let r: Vec<i16> = reference.chunks(rc).map(|f| f[first]).collect();
    let o: Vec<i16> = ours.chunks(oc).map(|f| f[0]).collect();
    let diff = (0..o.len().min(r.len())).find(|&i| (o[i] as i32 - r[i] as i32).abs() > 2)?;
    let same = |shift: usize| (diff..r.len()).all(|i| o.get(i + shift).is_some_and(|&v| (v as i32 - r[i] as i32).abs() <= 2));
    let shift = (1..=8).map(|k| k * 512).find(|&s| same(s))?;
    (o.len().abs_diff(duration) < r.len().abs_diff(duration)).then_some(shift)
}

/// How far `ours` (`oc` channels) is from channels `first..first + oc` of `reference`.
struct Diff {
    compared: usize,
    max: i32,
    mismatched: usize,
    len_ours: usize,
    len_ref: usize,
}

fn compare(ours: &[i16], oc: usize, reference: &[i16], rc: usize, first: usize) -> Diff {
    let (len_ours, len_ref) = (ours.len() / oc, reference.len() / rc);
    let n = len_ours.min(len_ref);
    let (mut max, mut mismatched) = (0, 0);
    for i in 0..n {
        for c in 0..oc {
            let d = (ours[i * oc + c] as i32 - reference[i * rc + first + c] as i32).abs();
            max = max.max(d);
            mismatched += (d > 2) as usize;
        }
    }
    Diff { compared: n, max, mismatched, len_ours, len_ref }
}

/// `lma-cli audio-cache <game dir> <out dir> [Data_NN]`: the install step, decoding every wave
/// bank (of one commentary language, or all) into the decoded-audio cache.
pub fn build_cache(game_dir: &str, out_dir: &str, language: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let t = std::time::Instant::now();
    let s = runtime::xact_cache::build(Path::new(game_dir), language, Path::new(out_dir), &|done, total| eprint!("\raudio-cache: {done}/{total} files"))?;
    eprintln!();
    println!("audio-cache: {} banks, {} waves ({} left to live decode), {:.1} MB written in {:.1} s", s.banks, s.waves, s.skipped, s.bytes as f64 / 1e6, t.elapsed().as_secs_f64());
    Ok(())
}

/// `lma-cli xma-check --cache <game dir> <cache dir> [Data_NN]`: every wave the cache holds (for
/// the language `audio-cache` was given) against a live decode of the same wave
/// (`runtime::xact::decode_wave`), sample for sample. Any difference, or a bank the cache lacks,
/// fails.
pub fn check_cache(game_dir: &str, cache_dir: &str, language: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let (mut banks, mut waves, mut live_only, mut bad) = (0usize, 0usize, 0usize, Vec::new());
    for path in runtime::xact_cache::sources(Path::new(game_dir), language)? {
        for bytes in runtime::xact_cache::banks_in(&path)? {
            let bank = lma_data::xact::WaveBank::parse(&bytes)?;
            let Some(cached) = runtime::xact_cache::Cached::open(Path::new(cache_dir), &bank, &bytes) else {
                bad.push(format!("{} ({}): not in the cache", path.display(), bank.name));
                continue;
            };
            banks += 1;
            for (i, w) in bank.waves.iter().enumerate() {
                let live = bank.wave_bytes(&bytes, i).and_then(|b| runtime::xact::decode_wave(w, b));
                match (cached.wave(i), live) {
                    (Some(c), Some(l)) if c == l => waves += 1,
                    (None, None) => live_only += 1,
                    (c, l) => {
                        // how far apart: the largest sample difference and how many samples differ
                        let (max, n) = match (&c, &l) {
                            (Some(c), Some(l)) => c.samples.iter().zip(&l.samples).fold((0, 0), |(m, n), (a, b)| {
                                let d = (*a as i32 - *b as i32).abs();
                                (m.max(d), n + (d != 0) as usize)
                            }),
                            _ => (0, 0),
                        };
                        bad.push(format!(
                            "{} #{i}: cache {:?} vs live {:?}, {n} samples differ (max {max})",
                            bank.name,
                            c.map(|p| (p.channels, p.rate, p.frames())),
                            l.map(|p| (p.channels, p.rate, p.frames()))
                        ))
                    }
                }
            }
        }
    }
    for b in &bad {
        println!("MISMATCH {b}");
    }
    println!("xma-check --cache: {banks} banks, {waves} waves identical to a live decode, {live_only} neither decodes, {} mismatched", bad.len());
    if !bad.is_empty() {
        return Err(format!("{} waves differ from a live decode", bad.len()).into());
    }
    Ok(())
}

pub fn main(game_dir: &str, filter: Option<&str>, max_clips: Option<usize>) -> Result<(), Box<dyn std::error::Error>> {
    if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
        return Err("ffmpeg not found on PATH (the reference decoder)".into());
    }
    let mut banks: Vec<PathBuf> = Vec::new();
    let mut stack = vec![PathBuf::from(game_dir)];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xwb")) && filter.is_none_or(|f| p.display().to_string().contains(f)) {
                banks.push(p);
            }
        }
    }
    banks.sort();
    if banks.is_empty() {
        return Err(format!("no wave banks under {game_dir}").into());
    }
    let tmp = std::env::temp_dir().join(format!("lma-xma-check-{}.wav", std::process::id()));
    let (mut clips, mut bad, mut skipped) = (0usize, 0usize, 0usize);
    'banks: for bank in &banks {
        for clip in bank_clips(bank) {
            if max_clips.is_some_and(|m| clips >= m) {
                break 'banks;
            }
            clips += 1;
            let Some(reference) = reference(&clip, &tmp) else {
                skipped += 1;
                continue;
            };
            let rc = clip.channels as usize;
            let mut problems = Vec::new();
            for s in 0..clip.channels.div_ceil(2) {
                let sc = (clip.channels - 2 * s).min(2);
                let ours = ours(&clip, s, sc);
                let d = compare(&ours, sc as usize, &reference, rc, 2 * s as usize);
                // LMA_XMA_CHECK_OUT=<dir>: both decodes of a mismatching stream, raw 16-bit LE
                if let (Some(dir), true) = (std::env::var_os("LMA_XMA_CHECK_OUT"), d.mismatched > 0) {
                    let stem = format!("{}_{}_s{s}", clip.bank.replace('/', "_"), clip.index);
                    let le = |v: &[i16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
                    let _ = std::fs::write(Path::new(&dir).join(format!("{stem}.ours.raw")), le(&ours));
                    let _ = std::fs::write(Path::new(&dir).join(format!("{stem}.ffmpeg.raw")), le(&reference));
                }
                // whole 256-byte blocks in the ring, and ffmpeg trims one 512-sample frame at the
                // end of a multi-stream file: lengths may differ by a little over a frame
                let short = d.len_ref.abs_diff(d.len_ours) > 512 + xma::BLOCK;
                if d.mismatched > 0 {
                    if let Some(n) = reference_dropped(&ours, sc as usize, &reference, rc, 2 * s as usize, clip.duration) {
                        println!("note {} #{} stream {s}: the ffmpeg reference dropped {n} samples near its end (ours matches the clip's duration better)", clip.bank, clip.index);
                        continue;
                    }
                }
                if d.mismatched > 0 || short || d.compared == 0 {
                    problems.push(format!(
                        "stream {s}: {} of {} frames differ (max {}), length ours {} vs ffmpeg {}",
                        d.mismatched, d.compared, d.max, d.len_ours, d.len_ref
                    ));
                }
            }
            if !problems.is_empty() {
                bad += 1;
                println!("MISMATCH {} #{} ({} ch, {} Hz, {} bytes): {}", clip.bank, clip.index, clip.channels, clip.rate, clip.data.len(), problems.join("; "));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    println!("xma-check: {clips} clips in {} banks, {bad} mismatched, {skipped} without a reference decode", banks.len());
    if bad > 0 {
        return Err(format!("{bad} clips decode differently").into());
    }
    Ok(())
}
