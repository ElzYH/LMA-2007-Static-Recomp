//! LMA_AUDIO=1 (#199): plays the game's mixed stereo (`runtime::audio::Audio::out`, 48 kHz) on the
//! default output device, resampled linearly to the device's rate. It plays ~250 ms behind the
//! game, and steers back there by playing up to 2% slower or faster: a stall (the game stalls
//! 100-280 ms when a cutscene or replay starts) takes sound out of that cushion, and without
//! refilling it the next stall ran dry (#199: the crowd stuttering in cutscenes; 17 underruns in a
//! 7-minute session with a fixed cushion). Running dry it resumes once ~100 ms is queued, and it
//! skips ahead when more than ~500 ms behind (turbo, catch-up). Each of those jumps (cut to silence,
//! back in, skip) is smoothed over `DECLICK` samples instead of stepping (#868: a click after a
//! loading screen).
//!
//! LMA_AUDIO_NULL=1 plays into nothing at the real rate instead (no device, no sound): the same
//! playback logic, so tests count underruns without anyone hearing them.
//!
//! `start_native` (#772, `LMA_NATIVE_AUDIO=2`): the device pulls its sound straight from the host
//! XACT engine (`runtime::native_audio::mix`) at the device's own clock, so no game stall can
//! starve it and no cushion is needed; the game's play / stop calls take effect at the next
//! callback.

use bevy::prelude::{info, warn};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// The player's volume, 0-100 (F2, saved in the config; #766). Read once per output buffer.
pub static VOLUME: AtomicU32 = AtomicU32::new(100);

/// Queued samples (48 kHz) above which playback skips ahead, and where it lands.
const LAG_MAX: usize = 30_000;
const LAG_TARGET: usize = 24_000;
/// Playback starts once this much (~450 ms) is queued, and steers the queue back to it, so the
/// game's usual stalls (cutscene and replay loads, a career-menu load's ~330 ms: #868, which ran
/// the old 250 ms dry) don't reach the speakers.
const PREBUFFER: usize = 21_600;
/// After running dry, playback resumes once this much (~100 ms) is queued: a short gap, and the
/// rate steering rebuilds the rest.
const RESUME: usize = 4_800;
/// Output samples over which a jump in the signal (running dry, resuming, skipping ahead) is
/// smoothed: ~2 ms, short enough to keep the timing and long enough not to click.
const DECLICK: u32 = 96;
/// The most the playback rate deviates to steer the queue (2%: a third of a semitone).
const STEER_MAX: f64 = 0.02;

/// Playing output: keep it alive for as long as sound should play.
pub enum Output {
    /// The device stream, which plays while it exists.
    Device { _stream: cpal::Stream },
    /// LMA_AUDIO_NULL: a thread consuming at the real rate, stopped on drop.
    Null(Arc<AtomicBool>),
}

impl Drop for Output {
    fn drop(&mut self) {
        if let Output::Null(stop) = self {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

/// The playback state between output callbacks.
struct Player {
    out: Arc<Mutex<VecDeque<[f32; 2]>>>,
    underruns: Arc<AtomicU64>,
    /// 48 kHz samples per output sample
    step: f64,
    /// position between the queue's first two samples
    pos: f64,
    /// hearing safety: a sustained, near full-scale signal (a decoding fault is loud noise) is
    /// turned down fast and let back slowly; ordinary game audio stays well below the threshold
    energy: f32,
    gain: f32,
    primed: bool,
    /// playback has started once (it starts at PREBUFFER, resumes at RESUME)
    started: bool,
    /// how far the rate may deviate to steer the queue (STEER_MAX; 0 in tests: the old fixed rate)
    steer: f64,
    /// the player's volume (`VOLUME`; a test's own in tests)
    volume: &'static AtomicU32,
    /// the last sample played (before gain and volume), and the step from it to the signal at the
    /// last jump, faded out over the `declick` samples left
    last: [f32; 2],
    jump: [f32; 2],
    declick: u32,
}

impl Player {
    fn new(out: Arc<Mutex<VecDeque<[f32; 2]>>>, underruns: Arc<AtomicU64>, step: f64) -> Self {
        Self { out, underruns, step, pos: 0.0, energy: 0.0, gain: 1.0, primed: false, started: false, steer: STEER_MAX, volume: &VOLUME, last: [0.0; 2], jump: [0.0; 2], declick: 0 }
    }

    /// Fill one output buffer of `channels`-sample frames.
    fn fill(&mut self, data: &mut [f32], channels: usize) {
        let mut q = self.out.lock().unwrap();
        // more than ~500 ms behind the game (turbo, a long catch-up): skip ahead to ~300 ms
        let mut jumped = false;
        if q.len() > LAG_MAX {
            let skip = q.len() - LAG_TARGET;
            q.drain(..skip);
            jumped = true;
        }
        if !self.primed && q.len() >= if self.started { RESUME } else { PREBUFFER } {
            self.primed = true;
            self.started = true;
            jumped = true;
        }
        // steer towards PREBUFFER queued: slower while below it (refilling the cushion a stall
        // took), faster above it
        let err = (q.len() as f64 - PREBUFFER as f64) / PREBUFFER as f64;
        let step = self.step * (1.0 + (err * 0.05).clamp(-self.steer, self.steer));
        // the player's volume, applied after the safety gain (which measures the game's own level)
        let volume = self.volume.load(Ordering::Relaxed).min(100) as f32 / 100.0;
        for frame in data.chunks_mut(channels) {
            if q.len() < 2 && self.primed {
                self.primed = false;
                self.underruns.fetch_add(1, Ordering::Relaxed);
                jumped = true;
            }
            let s = if self.primed && q.len() >= 2 {
                let (a, b, t) = (q[0], q[1], self.pos as f32);
                self.pos += step;
                while self.pos >= 1.0 && q.len() >= 2 {
                    q.pop_front();
                    self.pos -= 1.0;
                }
                [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
            } else {
                [0.0; 2]
            };
            // a jump starts from the last sample played and fades into the signal
            if std::mem::take(&mut jumped) {
                self.jump = [self.last[0] - s[0], self.last[1] - s[1]];
                self.declick = DECLICK;
            }
            let s = if self.declick > 0 {
                let k = self.declick as f32 / DECLICK as f32;
                self.declick -= 1;
                [s[0] + self.jump[0] * k, s[1] + self.jump[1] * k]
            } else {
                s
            };
            self.last = s;
            // ~300 ms mean square, against -3 dBFS RMS: a fault (sustained full-scale noise),
            // not a loud moment of the game (those are limited at the mix already)
            self.energy += ((s[0] * s[0] + s[1] * s[1]) * 0.5 - self.energy) * (1.0 / 14_400.0);
            let target = if self.energy > 0.5 { 0.1 } else { 1.0 };
            self.gain += (target - self.gain) * if target < self.gain { 1.0 / 480.0 } else { 1.0 / 96_000.0 };
            frame.fill(0.0);
            frame[0] = s[0] * self.gain * volume;
            if channels > 1 {
                frame[1] = s[1] * self.gain * volume;
            }
        }
    }
}

/// Starts the output (the default device, or nothing with LMA_AUDIO_NULL=1).
pub fn start(out: Arc<Mutex<VecDeque<[f32; 2]>>>, underruns: Arc<AtomicU64>) -> Option<Output> {
    open(move |step| {
        let mut player = Player::new(out, underruns, step);
        move |data: &mut [f32], channels: usize| player.fill(data, channels)
    })
}

/// Starts the output with the host XACT engine as its source (#772, `LMA_NATIVE_AUDIO=2`); the
/// game's audio callback schedule then stops taking frames from it.
pub fn start_native() -> Option<Output> {
    let out = open(move |step| {
        let mut player = NativePlayer::new(Box::new(runtime::native_audio::mix), step);
        move |data: &mut [f32], channels: usize| player.fill(data, channels)
    });
    runtime::native_audio::set_device_pulls(out.is_some());
    out
}

/// Where the native player's 48 kHz stereo comes from: adds the next frames into the slice.
type Source = Box<dyn FnMut(&mut [[f32; 2]]) + Send>;

/// Pulls 48 kHz stereo from the native mixer as the device asks for it, resampled linearly to
/// the device's rate (`step` 48 kHz frames per device frame), clamped to full scale.
struct NativePlayer {
    source: Source,
    buf: VecDeque<[f32; 2]>,
    pos: f64,
    step: f64,
    chunk: Vec<[f32; 2]>,
}

impl NativePlayer {
    /// 48 kHz frames mixed per refill (~5 ms).
    const CHUNK: usize = 256;

    fn new(source: Source, step: f64) -> Self {
        Self { source, buf: VecDeque::new(), pos: 0.0, step, chunk: vec![[0.0; 2]; Self::CHUNK] }
    }

    fn fill(&mut self, data: &mut [f32], channels: usize) {
        for frame in data.chunks_mut(channels) {
            while self.buf.len() < 2 {
                self.chunk.iter_mut().for_each(|s| *s = [0.0; 2]);
                (self.source)(&mut self.chunk);
                self.buf.extend(self.chunk.iter().copied());
            }
            let (a, b, t) = (self.buf[0], self.buf[1], self.pos as f32);
            let s = [(a[0] + (b[0] - a[0]) * t).clamp(-1.0, 1.0), (a[1] + (b[1] - a[1]) * t).clamp(-1.0, 1.0)];
            self.pos += self.step;
            while self.pos >= 1.0 {
                self.buf.pop_front();
                self.pos -= 1.0;
            }
            frame.fill(0.0);
            frame[0] = s[0];
            if channels > 1 {
                frame[1] = s[1];
            }
        }
    }
}

/// The default device (or the null output) driving the fill `make` builds for the device's rate.
fn open<F>(make: impl FnOnce(f64) -> F + Send + 'static) -> Option<Output>
where
    F: FnMut(&mut [f32], usize) + Send + 'static,
{
    if std::env::var("LMA_AUDIO_NULL").as_deref() == Ok("1") {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        std::thread::Builder::new()
            .name("audio-null".into())
            .spawn(move || {
                let mut fill = make(1.0);
                let started = std::time::Instant::now();
                let mut played = 0u64;
                let mut buf = Vec::new();
                while !stop2.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    let due = (started.elapsed().as_secs_f64() * runtime::audio::RATE as f64) as u64;
                    buf.resize(2 * (due - played) as usize, 0.0);
                    fill(&mut buf, 2);
                    played = due;
                }
            })
            .ok()?;
        info!("audio: null output (LMA_AUDIO_NULL), 48000 Hz");
        return Some(Output::Null(stop));
    }
    let device = cpal::default_host().default_output_device().or_else(|| {
        warn!("audio: no output device");
        None
    })?;
    let config = match device.default_output_config() {
        Ok(c) if c.sample_format() == cpal::SampleFormat::F32 => c.config(),
        Ok(c) => {
            warn!("audio: output format {:?} unsupported (f32 only)", c.sample_format());
            return None;
        }
        Err(e) => {
            warn!("audio: {e}");
            return None;
        }
    };
    let channels = config.channels as usize;
    let mut fill = make(runtime::audio::RATE as f64 / config.sample_rate as f64);
    let stream = device.build_output_stream(&config, move |data: &mut [f32], _: &cpal::OutputCallbackInfo| fill(data, channels), |e| warn!("audio: {e}"), None);
    let stream = match stream.and_then(|s| s.play().map(|_| s).map_err(|e| cpal::BuildStreamError::BackendSpecific { err: cpal::BackendSpecificError { description: e.to_string() } })) {
        Ok(s) => s,
        Err(e) => {
            warn!("audio: {e}");
            return None;
        }
    };
    info!("audio: {} ch at {} Hz on the default output", channels, config.sample_rate);
    Some(Output::Device { _stream: stream })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The native player plays a cue's samples as the mixer makes them (the output device at
    /// 48 kHz: step 1), keeps pulling across refills, and at 96 kHz plays each sample twice as long.
    #[test]
    fn native_player_pulls_from_the_mixer() {
        let e = runtime::xact::Engine::new(None, 1 << 20);
        let pcm = std::sync::Arc::new(runtime::xact::Pcm { channels: 1, rate: 48_000, samples: (0..600).map(|i| (i * 50) as i16).collect() });
        let g = runtime::xact::volume_gain(180) / 32768.0;
        e.mixer().play(pcm.clone(), 180, 0, 0, None);
        let engine = Arc::new(Mutex::new(e));
        let source = |e: &Arc<Mutex<runtime::xact::Engine>>| -> Source {
            let e = e.clone();
            Box::new(move |out: &mut [[f32; 2]]| e.lock().unwrap().mixer().mix(out))
        };
        let mut p = NativePlayer::new(source(&engine), 1.0);
        let mut out = vec![0f32; 2 * 600];
        p.fill(&mut out, 2);
        for i in [0usize, 1, 255, 256, 300, 599] {
            assert!((out[2 * i] - (i * 50) as f32 * g).abs() < 1e-4, "frame {i}: {}", out[2 * i]);
            assert_eq!(out[2 * i], out[2 * i + 1], "mono on both sides");
        }
        engine.lock().unwrap().mixer().play(pcm, 180, 0, 0, None);
        let mut p = NativePlayer::new(source(&engine), 0.5);
        let mut out = vec![0f32; 8];
        p.fill(&mut out, 1);
        assert!((out[2] - 50.0 * g).abs() < 1e-4 && (out[1] - 25.0 * g).abs() < 1e-4, "{out:?}");
    }

    #[test]
    fn plays_behind_and_counts_running_dry() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let under = Arc::new(AtomicU64::new(0));
        let mut p = Player::new(q.clone(), under.clone(), 1.0);
        let mut buf = vec![0f32; 2 * 480];
        q.lock().unwrap().extend(std::iter::repeat_n([0.25f32, -0.25], PREBUFFER - 1));
        p.fill(&mut buf, 2);
        assert!(buf.iter().all(|&s| s == 0.0), "silent until ~250 ms is queued");
        q.lock().unwrap().push_back([0.25, -0.25]);
        p.fill(&mut buf, 2);
        assert_eq!((buf[0], buf[1]), (0.0, 0.0), "fades in from silence");
        let d = 2 * DECLICK as usize;
        assert_eq!((buf[d], buf[d + 1]), (0.25, -0.25));
        assert_eq!(q.lock().unwrap().len(), PREBUFFER - 480, "at the target: the device's rate");
        // drained: one underrun, then silence until refilled
        let mut big = vec![0f32; 2 * PREBUFFER];
        p.fill(&mut big, 2);
        assert_eq!(under.load(Ordering::Relaxed), 1);
        q.lock().unwrap().clear(); // the last sample is left behind when running dry
        q.lock().unwrap().extend(std::iter::repeat_n([0.5f32, 0.5], RESUME - 1));
        p.fill(&mut buf, 2);
        assert!(buf.iter().all(|&s| s == 0.0), "waits for ~100 ms");
        q.lock().unwrap().push_back([0.5, 0.5]);
        p.fill(&mut buf, 2);
        assert_eq!(buf[2 * DECLICK as usize], 0.5, "resumes at ~100 ms");
        // below the target it plays slower (2%): the queue refills from the game's steady supply
        let before = q.lock().unwrap().len();
        p.fill(&mut buf, 2);
        let used = before - q.lock().unwrap().len();
        assert!((469..=471).contains(&used), "{used} of 480");
        q.lock().unwrap().clear();
        p.fill(&mut buf, 2);
        assert_eq!(under.load(Ordering::Relaxed), 2);
        // far behind: skips ahead to the target
        q.lock().unwrap().clear();
        q.lock().unwrap().extend(std::iter::repeat_n([0.5f32, 0.5], LAG_MAX + 1));
        p.fill(&mut buf, 2);
        // landed at ~300 ms, then a little faster than the device (above the target)
        let left = q.lock().unwrap().len();
        assert!((LAG_TARGET - 490..LAG_TARGET - 480).contains(&left), "{left}");
    }

    /// The player's volume scales what plays, live, from one buffer to the next.
    #[test]
    fn volume_scales_the_output() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let mut p = Player::new(q.clone(), Arc::new(AtomicU64::new(0)), 1.0);
        let volume: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(50)));
        p.volume = volume;
        q.lock().unwrap().extend(std::iter::repeat_n([0.4f32, -0.4], PREBUFFER + 2000));
        let mut buf = vec![0f32; 2 * 480];
        p.fill(&mut buf, 2);
        let d = 2 * DECLICK as usize;
        assert_eq!((buf[d], buf[d + 1]), (0.2, -0.2), "half volume");
        volume.store(0, Ordering::Relaxed);
        p.fill(&mut buf, 2);
        assert!(buf.iter().all(|&s| s == 0.0), "muted");
        volume.store(250, Ordering::Relaxed);
        p.fill(&mut buf, 2);
        assert_eq!(buf[0], 0.4, "never above full");
    }

    /// #868: running dry, resuming and skipping ahead never step the output: from one sample to
    /// the next it moves at most the signal's own step plus 1/DECLICK of the jump.
    #[test]
    fn jumps_are_smoothed() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let mut p = Player::new(q.clone(), Arc::new(AtomicU64::new(0)), 1.0);
        // a slow sine (the signal's own step is small), at 0.8
        let mut phase = 0u32;
        let mut tone = |n: usize| -> Vec<[f32; 2]> {
            (0..n)
                .map(|_| {
                    phase += 1;
                    let v = 0.8 * (phase as f32 * 0.01).sin();
                    [v, -v]
                })
                .collect()
        };
        let mut out = Vec::new();
        let mut buf = vec![0f32; 2 * 480];
        q.lock().unwrap().extend(tone(PREBUFFER + 3000));
        for _ in 0..40 {
            // runs dry after ~30 buffers (the start, then running dry)
            p.fill(&mut buf, 2);
            out.extend_from_slice(&buf);
        }
        q.lock().unwrap().extend(tone(RESUME + 2000)); // resumes mid-wave
        for _ in 0..3 {
            p.fill(&mut buf, 2);
            out.extend_from_slice(&buf);
        }
        let mut far = tone(LAG_MAX + 5000);
        far.iter_mut().skip(1000).for_each(|s| *s = [-s[0], -s[1]]); // a skip lands elsewhere in the wave
        q.lock().unwrap().extend(far);
        for _ in 0..3 {
            p.fill(&mut buf, 2);
            out.extend_from_slice(&buf);
        }
        let worst = out.chunks(2).zip(out.chunks(2).skip(1)).map(|(a, b)| (b[0] - a[0]).abs()).fold(0f32, f32::max);
        assert!(worst <= 0.8 * 0.0101 + 1.7 / DECLICK as f32, "a step of {worst}");
    }

    /// Five minutes of a match where every 20 s a stall costs 150 ms of sound (what a null-output
    /// run showed: the queue stepped down 245 -> 95 -> dry). At a fixed rate the cushion only
    /// shrinks and later stalls run dry; steered, it refills between stalls.
    #[test]
    fn steering_survives_repeated_stalls() {
        let run = |steer: f64| {
            let q = Arc::new(Mutex::new(VecDeque::new()));
            let under = Arc::new(AtomicU64::new(0));
            let mut p = Player::new(q.clone(), under.clone(), 1.0);
            p.steer = steer;
            let mut buf = vec![0f32; 2 * 480];
            let mut lowest = usize::MAX;
            for tick in 0..30_000u32 {
                // 10 ms ticks: the game produces 480 samples, except during a 150 ms stall
                let stalled = tick % 2000 >= 1000 && tick % 2000 < 1015;
                if !stalled {
                    q.lock().unwrap().extend(std::iter::repeat_n([0.1f32, 0.1], 480));
                }
                p.fill(&mut buf, 2);
                if tick > 100 {
                    lowest = lowest.min(q.lock().unwrap().len());
                }
            }
            (under.load(Ordering::Relaxed), lowest)
        };
        let (fixed, _) = run(0.0);
        let (steered, lowest) = run(STEER_MAX);
        assert!(fixed >= 3, "the fixed rate runs dry ({fixed} underruns)");
        assert_eq!(steered, 0, "steered: no underruns");
        assert!(lowest > 4_000, "the cushion never drops below ~80 ms ({lowest})");
    }
}
