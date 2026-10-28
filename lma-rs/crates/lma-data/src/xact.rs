//! XACT wave banks (`*.xwb`, #772): the game's sound, read from the user's own files. Every sound
//! the game plays is a wave in one of 42 banks (the sound folders' `.xwb` files, the `.mng` sound
//! banks, which are BG2 archives holding an `.xwb` + `.xsb` pair, and the per-language commentary
//! in `Data_01..05`), about 31,900 waves in all, nearly all XMA.
//!
//! Layout (format version 39, XACT 2, big-endian on the Xbox 360): `"DNBW"` (`WBND` as a
//! big-endian word read little-endian), u32 version, then five regions {u32 offset, u32 length}:
//! bank data, entry metadata, seek tables, entry names, wave data. Bank data: u32 flags, u32 entry
//! count, name\[64\], u32 metadata element size, u32 name element size, u32 alignment, u32 compact
//! flag, u64 build time. An entry (24 bytes): u32 flags (low 4 bits) and duration in samples
//! (the rest), u32 format, {u32 offset, u32 length} of its bytes in the wave data, {u32 start,
//! u32 length} of its loop in samples. The format word (version 39): tag bits 0-1 (0 PCM, 1 XMA,
//! 2 ADPCM, 3 WMA), channels 2-4, sample rate 5-22, block align 23-30, 16-bit samples bit 31.
//!
//! Sound banks (`*.xsb`, `SoundBank`) name the cues the game plays: 29,780 in all, every one a
//! simple cue, nearly all playing one wave with a fixed volume, pitch and category.
//! Global settings (`*.xgs`, `GlobalSettings`): the categories, global variables and RPC volume
//! curves (the match's `StadDisLow/Mid/High` driving the crowd beds).

use crate::{Error, Result};

const MAGIC: &[u8; 4] = b"DNBW";
const BANK_DATA: usize = 0x60;
const ENTRY: usize = 24;

/// How a wave's samples are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Pcm,
    Xma,
    Adpcm,
    Wma,
}

/// A wave's format (the bank's packed MINIWAVEFORMAT).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub codec: Codec,
    pub channels: u32,
    pub rate: u32,
    pub block_align: u32,
    /// 16-bit samples (PCM); 8-bit otherwise.
    pub bits16: bool,
}

impl Format {
    pub fn from_word(w: u32) -> Self {
        let codec = [Codec::Pcm, Codec::Xma, Codec::Adpcm, Codec::Wma][(w & 3) as usize];
        Self { codec, channels: (w >> 2) & 7, rate: (w >> 5) & 0x3_FFFF, block_align: (w >> 23) & 0xFF, bits16: w >> 31 != 0 }
    }

    pub fn word(&self) -> u32 {
        let tag = match self.codec {
            Codec::Pcm => 0,
            Codec::Xma => 1,
            Codec::Adpcm => 2,
            Codec::Wma => 3,
        };
        tag | (self.channels & 7) << 2 | (self.rate & 0x3_FFFF) << 5 | (self.block_align & 0xFF) << 23 | (self.bits16 as u32) << 31
    }
}

/// One wave of a bank.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wave {
    pub flags: u32,
    /// Length in samples (per channel).
    pub duration: u32,
    pub format: Format,
    /// Its bytes in the bank's wave data: (offset, length).
    pub play: (u32, u32),
    /// Its loop in samples: (start, length); (0, 0) for none.
    pub loop_region: (u32, u32),
    /// From the bank's name table, when it has one.
    pub name: Option<String>,
}

/// A parsed wave bank: its header and entries, with the wave data's place in the file.
#[derive(Clone, Debug)]
pub struct WaveBank {
    pub version: u32,
    pub flags: u32,
    pub name: String,
    pub alignment: u32,
    pub waves: Vec<Wave>,
    data: (usize, usize),
}

fn u32_at(d: &[u8], off: usize) -> Result<u32> {
    d.get(off..off + 4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| Error::Format(format!("xwb: ends at {off:#x}")))
}

fn c_string(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..n]).into_owned()
}

impl WaveBank {
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(..4) != Some(MAGIC) {
            return Err(Error::Format("xwb: not an XACT wave bank".into()));
        }
        let version = u32_at(d, 4)?;
        let region = |k: usize| -> Result<(usize, usize)> { Ok((u32_at(d, 8 + 8 * k)? as usize, u32_at(d, 12 + 8 * k)? as usize)) };
        let (bank, bank_len) = region(0)?;
        if bank_len < BANK_DATA || d.len() < bank + BANK_DATA {
            return Err(Error::Format(format!("xwb: bank data of {bank_len} bytes")));
        }
        let flags = u32_at(d, bank)?;
        let count = u32_at(d, bank + 4)? as usize;
        let name = c_string(&d[bank + 8..bank + 72]);
        let (meta_size, name_size, alignment, compact) = (u32_at(d, bank + 72)? as usize, u32_at(d, bank + 76)? as usize, u32_at(d, bank + 80)?, u32_at(d, bank + 84)?);
        if compact != 0 {
            return Err(Error::Format(format!("xwb {name}: compact entries are not read yet")));
        }
        if meta_size < ENTRY {
            return Err(Error::Format(format!("xwb {name}: entries of {meta_size} bytes")));
        }
        let (meta, meta_len) = region(1)?;
        if meta_len < count * meta_size {
            return Err(Error::Format(format!("xwb {name}: {count} entries don't fit {meta_len} bytes")));
        }
        let (names, names_len) = region(3)?;
        let data = region(4)?;
        if d.len() < data.0 + data.1 {
            return Err(Error::Format(format!("xwb {name}: wave data past the end of the file")));
        }
        let mut waves = Vec::with_capacity(count);
        for i in 0..count {
            let m = meta + i * meta_size;
            let fd = u32_at(d, m)?;
            let play = (u32_at(d, m + 8)?, u32_at(d, m + 12)?);
            if play.0 as usize + play.1 as usize > data.1 {
                return Err(Error::Format(format!("xwb {name}: wave {i} past the wave data")));
            }
            // a name table past the end of the file reads as no names, not a panic (review on #778)
            let wname = (name_size > 0 && names_len >= (i + 1) * name_size).then(|| d.get(names + i * name_size..names + (i + 1) * name_size).map(c_string)).flatten();
            waves.push(Wave {
                flags: fd & 0xF,
                duration: fd >> 4,
                format: Format::from_word(u32_at(d, m + 4)?),
                play,
                loop_region: (u32_at(d, m + 16)?, u32_at(d, m + 20)?),
                name: wname,
            });
        }
        Ok(Self { version, flags, name, alignment, waves, data })
    }

    /// Wave `i`'s bytes in `d` (the file this bank was parsed from).
    pub fn wave_bytes<'a>(&self, d: &'a [u8], i: usize) -> Option<&'a [u8]> {
        let w = self.waves.get(i)?;
        let at = self.data.0 + w.play.0 as usize;
        d.get(at..at + w.play.1 as usize)
    }
}

/// The wave banks in a file: the file itself when it is an `.xwb`, or the `.xwb` entries of a BG2
/// archive (an `.mng` sound bank), with their names.
pub fn wave_banks(d: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    if d.get(..4) == Some(MAGIC) {
        return Ok(vec![(String::new(), d.to_vec())]);
    }
    let a = crate::Archive::parse(d)?;
    Ok(a.entries.into_iter().filter(|(_, b)| b.get(..4) == Some(MAGIC)).collect())
}

/// One cue of a sound bank: its name and the sound it plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Cue {
    pub name: String,
    pub flags: u8,
    /// Index into `SoundBank::sounds`.
    pub sound: usize,
}

/// A sound: its mix settings and what it plays.
#[derive(Clone, Debug, PartialEq)]
pub struct Sound {
    pub flags: u8,
    pub category: u16,
    /// XACT's volume byte (the game's banks use 127-180).
    pub volume: u8,
    pub pitch: i16,
    pub priority: u8,
    /// The wave it plays: a simple sound's one wave, or a complex sound's first play-wave event
    /// (wave bank index in `SoundBank::wave_banks`, wave index).
    pub wave: Option<(u8, u16)>,
    /// A complex sound's play-wave event loops forever (loop count 255): the looping beds.
    pub looping: bool,
    /// The RPC curves on its volume, as offsets into the global settings file (`Rpc::offset`).
    pub rpcs: Vec<u32>,
}

impl Sound {
    /// Flags bit 0: the sound has tracks (a complex sound: in this game only the looping music
    /// and crowd beds, one clip each).
    pub fn complex(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// A parsed sound bank (`*.xsb`, `"KBDS"`, format version 39): its cues (all simple in this
/// game), their sounds, and the wave banks they name.
#[derive(Clone, Debug)]
pub struct SoundBank {
    pub version: u16,
    pub name: String,
    pub wave_banks: Vec<String>,
    pub cues: Vec<Cue>,
    pub sounds: Vec<Sound>,
    /// Cue name -> index, built once (the engine answers the game's cue names at run time).
    by_name: std::collections::HashMap<String, usize>,
}

fn u16_at(d: &[u8], off: usize) -> Result<u16> {
    d.get(off..off + 2).map(|b| u16::from_be_bytes(b.try_into().unwrap())).ok_or_else(|| Error::Format(format!("xsb: ends at {off:#x}")))
}

/// A complex sound at `s` (flags bit 0): after the 9-byte header, u8 tracks; with flags bit 1, an
/// RPC block (u16 length, u8 count, u32 offsets into the global settings); then per track u8
/// volume and u32 offset of its clip; a clip is u8 events, each u32 info (type in the low 5 bits:
/// 1 = play wave, 3 = play one of a variation table's waves) and its loop count at +6 (255 =
/// forever); a play-wave event names its wave (u16 at +8) and bank (u8 at +10). Returns the first play-wave
/// event's (bank, wave), whether it loops, and the RPC offsets. Enough for this game's 20 complex
/// sounds (one track, one clip, one event each).
fn complex_sound(d: &[u8], s: usize) -> Option<(Option<(u8, u16)>, bool, Vec<u32>)> {
    let flags = *d.get(s)?;
    let tracks = *d.get(s + 9)? as usize;
    let mut at = s + 10;
    let mut rpcs = Vec::new();
    if flags & 2 != 0 {
        let len = u16_at(d, at).ok()? as usize;
        let n = *d.get(at + 2)? as usize;
        for k in 0..n {
            rpcs.push(u32_at(d, at + 3 + 4 * k).ok()?);
        }
        at += len;
    }
    for t in 0..tracks {
        let clip = u32_at(d, at + 5 * t + 1).ok()? as usize;
        let events = *d.get(clip)? as usize;
        let mut e = clip + 1;
        for _ in 0..events {
            let info = u32_at(d, e).ok()?;
            let looping = *d.get(e + 6)? == 0xFF;
            match info & 0x1F {
                // play wave: the wave at +8, its bank at +10
                1 => return Some((Some((*d.get(e + 10)?, u16_at(d, e + 8).ok()?)), looping, rpcs)),
                // play a variation of waves (the training-ground beds): u16 count at +9, entries
                // of 5 bytes (u16 wave, u8 bank, u8 min / max weight) from +17; the first for now
                3 => return Some((Some((*d.get(e + 19)?, u16_at(d, e + 17).ok()?)), looping, rpcs)),
                _ => e += 12,
            }
        }
    }
    Some((None, false, rpcs))
}

impl SoundBank {
    /// Layout: `"KBDS"`, u16 version, u16 crc, u8 platform, u16 simple cues (at 9), u16 complex
    /// cues, .., u8 wave banks (at 0x11), u16 sounds (0x12), .., u32 cue-name table length (0x16: TEAMS.xsb's is 68,742 bytes),
    /// ten u32 region offsets from 0x1a (simple cues, complex cues, cue names, -, variation
    /// tables, -, wave-bank names, cue-name hash table, hash values, sounds; 0xFFFFFFFF = none),
    /// the bank name\[64\] at 0x4a. A simple cue is 5 bytes (u8 flags, u32 sound offset); a sound
    /// is u8 flags, u16 category, u8 volume, s16 pitch, u8 priority, u16 length, then for a simple
    /// sound its wave (u16 index, u8 bank).
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(..4) != Some(b"KBDS") {
            return Err(Error::Format("xsb: not an XACT sound bank".into()));
        }
        let version = u16_at(d, 4)?;
        let simple = u16_at(d, 9)? as usize;
        let complex = u16_at(d, 11)? as usize;
        if complex != 0 {
            return Err(Error::Format(format!("xsb: {complex} complex cues are not read yet")));
        }
        let short = || Error::Format("xsb: short header".into());
        let banks = *d.get(0x11).ok_or_else(short)? as usize;
        let names_len = u32_at(d, 0x16)? as usize;
        let off = |k: usize| -> Result<u32> { u32_at(d, 0x1a + 4 * k) };
        let (o_simple, o_names, o_banks) = (off(0)? as usize, off(2)?, off(6)? as usize);
        let name = c_string(d.get(0x4a..0x8a).ok_or_else(short)?);
        let wave_banks = (0..banks)
            .map(|k| d.get(o_banks + 64 * k..o_banks + 64 * (k + 1)).map(c_string).ok_or_else(|| Error::Format(format!("xsb {name}: wave-bank names past the end"))))
            .collect::<Result<Vec<_>>>()?;
        let cue_names: Vec<String> = if o_names == u32::MAX {
            Vec::new()
        } else {
            let o = o_names as usize;
            let t = d.get(o..o + names_len).ok_or_else(|| Error::Format(format!("xsb {name}: cue names past the end")))?;
            t.split(|&b| b == 0).map(|n| String::from_utf8_lossy(n).into_owned()).collect()
        };
        // sounds can be shared by offset: number them in the order the cues first name them
        let mut sound_at: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        let mut sounds = Vec::new();
        let mut cues = Vec::with_capacity(simple);
        for c in 0..simple {
            let at = o_simple + 5 * c;
            let flags = *d.get(at).ok_or_else(|| Error::Format(format!("xsb {name}: cue {c} past the end")))?;
            let s = u32_at(d, at + 1)? as usize;
            let index = match sound_at.get(&s) {
                Some(&i) => i,
                None => {
                    let head = d.get(s..s + 12).ok_or_else(|| Error::Format(format!("xsb {name}: cue {c}'s sound past the end")))?;
                    let (mut wave, mut looping, mut rpcs) = ((head[0] & 1 == 0).then(|| (head[11], u16::from_be_bytes([head[9], head[10]]))), false, Vec::new());
                    if head[0] & 1 != 0 {
                        (wave, looping, rpcs) = complex_sound(d, s).ok_or_else(|| Error::Format(format!("xsb {name}: cue {c}'s complex sound can't be read")))?;
                    }
                    if let Some((b, _)) = wave {
                        if b as usize >= wave_banks.len() {
                            return Err(Error::Format(format!("xsb {name}: cue {c} names wave bank {b} of {}", wave_banks.len())));
                        }
                    }
                    sounds.push(Sound {
                        flags: head[0],
                        category: u16::from_be_bytes([head[1], head[2]]),
                        volume: head[3],
                        pitch: i16::from_be_bytes([head[4], head[5]]),
                        priority: head[6],
                        wave,
                        looping,
                        rpcs,
                    });
                    sound_at.insert(s, sounds.len() - 1);
                    sounds.len() - 1
                }
            };
            cues.push(Cue { name: cue_names.get(c).cloned().unwrap_or_default(), flags, sound: index });
        }
        let mut by_name = std::collections::HashMap::with_capacity(cues.len());
        for (i, c) in cues.iter().enumerate() {
            by_name.entry(c.name.clone()).or_insert(i);
        }
        Ok(Self { version, name, wave_banks, cues, sounds, by_name })
    }

    /// A cue by name, as the game asks for them.
    pub fn cue(&self, name: &str) -> Option<usize> {
        self.by_name.get(name).copied()
    }
}

/// A sound category (Music, Crowd, Sound_FX, Speech, ...): what the game's category volume scales.
#[derive(Clone, Debug, PartialEq)]
pub struct Category {
    pub name: String,
    pub max_instances: u8,
    /// Index of the parent category, None at the top.
    pub parent: Option<u16>,
    /// XACT's volume byte (180 = 0 dB).
    pub volume: u8,
}

/// A global variable the game sets (the match's `StadDisLow/Mid/High`).
#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub flags: u8,
    pub initial: f32,
    pub min: f32,
    pub max: f32,
}

/// An RPC curve: a variable's value mapped onto a sound parameter (0 = volume, in hundredths of
/// a dB: -9600 is silence).
#[derive(Clone, Debug, PartialEq)]
pub struct Rpc {
    /// Where it starts in the file: what a sound's RPC list names it by.
    pub offset: u32,
    pub variable: u16,
    pub parameter: u16,
    /// (x, y, curve type) in increasing x.
    pub points: Vec<(f32, f32, u8)>,
}

impl Rpc {
    /// The curve at `x`: linear between points, flat outside them. (Curve types other than
    /// linear are drawn linear: the game's three volume curves are steep enough that it shows
    /// only at the knees.)
    pub fn eval(&self, x: f32) -> f32 {
        let p = &self.points;
        match p.iter().position(|q| q.0 >= x) {
            None => p.last().map_or(0.0, |q| q.1),
            Some(0) => p[0].1,
            Some(i) => {
                let (a, b) = (p[i - 1], p[i]);
                if b.0 <= a.0 {
                    b.1
                } else {
                    a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0)
                }
            }
        }
    }
}

/// XACT's global settings (`*.xgs`, `"FSGX"`, format version 39): the categories, the global
/// variables and the RPC curves the sound banks' sounds refer to.
#[derive(Clone, Debug)]
pub struct GlobalSettings {
    pub version: u16,
    pub categories: Vec<Category>,
    pub variables: Vec<Variable>,
    pub rpcs: Vec<Rpc>,
}

fn f32_at(d: &[u8], off: usize) -> Result<f32> {
    u32_at(d, off).map(f32::from_bits)
}

impl GlobalSettings {
    /// Layout: `"FSGX"`, u16 version, u16 crc, u8 platform, then u16 counts from 0x09
    /// (categories, variables, -, -, RPCs, DSP presets, DSP parameters) and u32 offsets from 0x17
    /// (categories, variables, -, category-name index, -, variable-name index, category names,
    /// variable names, RPCs, DSP presets, DSP parameters). A category is 10 bytes (u8 max
    /// instances, u16 fade in, u16 fade out, u8 flags, u16 parent or 0xFFFF, u8 volume, u8
    /// visibility); a variable 13 (u8 flags, f32 initial, min, max); an RPC u16 variable, u8
    /// points, u16 parameter, then points of f32 x, f32 y, u8 curve type. Names are NUL-separated
    /// in order.
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.get(..4) != Some(b"FSGX") {
            return Err(Error::Format("xgs: not XACT global settings".into()));
        }
        let version = u16_at(d, 4)?;
        let (ncat, nvar, nrpc) = (u16_at(d, 0x09)? as usize, u16_at(d, 0x0b)? as usize, u16_at(d, 0x11)? as usize);
        let off = |k: usize| -> Result<usize> { u32_at(d, 0x17 + 4 * k).map(|v| v as usize) };
        let (o_cat, o_var, o_catn, o_varn, o_rpc) = (off(0)?, off(1)?, off(6)?, off(7)?, off(8)?);
        let names = |at: usize, n: usize| -> Vec<String> { d.get(at..).unwrap_or(&[]).split(|&b| b == 0).take(n).map(|s| String::from_utf8_lossy(s).into_owned()).collect() };
        let (cat_names, var_names) = (names(o_catn, ncat), names(o_varn, nvar));
        let mut categories = Vec::with_capacity(ncat);
        for i in 0..ncat {
            let c = o_cat + 10 * i;
            let b = d.get(c..c + 10).ok_or_else(|| Error::Format(format!("xgs: category {i} past the end")))?;
            let parent = u16::from_be_bytes([b[6], b[7]]);
            categories.push(Category { name: cat_names.get(i).cloned().unwrap_or_default(), max_instances: b[0], parent: (parent != 0xFFFF).then_some(parent), volume: b[8] });
        }
        let mut variables = Vec::with_capacity(nvar);
        for i in 0..nvar {
            let v = o_var + 13 * i;
            let flags = *d.get(v).ok_or_else(|| Error::Format(format!("xgs: variable {i} past the end")))?;
            variables.push(Variable { name: var_names.get(i).cloned().unwrap_or_default(), flags, initial: f32_at(d, v + 1)?, min: f32_at(d, v + 5)?, max: f32_at(d, v + 9)? });
        }
        let mut rpcs = Vec::with_capacity(nrpc);
        let mut at = o_rpc;
        for i in 0..nrpc {
            let offset = at as u32;
            let variable = u16_at(d, at)?;
            let n = *d.get(at + 2).ok_or_else(|| Error::Format(format!("xgs: RPC {i} past the end")))? as usize;
            let parameter = u16_at(d, at + 3)?;
            let mut points = Vec::with_capacity(n);
            for k in 0..n {
                let q = at + 5 + 9 * k;
                points.push((f32_at(d, q)?, f32_at(d, q + 4)?, *d.get(q + 8).ok_or_else(|| Error::Format(format!("xgs: RPC {i} point {k} past the end")))?));
            }
            if variable as usize >= nvar {
                return Err(Error::Format(format!("xgs: RPC {i} names variable {variable} of {nvar}")));
            }
            rpcs.push(Rpc { offset, variable, parameter, points });
            at += 5 + 9 * n;
        }
        Ok(Self { version, categories, variables, rpcs })
    }

    pub fn category(&self, name: &str) -> Option<usize> {
        self.categories.iter().position(|c| c.name == name)
    }

    pub fn variable(&self, name: &str) -> Option<usize> {
        self.variables.iter().position(|v| v.name == name)
    }

    /// The RPC a sound's list names by file offset.
    pub fn rpc_at(&self, offset: u32) -> Option<&Rpc> {
        self.rpcs.iter().find(|r| r.offset == offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A version-39 bank with the given waves (format, duration, data, loop) and optional names.
    fn bank(waves: &[(Format, u32, Vec<u8>, (u32, u32))], names: Option<&[&str]>) -> Vec<u8> {
        let meta_off = 0x30 + BANK_DATA;
        let meta_len = waves.len() * ENTRY;
        let names_off = meta_off + meta_len;
        let names_len = names.map_or(0, |n| n.len() * 64);
        let data_off = (names_off + names_len).next_multiple_of(0x800);
        let mut data = Vec::new();
        let mut entries = Vec::new();
        for (f, dur, bytes, lp) in waves {
            let off = data.len() as u32;
            data.extend_from_slice(bytes);
            for v in [dur << 4, f.word(), off, bytes.len() as u32, lp.0, lp.1] {
                entries.extend_from_slice(&v.to_be_bytes());
            }
        }
        let mut d = Vec::new();
        d.extend_from_slice(MAGIC);
        d.extend_from_slice(&39u32.to_be_bytes());
        for (o, l) in [(0x30, BANK_DATA), (meta_off, meta_len), (names_off, 0), (names_off, names_len), (data_off, data.len())] {
            d.extend_from_slice(&(o as u32).to_be_bytes());
            d.extend_from_slice(&(l as u32).to_be_bytes());
        }
        let mut name = [0u8; 64];
        name[..4].copy_from_slice(b"TEST");
        for v in [0x0008_0001u32, waves.len() as u32] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&name);
        for v in [ENTRY as u32, if names.is_some() { 64 } else { 0 }, 0x800, 0] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&0u64.to_be_bytes());
        d.extend_from_slice(&entries);
        for n in names.unwrap_or(&[]) {
            let mut b = [0u8; 64];
            b[..n.len()].copy_from_slice(n.as_bytes());
            d.extend_from_slice(&b);
        }
        d.resize(data_off, 0);
        d.extend_from_slice(&data);
        d
    }

    /// The format words of the game's own banks decode as they play: MATCH.xwb's 16-bit mono PCM at
    /// 48 and 44.1 kHz, FEMusic's stereo XMA at 44.1 kHz.
    #[test]
    fn format_words_from_the_game() {
        assert_eq!(Format::from_word(0x8117_7004), Format { codec: Codec::Pcm, channels: 1, rate: 48_000, block_align: 2, bits16: true });
        assert_eq!(Format::from_word(0x8115_8884), Format { codec: Codec::Pcm, channels: 1, rate: 44_100, block_align: 2, bits16: true });
        let music = Format::from_word(0x8215_8889);
        assert_eq!((music.codec, music.channels, music.rate), (Codec::Xma, 2, 44_100));
        for w in [0x8117_7004u32, 0x8115_8884, 0x8215_8889] {
            assert_eq!(Format::from_word(w).word(), w, "{w:#x} round-trips");
        }
    }

    /// A bank's header, entries, names and wave bytes; a bad magic, a wave past the data and a
    /// compact bank are refused.
    #[test]
    fn parses_entries_and_wave_bytes() {
        let pcm = Format { codec: Codec::Pcm, channels: 1, rate: 48_000, block_align: 2, bits16: true };
        let xma = Format { codec: Codec::Xma, channels: 6, rate: 48_000, block_align: 0, bits16: true };
        let d = bank(&[(pcm, 3, vec![1, 2, 3, 4, 5, 6], (0, 0)), (xma, 512, vec![9; 2048], (128, 256))], Some(&["kick", "crowd"]));
        let b = WaveBank::parse(&d).unwrap();
        assert_eq!((b.version, b.name.as_str(), b.alignment, b.waves.len()), (39, "TEST", 0x800, 2));
        assert_eq!((b.waves[0].format, b.waves[0].duration, b.waves[0].play), (pcm, 3, (0, 6)));
        assert_eq!((b.waves[1].format.channels, b.waves[1].loop_region, b.waves[1].name.as_deref()), (6, (128, 256), Some("crowd")));
        assert_eq!(b.wave_bytes(&d, 0), Some(&[1u8, 2, 3, 4, 5, 6][..]));
        assert_eq!(b.wave_bytes(&d, 1).map(|w| w.len()), Some(2048));
        assert_eq!(b.wave_bytes(&d, 2), None);
        let unnamed = WaveBank::parse(&bank(&[(pcm, 1, vec![0, 0], (0, 0))], None)).unwrap();
        assert_eq!(unnamed.waves[0].name, None);
        // a name region pointing past the file: no names, no panic
        let mut far = d.clone();
        far[8 + 8 * 3..12 + 8 * 3].copy_from_slice(&0x00FF_0000u32.to_be_bytes());
        assert_eq!(WaveBank::parse(&far).unwrap().waves[1].name, None);

        assert!(WaveBank::parse(b"RIFF....").is_err());
        let mut past = d.clone();
        let meta = 0x30 + BANK_DATA;
        past[meta + 12..meta + 16].copy_from_slice(&0x10_0000u32.to_be_bytes());
        assert!(WaveBank::parse(&past).unwrap_err().to_string().contains("past the wave data"));
        let mut compact = d.clone();
        compact[0x30 + 84..0x30 + 88].copy_from_slice(&1u32.to_be_bytes());
        assert!(WaveBank::parse(&compact).unwrap_err().to_string().contains("compact"));
    }

    /// With `LMA_GAME_DIR` (the user's extracted game, never in the repo): every wave bank in it
    /// parses (the `.xwb` files and the `.xwb` inside every `.mng` sound bank), every wave's bytes
    /// are in its file, and the formats are the game's (PCM or XMA, 1/2/6 channels, 44.1/48 kHz).
    #[test]
    fn every_bank_of_the_game_parses() {
        let Some(dir) = std::env::var_os("LMA_GAME_DIR") else { return };
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xwb") || x.eq_ignore_ascii_case("mng")) {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        let (mut banks, mut waves) = (0, 0);
        for f in files {
            let d = std::fs::read(&f).unwrap();
            let Ok(found) = wave_banks(&d) else { continue }; // an .mng that isn't a sound bank (the database)
            for (name, bytes) in found {
                let b = WaveBank::parse(&bytes).unwrap_or_else(|e| panic!("{} {name}: {e}", f.display()));
                for (i, w) in b.waves.iter().enumerate() {
                    assert!(b.wave_bytes(&bytes, i).is_some(), "{} wave {i}", f.display());
                    assert!(matches!(w.format.codec, Codec::Pcm | Codec::Xma) && [1, 2, 6].contains(&w.format.channels) && [44_100, 48_000].contains(&w.format.rate), "{} wave {i}: {:?}", f.display(), w.format);
                }
                banks += 1;
                waves += b.waves.len();
            }
        }
        eprintln!("{banks} wave banks, {waves} waves");
        assert!(banks > 0, "no wave banks under LMA_GAME_DIR");
    }

    /// A v39 sound bank with simple cues (name, index into `sounds`), as the game's.
    fn sound_bank(cues: &[(&str, usize)], sounds: &[(u8, u16, u8, Option<(u8, u16)>)], banks: &[&str]) -> Vec<u8> {
        let wb_off = 0x92;
        let sounds_off = wb_off + 64 * banks.len();
        let mut sound_bytes = Vec::new();
        let mut sound_offs = Vec::new();
        for (flags, cat, vol, wave) in sounds {
            sound_offs.push(sounds_off + sound_bytes.len());
            sound_bytes.push(*flags);
            sound_bytes.extend_from_slice(&cat.to_be_bytes());
            sound_bytes.push(*vol);
            sound_bytes.extend_from_slice(&0i16.to_be_bytes());
            sound_bytes.push(0);
            sound_bytes.extend_from_slice(&12u16.to_be_bytes());
            match wave {
                Some((b, w)) => {
                    sound_bytes.extend_from_slice(&w.to_be_bytes());
                    sound_bytes.push(*b);
                }
                // a complex sound: no tracks (the reader returns no wave)
                None => sound_bytes.extend_from_slice(&[0, 0, 0]),
            }
        }
        let simple_off = sounds_off + sound_bytes.len();
        let names_off = simple_off + 5 * cues.len();
        let names: Vec<u8> = cues.iter().flat_map(|(n, _)| n.bytes().chain([0])).collect();
        let mut d = vec![0u8; 0x92];
        d[..4].copy_from_slice(b"KBDS");
        d[4..6].copy_from_slice(&39u16.to_be_bytes());
        d[8] = 3;
        d[9..11].copy_from_slice(&(cues.len() as u16).to_be_bytes());
        d[0x11] = banks.len() as u8;
        d[0x12..0x14].copy_from_slice(&(sounds.len() as u16).to_be_bytes());
        d[0x18..0x1a].copy_from_slice(&(names.len() as u16).to_be_bytes());
        for (k, v) in [simple_off as u32, u32::MAX, names_off as u32, u32::MAX, u32::MAX, u32::MAX, wb_off as u32, u32::MAX, u32::MAX, sounds_off as u32].into_iter().enumerate() {
            d[0x1a + 4 * k..0x1e + 4 * k].copy_from_slice(&v.to_be_bytes());
        }
        d[0x4a..0x4f].copy_from_slice(b"MATCH");
        for b in banks {
            let mut n = [0u8; 64];
            n[..b.len()].copy_from_slice(b.as_bytes());
            d.extend_from_slice(&n);
        }
        d.extend_from_slice(&sound_bytes);
        for (_, s) in cues {
            d.push(4);
            d.extend_from_slice(&(sound_offs[*s] as u32).to_be_bytes());
        }
        d.extend_from_slice(&names);
        d
    }

    /// A sound bank's cues resolve by name to their sound and wave (MATCH's shape: `BallNet` ->
    /// wave 0, volume 151, category 4); a shared sound is read once; a loop bed is a complex sound
    /// without a wave; a cue naming a wave bank the bank doesn't list is refused.
    #[test]
    fn sound_bank_cues_to_waves() {
        let d = sound_bank(&[("BallNet", 0), ("kickhard01", 1), ("kickhard01b", 1), ("amb_01low", 2)], &[(0, 4, 151, Some((0, 0))), (0, 4, 180, Some((0, 9))), (1, 3, 180, None)], &["MATCH"]);
        let b = SoundBank::parse(&d).unwrap();
        assert_eq!((b.version, b.name.as_str(), b.wave_banks.clone()), (39, "MATCH", vec!["MATCH".to_string()]));
        assert_eq!(b.cues.len(), 4);
        assert_eq!(b.sounds.len(), 3, "the shared sound read once");
        let c = &b.cues[b.cue("BallNet").unwrap()];
        assert_eq!((b.sounds[c.sound].wave, b.sounds[c.sound].volume, b.sounds[c.sound].category), (Some((0, 0)), 151, 4));
        assert_eq!(b.cues[2].sound, b.cues[1].sound);
        assert_eq!(b.sounds[b.cues[1].sound].wave, Some((0, 9)));
        let bed = &b.sounds[b.cues[b.cue("amb_01low").unwrap()].sound];
        assert!(bed.complex() && bed.wave.is_none());
        assert_eq!(b.cue("nope"), None);
        assert!(SoundBank::parse(b"DNBW").is_err());
        let bad = sound_bank(&[("x", 0)], &[(0, 1, 100, Some((3, 0)))], &["ONE"]);
        assert!(SoundBank::parse(&bad).unwrap_err().to_string().contains("wave bank 3"));
    }

    /// With `LMA_GAME_DIR`: every sound bank (`.xsb`, and the one inside each `.mng` sound bank)
    /// parses, and every simple sound's wave exists in the wave bank of the same pair.
    #[test]
    fn every_sound_bank_of_the_game_resolves() {
        let Some(dir) = std::env::var_os("LMA_GAME_DIR") else { return };
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("xsb") || x.eq_ignore_ascii_case("mng")) {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(std::path::Path::new(&dir), &mut files);
        let (mut banks, mut cues, mut complex) = (0, 0, 0);
        for f in files {
            let d = std::fs::read(&f).unwrap();
            // (xsb bytes, its wave bank's bytes)
            let pairs: Vec<(Vec<u8>, Vec<u8>)> = if d.get(..4) == Some(b"KBDS") {
                let xwb = std::fs::read(f.with_extension("xwb")).unwrap_or_else(|e| panic!("{}: its .xwb: {e}", f.display()));
                vec![(d, xwb)]
            } else if let Ok(a) = crate::Archive::parse(&d) {
                let x = a.entries.iter().find(|(_, b)| b.get(..4) == Some(b"KBDS")).map(|e| e.1.clone());
                let w = a.entries.iter().find(|(_, b)| b.get(..4) == Some(MAGIC)).map(|e| e.1.clone());
                x.zip(w).into_iter().collect()
            } else {
                Vec::new()
            };
            for (x, w) in pairs {
                let sb = SoundBank::parse(&x).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
                let wb = WaveBank::parse(&w).unwrap();
                for c in &sb.cues {
                    let s = &sb.sounds[c.sound];
                    let (_, wave) = s.wave.unwrap_or_else(|| panic!("{} cue {}: no wave", f.display(), c.name));
                    assert!((wave as usize) < wb.waves.len(), "{} cue {}: wave {wave} of {}", f.display(), c.name, wb.waves.len());
                    if s.complex() {
                        complex += 1;
                        assert!(s.looping, "{} cue {}: the game's complex sounds are its looping beds", f.display(), c.name);
                    }
                    assert!(!c.name.is_empty(), "{}: a cue without a name", f.display());
                }
                banks += 1;
                cues += sb.cues.len();
            }
        }
        eprintln!("{banks} sound banks, {cues} cues, {complex} complex sounds");
        assert!(banks > 0, "no sound banks under LMA_GAME_DIR");
    }

    /// Global settings in the game's shape: categories with parents and volumes, variables with
    /// their ranges, an RPC curve evaluated linearly and flat past its ends; an RPC naming a
    /// variable that isn't there is refused.
    #[test]
    fn global_settings_categories_variables_curves() {
        let cats: [(&str, u16, u8); 3] = [("Global", 0xFFFF, 180), ("Music", 0, 180), ("Crowd", 0, 160)];
        let vars: [(&str, f32, f32); 2] = [("Distance", 0.0, 1e6), ("StadDisLow", 0.0, 100.0)];
        let rpc_points: [(f32, f32); 3] = [(0.0, -9600.0), (50.0, -600.0), (100.0, 0.0)];
        let build = |rpc_var: u16| -> Vec<u8> {
            let o_cat = 0x43;
            let o_var = o_cat + 10 * cats.len();
            let o_rpc = o_var + 13 * vars.len();
            let o_catn = o_rpc + 5 + 9 * rpc_points.len();
            let catn: Vec<u8> = cats.iter().flat_map(|c| c.0.bytes().chain([0])).collect();
            let o_varn = o_catn + catn.len();
            let mut d = vec![0u8; o_cat];
            d[..4].copy_from_slice(b"FSGX");
            d[4..6].copy_from_slice(&39u16.to_be_bytes());
            for (k, v) in [cats.len() as u16, vars.len() as u16, 0, 0, 1, 0, 0].into_iter().enumerate() {
                d[0x09 + 2 * k..0x0b + 2 * k].copy_from_slice(&v.to_be_bytes());
            }
            for (k, v) in [o_cat, o_var, 0, 0, 0, 0, o_catn, o_varn, o_rpc, 0xFFFF_FFFF, 0xFFFF_FFFF].into_iter().enumerate() {
                d[0x17 + 4 * k..0x1b + 4 * k].copy_from_slice(&(v as u32).to_be_bytes());
            }
            for (_, parent, vol) in cats {
                d.extend_from_slice(&[0xFF, 0, 0, 0, 0, 0]);
                d.extend_from_slice(&parent.to_be_bytes());
                d.extend_from_slice(&[vol, 2]);
            }
            for (_, min, max) in vars {
                d.push(0x0F);
                for f in [0.0f32, min, max] {
                    d.extend_from_slice(&f.to_be_bytes());
                }
            }
            d.extend_from_slice(&rpc_var.to_be_bytes());
            d.push(rpc_points.len() as u8);
            d.extend_from_slice(&0u16.to_be_bytes());
            for (x, y) in rpc_points {
                d.extend_from_slice(&x.to_be_bytes());
                d.extend_from_slice(&y.to_be_bytes());
                d.push(0);
            }
            d.extend_from_slice(&catn);
            d.extend(vars.iter().flat_map(|v| v.0.bytes().chain([0])));
            d
        };
        let g = GlobalSettings::parse(&build(1)).unwrap();
        assert_eq!(g.version, 39);
        assert_eq!(g.categories.iter().map(|c| (c.name.as_str(), c.parent, c.volume)).collect::<Vec<_>>(), vec![("Global", None, 180), ("Music", Some(0), 180), ("Crowd", Some(0), 160)]);
        assert_eq!((g.category("Crowd"), g.variable("StadDisLow")), (Some(2), Some(1)));
        assert_eq!((g.variables[1].min, g.variables[1].max), (0.0, 100.0));
        let r = &g.rpcs[0];
        assert_eq!((r.variable, r.parameter, r.points.len()), (1, 0, 3));
        assert_eq!((r.eval(-5.0), r.eval(0.0), r.eval(25.0), r.eval(75.0), r.eval(500.0)), (-9600.0, -9600.0, -5100.0, -300.0, 0.0));
        assert!(GlobalSettings::parse(&build(7)).unwrap_err().to_string().contains("variable 7"));
        assert!(GlobalSettings::parse(b"KBDS").is_err());
    }

    /// With `LMA_GAME_DIR`: LMA2007.xgs has the game's categories, the match's three crowd
    /// variables, and one volume curve for each of them, which the large crowd's three looping
    /// beds name in order.
    #[test]
    fn the_games_global_settings() {
        let Some(dir) = std::env::var_os("LMA_GAME_DIR") else { return };
        let d = std::fs::read(std::path::Path::new(&dir).join("Common/MainGame/Sound/LMA2007.xgs")).unwrap();
        let g = GlobalSettings::parse(&d).unwrap();
        for c in ["Global", "Default", "Music", "Crowd", "Sound_FX", "Speech"] {
            assert!(g.category(c).is_some(), "category {c}: {:?}", g.categories);
        }
        for v in ["StadDisLow", "StadDisMid", "StadDisHigh"] {
            let i = g.variable(v).unwrap_or_else(|| panic!("variable {v}"));
            let r = g.rpcs.iter().find(|r| r.variable as usize == i).unwrap_or_else(|| panic!("no curve for {v}"));
            assert_eq!(r.parameter, 0, "{v}'s curve sets the volume");
            assert!(r.points.len() >= 2 && r.points.iter().all(|p| (-10_000.0..=600.0).contains(&p.1)), "{v}: {:?}", r.points);
        }
        // the large crowd's three beds name the three curves, low / mid / high
        let mng = std::fs::read(std::path::Path::new(&dir).join("Common/MatchHi/Sound/Crowd/Ambient/AMBLarge.mng")).unwrap();
        let a = crate::Archive::parse(&mng).unwrap();
        let sb = SoundBank::parse(a.entries.iter().find(|(_, b)| b.get(..4) == Some(b"KBDS")).map(|e| e.1.as_slice()).unwrap()).unwrap();
        let curves: Vec<String> = sb.cues.iter().map(|c| {
            let s = &sb.sounds[c.sound];
            assert!(s.looping && s.rpcs.len() == 1, "{}: {s:?}", c.name);
            g.variables[g.rpc_at(s.rpcs[0]).unwrap_or_else(|| panic!("{}: curve {:#x}", c.name, s.rpcs[0])).variable as usize].name.clone()
        }).collect();
        assert_eq!(curves, ["StadDisLow", "StadDisMid", "StadDisHigh"]);
        eprintln!("{} categories, {} variables, {} curves", g.categories.len(), g.variables.len(), g.rpcs.len());
    }

    /// An `.mng` sound bank (a BG2 archive) yields its `.xwb` entry; a plain `.xwb` yields itself.
    #[test]
    fn wave_banks_in_an_mng() {
        let pcm = Format { codec: Codec::Pcm, channels: 1, rate: 44_100, block_align: 2, bits16: true };
        let xwb = bank(&[(pcm, 1, vec![0, 0], (0, 0))], None);
        // BG2: magic, 0x14, table bytes, data start, data length; entries {name[0x100], offset, size}
        let files: [(&str, &[u8]); 2] = [("MENU.xwb", &xwb), ("MENU.xsb", b"KBDS....")];
        let (mut table, mut body) = (Vec::new(), Vec::new());
        for (name, data) in files {
            let mut n = name.as_bytes().to_vec();
            n.resize(0x100, 0);
            table.extend_from_slice(&n);
            table.extend_from_slice(&(body.len() as u32).to_be_bytes());
            table.extend_from_slice(&(data.len() as u32).to_be_bytes());
            body.extend_from_slice(data);
            body.resize(body.len().next_multiple_of(0x800), 0);
        }
        let mut mng = b"BG2\0".to_vec();
        for v in [0x14, table.len(), 0x800, body.len()] {
            mng.extend_from_slice(&(v as u32).to_be_bytes());
        }
        mng.extend_from_slice(&table);
        mng.resize(0x800, 0);
        mng.extend_from_slice(&body);
        let found = wave_banks(&mng).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0.as_str(), found[0].1 == xwb), ("MENU.xwb", true));
        assert_eq!(wave_banks(&xwb).unwrap().len(), 1);
    }
}
