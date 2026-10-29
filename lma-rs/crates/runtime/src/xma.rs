//! LMA_AUDIO=1 (#199, step 2): the XMA decoder hardware.
//!
//! The game's sound engine owns up to `XMA_CONTEXTS` decoder contexts: 64-byte records in physical
//! memory (handed out by XMACreateContext) that name two input buffers of 2048-byte XMA packets
//! and an output ring of 256-byte blocks. It starts a context by setting its bit in the kick
//! registers and resets it through the clear registers (`crate::audio::XMA_REGS`). The hardware
//! decodes packets from the current input buffer into the output ring as 16-bit big-endian PCM
//! (interleaved when stereo), moves on to the other buffer when one runs out (clearing its valid
//! bit, which is how the game knows to refill it), and stops when the ring is full or both inputs
//! are spent. Context layout as documented by Xenia (`XMA_CONTEXT_DATA`); bit fields count from
//! the least significant bit of each big-endian word.
//!
//! The packet decoder itself sits behind `Decoder`, so the hardware model is the same whichever
//! one decodes (#199: FFmpeg's or a port).

/// Bytes in an XMA packet, and in an output block.
pub const PACKET: usize = 2048;
/// Frames a loop seam's step is faded out over (2.7 ms at 48 kHz).
const SEAM_FADE: usize = 128;
pub const BLOCK: usize = 256;

/// Decodes one context's packets, in order, into PCM.
pub trait Decoder: Send {
    /// Appends the samples (interleaved when stereo) that `packet` completes to `out`; false if
    /// the decoder rejected the packet.
    fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> bool;
    /// Forgets everything decoded so far (a context cleared, or a jump in the stream).
    fn reset(&mut self);
    /// The stream ended: appends what the decoder still holds back, then starts afresh.
    fn drain(&mut self, out: &mut Vec<i16>) {
        let _ = out;
        self.reset();
    }
}

/// The fields of a context the hardware reads and writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub input_packets: [u32; 2],
    pub input_valid: [bool; 2],
    pub input_ptr: [u32; 2],
    pub current_buffer: usize,
    /// Bits into the current input buffer.
    pub input_read_offset: u32,
    /// The hardware loop: bits into the input buffer, and how many more times (255: forever).
    pub loop_start: u32,
    pub loop_end: u32,
    pub loop_count: u32,
    pub output_blocks: u32,
    pub output_write: u32,
    pub output_read: u32,
    pub output_valid: bool,
    pub output_ptr: u32,
    pub stereo: bool,
    /// 0: 24 kHz, 1: 32 kHz, 2: 44.1 kHz, 3: 48 kHz.
    pub sample_rate: u32,
}

fn bits(w: u32, lo: u32, n: u32) -> u32 {
    (w >> lo) & ((1 << n) - 1)
}

fn set_bits(w: &mut u32, lo: u32, n: u32, v: u32) {
    let m = ((1u32 << n) - 1) << lo;
    *w = (*w & !m) | ((v << lo) & m);
}

impl Context {
    /// From the context's 16 big-endian words.
    pub fn parse(w: &[u32; 16]) -> Self {
        Self {
            input_packets: [bits(w[0], 0, 12), bits(w[1], 0, 12)],
            loop_count: bits(w[0], 12, 8),
            input_valid: [bits(w[0], 20, 1) != 0, bits(w[0], 21, 1) != 0],
            output_blocks: bits(w[0], 22, 5),
            output_write: bits(w[0], 27, 5),
            sample_rate: bits(w[1], 27, 2),
            stereo: bits(w[1], 29, 1) != 0,
            output_valid: bits(w[1], 31, 1) != 0,
            input_read_offset: bits(w[2], 0, 26),
            loop_start: bits(w[3], 0, 26),
            loop_end: bits(w[4], 0, 26),
            current_buffer: bits(w[4], 31, 1) as usize,
            input_ptr: [w[5], w[6]],
            output_ptr: w[7],
            output_read: bits(w[9], 0, 5),
        }
    }

    /// Writes back the fields the hardware changes (the rest of `w` stays the game's).
    pub fn store(&self, w: &mut [u32; 16]) {
        set_bits(&mut w[0], 12, 8, self.loop_count);
        set_bits(&mut w[0], 20, 1, self.input_valid[0] as u32);
        set_bits(&mut w[0], 21, 1, self.input_valid[1] as u32);
        set_bits(&mut w[0], 27, 5, self.output_write);
        set_bits(&mut w[1], 31, 1, self.output_valid as u32);
        set_bits(&mut w[2], 0, 26, self.input_read_offset);
        set_bits(&mut w[4], 31, 1, self.current_buffer as u32);
    }

    /// Bytes per output sample frame (one sample per channel).
    fn frame_bytes(&self) -> usize {
        if self.stereo { 4 } else { 2 }
    }
}

/// One context's hardware state between kicks.
#[derive(Default)]
pub struct Channel {
    pub enabled: bool,
    /// Runs in a row that found no valid input, and whether the decoder was drained since (the
    /// stream ended: what it held back is out, and new input starts a fresh stream).
    pub starved: u8,
    pub drained: bool,
    /// Kicked since the hardware last ran: the real decoder works asynchronously between a kick
    /// and the next lock, so a context kicked and locked again between two runs still gets its turn.
    pub kicked: bool,
    /// Decoded samples not yet written to the output ring.
    pub pending: Vec<i16>,
    /// The last sample frame decoded (one sample per channel), and at a loop's jump the frame the
    /// loop ended on, to smooth the seam with (`smooth_seam`).
    pub last: [i16; 2],
    pub seam: Option<[i16; 2]>,
    /// The seam's step being faded out: per channel, and frames done.
    pub fade: Option<([i32; 2], usize)>,
    pub decoder: Option<Box<dyn Decoder>>,
    /// The (channels, rate) `decoder` was made for: a context the game reuses for another sound
    /// without clearing it gets a new decoder, never one decoding with the wrong layout (#199:
    /// that is loud noise).
    pub format: (u32, u32),
    /// LMA_XMA_DUMP (#199, tests): `<dir>/ctxNNN`; each decoder sequence (a new decoder, a clear,
    /// a loop jump) writes the packets it was fed (`.xma`) and what it decoded (`.pcm`, 16-bit LE)
    /// to `<prefix>_<seq>_<channels>ch_<rate>.*`, so `tools/audio_check.py` can decode the same
    /// packets independently and compare.
    pub dump_to: Option<std::path::PathBuf>,
    pub dump_seq: u32,
    pub dump: Option<Box<(std::fs::File, std::fs::File)>>,
    /// Invariant counters (#199): ring blocks the game read that this context never wrote (a
    /// stale replay: a stutter loop), loop jumps, resets (clears, new decoders) and packets the
    /// decoder rejected. `seen`: the game's read offset and the fresh blocks after the last run.
    pub stale_blocks: u64,
    pub loop_jumps: u64,
    pub resets: u64,
    pub decode_errors: u64,
    /// Runs that found an input or output buffer outside live physical memory (#199).
    pub dead_reads: u64,
    /// Buffer addresses already reported as not live.
    pub dead_seen: std::collections::HashSet<u32>,
    pub seen: Option<(u32, u32)>,
    /// LMA_XMA_LOG: the context's words as last logged (changes are logged).
    pub logged: [u32; 16],
    /// Diagnostics: packets decoded, samples they gave, and the loudest of them.
    pub packets: u64,
    pub samples: u64,
    pub peak: i16,
}

/// The guest memory the hardware works on (physical addresses; `Machine` maps them).
pub trait Mem {
    fn read(&mut self, phys: u32, buf: &mut [u8]);
    fn write(&mut self, phys: u32, buf: &[u8]);
}

impl Channel {
    /// Runs context `ctx` until its output ring is full or its input runs out. Returns whether
    /// anything changed (the context is then written back).
    pub fn work(&mut self, ctx: &mut Context, mem: &mut dyn Mem) -> bool {
        let run = self.enabled || std::mem::take(&mut self.kicked);
        if !run || ctx.output_blocks == 0 {
            return false;
        }
        if self.decoder.is_none() {
            return false;
        }
        // FFmpeg keeps one packet's frames back until the next packet; the hardware doesn't. A
        // context whose input stays empty for two runs has reached the end of its sound: drain
        // the decoder, or its last ~85 ms never come out (and the game waits for them)
        if ctx.input_valid.iter().any(|&v| v) {
            if self.drained {
                self.decoder.as_mut().unwrap().reset();
                self.drained = false;
            }
            self.starved = 0;
        } else {
            self.starved = self.starved.saturating_add(1);
            if self.starved >= 2 && !self.drained {
                let n = self.pending.len();
                self.decoder.as_mut().unwrap().drain(&mut self.pending);
                self.drained = true;
                if self.dump_to.is_some() && self.pending.len() > n {
                    use std::io::Write;
                    let new: Vec<u8> = self.pending[n..].iter().flat_map(|s| s.to_le_bytes()).collect();
                    if let Some(d) = self.dump_files() {
                        let _ = d.1.write_all(&new);
                    }
                }
                self.dump = None; // the sequence ends here
            }
        }
        let before = *ctx;
        let cap = ctx.output_blocks as usize * BLOCK;
        loop {
            // write what's decoded into the ring, up to the game's read offset
            if !self.pending.is_empty() && ctx.output_valid {
                let (r, w) = (ctx.output_read as usize * BLOCK, ctx.output_write as usize * BLOCK);
                let free = if r == w { cap } else if w < r { r - w } else { cap - w + r };
                // whole blocks only, and whole sample frames
                let n = (free.min(self.pending.len() * 2) / BLOCK) * BLOCK;
                let n = n - n % ctx.frame_bytes().max(1);
                if n > 0 {
                    let mut bytes = Vec::with_capacity(n);
                    for s in self.pending.drain(..n / 2) {
                        bytes.extend_from_slice(&s.to_be_bytes());
                    }
                    let first = (cap - w).min(n);
                    mem.write(ctx.output_ptr + w as u32, &bytes[..first]);
                    if first < n {
                        mem.write(ctx.output_ptr, &bytes[first..]);
                    }
                    ctx.output_write = (((w + n) % cap) / BLOCK) as u32;
                    if ctx.output_write == ctx.output_read {
                        ctx.output_valid = false; // full: the game makes it valid again as it reads
                    }
                }
            }
            if self.pending.len() * 2 >= BLOCK || !ctx.output_valid {
                break; // the ring is full (or ran out of room for whole blocks)
            }
            // decode the next packet of the current input buffer
            let b = ctx.current_buffer;
            if !ctx.input_valid[b] {
                break;
            }
            let packet = (ctx.input_read_offset as usize / 8) / PACKET;
            if packet >= ctx.input_packets[b] as usize {
                // this buffer is spent: hand it back and go on with the other one
                ctx.input_valid[b] = false;
                ctx.current_buffer ^= 1;
                ctx.input_read_offset = 32; // after the next buffer's first packet header
                continue;
            }
            let mut buf = [0u8; PACKET];
            mem.read(ctx.input_ptr[b] + (packet * PACKET) as u32, &mut buf);
            let n = self.pending.len();
            if !self.decoder.as_mut().unwrap().decode(&buf, &mut self.pending) {
                self.decode_errors += 1;
            }
            self.smooth_seam(n, if ctx.stereo { 2 } else { 1 });
            if self.dump_to.is_some() {
                use std::io::Write;
                let new: Vec<u8> = self.pending[n..].iter().flat_map(|s| s.to_le_bytes()).collect();
                if let Some(d) = self.dump_files() {
                    let _ = d.0.write_all(&buf);
                    let _ = d.1.write_all(&new);
                }
            }
            self.packets += 1;
            self.samples += (self.pending.len() - n) as u64;
            self.peak = self.pending[n..].iter().fold(self.peak, |p, s| p.max(s.saturating_abs()));
            // a sound of several streams interleaves their packets, one context per stream: the
            // packet header's last byte says how many packets of the others follow this one
            let next = match buf[3] {
                0xFF => ctx.input_packets[b] as usize, // the stream's last packet
                skip => packet + 1 + skip as usize,
            };
            if ctx.loop_count > 0 && ctx.loop_end > ctx.loop_start && next * PACKET * 8 > ctx.loop_end as usize {
                // past the loop's end: back to its start
                ctx.input_read_offset = ctx.loop_start;
                if ctx.loop_count != 255 {
                    ctx.loop_count -= 1;
                }
                // what the decoder holds back is the loop's last ~85 ms: out first, then afresh
                // (a reset dropped it, a short jump each time a loop went round: #868's "spark")
                self.decoder.as_mut().unwrap().drain(&mut self.pending);
                let chs = if ctx.stereo { 2 } else { 1 };
                if self.pending.len() >= chs {
                    self.last[..chs].copy_from_slice(&self.pending[self.pending.len() - chs..]);
                }
                // the decoder starts the loop afresh, without the overlap of the frame before it:
                // its first samples rise from silence, a step (a click) at every seam
                self.seam = Some(self.last);
                self.loop_jumps += 1;
                self.dump = None; // a new sequence: the reference decode restarts here too
            } else {
                ctx.input_read_offset = (next.min(ctx.input_packets[b] as usize) * PACKET * 8 + 32) as u32;
            }
        }
        *ctx != before
    }

    /// A decoder for `format` (channels, rate) from `make`: kept while the context's format stays,
    /// remade (dropping what was decoded for the old one) when it changes.
    pub fn ensure_decoder(&mut self, format: (u32, u32), make: impl FnOnce(u32, u32) -> Option<Box<dyn Decoder>>) {
        if self.decoder.is_none() || self.format != format {
            self.resets += self.decoder.is_some() as u64;
            self.seen = None;
            self.decoder = make(format.0, format.1);
            self.format = format;
            self.pending.clear();
            self.seam = None;
            self.fade = None;
            self.dump = None;
        }
    }

    /// LMA_XMA_DUMP: the open sequence's files, opened on first use.
    fn dump_files(&mut self) -> Option<&mut (std::fs::File, std::fs::File)> {
        if self.dump.is_none() {
            let base = self.dump_to.as_ref()?;
            let name = format!("{}_{:04}_{}ch_{}", base.display(), self.dump_seq, self.format.0, self.format.1);
            self.dump_seq += 1;
            let open = |ext: &str| std::fs::File::create(format!("{name}.{ext}")).ok();
            self.dump = Some(Box::new((open("xma")?, open("pcm")?)));
        }
        self.dump.as_deref_mut()
    }

    /// The clear register: back to the start, nothing pending, and a new decoder at the next
    /// kick (the context may now hold another sound, with other channels or rate).
    /// Before a run: the blocks the game read since the last one that this context never wrote
    /// (counted in `stale_blocks`).
    pub fn check_reads(&mut self, ctx: &Context) -> u32 {
        let (Some((r0, avail)), cap) = (self.seen, ctx.output_blocks) else { return 0 };
        if cap == 0 {
            return 0;
        }
        let stale = ((ctx.output_read + cap - r0) % cap).saturating_sub(avail);
        self.stale_blocks += stale as u64;
        stale
    }

    /// After a run: where the game's reads stand and how many fresh blocks it may read.
    pub fn note_ring(&mut self, ctx: &Context) {
        let cap = ctx.output_blocks.max(1);
        let full = !ctx.output_valid && ctx.output_read == ctx.output_write;
        let avail = if full { cap } else { (ctx.output_write + cap - ctx.output_read) % cap };
        self.seen = Some((ctx.output_read, avail));
    }

    /// After a decode that appended `pending[n..]` (`chs` channels): remembers the last frame, and
    /// after a loop's jump fades the step between the loop's end and its fresh start out over
    /// SEAM_FADE frames (#868: the step was 8x a normal sample's change, heard as a spark).
    fn smooth_seam(&mut self, n: usize, chs: usize) {
        if self.pending.len() < n + chs {
            return;
        }
        if let Some(end) = self.seam.take() {
            let mut d = [0i32; 2];
            for c in 0..chs {
                d[c] = end[c] as i32 - self.pending[n + c] as i32;
            }
            self.fade = Some((d, 0));
        }
        if let Some((d, mut done)) = self.fade {
            for f in self.pending[n..].chunks_exact_mut(chs) {
                if done >= SEAM_FADE {
                    break;
                }
                for c in 0..chs {
                    let v = f[c] as i32 + d[c] * (SEAM_FADE - done) as i32 / SEAM_FADE as i32;
                    f[c] = v.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
                }
                done += 1;
            }
            self.fade = (done < SEAM_FADE).then_some((d, done));
        }
        let len = self.pending.len();
        self.last[..chs].copy_from_slice(&self.pending[len - chs..]);
    }

    pub fn clear(&mut self) {
        self.resets += 1;
        self.seam = None;
        self.fade = None;
        self.seen = None;
        self.enabled = false;
        self.pending.clear();
        self.decoder = None;
        self.dump = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "Decodes" each packet to 300 samples of its first byte.
    struct Fake;
    impl Decoder for Fake {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> bool {
            out.extend(std::iter::repeat_n(packet[0] as i16, 300));
            true
        }
        fn reset(&mut self) {}
    }

    struct Ram(Vec<u8>);
    impl Mem for Ram {
        fn read(&mut self, phys: u32, buf: &mut [u8]) {
            buf.copy_from_slice(&self.0[phys as usize..phys as usize + buf.len()]);
        }
        fn write(&mut self, phys: u32, buf: &[u8]) {
            self.0[phys as usize..phys as usize + buf.len()].copy_from_slice(buf);
        }
    }

    #[test]
    fn context_fields_round_trip() {
        let mut w = [0u32; 16];
        w[0] = 3 | (1 << 20) | (4 << 22) | (2 << 27);
        w[1] = 5 | (3 << 27) | (1 << 29) | (1 << 31);
        w[2] = 32;
        w[4] = 1 << 31;
        (w[5], w[6], w[7], w[9]) = (0x1000, 0x2000, 0x3000, 1);
        let c = Context::parse(&w);
        assert_eq!(c.input_packets, [3, 5]);
        assert_eq!(c.input_valid, [true, false]);
        assert_eq!((c.output_blocks, c.output_write, c.output_read), (4, 2, 1));
        assert!(c.stereo && c.output_valid);
        assert_eq!((c.sample_rate, c.current_buffer, c.input_read_offset), (3, 1, 32));
        assert_eq!((c.input_ptr, c.output_ptr), ([0x1000, 0x2000], 0x3000));
        let mut back = w;
        c.store(&mut back);
        assert_eq!(back, w);
        // what the hardware changes goes back into the game's words
        let d = Context { loop_count: 7, output_write: 3, input_valid: [false, true], input_read_offset: 0x1234, current_buffer: 0, output_valid: false, ..c };
        d.store(&mut back);
        assert_eq!(Context::parse(&back), d);
    }

    #[test]
    fn decodes_into_the_ring_and_hands_back_spent_buffers() {
        let mut ram = Ram(vec![0; 0x10000]);
        // input 0: two packets (first bytes 7, 8) at 0x1000; input 1: one packet (9) at 0x3000
        ram.0[0x1000] = 7;
        ram.0[0x1000 + PACKET] = 8;
        ram.0[0x3000] = 9;
        let mut ctx = Context {
            input_packets: [2, 1],
            input_valid: [true, true],
            input_ptr: [0x1000, 0x3000],
            input_read_offset: 32,
            output_blocks: 8,
            output_valid: true,
            output_ptr: 0x8000,
            ..Default::default()
        };
        let mut ch = Channel { enabled: true, decoder: Some(Box::new(Fake)), ..Default::default() };
        assert!(ch.work(&mut ctx, &mut ram));
        // 900 samples decoded = 1800 bytes: 7 whole blocks written, the rest kept for later
        assert_eq!(ctx.output_write, 7);
        assert_eq!(ch.pending.len(), 900 - 7 * BLOCK / 2);
        assert_eq!((ctx.input_valid, ctx.current_buffer), ([false, false], 0));
        assert_eq!(i16::from_be_bytes([ram.0[0x8000], ram.0[0x8001]]), 7);
        assert_eq!(i16::from_be_bytes([ram.0[0x8000 + 600], ram.0[0x8001 + 600]]), 8);
        assert_eq!(i16::from_be_bytes([ram.0[0x8000 + 1200], ram.0[0x8001 + 1200]]), 9);
        // the game reads 4 blocks and refills input 0: decoding resumes and wraps around the ring
        ctx.output_read = 4;
        ctx.input_valid[0] = true;
        ctx.input_read_offset = 32;
        assert!(ch.work(&mut ctx, &mut ram));
        assert_eq!(ctx.output_write, 3);
        // one block free, but less than a block decoded: kept until the other input is refilled
        assert_eq!(ch.pending.len(), 92);
        assert!(ctx.output_valid);
        assert_eq!(ctx.input_valid, [false, false]);
    }

    #[test]
    fn follows_its_own_stream_and_loops() {
        // packets 0, 2, 4 belong to stream 0 (skip 1 each), 1, 3, 5 to stream 1
        let mut ram = Ram(vec![0; 0x10000]);
        for p in 0..6 {
            ram.0[0x1000 + p * PACKET] = 10 + p as u8;
            ram.0[0x1000 + p * PACKET + 3] = 1;
        }
        let mut ctx = Context {
            input_packets: [6, 0],
            input_valid: [true, false],
            input_ptr: [0x1000, 0],
            input_read_offset: 32,
            // loop packets 0..=2 of the buffer twice more
            loop_start: 32,
            loop_end: (2 * PACKET * 8 + 100) as u32,
            loop_count: 2,
            output_blocks: 31,
            output_valid: true,
            output_ptr: 0x8000,
            ..Default::default()
        };
        let mut ch = Channel { enabled: true, decoder: Some(Box::new(Fake)), ..Default::default() };
        ch.work(&mut ctx, &mut ram);
        // each packet's sample 130 (a seam's fade, SEAM_FADE frames, changes only the first ones)
        let firsts: Vec<i16> = (0..ch.packets as usize).map(|k| i16::from_be_bytes([ram.0[0x8000 + 600 * k + 260], ram.0[0x8001 + 600 * k + 260]])).collect();
        // stream 0 only (10, 12), looped twice back to packet 0, then on to packet 4
        assert_eq!(firsts[..7], [10, 12, 10, 12, 10, 12, 14]);
        assert_eq!(ctx.loop_count, 0);
    }

    /// Holds each packet's samples back until the next packet, as FFmpeg does.
    struct Late(Option<i16>);
    impl Decoder for Late {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> bool {
            if let Some(v) = self.0.replace(packet[0] as i16) {
                out.extend(std::iter::repeat_n(v, 300));
            }
            true
        }
        fn reset(&mut self) {
            self.0 = None;
        }
        fn drain(&mut self, out: &mut Vec<i16>) {
            if let Some(v) = self.0.take() {
                out.extend(std::iter::repeat_n(v, 300));
            }
        }
    }

    /// #868: a loop's jump drains the decoder, so the loop's last packet is heard every time round.
    #[test]
    fn a_loop_keeps_its_last_packet() {
        let mut ram = Ram(vec![0; 0x10000]);
        for p in 0..3 {
            ram.0[0x1000 + p * PACKET] = 10 + p as u8;
        }
        let mut ctx = Context {
            input_packets: [3, 0],
            input_valid: [true, false],
            input_ptr: [0x1000, 0],
            input_read_offset: 32,
            loop_start: 32,
            loop_end: (PACKET * 8 + 100) as u32,
            loop_count: 1,
            output_blocks: 31,
            output_valid: true,
            output_ptr: 0x8000,
            ..Default::default()
        };
        let mut ch = Channel { enabled: true, decoder: Some(Box::new(Late(None))), ..Default::default() };
        ch.work(&mut ctx, &mut ram);
        let firsts: Vec<i16> = (0..4).map(|k| i16::from_be_bytes([ram.0[0x8000 + 600 * k + 260], ram.0[0x8001 + 600 * k + 260]])).collect();
        // packets 0, 1, back to 0, 1, then 2: nothing of the first time round is lost
        assert_eq!(firsts, [10, 11, 10, 11]);
    }

    /// #868: the loop's fresh start is pulled to the level the loop ended on, then fades to its own
    /// samples over SEAM_FADE frames: no step at the seam.
    #[test]
    fn a_loop_seam_has_no_step() {
        struct Level;
        impl Decoder for Level {
            fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> bool {
                out.extend(std::iter::repeat_n(packet[0] as i16 * 100, 300));
                true
            }
            fn reset(&mut self) {}
        }
        let mut ram = Ram(vec![0; 0x10000]);
        ram.0[0x1000] = 1; // the loop starts at level 100
        ram.0[0x1000 + PACKET] = 30; // and ends at 3000
        let mut ctx = Context {
            input_packets: [2, 0],
            input_valid: [true, false],
            input_ptr: [0x1000, 0],
            input_read_offset: 32,
            loop_start: 32,
            loop_end: (PACKET * 8 + 100) as u32,
            loop_count: 1,
            output_blocks: 31,
            output_valid: true,
            output_ptr: 0x8000,
            ..Default::default()
        };
        let mut ch = Channel { enabled: true, decoder: Some(Box::new(Level)), ..Default::default() };
        ch.work(&mut ctx, &mut ram);
        let out: Vec<i16> = (0..1200).map(|k| i16::from_be_bytes([ram.0[0x8000 + 2 * k], ram.0[0x8001 + 2 * k]])).collect();
        assert_eq!((out[0], out[299], out[300], out[599]), (100, 100, 3000, 3000), "the first time round is as decoded");
        // the seam: 3000, then 3000 again (the start pulled up), falling to 100 over SEAM_FADE frames
        assert_eq!(out[600], 3000);
        assert!(out[600..600 + SEAM_FADE].windows(2).all(|w| w[1] <= w[0] && w[0] - w[1] <= 25), "a smooth fall");
        assert_eq!(out[600 + SEAM_FADE], 100);
        assert!(ch.fade.is_none() && ch.seam.is_none());
    }

    #[test]
    fn decoder_is_remade_when_the_format_changes() {
        let mut made = Vec::new();
        let mut ch = Channel::default();
        let mut make = |c, r| {
            made.push((c, r));
            Some(Box::new(Fake) as Box<dyn Decoder>)
        };
        ch.ensure_decoder((1, 48_000), &mut make);
        ch.pending.push(5);
        ch.ensure_decoder((1, 48_000), &mut make); // same sound format: kept
        assert_eq!(ch.pending, [5]);
        ch.ensure_decoder((1, 44_100), &mut make); // another rate: new decoder, nothing carried over
        ch.ensure_decoder((2, 44_100), &mut make); // stereo
        assert!(ch.pending.is_empty());
        assert_eq!(made, [(1, 48_000), (1, 44_100), (2, 44_100)]);
    }

    #[test]
    fn counts_reads_of_blocks_never_written() {
        let mut ch = Channel::default();
        let mut ctx = Context { output_blocks: 8, output_write: 3, output_read: 0, output_valid: true, ..Default::default() };
        ch.note_ring(&ctx); // 3 fresh blocks
        ctx.output_read = 3; // the game read them: fine
        assert_eq!(ch.check_reads(&ctx), 0);
        ch.note_ring(&ctx); // nothing fresh
        ctx.output_read = 5; // the game read 2 more anyway: replayed old audio
        assert_eq!(ch.check_reads(&ctx), 2);
        assert_eq!(ch.stale_blocks, 2);
    }

    #[test]
    fn nothing_happens_without_a_kick() {
        let mut ram = Ram(vec![0; 0x100]);
        let mut ctx = Context { output_blocks: 1, output_valid: true, input_valid: [true, false], input_packets: [1, 0], ..Default::default() };
        let mut ch = Channel { decoder: Some(Box::new(Fake)), ..Default::default() };
        assert!(!ch.work(&mut ctx, &mut ram));
    }
}

/// A decoder for a context of `channels` at `rate` Hz, if this build has one (feature
/// `xma-ffmpeg`); without one, XMA contexts stay silent.
pub fn decoder(channels: u32, rate: u32) -> Option<Box<dyn Decoder>> {
    #[cfg(feature = "xma-ffmpeg")]
    return ffmpeg::Ffmpeg::open(channels, rate).map(|d| Box::new(d) as Box<dyn Decoder>);
    #[cfg(not(feature = "xma-ffmpeg"))]
    {
        let _ = (channels, rate);
        None
    }
}

/// Context sample-rate codes in Hz.
pub fn rate_hz(code: u32) -> u32 {
    [24_000, 32_000, 44_100, 48_000][code as usize & 3]
}

#[cfg(feature = "xma-ffmpeg")]
mod ffmpeg {
    use std::ffi::{c_int, c_short, c_uchar, c_void};

    extern "C" {
        fn lma_xma_open(channels: c_int, rate: c_int) -> *mut c_void;
        fn lma_xma_decode(x: *mut c_void, pkt: *const c_uchar, len: c_int, out: *mut c_short, max: c_int) -> c_int;
        fn lma_xma_reset(x: *mut c_void);
        fn lma_xma_drain(x: *mut c_void, out: *mut c_short, max: c_int) -> c_int;
        fn lma_xma_close(x: *mut c_void);
    }

    /// FFmpeg's xma2 decoder (`xma_ffmpeg.c`).
    pub struct Ffmpeg(*mut c_void);

    // the decoder is used by one thread at a time (it moves with the Machine)
    unsafe impl Send for Ffmpeg {}

    impl Ffmpeg {
        pub fn open(channels: u32, rate: u32) -> Option<Self> {
            let p = unsafe { lma_xma_open(channels as c_int, rate as c_int) };
            (!p.is_null()).then_some(Self(p))
        }
    }

    impl super::Decoder for Ffmpeg {
        fn decode(&mut self, packet: &[u8], out: &mut Vec<i16>) -> bool {
            // a packet's header counts its frames in 6 bits (quiet passages pack 30 and more), each
            // 512 samples per channel; samples past this would be dropped
            let mut buf = vec![0i16; 64 * 512 * 2];
            let n = unsafe { lma_xma_decode(self.0, packet.as_ptr(), packet.len() as c_int, buf.as_mut_ptr(), buf.len() as c_int) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            }
            n >= 0
        }

        fn reset(&mut self) {
            unsafe { lma_xma_reset(self.0) }
        }

        fn drain(&mut self, out: &mut Vec<i16>) {
            let mut buf = vec![0i16; 64 * 512 * 2];
            let n = unsafe { lma_xma_drain(self.0, buf.as_mut_ptr(), buf.len() as c_int) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            }
        }
    }

    impl Drop for Ffmpeg {
        fn drop(&mut self) {
            unsafe { lma_xma_close(self.0) }
        }
    }
}
