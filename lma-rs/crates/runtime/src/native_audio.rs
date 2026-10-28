//! `LMA_NATIVE_AUDIO=1` (#772): the game's sound comes from the host XACT engine
//! (`xact::Engine`, waves from the install's `xact_cache`), not from the guest mixer.
//!
//! Choppy sound was the guest falling behind the real-time audio clock: callbacks where the
//! game's own mixer had produced nothing became 5.3 ms holes (#772). Here every XACT call the
//! game's sound module (0x822e2400..0x822e3600) makes is mirrored to the host engine: bank
//! creation, Play, a cue's Destroy / Stop / Pause, the global variables (the crowd beds'
//! StadDisLow/Mid/High) and the category volumes. Every output frame (`audio::Audio::submit`,
//! also the silent fills) is then a frame of the host mix, so the sound plays at the host's
//! pace whether or not the guest mixer kept up.
//!
//! `LMA_NATIVE_AUDIO=1` keeps the game's own XACT running behind the hooks (muted): its cue
//! objects answer the game's GetState waits (commentary lines). `LMA_NATIVE_AUDIO=2` answers
//! GetState from the host engine for the cues it plays and no longer runs the guest's audio
//! callback: the guest mixer and the XMA hardware model it drives stop running (a frame of host
//! mix goes out on the callback's schedule instead).
//!
//! A state loaded mid-game saved its banks before these hooks saw them: a sound bank first met
//! at Play is found through the `KBDS` data its object points at, and a wave bank the host
//! engine lacks is read from the game files by name (`LMA_AUDIO_LANGUAGE`, default `Data_01`,
//! picks the commentary language's folder).

use crate::xact::Engine;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Engine +0x14: CreateSoundBank (data r4, size r5, the new bank's pointer stored at r8).
pub const CREATE_SOUND_BANK: u32 = 0x8238_E8F8;
/// Engine +0x18: CreateInMemoryWaveBank (data r4, size r5).
pub const CREATE_WAVE_BANK: u32 = 0x8238_EA10;
/// Engine +0x10: a category's volume (category r4, volume f1).
pub const SET_VOLUME: u32 = 0x8238_F728;
/// Engine +0x3c: SetGlobalVariable (index r4, value f1).
pub const SET_GLOBAL: u32 = 0x8238_EFD0;
/// Sound bank +0x8: Play (cue index r4, the new cue's pointer stored at r7 when nonzero).
pub const PLAY: u32 = 0x8239_4FD8;
/// Cue +0xc: Destroy.
pub const CUE_DESTROY: u32 = 0x8239_2B88;
/// Cue +0x14: Stop.
pub const CUE_STOP: u32 = 0x8239_3290;
/// Cue +0x30: Pause (r4 1) / resume (r4 0).
pub const CUE_PAUSE: u32 = 0x8239_2D48;
/// Cue +0x8: GetState (state written to r4): with `LMA_NATIVE_AUDIO=2` answered by the host.
pub const CUE_GET_STATE: u32 = 0x8239_2E80;
pub const HOOKS: [u32; 9] = [CREATE_SOUND_BANK, CREATE_WAVE_BANK, SET_VOLUME, SET_GLOBAL, PLAY, CUE_DESTROY, CUE_STOP, CUE_PAUSE, CUE_GET_STATE];

/// XACT's cue states, as GetState reports them (the game waits on PLAYING).
const XACT_PREPARED: u32 = 0x4;
const XACT_PLAYING: u32 = 0x8;
const XACT_STOPPED: u32 = 0x20;

/// The sound bank file's magic, as the big-endian word the guest holds.
const KBDS: u32 = u32::from_be_bytes(*b"KBDS");

/// `LMA_NATIVE_AUDIO`: 1 mirrors the calls (the guest mixer still runs, muted); 2 also answers
/// the game's GetState natively and no longer runs the guest's audio callback, so neither the
/// guest mixer nor the XMA hardware model runs.
pub fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| std::env::var("LMA_NATIVE_AUDIO").ok().and_then(|v| v.parse().ok()).unwrap_or(0))
}

pub fn enabled() -> bool {
    level() >= 1
}

/// The host engine replaces the guest mixer (`LMA_NATIVE_AUDIO=2`).
pub fn replaces_mixer() -> bool {
    level() >= 2
}

/// lma-app's output device pulls the host mix itself (`start_native`): the callback schedule
/// in `Machine::service_audio` then puts out nothing.
static DEVICE_PULLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_device_pulls(on: bool) {
    DEVICE_PULLS.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn device_pulls() -> bool {
    DEVICE_PULLS.load(std::sync::atomic::Ordering::Relaxed)
}

struct State {
    engine: Engine,
    game_dir: PathBuf,
    language: String,
    /// Guest sound bank object -> the engine's sound bank.
    banks: HashMap<u32, usize>,
    /// Guest cue object -> the engine's instance.
    cues: HashMap<u32, u64>,
    /// Instances played without a cue pointer, released once stopped.
    oneshots: Vec<u64>,
    /// (guest address, instance): where a Play stores its cue, read once the guest has written
    /// it. The guest's Play takes a lock, and a kernel call returns to the dispatcher mid-call,
    /// so the hook's code after the call can run before Play has stored anything.
    pending_cues: Vec<(u32, u64)>,
    /// (guest address, its value before the call, sound bank): where CreateSoundBank stores its
    /// object, read the same way once it changes.
    pending_banks: Vec<(u32, u32, usize)>,
    /// Wave bank name (upper case) -> its file, built when first needed.
    disk: Option<HashMap<String, PathBuf>>,
    plays: u64,
    unknown: u64,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();
static GAME: OnceLock<HashMap<u32, crate::rec::FuncFn>> = OnceLock::new();

/// Start the host engine for the game in `game_dir`: its global settings (categories, variables,
/// curves) and, when the install wrote one, the decoded-audio cache in `<user_dir>/audio`.
pub fn init(game_dir: &Path, user_dir: &Path) {
    let settings = std::fs::read(game_dir.join("Common/MainGame/Sound/LMA2007.xgs")).ok().and_then(|d| lma_data::xact::GlobalSettings::parse(&d).ok());
    if settings.is_none() {
        eprintln!("native audio: no global settings (Common/MainGame/Sound/LMA2007.xgs): no categories or crowd curves");
    }
    let mut engine = Engine::new(settings, 512 << 20);
    let cache = user_dir.join("audio");
    if cache.is_dir() {
        engine.cache_dir = Some(cache);
    }
    let language = std::env::var("LMA_AUDIO_LANGUAGE").unwrap_or_else(|_| "Data_01".into());
    let state = State { engine, game_dir: game_dir.to_path_buf(), language, banks: HashMap::new(), cues: HashMap::new(), oneshots: Vec::new(), pending_cues: Vec::new(), pending_banks: Vec::new(), disk: None, plays: 0, unknown: 0 };
    let _ = STATE.set(Mutex::new(state));
}

/// The hook blocks for `HOOKS`, given the game's blocks there.
pub fn blocks_for(game: HashMap<u32, crate::rec::FuncFn>) -> Vec<(u32, crate::rec::FuncFn)> {
    let addrs: Vec<u32> = game.keys().copied().collect();
    let _ = GAME.set(game);
    addrs.into_iter().map(|a| (a, block as crate::rec::FuncFn)).collect()
}

/// Add the host engine's next `out.len()` frames (stereo, 48 kHz) into `out`.
pub fn mix(out: &mut [[f32; 2]]) {
    if let Some(s) = STATE.get() {
        s.lock().unwrap().engine.mixer.mix(out);
    }
}

/// One line for the run's summary: what the host engine did.
pub fn summary() -> Option<String> {
    let s = STATE.get()?.lock().unwrap();
    Some(format!(
        "native audio: {} plays ({} on banks it couldn't find), {} sound banks known, {} voices now; waves: {} from the install's cache, {} decoded live",
        s.plays,
        s.unknown,
        s.banks.len(),
        s.engine.mixer.voices(),
        s.engine.cache_reads,
        s.engine.live_decodes
    ))
}

fn guest_bytes(m: &mut guest::mem::Memory, at: u32, len: u32) -> Vec<u8> {
    let mut v = vec![0u8; len as usize];
    m.read(at, &mut v);
    v
}

fn guest_pointer(a: u32) -> bool {
    (0x4000_0000..0x8000_0000).contains(&a) || (0xA000_0000..0xE000_0000).contains(&a)
}

impl State {
    /// Map the guest objects that pending Plays and CreateSoundBanks have stored by now.
    fn resolve(&mut self, m: &mut guest::mem::Memory) {
        let (cues, banks) = (&mut self.cues, &mut self.banks);
        self.pending_cues.retain(|&(at, h)| match m.read32(at) {
            0 => true,
            cue => {
                cues.insert(cue, h);
                false
            }
        });
        self.pending_banks.retain(|&(at, before, i)| match m.read32(at) {
            obj if obj == before => true,
            obj => {
                banks.insert(obj, i);
                false
            }
        });
    }

    /// The engine's sound bank for guest bank object `obj`: known from its creation, or found
    /// through a pointer to its `KBDS` data in the object's first 512 bytes.
    fn bank_of(&mut self, m: &mut guest::mem::Memory, obj: u32) -> Option<usize> {
        if let Some(&i) = self.banks.get(&obj) {
            return Some(i);
        }
        let data = (0..512u32).step_by(4).map(|k| m.read32(obj.wrapping_add(k))).collect::<Vec<_>>().into_iter().find(|&p| guest_pointer(p) && m.read32(p) == KBDS)?;
        // the sound bank's own offsets bound what it reads; 1 MB covers the game's (all < 8 KB)
        let i = self.engine.load_sound_bank(&guest_bytes(m, data, 1 << 20)).ok()?;
        self.banks.insert(obj, i);
        Some(i)
    }

    /// Load from the game files the wave banks sound bank `sb` names that the engine lacks.
    fn ensure_waves(&mut self, sb: usize) {
        let missing: Vec<String> = self.engine.wave_bank_names(sb).into_iter().filter(|n| !self.engine.has_wave_bank(n)).collect();
        if missing.is_empty() {
            return;
        }
        if self.disk.is_none() {
            let mut index = HashMap::new();
            for path in crate::xact_cache::sources(&self.game_dir, Some(&self.language)).unwrap_or_default() {
                for bytes in crate::xact_cache::banks_in(&path).unwrap_or_default() {
                    if let Ok(b) = lma_data::xact::WaveBank::parse(&bytes) {
                        index.entry(b.name.to_ascii_uppercase()).or_insert_with(|| path.clone());
                    }
                }
            }
            self.disk = Some(index);
        }
        for name in missing {
            let Some(path) = self.disk.as_ref().and_then(|d| d.get(&name.to_ascii_uppercase())).cloned() else {
                eprintln!("native audio: wave bank {name} isn't in the game files");
                continue;
            };
            for bytes in crate::xact_cache::banks_in(&path).unwrap_or_default() {
                if lma_data::xact::WaveBank::parse(&bytes).is_ok_and(|b| b.name.eq_ignore_ascii_case(&name)) {
                    let _ = self.engine.load_wave_bank(bytes);
                }
            }
        }
    }
}

fn block(c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory, pc: u32, b: &mut i64) -> Result<u32, guest::cpu::Stop> {
    let Some(game) = GAME.get().and_then(|g| g.get(&pc)).copied() else { unreachable!("native_audio::block is only installed through blocks_for") };
    let (r3, r4, r5, r7, r8, f1) = (c.gpr[3] as u32, c.gpr[4] as u32, c.gpr[5] as u32, c.gpr[7] as u32, c.gpr[8] as u32, c.fpr[1] as f32);
    // a cue the host plays: its state is the host's (the guest cue never advances without its mixer)
    if pc == CUE_GET_STATE && replaces_mixer() {
        if let Some(st) = STATE.get() {
            let s = st.lock().unwrap();
            if let Some(&h) = s.cues.get(&r3) {
                let state = match s.engine.state(h) {
                    Some(crate::xact::CueState::Playing) => XACT_PLAYING,
                    Some(crate::xact::CueState::Prepared) => XACT_PREPARED,
                    _ => XACT_STOPPED,
                };
                if r4 != 0 {
                    m.write32(r4, state);
                }
                c.gpr[3] = 0;
                return Ok(c.lr as u32 & !3);
            }
        }
    }
    // CreateSoundBank's slot before the call: the new object is whatever the guest stores there
    let r8_before = if pc == CREATE_SOUND_BANK && r8 != 0 { m.read32(r8) } else { 0 };
    let next = crate::hooks::call_to_the_end(c, m, pc, b, game)?;
    let Some(st) = STATE.get() else { return Ok(next) };
    let mut s = st.lock().unwrap();
    s.resolve(m);
    match pc {
        CREATE_SOUND_BANK => {
            if let Ok(i) = s.engine.load_sound_bank(&guest_bytes(m, r4, r5)) {
                if r8 != 0 {
                    s.pending_banks.push((r8, r8_before, i));
                }
            }
        }
        CREATE_WAVE_BANK => {
            let _ = s.engine.load_wave_bank(guest_bytes(m, r4, r5));
        }
        PLAY => {
            s.plays += 1;
            let Some(sb) = s.bank_of(m, r3) else {
                s.unknown += 1;
                return Ok(next);
            };
            s.ensure_waves(sb);
            if let Some(h) = s.engine.play_cue(sb, r4 as usize) {
                if r7 != 0 {
                    s.pending_cues.retain(|&(at, _)| at != r7);
                    s.pending_cues.push((r7, h));
                } else {
                    s.oneshots.push(h);
                }
            }
            let s = &mut *s;
            let engine = &mut s.engine;
            s.oneshots.retain(|&h| {
                let done = engine.state(h) != Some(crate::xact::CueState::Playing);
                if done {
                    engine.release(h);
                }
                !done
            });
        }
        CUE_DESTROY => {
            if let Some(h) = s.cues.remove(&r3) {
                s.engine.stop(h);
                s.engine.release(h);
            }
        }
        CUE_STOP => {
            if let Some(&h) = s.cues.get(&r3) {
                s.engine.stop(h);
            }
        }
        CUE_PAUSE => {
            if let Some(&h) = s.cues.get(&r3) {
                s.engine.pause(h, r4 != 0);
            }
        }
        SET_GLOBAL => s.engine.set_global(r4 as usize, f1),
        SET_VOLUME => s.engine.mixer.set_category_volume(r4 as u16, f1),
        _ => {}
    }
    Ok(next)
}
