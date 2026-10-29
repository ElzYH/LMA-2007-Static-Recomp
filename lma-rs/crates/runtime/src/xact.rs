//! Native XACT (#772): the game's sound without the emulated audio hardware. This first piece
//! decodes a wave once, from its bank's bytes, to interleaved 16-bit PCM that a host mixer can
//! play; the cue engine that answers the game's XACT calls builds on it.
//!
//! XMA waves go through the same decoder model the emulated path uses (`xma::Channel`, FFmpeg's
//! xma2 with `--features xma-ffmpeg`), driven the way `lma-cli xma-check` drives it: one context
//! per stream of at most two channels (a 5.1 crowd bed is three streams), the streams' packets
//! interleaved in the clip (stream k starts at packet k), the output ring read as fast as it
//! fills. PCM waves are the bank's big-endian samples. `Mixer` plays decoded waves to stereo at
//! `OUT_RATE` (XACT's volume curve, pitch, categories, loops, the 5.1 fold-down); `WaveCache` keeps
//! each decoded wave once; `Engine` is the native XACT the game's calls will drive.

use crate::xma::{self, Channel, Context, Mem};
use lma_data::xact::{Codec, Wave};

/// A decoded wave: interleaved 16-bit samples at `rate`.
#[derive(Clone, Debug, PartialEq)]
pub struct Pcm {
    pub channels: u32,
    pub rate: u32,
    pub samples: Vec<i16>,
}

impl Pcm {
    /// Samples per channel.
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

/// Decode `w` from its bytes (`WaveBank::wave_bytes`). None for a codec this game doesn't use
/// (ADPCM, WMA) or an XMA wave without the decoder (built without `xma-ffmpeg`).
pub fn decode_wave(w: &Wave, bytes: &[u8]) -> Option<Pcm> {
    let (channels, rate) = (w.format.channels.max(1), w.format.rate);
    let samples = match w.format.codec {
        Codec::Pcm if w.format.bits16 => bytes.chunks_exact(2).map(|b| i16::from_be_bytes([b[0], b[1]])).collect(),
        Codec::Pcm => bytes.iter().map(|&b| ((b as i16) - 128) << 8).collect(),
        Codec::Xma => decode_xma(bytes, channels, rate, w.duration as usize)?,
        Codec::Adpcm | Codec::Wma => return None,
    };
    Some(Pcm { channels, rate, samples })
}

/// Flat memory for the decoder model: the clip, then its output ring.
struct Ram(Vec<u8>);

impl Mem for Ram {
    // a read or write past the buffer (a bad stream offset) is zeros / dropped with a log line,
    // never a panic on the audio thread (review on #780)
    fn read(&mut self, phys: u32, buf: &mut [u8]) {
        match self.0.get(phys as usize..phys as usize + buf.len()) {
            Some(src) => buf.copy_from_slice(src),
            None => {
                eprintln!("xact: decoder read {:#x}+{} past its {} bytes", phys, buf.len(), self.0.len());
                buf.fill(0);
            }
        }
    }
    fn write(&mut self, phys: u32, buf: &[u8]) {
        let n = self.0.len();
        match self.0.get_mut(phys as usize..phys as usize + buf.len()) {
            Some(dst) => dst.copy_from_slice(buf),
            None => eprintln!("xact: decoder write {:#x}+{} past its {n} bytes", phys, buf.len()),
        }
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

/// Stream `stream` (of `stream_channels` channels) of an XMA clip, interleaved; None without a
/// decoder.
fn decode_stream(clip: &[u8], stream: u32, stream_channels: u32, rate: u32) -> Option<Vec<i16>> {
    const BASE: u32 = 0x1000;
    const BLOCKS: u32 = 31;
    let ring = BASE + clip.len() as u32 + 0x1000;
    let mut ram = Ram(vec![0; (ring + BLOCKS * xma::BLOCK as u32 + 0x1000) as usize]);
    ram.0[BASE as usize..BASE as usize + clip.len()].copy_from_slice(clip);
    let mut ctx = Context {
        input_packets: [(clip.len() / xma::PACKET) as u32, 0],
        input_valid: [true, false],
        input_ptr: [BASE, 0],
        input_read_offset: stream * xma::PACKET as u32 * 8 + 32,
        output_blocks: BLOCKS,
        output_valid: true,
        output_ptr: ring,
        stereo: stream_channels == 2,
        sample_rate: rate_code(rate),
        ..Default::default()
    };
    let mut ch = Channel { enabled: true, ..Default::default() };
    ch.ensure_decoder((stream_channels, rate), xma::decoder);
    if ch.decoder.is_none() {
        return None;
    }
    let mut out = Vec::new();
    let mut idle = 0;
    for _ in 0..1_000_000 {
        let progressed = ch.work(&mut ctx, &mut ram);
        let (r, w, cap) = (ctx.output_read as usize, ctx.output_write as usize, BLOCKS as usize);
        let n = if !ctx.output_valid && r == w { cap } else { (w + cap - r) % cap };
        for k in 0..n {
            let o = ring as usize + ((r + k) % cap) * xma::BLOCK;
            out.extend(ram.0[o..o + xma::BLOCK].chunks(2).map(|b| i16::from_be_bytes([b[0], b[1]])));
        }
        ctx.output_read = ctx.output_write;
        ctx.output_valid = true;
        // a few idle rounds at the end: the decoder's last frames come out on the second
        idle = if !progressed && n == 0 { idle + 1 } else { 0 };
        if idle >= 3 {
            break;
        }
    }
    Some(out)
}

/// An XMA clip of `channels` channels: its streams decoded and interleaved in channel order,
/// cut to `duration` samples per channel (0: as decoded).
fn decode_xma(clip: &[u8], channels: u32, rate: u32, duration: usize) -> Option<Vec<i16>> {
    let streams: Vec<(u32, Vec<i16>)> = (0..channels.div_ceil(2))
        .map(|s| {
            let sc = (channels - 2 * s).min(2);
            decode_stream(clip, s, sc, rate).map(|v| (sc, v))
        })
        .collect::<Option<_>>()?;
    Some(interleave(&streams, duration))
}

/// Streams of (channels, interleaved samples) as one interleaved signal, as long as the shortest
/// stream (or `duration` frames when nonzero and shorter).
fn interleave(streams: &[(u32, Vec<i16>)], duration: usize) -> Vec<i16> {
    let mut frames = streams.iter().map(|(c, v)| v.len() / (*c as usize).max(1)).min().unwrap_or(0);
    if duration > 0 {
        frames = frames.min(duration);
    }
    let channels: usize = streams.iter().map(|(c, _)| *c as usize).sum();
    let mut out = Vec::with_capacity(frames * channels);
    for f in 0..frames {
        for (c, v) in streams {
            let c = *c as usize;
            out.extend_from_slice(&v[f * c..f * c + c]);
        }
    }
    out
}

/// The host mixer's output rate: lma-app's device stream.
pub const OUT_RATE: u32 = 48_000;

/// XACT's volume byte as a linear gain: 180 is 0 dB, 151 about -3 dB, 0 silence (the curve
/// MonoGame's XACT reader fits to XACT's tool).
pub fn volume_gain(byte: u8) -> f32 {
    if byte == 0 {
        return 0.0;
    }
    let db = (-96.0 - 67.738_521_233_404_7) / (1.0 + (byte as f64 / 80.174_860_029_796_3).powf(0.432_254_984_608_615)) + 67.738_521_233_404_7;
    10f64.powf(db / 20.0) as f32
}

/// One playing wave.
#[derive(Clone, Debug)]
struct Voice {
    id: u64,
    pcm: std::sync::Arc<Pcm>,
    /// Position in source frames.
    pos: f64,
    /// Source frames per output frame (rate and pitch).
    step: f64,
    gain: f32,
    /// An RPC curve's volume on top of `gain` (1.0 without one).
    rpc_gain: f32,
    category: u16,
    /// (start, end) in source frames, for a wave that loops.
    looping: Option<(f64, f64)>,
    /// Paused: kept, not advanced, silent.
    paused: bool,
}

/// Mixes playing waves to stereo at `OUT_RATE` on the host: the native replacement for the
/// guest's XAudio mix (#772). Waves are decoded once (`decode_wave`) and shared.
#[derive(Default, Debug)]
pub struct Mixer {
    voices: Vec<Voice>,
    categories: std::collections::HashMap<u16, f32>,
    next_id: u64,
}

impl Mixer {
    /// Start `pcm` with XACT's volume byte and pitch (1/100 semitone), in `category`, looping
    /// over `loop_frames` (start, length in source frames; length 0 = to the end) when given.
    /// Returns the voice's id.
    pub fn play(&mut self, pcm: std::sync::Arc<Pcm>, volume: u8, pitch: i16, category: u16, loop_frames: Option<(u32, u32)>) -> u64 {
        self.next_id += 1;
        let frames = pcm.frames() as f64;
        let looping = loop_frames.map(|(s, l)| {
            let start = (s as f64).min(frames);
            let end = if l == 0 { frames } else { (start + l as f64).min(frames) };
            (start, end)
        });
        let step = pcm.rate as f64 / OUT_RATE as f64 * 2f64.powf(pitch as f64 / 1200.0);
        self.voices.push(Voice { id: self.next_id, pcm, pos: 0.0, step, gain: volume_gain(volume), rpc_gain: 1.0, category, looping, paused: false });
        self.next_id
    }

    pub fn stop(&mut self, id: u64) {
        self.voices.retain(|v| v.id != id);
    }

    /// A voice's RPC volume (linear), on top of its sound's volume.
    pub fn set_rpc_gain(&mut self, id: u64, gain: f32) {
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
            v.rpc_gain = gain;
        }
    }

    /// Pause (true) or resume (false) a voice where it is.
    pub fn set_paused(&mut self, id: u64, paused: bool) {
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id) {
            v.paused = paused;
        }
    }

    /// Still sounding (a looping wave until stopped).
    pub fn playing(&self, id: u64) -> bool {
        self.voices.iter().any(|v| v.id == id)
    }

    /// A category's volume (linear; 1.0 when never set), as the game's "Setting Category %d volume".
    pub fn set_category_volume(&mut self, category: u16, gain: f32) {
        self.categories.insert(category, gain);
    }

    pub fn voices(&self) -> usize {
        self.voices.len()
    }

    /// Add the playing waves into `out` (stereo at `OUT_RATE`), advancing them; finished waves end.
    pub fn mix(&mut self, out: &mut [[f32; 2]]) {
        const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
        for v in &mut self.voices {
            let ch = v.pcm.channels.max(1) as usize;
            let frames = v.pcm.frames();
            if frames == 0 || v.paused {
                continue;
            }
            // category 0 is XACT's Global, the parent of every other: its volume is the master
            let master = if v.category == 0 { 1.0 } else { self.categories.get(&0).copied().unwrap_or(1.0) };
            let gain = v.gain * v.rpc_gain * master * self.categories.get(&v.category).copied().unwrap_or(1.0) / 32768.0;
            let s = &v.pcm.samples;
            for o in out.iter_mut() {
                if let Some((start, end)) = v.looping {
                    if end > start && v.pos >= end {
                        v.pos = start + (v.pos - end) % (end - start);
                    }
                }
                let i = v.pos as usize;
                if i >= frames {
                    break;
                }
                // linear interpolation between frame i and the next (a loop's start past its end)
                let t = (v.pos - i as f64) as f32;
                let next = match v.looping {
                    Some((start, end)) if end > start && (i + 1) as f64 >= end => start as usize,
                    _ => (i + 1).min(frames - 1),
                };
                let at = |c: usize| s[i * ch + c] as f32 * (1.0 - t) + s[next * ch + c] as f32 * t;
                let (l, r) = match ch {
                    1 => (at(0), at(0)),
                    // 5.1 (L R C LFE SL SR): the emulated path's fold-down (`audio::Audio::submit`)
                    6 => (at(0) + K * at(2) + K * at(4), at(1) + K * at(2) + K * at(5)),
                    _ => (at(0), at(1)),
                };
                o[0] += l * gain;
                o[1] += r * gain;
                v.pos += v.step;
            }
        }
        self.voices.retain(|v| v.looping.is_some_and(|(s, e)| e > s) || (v.pos as usize) < v.pcm.frames());
    }
}

/// Decoded waves kept for reuse, keyed by (bank, wave index): each wave is decoded once, when the
/// game first prepares it, and dropped least-recently-used once the cache passes its byte budget
/// (the commentary alone would be several GB decoded). A playing voice holds its own `Arc`, so
/// eviction never cuts a sound short.
#[derive(Debug)]
pub struct WaveCache {
    budget: usize,
    bytes: usize,
    tick: u64,
    entries: std::collections::HashMap<(u32, u32), (std::sync::Arc<Pcm>, u64)>,
    pub decodes: u64,
}

impl WaveCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self { budget: budget_bytes, bytes: 0, tick: 0, entries: Default::default(), decodes: 0 }
    }

    /// Wave `wave` of bank `bank`: from the cache, or decoded by `decode` and kept. None when it
    /// can't be decoded (and nothing is kept).
    pub fn get(&mut self, bank: u32, wave: u32, decode: impl FnOnce() -> Option<Pcm>) -> Option<std::sync::Arc<Pcm>> {
        self.tick += 1;
        if let Some((pcm, used)) = self.entries.get_mut(&(bank, wave)) {
            *used = self.tick;
            return Some(pcm.clone());
        }
        let pcm = std::sync::Arc::new(decode()?);
        self.decodes += 1;
        self.bytes += pcm.samples.len() * 2;
        self.entries.insert((bank, wave), (pcm.clone(), self.tick));
        while self.bytes > self.budget && self.entries.len() > 1 {
            let oldest = *self.entries.iter().filter(|(k, _)| **k != (bank, wave)).min_by_key(|(_, (_, used))| *used).map(|(k, _)| k).unwrap();
            if let Some((p, _)) = self.entries.remove(&oldest) {
                self.bytes -= p.samples.len() * 2;
            }
        }
        Some(pcm)
    }

    /// Bytes of decoded samples held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A cue instance's state, as the game asks it (XACT's prepared / playing / stopped).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CueState {
    Prepared,
    Playing,
    Stopped,
}

/// A loaded wave bank: its parsed entries and its bytes.
struct LoadedWaves {
    name: String,
    bank: lma_data::xact::WaveBank,
    bytes: std::sync::Arc<Vec<u8>>,
    /// Its waves decoded at install (`xact_cache`), when the cache holds this bank.
    cached: Option<crate::xact_cache::Cached>,
}

/// A prepared or playing cue.
struct Instance {
    sound_bank: usize,
    cue: usize,
    pcm: Option<std::sync::Arc<Pcm>>,
    voice: Option<u64>,
    stopped: bool,
}

/// The native XACT engine (#772): the banks the game loads, its cues, categories and global
/// variables, playing through `Mixer` with waves from `WaveCache`. The game's XACT calls (the 15
/// entry points of the #772 map) become calls on this; nothing here runs guest code.
pub struct Engine {
    pub settings: Option<lma_data::xact::GlobalSettings>,
    waves: Vec<LoadedWaves>,
    sounds: Vec<lma_data::xact::SoundBank>,
    instances: std::collections::HashMap<u64, Instance>,
    next: u64,
    globals: Vec<f32>,
    pub cache: WaveCache,
    /// The voices, behind their own lock: the output device mixes from here (`native_audio::mix`)
    /// while the engine's slow work (bank parsing, live XMA decodes) runs under the caller's lock,
    /// so a voice change or a mix is all the device ever waits for (#772).
    pub mixer: SharedMixer,
    /// The decoded-audio cache written at install (`userdata/audio`): banks loaded after this is
    /// set read their waves from it and decode live only what it lacks.
    pub cache_dir: Option<std::path::PathBuf>,
    /// Waves read from the install's cache, and decoded live.
    pub cache_reads: u64,
    pub live_decodes: u64,
    /// When set, a wave neither in memory nor in the install's cache is decoded on this thread
    /// instead of in `prepare`: that cue plays silence once and the next one has its wave, so the
    /// caller (the game thread, for native audio) never waits for an XMA decode (#772).
    pub decoder: Option<Decoder>,
}

/// A wave's key in the engine's cache: (wave bank index, wave index).
type WaveKey = (u32, u32);

/// The background decoder (`Engine::decoder`): one thread, waves in the order asked for.
pub struct Decoder {
    tx: std::sync::mpsc::Sender<(WaveKey, lma_data::xact::Wave, Vec<u8>)>,
    done: std::sync::Arc<std::sync::Mutex<Vec<(WaveKey, Option<Pcm>)>>>,
    pending: std::collections::HashSet<WaveKey>,
}

impl Decoder {
    pub fn spawn() -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<(WaveKey, lma_data::xact::Wave, Vec<u8>)>();
        let done = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let finished = done.clone();
        let _ = std::thread::Builder::new().name("xact decode".into()).spawn(move || {
            for (key, wave, bytes) in rx {
                let pcm = decode_wave(&wave, &bytes);
                finished.lock().unwrap_or_else(|p| p.into_inner()).push((key, pcm));
            }
        });
        Self { tx, done, pending: Default::default() }
    }

    /// Ask for `key`'s wave once (a wave already on its way isn't sent again).
    fn submit(&mut self, key: WaveKey, wave: lma_data::xact::Wave, bytes: Vec<u8>) {
        if self.pending.insert(key) {
            let _ = self.tx.send((key, wave, bytes));
        }
    }

    /// The waves finished since the last call.
    fn take(&mut self) -> Vec<(WaveKey, Option<Pcm>)> {
        let done = std::mem::take(&mut *self.done.lock().unwrap_or_else(|p| p.into_inner()));
        for (key, _) in &done {
            self.pending.remove(key);
        }
        done
    }
}

/// The engine's mixer, shared with the output device's thread.
pub type SharedMixer = std::sync::Arc<std::sync::Mutex<Mixer>>;

/// `mixer` locked (a poisoned lock taken over); a function of the field alone, so the engine can
/// lock it while it holds one of its instances mutably.
fn lock_mixer(mixer: &SharedMixer) -> std::sync::MutexGuard<'_, Mixer> {
    mixer.lock().unwrap_or_else(|p| p.into_inner())
}

impl Engine {
    /// The mixer, locked for one change or mix; a panic elsewhere while holding it doesn't silence the
    /// sound (the poisoned lock is taken over).
    pub fn mixer(&self) -> std::sync::MutexGuard<'_, Mixer> {
        lock_mixer(&self.mixer)
    }

    /// An engine with the game's global settings (categories, variables, curves), when loaded.
    pub fn new(settings: Option<lma_data::xact::GlobalSettings>, cache_bytes: usize) -> Self {
        let globals = settings.as_ref().map_or(Vec::new(), |g| g.variables.iter().map(|v| v.initial).collect());
        Self { settings, waves: Vec::new(), sounds: Vec::new(), instances: Default::default(), next: 0, globals, cache: WaveCache::new(cache_bytes), mixer: SharedMixer::default(), cache_dir: None, cache_reads: 0, live_decodes: 0, decoder: None }
    }

    /// A wave bank from its file bytes; its index.
    pub fn load_wave_bank(&mut self, bytes: Vec<u8>) -> lma_data::Result<usize> {
        let bank = lma_data::xact::WaveBank::parse(&bytes)?;
        let cached = self.cache_dir.as_deref().and_then(|d| crate::xact_cache::Cached::open(d, &bank, &bytes));
        self.waves.push(LoadedWaves { name: bank.name.clone(), bank, bytes: std::sync::Arc::new(bytes), cached });
        Ok(self.waves.len() - 1)
    }

    /// A sound bank from its file bytes; its index. Its wave banks are found by name when a cue
    /// is prepared, so the banks can load in either order.
    pub fn load_sound_bank(&mut self, bytes: &[u8]) -> lma_data::Result<usize> {
        self.sounds.push(lma_data::xact::SoundBank::parse(bytes)?);
        Ok(self.sounds.len() - 1)
    }

    /// Every wave and sound bank in a file: an `.mng` sound bank (a BG2 archive holding an `.xwb`
    /// and an `.xsb`), or a lone `.xwb` / `.xsb`. Returns the sound banks' indices, e.g. for the
    /// launcher's menu sounds: `Common/MainGame/Sound/Menu.mng`, then `play_cue(sb, cue_index(sb,
    /// "bp_menu_select"))` (cues `bp_menu_UDLR`, `bp_menu_select`, `bp_menu_back`, `bp_menu_error`;
    /// the looping `Frontendmusic` is `Common/Frontend/Sound/FEMusic.xwb` + `.xsb`).
    pub fn load_file(&mut self, bytes: &[u8]) -> lma_data::Result<Vec<usize>> {
        let parts: Vec<Vec<u8>> = if bytes.get(..4) == Some(b"BG2\0") {
            lma_data::Archive::parse(bytes)?.entries.into_iter().map(|(_, b)| b).collect()
        } else {
            vec![bytes.to_vec()]
        };
        let mut sound_banks = Vec::new();
        for b in parts {
            match b.get(..4) {
                Some(b"DNBW") => {
                    self.load_wave_bank(b)?;
                }
                Some(b"KBDS") => sound_banks.push(self.load_sound_bank(&b)?),
                _ => {}
            }
        }
        Ok(sound_banks)
    }

    /// A cue's index in sound bank `sb` by name.
    pub fn cue_index(&self, sb: usize, name: &str) -> Option<usize> {
        self.sounds.get(sb)?.cue(name)
    }

    /// `<bank>/<cue>` for cue `cue` of sound bank `sb` (the black box's event log names sounds).
    pub fn cue_label(&self, sb: usize, cue: usize) -> Option<String> {
        let bank = self.sounds.get(sb)?;
        Some(format!("{}/{}", bank.name, bank.cues.get(cue)?.name))
    }

    /// Move the background decoder's finished waves into the cache.
    fn collect_decoded(&mut self) {
        let Some(d) = self.decoder.as_mut() else { return };
        for ((bank, wave), pcm) in d.take() {
            if let Some(pcm) = pcm {
                self.live_decodes += 1;
                let _ = self.cache.get(bank, wave, || Some(pcm));
            }
        }
    }

    /// Prepare cue `cue` of sound bank `sb`: its wave decoded (once, through the cache). Returns
    /// the instance's handle; None for a cue that doesn't exist. A cue whose wave can't be
    /// decoded (or a looping bed, whose clip events aren't read yet) is still a handle and plays
    /// silence, so the game's waits on it end.
    pub fn prepare(&mut self, sb: usize, cue: usize) -> Option<u64> {
        self.collect_decoded();
        let bank = self.sounds.get(sb)?;
        let c = bank.cues.get(cue)?;
        let sound = &bank.sounds[c.sound];
        let pcm = sound.wave.and_then(|(b, w)| {
            let name = bank.wave_banks.get(b as usize)?;
            let wb = self.waves.iter().position(|l| l.name.eq_ignore_ascii_case(name))?;
            let l = &self.waves[wb];
            let (wave, bytes) = (l.bank.waves.get(w as usize)?.clone(), l.bytes.clone());
            let (reads, decodes, decoder) = (&mut self.cache_reads, &mut self.live_decodes, &mut self.decoder);
            self.cache.get(wb as u32, w as u32, || {
                if let Some(p) = l.cached.as_ref().and_then(|c| c.wave(w as usize)) {
                    *reads += 1;
                    return Some(p);
                }
                let raw = l.bank.wave_bytes(&bytes, w as usize)?;
                if let Some(d) = decoder.as_mut() {
                    // silent this time; the decode thread has it ready for the next play
                    d.submit((wb as u32, w as u32), wave.clone(), raw.to_vec());
                    return None;
                }
                *decodes += 1;
                decode_wave(&wave, raw)
            })
        });
        self.next += 1;
        self.instances.insert(self.next, Instance { sound_bank: sb, cue, pcm, voice: None, stopped: false });
        Some(self.next)
    }

    /// Start a prepared cue (with its sound's volume, pitch and category).
    pub fn play(&mut self, handle: u64) {
        // the RPC gain first: the voice starts at it under one mixer lock, so the output device
        // can't mix a buffer of it at the default gain in between (a click at the cue's start)
        let g = self.rpc_gain(handle);
        let Some(i) = self.instances.get_mut(&handle) else { return };
        if i.voice.is_some() || i.stopped {
            return;
        }
        let sound = &self.sounds[i.sound_bank].sounds[self.sounds[i.sound_bank].cues[i.cue].sound];
        let (volume, pitch, category) = (sound.volume, sound.pitch, sound.category);
        let loop_frames = sound.looping.then_some((0, 0));
        let voice = match &i.pcm {
            Some(pcm) => {
                let mut mixer = lock_mixer(&self.mixer);
                let voice = mixer.play(pcm.clone(), volume, pitch, category, loop_frames);
                if voice != 0 {
                    mixer.set_rpc_gain(voice, g);
                }
                voice
            }
            None => 0,
        };
        i.voice = Some(voice);
    }

    /// The RPC volume of an instance's sound at the current global values (linear; 1.0 without
    /// curves): each curve's value in hundredths of a dB, summed.
    fn rpc_gain(&self, handle: u64) -> f32 {
        let (Some(i), Some(g)) = (self.instances.get(&handle), self.settings.as_ref()) else { return 1.0 };
        let sound = &self.sounds[i.sound_bank].sounds[self.sounds[i.sound_bank].cues[i.cue].sound];
        let millibels: f32 = sound.rpcs.iter().filter_map(|&o| g.rpc_at(o)).filter(|r| r.parameter == 0).map(|r| r.eval(self.globals.get(r.variable as usize).copied().unwrap_or(0.0))).sum();
        10f32.powf(millibels / 2000.0)
    }

    /// Prepare and play in one go (XACT's Play on a sound bank).
    pub fn play_cue(&mut self, sb: usize, cue: usize) -> Option<u64> {
        let h = self.prepare(sb, cue)?;
        self.play(h);
        Some(h)
    }

    pub fn stop(&mut self, handle: u64) {
        if let Some(i) = self.instances.get_mut(&handle) {
            if let Some(v) = i.voice {
                lock_mixer(&self.mixer).stop(v);
            }
            i.stopped = true;
        }
    }

    /// Stop sound bank `sb`'s instances of `cue`, or all of them (XACT's SoundBank::Stop).
    pub fn stop_cues(&mut self, sb: usize, cue: Option<usize>) {
        let hs: Vec<u64> = self.instances.iter().filter(|(_, i)| i.sound_bank == sb && cue.is_none_or(|c| i.cue == c)).map(|(&h, _)| h).collect();
        for h in hs {
            self.stop(h);
        }
    }

    /// Pause (true) or resume (false) a playing instance (XACT's Cue::Pause).
    pub fn pause(&mut self, handle: u64, paused: bool) {
        if let Some(v) = self.instances.get(&handle).and_then(|i| i.voice) {
            lock_mixer(&self.mixer).set_paused(v, paused);
        }
    }

    /// The wave banks sound bank `sb` plays from, by name.
    pub fn wave_bank_names(&self, sb: usize) -> Vec<String> {
        self.sounds.get(sb).map_or(Vec::new(), |b| b.wave_banks.clone())
    }

    /// A wave bank of this name is loaded.
    pub fn has_wave_bank(&self, name: &str) -> bool {
        self.waves.iter().any(|l| l.name.eq_ignore_ascii_case(name))
    }

    pub fn state(&self, handle: u64) -> Option<CueState> {
        let i = self.instances.get(&handle)?;
        Some(match i.voice {
            _ if i.stopped => CueState::Stopped,
            None => CueState::Prepared,
            Some(v) if v != 0 && lock_mixer(&self.mixer).playing(v) => CueState::Playing,
            Some(_) => CueState::Stopped,
        })
    }

    /// Forget an instance (the game's release of a finished cue).
    pub fn release(&mut self, handle: u64) {
        self.stop(handle);
        self.instances.remove(&handle);
    }

    /// A category's volume by name (the game's "Setting Category %d volume"), linear.
    pub fn set_category_volume(&mut self, name: &str, gain: f32) -> bool {
        match self.settings.as_ref().and_then(|g| g.category(name)) {
            Some(i) => {
                lock_mixer(&self.mixer).set_category_volume(i as u16, gain);
                true
            }
            None => false,
        }
    }

    /// A global variable's index by name and its value (the match's StadDisLow/Mid/High).
    pub fn global_index(&self, name: &str) -> Option<usize> {
        self.settings.as_ref()?.variable(name)
    }

    /// Set a global variable (clamped to its range) and apply it to the playing sounds whose
    /// volume curves read it: the match's StadDisLow/Mid/High moving the crowd beds.
    pub fn set_global(&mut self, index: usize, value: f32) {
        if let (Some(v), Some(g)) = (self.globals.get_mut(index), self.settings.as_ref()) {
            let var = &g.variables[index];
            *v = value.clamp(var.min.min(var.max), var.max.max(var.min));
        }
        let playing: Vec<(u64, u64)> = self.instances.iter().filter_map(|(&h, i)| i.voice.filter(|&v| v != 0).map(|v| (h, v))).collect();
        for (h, v) in playing {
            let g = self.rpc_gain(h);
            lock_mixer(&self.mixer).set_rpc_gain(v, g);
        }
    }

    pub fn global(&self, index: usize) -> Option<f32> {
        self.globals.get(index).copied()
    }

    /// Instances held (prepared, playing or stopped and not released yet).
    pub fn instances(&self) -> usize {
        self.instances.len()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lma_data::xact::Format;

    fn wave(codec: Codec, channels: u32, bits16: bool, duration: u32) -> Wave {
        Wave { flags: 0, duration, format: Format { codec, channels, rate: 48_000, block_align: 2, bits16 }, play: (0, 0), loop_region: (0, 0), name: None }
    }

    fn pcm(channels: u32, rate: u32, samples: Vec<i16>) -> std::sync::Arc<Pcm> {
        std::sync::Arc::new(Pcm { channels, rate, samples })
    }

    /// XACT's volume byte: 180 is unity, 151 about -3 dB, 0 silent.
    #[test]
    fn volume_bytes() {
        assert!((volume_gain(180) - 1.0).abs() < 0.01, "{}", volume_gain(180));
        let db151 = 20.0 * volume_gain(151).log10();
        assert!((db151 + 3.0).abs() < 0.2, "{db151}");
        assert_eq!(volume_gain(0), 0.0);
    }

    /// A mono wave at the output rate plays its samples on both sides, then ends; a stopped voice
    /// stops; a category's volume scales its voices.
    #[test]
    fn mixer_plays_stops_and_scales() {
        let mut m = Mixer::default();
        let id = m.play(pcm(1, OUT_RATE, vec![16384, -16384, 8192]), 180, 0, 4, None);
        let mut out = vec![[0f32; 2]; 5];
        m.mix(&mut out);
        let g = volume_gain(180);
        assert!((out[0][0] - 0.5 * g).abs() < 1e-3 && (out[0][1] - 0.5 * g).abs() < 1e-3, "{out:?}");
        assert!((out[1][0] + 0.5 * g).abs() < 1e-3 && out[3] == [0.0, 0.0], "{out:?}");
        assert!(!m.playing(id), "a one-shot ends");
        let a = m.play(pcm(1, OUT_RATE, vec![16384; 100]), 180, 0, 2, None);
        m.set_category_volume(2, 0.5);
        let mut out = vec![[0f32; 2]; 2];
        m.mix(&mut out);
        assert!((out[0][0] - 0.25 * g).abs() < 1e-3, "{out:?}");
        m.stop(a);
        assert_eq!(m.voices(), 0);
    }

    /// A 44.1 kHz wave takes 48/44.1 as many output frames; a looping wave wraps to its loop start
    /// and keeps going; a 5.1 bed folds its centre and surrounds into both sides, the LFE left out.
    #[test]
    fn mixer_resamples_loops_and_downmixes() {
        let mut m = Mixer::default();
        let id = m.play(pcm(1, 44_100, vec![1000; 441]), 180, 0, 0, None);
        let mut out = vec![[0f32; 2]; 600];
        m.mix(&mut out);
        let sounding = out.iter().filter(|s| s[0] != 0.0).count();
        assert!((479..=481).contains(&sounding), "{sounding} frames for 10 ms at 44.1 kHz");
        assert!(!m.playing(id));

        let g = volume_gain(180) / 32768.0;
        let id = m.play(pcm(1, OUT_RATE, vec![100, 200, 300, 400]), 180, 0, 0, Some((2, 2)));
        let mut out = vec![[0f32; 2]; 8];
        m.mix(&mut out);
        let got: Vec<i32> = out.iter().map(|s| (s[0] / g).round() as i32).collect();
        assert_eq!(got, vec![100, 200, 300, 400, 300, 400, 300, 400], "loops over frames 2..4");
        assert!(m.playing(id), "a loop keeps playing until stopped");

        let mut m = Mixer::default();
        m.play(pcm(6, OUT_RATE, vec![1000, 0, 2000, 9999, 3000, 0]), 180, 0, 0, None);
        let mut out = vec![[0f32; 2]; 1];
        m.mix(&mut out);
        let k = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0][0] / g - (1000.0 + k * 2000.0 + k * 3000.0)).abs() < 1.0, "{out:?}");
        assert!((out[0][1] / g - k * 2000.0).abs() < 1.0, "{out:?}");
    }

    /// A wave is decoded once and then served from the cache; past the budget the least recently
    /// used wave goes, never the one just asked for; a wave that can't be decoded isn't kept.
    #[test]
    fn wave_cache_decodes_once_and_evicts_lru() {
        let mut c = WaveCache::new(10);
        let wave = |n: usize| move || Some(Pcm { channels: 1, rate: OUT_RATE, samples: vec![0; n] });
        let a = c.get(0, 1, wave(2)).unwrap(); // 4 bytes
        assert_eq!((c.decodes, c.bytes()), (1, 4));
        let again = c.get(0, 1, || panic!("decoded twice")).unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &again));
        c.get(0, 2, wave(2)).unwrap(); // 8 bytes
        c.get(0, 1, || None).unwrap(); // wave 1 used last now
        c.get(1, 7, wave(2)).unwrap(); // 12 > 10: wave 2 goes (least recently used)
        assert_eq!((c.len(), c.bytes()), (2, 8));
        assert!(c.get(0, 2, || None).is_none(), "evicted, and None isn't kept");
        assert!(c.get(0, 1, || None).is_some() && c.get(1, 7, || None).is_some());
        let big = c.get(2, 0, wave(100)).unwrap();
        assert_eq!((c.len(), big.samples.len()), (1, 100), "an entry larger than the budget stays alone");
        assert_eq!(c.decodes, 4);
    }

    /// A v39 wave bank with one 16-bit mono PCM wave per entry of `waves` (samples), named `name`.
    pub(crate) fn pcm_wave_bank(name: &str, waves: &[Vec<i16>]) -> Vec<u8> {
        let meta = 0x30 + 0x60;
        let data_off = (meta + 24 * waves.len()).next_multiple_of(0x800);
        let mut data = Vec::new();
        let mut entries = Vec::new();
        for w in waves {
            let off = data.len() as u32;
            data.extend(w.iter().flat_map(|s| s.to_be_bytes()));
            let fmt = lma_data::xact::Format { codec: Codec::Pcm, channels: 1, rate: OUT_RATE, block_align: 2, bits16: true }.word();
            for v in [(w.len() as u32) << 4, fmt, off, (w.len() * 2) as u32, 0, 0] {
                entries.extend_from_slice(&v.to_be_bytes());
            }
        }
        let mut d = b"DNBW".to_vec();
        d.extend_from_slice(&39u32.to_be_bytes());
        for (o, l) in [(0x30, 0x60), (meta, 24 * waves.len()), (0, 0), (0, 0), (data_off, data.len())] {
            d.extend_from_slice(&(o as u32).to_be_bytes());
            d.extend_from_slice(&(l as u32).to_be_bytes());
        }
        d.extend_from_slice(&0u32.to_be_bytes());
        d.extend_from_slice(&(waves.len() as u32).to_be_bytes());
        let mut n = [0u8; 64];
        n[..name.len()].copy_from_slice(name.as_bytes());
        d.extend_from_slice(&n);
        for v in [24u32, 0, 0x800, 0] {
            d.extend_from_slice(&v.to_be_bytes());
        }
        d.extend_from_slice(&0u64.to_be_bytes());
        d.extend_from_slice(&entries);
        d.resize(data_off, 0);
        d.extend_from_slice(&data);
        d
    }

    /// A v39 sound bank naming wave bank `wb`, one simple cue per (name, wave, volume, category).
    pub(crate) fn simple_sound_bank(wb: &str, cues: &[(&str, u16, u8, u16)]) -> Vec<u8> {
        let wb_off = 0x92;
        let sounds_off = wb_off + 64;
        let simple_off = sounds_off + 12 * cues.len();
        let names_off = simple_off + 5 * cues.len();
        let names: Vec<u8> = cues.iter().flat_map(|c| c.0.bytes().chain([0])).collect();
        let mut d = vec![0u8; 0x92];
        d[..4].copy_from_slice(b"KBDS");
        d[4..6].copy_from_slice(&39u16.to_be_bytes());
        d[9..11].copy_from_slice(&(cues.len() as u16).to_be_bytes());
        d[0x11] = 1;
        d[0x12..0x14].copy_from_slice(&(cues.len() as u16).to_be_bytes());
        d[0x16..0x1a].copy_from_slice(&(names.len() as u32).to_be_bytes());
        for (k, v) in [simple_off as u32, u32::MAX, names_off as u32, u32::MAX, u32::MAX, u32::MAX, wb_off as u32, u32::MAX, u32::MAX, sounds_off as u32].into_iter().enumerate() {
            d[0x1a + 4 * k..0x1e + 4 * k].copy_from_slice(&v.to_be_bytes());
        }
        let mut n = [0u8; 64];
        n[..wb.len()].copy_from_slice(wb.as_bytes());
        d.extend_from_slice(&n);
        for (_, wave, vol, cat) in cues {
            d.push(0);
            d.extend_from_slice(&cat.to_be_bytes());
            d.push(*vol);
            d.extend_from_slice(&[0, 0, 0, 0, 12]);
            d.extend_from_slice(&wave.to_be_bytes());
            d.push(0);
        }
        for k in 0..cues.len() {
            d.push(4);
            d.extend_from_slice(&((sounds_off + 12 * k) as u32).to_be_bytes());
        }
        d.extend_from_slice(&names);
        d
    }

    /// The engine end to end on banks built by hand: a cue by name is prepared (its wave decoded
    /// once), played (mixed at its volume), and ends stopped; stopping and releasing work; a cue
    /// prepared before its wave bank loaded still gets a handle and reads as finished.
    #[test]
    fn engine_cues_from_banks_to_the_mixer() {
        let mut e = Engine::new(None, 1 << 20);
        let sb = e.load_sound_bank(&simple_sound_bank("MATCH", &[("BallNet", 0, 180, 4), ("kick", 1, 180, 4)])).unwrap();
        let early = e.play_cue(sb, 0).unwrap();
        assert_eq!(e.state(early), Some(CueState::Stopped), "no wave bank yet: plays silence, finishes at once");
        e.load_wave_bank(pcm_wave_bank("MATCH", &[vec![16384; 4], vec![-8192; 100]])).unwrap();
        let c = e.cue_index(sb, "BallNet").unwrap();
        let h = e.prepare(sb, c).unwrap();
        assert_eq!((e.state(h), e.cache.decodes), (Some(CueState::Prepared), 1));
        e.play(h);
        assert_eq!(e.state(h), Some(CueState::Playing));
        let mut out = vec![[0f32; 2]; 8];
        e.mixer().mix(&mut out);
        assert!((out[0][0] - 0.5 * volume_gain(180)).abs() < 1e-3 && out[5] == [0.0, 0.0], "{out:?}");
        assert_eq!(e.state(h), Some(CueState::Stopped), "a one-shot ends");
        let again = e.play_cue(sb, c).unwrap();
        assert_eq!(e.cache.decodes, 1, "the wave is decoded once");
        e.stop(again);
        assert_eq!(e.state(again), Some(CueState::Stopped));
        let kick = e.play_cue(sb, e.cue_index(sb, "kick").unwrap()).unwrap();
        assert_eq!(e.state(kick), Some(CueState::Playing));
        e.release(kick);
        assert_eq!((e.state(kick), e.mixer().voices()), (None, 0));
        assert_eq!((e.cue_index(sb, "nope"), e.prepare(sb, 9), e.prepare(5, 0)), (None, None, None));
    }

    /// With the background decoder a wave missing from the caches is decoded off the calling thread:
    /// the first cue plays silence, a later one has the wave; it's asked for once (#772).
    #[test]
    fn the_decoder_thread_decodes_a_missing_wave_for_the_next_play() {
        let mut e = Engine::new(None, 1 << 20);
        e.decoder = Some(Decoder::spawn());
        let sb = e.load_sound_bank(&simple_sound_bank("MATCH", &[("BallNet", 0, 180, 4)])).unwrap();
        e.load_wave_bank(pcm_wave_bank("MATCH", &[vec![16384; 4]])).unwrap();
        let first = e.play_cue(sb, 0).unwrap();
        assert_eq!(e.state(first), Some(CueState::Stopped), "no wave yet: silence, finished at once");
        let _ = e.play_cue(sb, 0); // asked again while on its way: not sent twice
        let t = std::time::Instant::now();
        loop {
            let h = e.play_cue(sb, 0).unwrap();
            if e.state(h) == Some(CueState::Playing) {
                break;
            }
            assert!(t.elapsed() < std::time::Duration::from_secs(5), "the decode thread never answered");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!((e.live_decodes, e.cache.decodes), (1, 1), "decoded once, by the thread");
    }

    /// Pause holds a voice silent where it is; the wave-bank queries native audio loads by.
    #[test]
    fn engine_pauses_and_names_its_wave_banks() {
        let mut e = Engine::new(None, 1 << 20);
        let sb = e.load_sound_bank(&simple_sound_bank("MENU", &[("click", 0, 180, 0)])).unwrap();
        assert_eq!((e.wave_bank_names(sb), e.has_wave_bank("menu")), (vec!["MENU".to_string()], false));
        e.load_wave_bank(pcm_wave_bank("MENU", &[vec![16384; 8]])).unwrap();
        assert!(e.has_wave_bank("menu") && e.wave_bank_names(7).is_empty());
        let h = e.play_cue(sb, 0).unwrap();
        e.pause(h, true);
        let mut out = vec![[0f32; 2]; 4];
        e.mixer().mix(&mut out);
        assert_eq!((out[0], e.state(h)), ([0.0, 0.0], Some(CueState::Playing)), "paused: silent, not finished");
        e.pause(h, false);
        e.mixer().mix(&mut out);
        assert!(out[0][0] > 0.0, "resumed from where it was");
    }

    /// SoundBank::Stop ends one cue's instances or, with no cue, all of the bank's.
    #[test]
    fn a_sound_bank_stops_its_cues() {
        let mut e = Engine::new(None, 1 << 20);
        let sb = e.load_sound_bank(&simple_sound_bank("MENU", &[("music", 0, 180, 0), ("click", 0, 180, 0)])).unwrap();
        e.load_wave_bank(pcm_wave_bank("MENU", &[vec![16384; 64]])).unwrap();
        let (music, click) = (e.play_cue(sb, 0).unwrap(), e.play_cue(sb, 1).unwrap());
        e.stop_cues(sb, Some(0));
        assert_eq!((e.state(music), e.state(click)), (Some(CueState::Stopped), Some(CueState::Playing)));
        e.stop_cues(sb + 1, None);
        assert_eq!(e.state(click), Some(CueState::Playing), "another bank's stop");
        e.stop_cues(sb, None);
        assert_eq!(e.state(click), Some(CueState::Stopped));
    }

    /// Category 0 (Global) scales every category; the others only their own.
    #[test]
    fn the_global_category_is_the_master_volume() {
        let pcm = std::sync::Arc::new(Pcm { channels: 1, rate: OUT_RATE, samples: vec![16384; 4] });
        let mut m = Mixer::default();
        m.play(pcm.clone(), 180, 0, 3, None);
        m.set_category_volume(0, 0.5);
        m.set_category_volume(3, 0.5);
        m.set_category_volume(4, 0.0);
        let mut out = vec![[0f32; 2]; 1];
        m.mix(&mut out);
        assert!((out[0][0] - 0.125 * volume_gain(180)).abs() < 1e-4, "{out:?}");
    }

    /// The output device's copy of the mixer hears what the engine plays, and mixes while the
    /// engine itself is locked by the game thread (#772: a decode under that lock stalled the device).
    #[test]
    fn the_device_mixes_without_the_engine_lock() {
        let pcm = std::sync::Arc::new(Pcm { channels: 1, rate: OUT_RATE, samples: vec![16384; 4] });
        let e = Engine::new(None, 1 << 20);
        let device = e.mixer.clone();
        e.mixer().play(pcm, 180, 0, 0, None);
        let engine = std::sync::Mutex::new(e);
        let _held = engine.lock().unwrap(); // the game thread in a hook
        let mut out = vec![[0f32; 2]; 1];
        device.lock().unwrap().mix(&mut out);
        assert!((out[0][0] - 0.5 * volume_gain(180)).abs() < 1e-4, "{out:?}");
    }

    /// The decoder model's memory: a read past it is zeros and a write past it is dropped, not a panic.
    #[test]
    fn decoder_memory_past_the_end_is_safe() {
        let mut r = Ram(vec![1, 2, 3, 4]);
        let mut b = [9u8; 2];
        r.read(1, &mut b);
        assert_eq!(b, [2, 3]);
        r.read(3, &mut b);
        assert_eq!(b, [0, 0], "past the end: zeros");
        r.write(3, &[7, 7]);
        r.write(0, &[5]);
        assert_eq!(r.0, vec![5, 2, 3, 4], "the write past the end dropped");
    }

    /// An `.mng` sound bank (a BG2 archive of an `.xwb` + `.xsb`) loads in one call and its cues play.
    #[test]
    fn engine_loads_an_mng_pair() {
        let (xwb, xsb) = (pcm_wave_bank("MENU", &[vec![100; 8]]), simple_sound_bank("MENU", &[("bp_menu_select", 0, 170, 4)]));
        let (mut table, mut body) = (Vec::new(), Vec::new());
        for (name, data) in [("MENU.xwb", &xwb), ("MENU.xsb", &xsb)] {
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
        let mut e = Engine::new(None, 1 << 20);
        let sbs = e.load_file(&mng).unwrap();
        assert_eq!(sbs.len(), 1);
        let h = e.play_cue(sbs[0], e.cue_index(sbs[0], "bp_menu_select").unwrap()).unwrap();
        assert_eq!(e.state(h), Some(CueState::Playing));
    }

    /// PCM waves are the bank's big-endian samples (16-bit) or unsigned bytes (8-bit).
    #[test]
    fn pcm_waves_decode_as_stored() {
        let p = decode_wave(&wave(Codec::Pcm, 1, true, 3), &[0x00, 0x01, 0xFF, 0xFF, 0x80, 0x00]).unwrap();
        assert_eq!((p.channels, p.rate, p.samples.clone(), p.frames()), (1, 48_000, vec![1, -1, i16::MIN], 3));
        let p8 = decode_wave(&wave(Codec::Pcm, 2, false, 1), &[0x80, 0xFF]).unwrap();
        assert_eq!((p8.samples.clone(), p8.frames()), (vec![0, 0x7F00], 1));
        assert_eq!(decode_wave(&wave(Codec::Wma, 2, true, 1), &[0; 4]), None);
    }

    /// A 5.1 clip's three stereo streams interleave into channel order (FL FR, FC LFE, SL SR),
    /// cut to the shortest stream and to the wave's duration.
    #[test]
    fn streams_interleave_in_channel_order() {
        let s0 = (2, vec![1, 2, 11, 12, 21, 22]);
        let s1 = (2, vec![3, 4, 13, 14]);
        let s2 = (2, vec![5, 6, 15, 16, 25, 26]);
        assert_eq!(interleave(&[s0.clone(), s1.clone(), s2.clone()], 0), vec![1, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16]);
        assert_eq!(interleave(&[s0, s1, s2], 1), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(interleave(&[(1, vec![7, 8, 9])], 0), vec![7, 8, 9], "a mono clip is its one stream");
    }

    /// With `LMA_GAME_DIR` and the decoder: MATCH.mng's PCM waves, FEMusic's stereo XMA and a 5.1
    /// crowd bed (three streams) decode to about their stated duration.
    #[cfg(feature = "xma-ffmpeg")]
    #[test]
    fn game_waves_decode() {
        let Some(dir) = std::env::var_os("LMA_GAME_DIR") else { return };
        let dir = std::path::Path::new(&dir).join("Common");
        for (file, check) in [("MatchHi/Sound/Match.mng", 3usize), ("Frontend/Sound/FEMusic.xwb", 1), ("MatchHi/Sound/Crowd/Ambient/AMBLarge.mng", 1)] {
            let d = std::fs::read(dir.join(file)).unwrap();
            for (_, bytes) in lma_data::xact::wave_banks(&d).unwrap() {
                let b = lma_data::xact::WaveBank::parse(&bytes).unwrap();
                for i in 0..check.min(b.waves.len()) {
                    let w = &b.waves[i];
                    let p = decode_wave(w, b.wave_bytes(&bytes, i).unwrap()).unwrap_or_else(|| panic!("{file} wave {i}"));
                    assert_eq!(p.channels, w.format.channels);
                    let (got, want) = (p.frames(), w.duration as usize);
                    assert!(got + 1024 >= want && got <= want, "{file} wave {i}: {got} frames, the bank says {want}");
                }
            }
        }
    }
}
