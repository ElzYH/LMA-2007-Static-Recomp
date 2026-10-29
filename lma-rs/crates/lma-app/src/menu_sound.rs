//! The launcher's menu sounds (#766 c): the game's own clicks from the player's disc
//! (`Common/MainGame/Sound/Menu.mng`: MENU.xwb + MENU.xsb, twelve short 16-bit PCM waves), played
//! through the native XACT engine (`runtime::xact::Engine`) on a stream of their own, so a click
//! sounds when the key goes down rather than after the game's audio buffer (`audio_out` keeps
//! ~300 ms queued). Nothing is shipped: without the game there is no sound, as before.
//!
//! The sounds follow the launcher's state, not its input code: the selection moving plays
//! `bp_menu_UDLR`, the version changing `bp_tog_LR`, the game starting `bp_menu_select`. Once
//! the game runs, the stream closes (the game has its own sound).

use crate::launcher::Launcher;
use bevy::prelude::*;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use runtime::xact::{Engine, OUT_RATE};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Where a game folder keeps its menu sound bank.
pub fn menu_mng(game_dir: &Path) -> PathBuf {
    game_dir.join("Common").join("MainGame").join("Sound").join("Menu.mng")
}

/// What the launcher did since the last frame, as a cue to play (None: nothing to hear).
pub fn cue_for(before: &(usize, Option<String>, bool), now: &(usize, Option<String>, bool)) -> Option<&'static str> {
    let (sel0, ver0, pending0) = before;
    let (sel, ver, pending) = now;
    if *pending0 && !*pending {
        Some("bp_menu_select")
    } else if !*pending {
        None
    } else if ver != ver0 {
        Some("bp_tog_LR")
    } else if sel != sel0 {
        Some("bp_menu_UDLR")
    } else {
        None
    }
}

/// The engine with Menu.mng's banks loaded, and the sound bank's index; None for a file that
/// isn't the game's menu bank.
pub fn load(mng: &[u8]) -> Option<(Engine, usize)> {
    let archive = lma_data::Archive::parse(mng).ok()?;
    let mut e = Engine::new(None, 4 << 20);
    for (_, bytes) in lma_data::xact::wave_banks(mng).ok()? {
        e.load_wave_bank(bytes).ok()?;
    }
    let xsb = archive.entries.iter().find(|(n, _)| n.to_ascii_lowercase().ends_with(".xsb"))?;
    let sb = e.load_sound_bank(&xsb.1).ok()?;
    Some((e, sb))
}

/// The open stream. cpal's isn't Send on every platform, so it's a non-send resource (`follow`
/// runs on the main thread), dropped with the app's world: not a thread-local, whose drop at exit
/// panicked inside cpal ("thread local panicked on drop", live check 22 Oct).
#[derive(Default)]
pub struct MenuStream(Option<cpal::Stream>);

/// The menu sounds of the game the launcher points at.
#[derive(Resource, Default)]
pub struct MenuSounds {
    tried: Option<PathBuf>,
    engine: Option<(Arc<Mutex<Engine>>, usize)>,
    playing: Vec<u64>,
    /// The game started: the stream closes once the start click has played.
    closing: Option<f32>,
}

/// The mixer's 48 kHz output, linearly resampled to a device's rate (a 44.1 kHz device would
/// otherwise play the clicks 9% sharp).
pub struct Resample {
    /// Mixer frames per device frame.
    step: f64,
    /// Position in `src`, in mixer frames.
    pos: f64,
    /// Mixer frames pulled and not yet passed: `src[0]` is the one at or before `pos`. Frames past
    /// what a call used stay for the next (dropping them skipped a sample at call boundaries).
    src: Vec<[f32; 2]>,
}

impl Resample {
    pub fn new(device_rate: u32) -> Self {
        Self { step: OUT_RATE as f64 / device_rate.max(1) as f64, pos: 0.0, src: vec![[0.0; 2]] }
    }

    /// `out.len()` device frames, pulling what they need from `mix` (which fills mixer frames).
    pub fn fill(&mut self, out: &mut [[f32; 2]], mut mix: impl FnMut(&mut [[f32; 2]])) {
        let end = self.pos + out.len() as f64 * self.step;
        // frames up to index ceil(end) are read (the last as an interpolation partner)
        let need = end.ceil() as usize + 1;
        if need > self.src.len() {
            let have = self.src.len();
            self.src.resize(need, [0.0; 2]);
            mix(&mut self.src[have..]);
        }
        for (i, o) in out.iter_mut().enumerate() {
            let x = self.pos + i as f64 * self.step;
            let (k, t) = (x as usize, (x - x.floor()) as f32);
            let (a, b) = (self.src[k], self.src[k + 1]);
            *o = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        }
        let k = end as usize;
        self.src.drain(..k);
        self.pos = end - k as f64;
    }
}

/// A stream on the default output that mixes `engine` in its callback; None without a device.
fn open(engine: Arc<Mutex<Engine>>) -> Option<cpal::Stream> {
    let device = cpal::default_host().default_output_device()?;
    let config = device.default_output_config().ok().filter(|c| c.sample_format() == cpal::SampleFormat::F32)?.config();
    let channels = config.channels as usize;
    let mut resample = Resample::new(config.sample_rate);
    let mut buf: Vec<[f32; 2]> = Vec::new();
    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                buf.clear();
                buf.resize(data.len() / channels.max(1), [0.0; 2]);
                resample.fill(&mut buf, |src| {
                    // never wait for the main thread here (it holds the engine while it prepares
                    // a cue): a busy engine is one buffer of silence, not a stall of the device
                    if let Ok(e) = engine.try_lock() {
                        e.mixer().mix(src);
                    }
                });
                for (f, s) in data.chunks_mut(channels.max(1)).zip(&buf) {
                    for (c, out) in f.iter_mut().enumerate() {
                        *out = if c < 2 { s[c].clamp(-1.0, 1.0) } else { 0.0 };
                    }
                }
            },
            |e| warn!("menu sound: {e}"),
            None,
        )
        .ok()?;
    stream.play().ok()?;
    Some(stream)
}

/// Loads the game's menu bank when the launcher's game folder changes, plays the cue for what
/// the launcher just did, releases finished cues, and closes the stream after the game starts.
pub fn follow(l: Res<Launcher>, mut s: ResMut<MenuSounds>, mut stream: NonSendMut<MenuStream>, time: Res<Time>, mut last: Local<Option<(usize, Option<String>, bool)>>) {
    let s = &mut *s;
    if let Some(p) = l.pending.as_ref() {
        let dir = Path::new(&p.xex).parent().unwrap_or(Path::new(".")).to_path_buf();
        if s.tried.as_ref() != Some(&dir) && menu_mng(&dir).is_file() {
            s.tried = Some(dir.clone());
            s.engine = std::fs::read(menu_mng(&dir)).ok().and_then(|d| load(&d)).map(|(e, sb)| (Arc::new(Mutex::new(e)), sb));
            match &s.engine {
                Some((e, _)) => match open(e.clone()) {
                    Some(st) => stream.0 = Some(st),
                    None => s.engine = None,
                },
                None => warn!("menu sound: {} isn't the game's menu bank", menu_mng(&dir).display()),
            }
        }
    }
    let now = (l.sel, l.pending.as_ref().and_then(|p| p.mod_id.clone()), l.pending.is_some());
    let before = last.replace(now.clone());
    let Some((engine, sb)) = s.engine.as_ref() else { return };
    let Ok(mut e) = engine.lock() else { return };
    if let Some(cue) = before.as_ref().and_then(|b| cue_for(b, &now)) {
        if let Some(h) = e.cue_index(*sb, cue).and_then(|c| e.prepare(*sb, c)) {
            e.play(h);
            s.playing.push(h);
        }
        if cue == "bp_menu_select" {
            s.closing = Some(time.elapsed_secs() + 1.0);
        }
    }
    s.playing.retain(|&h| {
        let done = e.state(h) != Some(runtime::xact::CueState::Playing);
        if done {
            e.release(h);
        }
        !done
    });
    drop(e);
    if s.closing.is_some_and(|t| time.elapsed_secs() >= t) {
        stream.0 = None;
        *s = MenuSounds { tried: s.tried.take(), ..Default::default() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_changes_to_cues() {
        let at = |sel: usize, ver: Option<&str>, pending: bool| (sel, ver.map(String::from), pending);
        assert_eq!(cue_for(&at(0, None, true), &at(1, None, true)), Some("bp_menu_UDLR"));
        assert_eq!(cue_for(&at(1, None, true), &at(3, Some("fc27"), true)), Some("bp_tog_LR"), "a version change moves the selection too: one click");
        assert_eq!(cue_for(&at(3, None, true), &at(3, None, false)), Some("bp_menu_select"));
        assert_eq!(cue_for(&at(3, None, true), &at(3, None, true)), None);
        assert_eq!(cue_for(&at(3, None, false), &at(4, None, false)), None, "the game is running: silent");
    }

    /// At 48 kHz the mixer's frames pass through (one frame late); at 24 kHz every other one is
    /// taken; a ramp stays a ramp across calls at 44.1 kHz.
    #[test]
    fn resampling_to_the_device() {
        // each mixer frame one higher than the last: a ramp
        fn ramp(n: &mut f32) -> impl FnMut(&mut [[f32; 2]]) + '_ {
            move |src| {
                for s in src {
                    *n += 1.0;
                    *s = [*n, -*n];
                }
            }
        }
        let (mut n, mut r) = (0.0, Resample::new(48_000));
        let mut out = vec![[0.0; 2]; 4];
        r.fill(&mut out, ramp(&mut n));
        assert_eq!(out, vec![[0.0, 0.0], [1.0, -1.0], [2.0, -2.0], [3.0, -3.0]]);
        r.fill(&mut out, ramp(&mut n));
        assert_eq!(out[0], [4.0, -4.0], "continues across calls");

        let (mut n, mut r) = (0.0, Resample::new(24_000));
        r.fill(&mut out, ramp(&mut n));
        assert_eq!(out.iter().map(|s| s[0]).collect::<Vec<_>>(), vec![0.0, 2.0, 4.0, 6.0]);

        let (mut n, mut r) = (0.0, Resample::new(44_100));
        let mut all = Vec::new();
        for _ in 0..5 {
            let mut out = vec![[0.0; 2]; 441];
            r.fill(&mut out, ramp(&mut n));
            all.extend(out.iter().map(|s| s[0]));
        }
        // 2205 device frames cover 2400 mixer frames: the value at device frame i is i * 48/44.1
        for (i, v) in all.iter().enumerate() {
            assert!((v - i as f32 * 48_000.0 / 44_100.0).abs() < 1e-2, "frame {i}: {v}");
        }
    }

    /// A file that isn't the game's menu bank gives no engine (and no sound), not an error.
    #[test]
    fn loads_only_a_menu_bank() {
        assert!(load(b"not an archive").is_none());
    }
}
