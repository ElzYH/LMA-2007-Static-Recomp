//! LMA_AUDIO=1 (#199): the kernel's XAudio render driver (the XMA decoder hardware is `crate::xma`).
//!
//! The game's sound engine registers a render-driver client (a callback and its argument). The
//! console calls it once per audio frame (256 samples at 48 kHz, ~5.33 ms), and the callback mixes
//! one frame and hands it over with XAudioSubmitRenderDriverFrame: 6 channels (FL, FR, C, LFE, BL,
//! BR) x 256 big-endian floats, channel by channel. Here a suspended guest thread is woken with
//! pc = the callback every frame of guest time (deterministic, like the GPU interrupt thread), and
//! submitted frames are mixed down to stereo into `out`, which the host output drains.
//!
//! Off (the default), the kernel calls behave as before (they return 0 and nothing is called
//! back), so runs stay bit-exact.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Return address of the callback: back to the scheduler (`Machine::run_slice`).
pub const AUDIO_MAGIC: u32 = 0xBCBC_BCE0;
/// Samples per frame and per second.
pub const FRAME: usize = 256;
pub const RATE: u64 = 48_000;
/// Driver handle the game gets back (any value; one driver).
pub const DRIVER: u32 = 0x4155_0000;
/// XMA decoder contexts the hardware has.
pub const XMA_CONTEXTS: u32 = 320;
/// The audio hardware's registers (MMIO, little-endian words). 0x1800: the context array's
/// physical address, from which the game numbers contexts; 0x1940 / 0x1A40 / 0x1A80 + 4 * (n / 32):
/// kick / lock / clear, bit n % 32 for context n.
pub const XMA_REGS: u32 = 0x7FEA_0000;
/// At most this many stereo samples wait for the host (~170 ms); older ones are dropped.
const QUEUE_MAX: usize = 32_768; // ~680 ms: room for lma-app's 250-500 ms output buffer
/// The most audio frames (5.3 ms each) the callback catches up after a stall: ~0.5 s, what lma-app's
/// output keeps in hand. Further behind (a pause, loading) it skips to now.
pub const CATCH_UP_MAX: u64 = 96;

pub struct Audio {
    pub enabled: bool,
    /// The registered client: (callback, argument).
    pub client: Option<(u32, u32)>,
    /// The guest thread that runs the callback.
    pub thread: Option<usize>,
    /// Guest time (`Machine::now`) of the next callback.
    pub next_frame: u64,
    /// Stereo samples (left, right) for the host output, oldest first.
    pub out: Arc<Mutex<VecDeque<[f32; 2]>>>,
    /// Frames submitted so far, and callbacks run (tests, status).
    pub frames: u64,
    /// Submitted frames with any sample above -80 dB (diagnostic: is anything audible yet?).
    pub loud: u64,
    /// The loudest sample submitted on each of the six channels (diagnostic).
    pub ch_peak: [f32; 6],
    /// Submitted samples that were NaN or infinite (played as silence).
    pub non_finite: u64,
    /// Times the host output ran dry (counted by lma-app's output callback).
    pub underruns: Arc<std::sync::atomic::AtomicU64>,
    /// LMA_AUDIO_DUMP=<path>: everything submitted, streamed to a WAV while the game runs.
    pub dump: Option<WavStream>,
    /// The downmix's peak limiter gain (5.1 summed into stereo can exceed full scale on loud
    /// moments, e.g. a goal: limited instead of clipped).
    pub limit_gain: f32,
    pub calls: u64,
    /// Audio frames skipped because the game fell too far behind (a stall over `CATCH_UP_MAX`).
    pub dropped: u64,
    /// The most guest ticks an audio callback came late (after a stall).
    pub late_max: u64,
    /// The same since the status line last read it (`take_late_ms`).
    pub late_recent: u64,
    /// More than 100 ms behind at the last callback (a stall being caught up).
    pub catching_up: bool,
    /// `frames` when the last callback started: unchanged at the next one means that callback
    /// submitted nothing.
    pub frames_at_call: u64,
    /// Callbacks that submitted nothing, filled with a frame of silence (`silence_fill`).
    pub silent_fills: u64,
    /// LMA_AUDIO_WAV=<path>: every stereo sample submitted, for lma-cli to write as a WAV file.
    pub capture: Option<Vec<[f32; 2]>>,
    /// XMA contexts: their block in physical memory (allocated at start with audio on; 0 in a
    /// state saved without audio until the first context), and which are in use.
    pub xma_base: u32,
    pub xma_used: Vec<bool>,
    /// The XMA hardware's per-context state (`crate::xma`), and how often a context made progress.
    pub xma_hw: Vec<crate::xma::Channel>,
    pub xma_updates: u64,
    /// LMA_XMA_LOG=1: log XMA register commands and buffer hand-backs in the kernel log.
    pub xma_log: bool,
    /// LMA_XMA_DUMP=<dir> (created at start): where the contexts' decoder dumps go.
    pub xma_dump_dir: Option<std::path::PathBuf>,
}

impl Audio {
    pub fn new() -> Self {
        Self { enabled: std::env::var("LMA_AUDIO").as_deref() == Ok("1"), client: None, thread: None, next_frame: 0, out: Default::default(), frames: 0, loud: 0, ch_peak: [0.0; 6], non_finite: 0, underruns: Default::default(), dropped: 0, late_max: 0, late_recent: 0, catching_up: false, frames_at_call: 0, silent_fills: 0, limit_gain: 1.0, dump: std::env::var_os("LMA_AUDIO_DUMP").and_then(|p| WavStream::create(std::path::Path::new(&p))), calls: 0, capture: std::env::var_os("LMA_AUDIO_WAV").map(|_| Vec::new()), xma_base: 0, xma_used: vec![false; XMA_CONTEXTS as usize], xma_hw: Vec::new(), xma_updates: 0, xma_log: std::env::var("LMA_XMA_LOG").as_deref() == Ok("1"), xma_dump_dir: std::env::var_os("LMA_XMA_DUMP").map(std::path::PathBuf::from).filter(|d| std::fs::create_dir_all(d).is_ok()) }
    }

    /// Guest-time ticks per audio frame for a clock of `hz`.
    pub fn period(hz: u64) -> u64 {
        hz * FRAME as u64 / RATE
    }

    /// Context n's address (in the physical window, as XMACreateContext hands it out).
    pub fn xma_context(&self, n: u32) -> u32 {
        self.xma_base + n * 64
    }

    /// A free XMA context's index, now in use.
    pub fn xma_alloc(&mut self) -> Option<u32> {
        let i = self.xma_used.iter().position(|&u| !u)?;
        self.xma_used[i] = true;
        Some(i as u32)
    }

    pub fn xma_free(&mut self, i: u32) {
        if let Some(u) = self.xma_used.get_mut(i as usize) {
            *u = false;
        }
    }

    /// One submitted frame (`samples`: 6 x 256 big-endian floats, channel by channel), mixed
    /// down to stereo.
    pub fn submit(&mut self, samples: &[u8]) {
        // NaN or infinity plays as silence: a real device turns them into loud noise
        let ch = |c: usize, i: usize| {
            let o = (c * FRAME + i) * 4;
            let v = f32::from_be_bytes([samples[o], samples[o + 1], samples[o + 2], samples[o + 3]]);
            if v.is_finite() { v } else { 0.0 }
        };
        let raw = |c: usize, i: usize| {
            let o = (c * FRAME + i) * 4;
            f32::from_be_bytes([samples[o], samples[o + 1], samples[o + 2], samples[o + 3]])
        };
        const K: f32 = std::f32::consts::FRAC_1_SQRT_2;
        // LMA_NATIVE_AUDIO=1: the frame is the host engine's mix, whatever the guest submitted
        let mut host = [[0f32; 2]; FRAME];
        let native = crate::native_audio::enabled();
        if native {
            crate::native_audio::mix(&mut host);
        }
        let mut out = self.out.lock().unwrap();
        let mut peak = 0f32;
        for c in 0..6 {
            for i in 0..FRAME {
                let v = raw(c, i);
                if v.is_finite() {
                    self.ch_peak[c] = self.ch_peak[c].max(v.abs());
                } else {
                    self.non_finite += 1;
                }
            }
        }
        for i in 0..FRAME {
            let (c, l, r) = (ch(2, i), ch(0, i), ch(1, i));
            let (left, right) = if native { (host[i][0], host[i][1]) } else { (l + K * c + K * ch(4, i), r + K * c + K * ch(5, i)) };
            peak = peak.max(left.abs()).max(right.abs());
            // peak limiter: down at once to stay under the ceiling, back up over ~0.1 s
            const CEILING: f32 = 0.95;
            let p = left.abs().max(right.abs());
            if p * self.limit_gain > CEILING {
                self.limit_gain = CEILING / p;
            }
            let s = [(left * self.limit_gain).clamp(-1.0, 1.0), (right * self.limit_gain).clamp(-1.0, 1.0)];
            self.limit_gain += (1.0 - self.limit_gain) * (1.0 / 4800.0);
            out.push_back(s);
            if let Some(c) = &mut self.capture {
                c.push(s);
            }
            if let Some(d) = &mut self.dump {
                d.push(s);
            }
        }
        self.loud += (peak > 1e-4) as u64;
        while out.len() > QUEUE_MAX {
            out.pop_front();
        }
        self.frames += 1;
    }
}

impl Audio {
    /// A callback that submitted nothing: the hardware plays a frame of silence there and keeps
    /// its pace. Without it the output fell 5.3 ms behind per such callback (about 7% of them in
    /// a match), more than its rate steering makes up, and ran dry (#199, #221).
    pub fn silence_fill(&mut self) {
        self.submit(&[0u8; 6 * FRAME * 4]);
        self.silent_fills += 1;
    }

    /// The invariant counters for the status line and logs: stale ring blocks, loop jumps,
    /// context resets, decoder errors (summed over contexts), output underruns, audio frames
    /// dropped after stalls.
    pub fn errors(&self) -> [u64; 6] {
        let mut e = [0u64; 6];
        for c in &self.xma_hw {
            e[0] += c.stale_blocks;
            e[1] += c.loop_jumps;
            e[2] += c.resets;
            e[3] += c.decode_errors;
        }
        e[4] = self.underruns.load(std::sync::atomic::Ordering::Relaxed);
        e[5] = self.dropped;
        e
    }

    /// The latest an audio callback came since the last call, in ms (status line).
    pub fn take_late_ms(&mut self) -> u64 {
        std::mem::take(&mut self.late_recent) / (crate::threads::CLOCK_HZ / 1000)
    }
}

/// A 16-bit stereo WAV at `RATE` written as samples arrive; the header's sizes are rewritten about
/// once a second, so the file is valid even if the process is killed.
pub struct WavStream {
    file: std::io::BufWriter<std::fs::File>,
    frames: u64,
}

impl WavStream {
    pub fn create(path: &std::path::Path) -> Option<Self> {
        use std::io::Write;
        let mut file = std::io::BufWriter::new(std::fs::File::create(path).ok()?);
        file.write_all(&wav(&[])).ok()?;
        Some(Self { file, frames: 0 })
    }

    pub fn push(&mut self, s: [f32; 2]) {
        use std::io::Write;
        for v in s {
            let _ = self.file.write_all(&((v * 32767.0) as i16).to_le_bytes());
        }
        self.frames += 1;
        if self.frames % RATE == 0 {
            self.patch();
        }
    }

    /// Writes the current sizes into the header.
    fn patch(&mut self) {
        use std::io::{Seek, SeekFrom, Write};
        let data = (self.frames * 4).min(u32::MAX as u64 - 36) as u32;
        let _ = self.file.flush();
        let f = self.file.get_mut();
        let _ = f.seek(SeekFrom::Start(4)).and_then(|_| f.write_all(&(36 + data).to_le_bytes()));
        let _ = f.seek(SeekFrom::Start(40)).and_then(|_| f.write_all(&data.to_le_bytes()));
        let _ = f.seek(SeekFrom::End(0));
    }
}

impl Drop for WavStream {
    fn drop(&mut self) {
        self.patch();
    }
}

/// 16-bit stereo WAV at `RATE` of `samples`.
pub fn wav(samples: &[[f32; 2]]) -> Vec<u8> {
    let data = samples.len() as u32 * 4;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&(RATE as u32).to_le_bytes());
    w.extend_from_slice(&(RATE as u32 * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for s in samples.iter().flatten() {
        w.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
    }
    w
}

impl Default for Audio {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One frame as the guest submits it: 6 channels x FRAME big-endian floats, channel by channel.
    fn frame(ch: [f32; 6]) -> Vec<u8> {
        ch.iter().flat_map(|v| std::iter::repeat_n(v.to_be_bytes(), FRAME)).flatten().collect()
    }

    #[test]
    fn downmix_to_stereo() {
        let mut a = Audio::new();
        a.submit(&frame([0.25, -0.5, 0.2, 0.9, 0.1, 0.0]));
        let out = a.out.lock().unwrap();
        assert_eq!(out.len(), FRAME);
        let k = std::f32::consts::FRAC_1_SQRT_2;
        // LFE (channel 3) is left out; centre and surrounds fold in at -3 dB
        assert_eq!(out[0], [0.25 + k * 0.2 + k * 0.1, -0.5 + k * 0.2]);
        assert_eq!(a.frames, 1);
        assert_eq!(a.loud, 1);
    }

    #[test]
    fn loud_surround_is_limited_not_clipped() {
        let mut a = Audio::new();
        // every channel loud: the stereo sum is ~1.9x full scale
        a.submit(&frame([0.78, 0.78, 0.78, 0.78, 0.78, 0.78]));
        let out = a.out.lock().unwrap();
        assert!(out.iter().all(|s| s[0].abs() <= 0.951 && s[1].abs() <= 0.951));
        assert!(out.iter().all(|s| s[0] > 0.9), "keeps the level near the ceiling");
    }

    #[test]
    fn non_finite_samples_play_as_silence() {
        let mut a = Audio::new();
        a.submit(&frame([f32::NAN, f32::INFINITY, 0.0, 0.0, 0.0, 0.0]));
        assert_eq!(a.out.lock().unwrap()[0], [0.0, 0.0]);
        assert_eq!(a.non_finite, 2 * FRAME as u64);
    }

    #[test]
    fn silence_clamping_and_queue_cap() {
        let mut a = Audio::new();
        a.submit(&frame([0.0; 6]));
        assert_eq!(a.loud, 0);
        a.submit(&frame([4.0, -4.0, 0.0, 0.0, 0.0, 0.0]));
        // limited to the ceiling, not clipped
        let last = *a.out.lock().unwrap().back().unwrap();
        assert!((last[0] - 0.95).abs() < 0.01 && (last[1] + 0.95).abs() < 0.01, "{last:?}");
        for _ in 0..QUEUE_MAX / FRAME + 4 {
            a.submit(&frame([0.0; 6]));
        }
        assert_eq!(a.out.lock().unwrap().len(), QUEUE_MAX);
    }

    #[test]
    fn wav_header_and_samples() {
        let w = wav(&[[0.5, -1.0]]);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 4);
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), RATE as u32);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(i16::from_le_bytes([w[44], w[45]]), 16383);
        assert_eq!(i16::from_le_bytes([w[46], w[47]]), -32767);
    }

    #[test]
    fn xma_contexts_run_out_and_come_back() {
        let mut a = Audio::new();
        for i in 0..XMA_CONTEXTS {
            assert_eq!(a.xma_alloc(), Some(i));
        }
        assert_eq!(a.xma_alloc(), None);
        a.xma_free(7);
        assert_eq!(a.xma_alloc(), Some(7));
    }
}
