//! The decoded-audio cache (#772): every wave of the game's wave banks decoded once, when the
//! player's disc is installed, to 16-bit PCM on disk. The native XACT engine then plays
//! pre-decoded audio and FFmpeg runs only at install.
//!
//! One file per wave bank, `<bank name>-<source hash>.pcm`, in the cache directory
//! (`userdata/audio`). The hash is over the bank's bytes, so a bank that changes (another disc, a
//! mod) never reads a stale file. Layout, little-endian:
//! - `LMAPCM1\0`, u64 source hash, u32 wave count, u32 zero
//! - per wave: u32 channels, u32 rate, u64 frames, u64 byte offset (channels 0: not decoded, the
//!   engine decodes it live)
//! - the samples, interleaved i16, each wave at a 4 KB boundary
//!
//! A file is written as `.tmp` and renamed when complete, so an interrupted install leaves no
//! half-written cache behind.

use crate::xact::{decode_wave, Pcm};
use lma_data::xact::WaveBank;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"LMAPCM1\0";
const HEADER: u64 = 24;
const ENTRY: u64 = 24;
const ALIGN: u64 = 4096;

/// A wave's place in a cache file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    channels: u32,
    rate: u32,
    frames: u64,
    offset: u64,
}

/// The cache file's name for a bank parsed from `bytes`.
pub fn file_name(bank: &WaveBank, bytes: &[u8]) -> String {
    let name: String = bank.name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("{name}-{:016x}.pcm", crate::raster::bytes_hash(bytes))
}

/// What writing a bank's cache did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Written {
    pub waves: usize,
    /// Waves left to live decode (a codec without a decoder).
    pub skipped: usize,
    pub bytes: u64,
}

/// Decode every wave of the bank in `bytes` into `dir`. A bank already cached is left as it is
/// (`Written::bytes` 0).
pub fn write_bank(dir: &Path, bytes: &[u8]) -> std::io::Result<Written> {
    let bank = WaveBank::parse(bytes).map_err(|e| std::io::Error::other(e.to_string()))?;
    let path = dir.join(file_name(&bank, bytes));
    let mut done = Written { waves: bank.waves.len(), ..Default::default() };
    if path.exists() {
        return Ok(done);
    }
    std::fs::create_dir_all(dir)?;
    // a temporary name of this writer's own: two source files can hold the same bank (same bytes,
    // same cache name), and two threads writing one .tmp truncated each other's data
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!("{}-{}.tmp", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    let count = bank.waves.len() as u64;
    let mut at = (HEADER + ENTRY * count).next_multiple_of(ALIGN);
    f.seek(SeekFrom::Start(at))?;
    let mut entries = Vec::with_capacity(bank.waves.len());
    for (i, w) in bank.waves.iter().enumerate() {
        let Some(pcm) = bank.wave_bytes(bytes, i).and_then(|b| decode_wave(w, b)) else {
            done.skipped += 1;
            entries.push(Entry { channels: 0, rate: 0, frames: 0, offset: 0 });
            continue;
        };
        let raw: Vec<u8> = pcm.samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        f.write_all(&raw)?;
        entries.push(Entry { channels: pcm.channels, rate: pcm.rate, frames: pcm.frames() as u64, offset: at });
        let end = at + raw.len() as u64;
        at = end.next_multiple_of(ALIGN);
        f.write_all(&vec![0; (at - end) as usize])?;
    }
    f.seek(SeekFrom::Start(0))?;
    f.write_all(MAGIC)?;
    f.write_all(&crate::raster::bytes_hash(bytes).to_le_bytes())?;
    f.write_all(&(count as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    for e in &entries {
        f.write_all(&e.channels.to_le_bytes())?;
        f.write_all(&e.rate.to_le_bytes())?;
        f.write_all(&e.frames.to_le_bytes())?;
        f.write_all(&e.offset.to_le_bytes())?;
    }
    f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    // a second writer of the same bank lost the race: its file is identical, keep the first
    if path.exists() {
        let _ = std::fs::remove_file(&tmp);
        return Ok(done);
    }
    std::fs::rename(&tmp, &path)?;
    done.bytes = at;
    Ok(done)
}

/// A bank's cache file, opened: waves are read from disk when asked for.
#[derive(Debug)]
pub struct Cached {
    file: std::sync::Mutex<std::fs::File>,
    entries: Vec<Entry>,
}

impl Cached {
    /// The cache for the bank parsed from `bytes`, when `dir` holds a complete one.
    pub fn open(dir: &Path, bank: &WaveBank, bytes: &[u8]) -> Option<Self> {
        let mut file = std::fs::File::open(dir.join(file_name(bank, bytes))).ok()?;
        let mut head = [0u8; HEADER as usize];
        file.read_exact(&mut head).ok()?;
        let count = u32::from_le_bytes(head[16..20].try_into().unwrap()) as usize;
        if &head[..8] != MAGIC || head[8..16] != crate::raster::bytes_hash(bytes).to_le_bytes() || count != bank.waves.len() {
            return None;
        }
        let mut table = vec![0u8; count * ENTRY as usize];
        file.read_exact(&mut table).ok()?;
        let entries = table
            .chunks_exact(ENTRY as usize)
            .map(|e| Entry {
                channels: u32::from_le_bytes(e[0..4].try_into().unwrap()),
                rate: u32::from_le_bytes(e[4..8].try_into().unwrap()),
                frames: u64::from_le_bytes(e[8..16].try_into().unwrap()),
                offset: u64::from_le_bytes(e[16..24].try_into().unwrap()),
            })
            .collect();
        Some(Self { file: std::sync::Mutex::new(file), entries })
    }

    /// Wave `i`, decoded; None when the install couldn't decode it (decode it live).
    pub fn wave(&self, i: usize) -> Option<Pcm> {
        let e = *self.entries.get(i)?;
        if e.channels == 0 {
            return None;
        }
        let mut raw = vec![0u8; (e.frames * e.channels as u64 * 2) as usize];
        let mut f = self.file.lock().ok()?;
        f.seek(SeekFrom::Start(e.offset)).ok()?;
        f.read_exact(&mut raw).ok()?;
        Some(Pcm { channels: e.channels, rate: e.rate, samples: raw.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect() })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The files holding the game's wave banks under `game_dir`: every `.xwb`, and every `.mng` in a
/// `Sound` folder (a sound bank archive). `language` (`Data_01`..`Data_05`, the commentary)
/// keeps only that language's folder and the shared ones; None keeps all.
pub fn sources(game_dir: &Path, language: Option<&str>) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![game_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)?.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(game_dir).unwrap_or(&p);
            let parts: Vec<String> = rel.iter().map(|c| c.to_string_lossy().to_ascii_lowercase()).collect();
            if let Some(l) = language {
                if parts.iter().any(|c| c.starts_with("data_") && !c.eq_ignore_ascii_case(l)) {
                    continue;
                }
            }
            let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase());
            if p.is_dir() {
                stack.push(p);
            } else if ext.as_deref() == Some("xwb") || (ext.as_deref() == Some("mng") && parts.iter().any(|c| c.contains("sound"))) {
                out.push(p);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The wave banks in a source file (`sources`): a lone `.xwb` or those inside an `.mng` archive.
pub fn banks_in(path: &Path) -> std::io::Result<Vec<Vec<u8>>> {
    let d = std::fs::read(path)?;
    Ok(lma_data::xact::wave_banks(&d).map(|v| v.into_iter().map(|(_, b)| b).collect()).unwrap_or_default())
}

/// What building the cache did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub banks: usize,
    pub waves: usize,
    pub skipped: usize,
    /// Bytes written this time (banks already cached add nothing).
    pub bytes: u64,
}

/// The first-run install step: decode every wave bank of the game (`sources`) into `out_dir`, on
/// all cores. `progress(done, total)` is called after each source file, from worker threads.
/// Needs the XMA decoder (`xma-ffmpeg`); without it nothing is written.
pub fn build(game_dir: &Path, language: Option<&str>, out_dir: &Path, progress: &(dyn Fn(usize, usize) + Sync)) -> std::io::Result<Summary> {
    if !cfg!(feature = "xma-ffmpeg") {
        return Err(std::io::Error::other("built without the XMA decoder (xma-ffmpeg)"));
    }
    let files = sources(game_dir, language)?;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let finished = std::sync::atomic::AtomicUsize::new(0);
    let total = std::sync::Mutex::new(Ok(Summary::default()));
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                use std::sync::atomic::Ordering::Relaxed;
                let Some(path) = files.get(next.fetch_add(1, Relaxed)) else { break };
                let r = banks_in(path).and_then(|banks| {
                    let mut sum = Summary::default();
                    for b in banks {
                        let w = write_bank(out_dir, &b)?;
                        sum.banks += 1;
                        sum.waves += w.waves;
                        sum.skipped += w.skipped;
                        sum.bytes += w.bytes;
                    }
                    Ok(sum)
                });
                let mut t = total.lock().unwrap();
                match (r, &mut *t) {
                    (Ok(sum), Ok(t)) => {
                        t.banks += sum.banks;
                        t.waves += sum.waves;
                        t.skipped += sum.skipped;
                        t.bytes += sum.bytes;
                    }
                    (Err(e), t @ Ok(_)) => *t = Err(std::io::Error::other(format!("{}: {e}", path.display()))),
                    _ => {}
                }
                drop(t);
                progress(finished.fetch_add(1, Relaxed) + 1, files.len());
            });
        }
    });
    total.into_inner().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_waves_read_back_as_decoded() {
        let samples: Vec<i16> = (0..3000).map(|i| (i * 7 % 2000 - 1000) as i16).collect();
        let bytes = crate::xact::tests::pcm_wave_bank("cache", &[samples.clone()]);
        let bank = WaveBank::parse(&bytes).unwrap();
        let live = decode_wave(&bank.waves[0], bank.wave_bytes(&bytes, 0).unwrap()).unwrap();
        assert_eq!(live.samples, samples);
        let dir = std::env::temp_dir().join(format!("lma-xact-cache-test-{}", std::process::id()));
        let w = write_bank(&dir, &bytes).unwrap();
        assert_eq!((w.waves, w.skipped), (1, 0));
        assert_eq!(w.bytes % ALIGN, 0);
        let c = Cached::open(&dir, &bank, &bytes).unwrap();
        assert_eq!(c.wave(0), Some(live));
        assert_eq!(c.wave(1), None);
        // a second write leaves the file as it is
        assert_eq!(write_bank(&dir, &bytes).unwrap().bytes, 0);
        // the same bank from two threads at once: one file, whole, and no .tmp left behind
        let race = dir.join("race");
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| write_bank(&race, &bytes).unwrap());
            }
        });
        let names: Vec<String> = std::fs::read_dir(&race).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, [file_name(&bank, &bytes)], "{names:?}");
        assert_eq!(Cached::open(&race, &bank, &bytes).unwrap().wave(0).map(|p| p.samples), Some(samples.clone()));
        // other bytes (a changed bank) don't read this file
        let mut other = bytes.clone();
        *other.last_mut().unwrap() ^= 1;
        assert!(Cached::open(&dir, &WaveBank::parse(&other).unwrap(), &other).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_engine_plays_from_the_cache() {
        let dir = std::env::temp_dir().join(format!("lma-xact-cache-engine-{}", std::process::id()));
        let wb = crate::xact::tests::pcm_wave_bank("menu", &[vec![1000; 64]]);
        write_bank(&dir, &wb).unwrap();
        let sb = crate::xact::tests::simple_sound_bank("menu", &[("click", 0, 180, 0)]);
        for (cache_dir, reads, decodes) in [(Some(dir.clone()), 1, 0), (None, 0, 1)] {
            let mut e = crate::xact::Engine::new(None, 1 << 20);
            e.cache_dir = cache_dir;
            e.load_wave_bank(wb.clone()).unwrap();
            let s = e.load_sound_bank(&sb).unwrap();
            e.play_cue(s, 0).unwrap();
            assert_eq!((e.cache_reads, e.live_decodes), (reads, decodes));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
