//! The render backend on its own thread (#95), like the console's GPU next to its CPU.
//!
//! The command processor stays on the guest thread. In real time (asynchronous fences) each
//! draw and present goes to a render thread as a message: the registers that changed (64-word
//! blocks) and the shaders when they changed. The render thread reads guest memory itself
//! (`guest::mem::MemReader`) and writes its resolve copies straight back, as the real GPU does;
//! fence tickets (#73) order those writes for the guest. Guest code and rendering overlap.
//!
//! Deterministic runs and recordings use synchronous fences: then every call runs inline, on
//! the guest thread with the real `Memory`, exactly as without this wrapper.

use crate::backend::{BackendStats, GuestMem, Image, RenderBackend};
use crate::mem::{MemReader, Memory};
use crate::shader_exec::Vec4;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BLOCK: usize = 64;
const RB_MODECONTROL: usize = 0x2208;
const RB_COPY_CONTROL: usize = 0x2318;
const RB_COPY_DEST_BASE: usize = 0x2319;
const RB_COPY_DEST_PITCH: usize = 0x231A;
const FETCH_BASE: usize = 0x4800;

enum Cmd {
    /// Changed register blocks, new shaders, then the draw.
    /// `snap`: copies of the guest memory the draw reads (indices, small vertex streams), taken
    /// when it was sent (LMA_FENCE_FAST), so the game may reuse that memory right away.
    Draw { regs: Vec<(u32, [u32; BLOCK])>, vs: Option<Arc<[u32]>>, ps: Option<Arc<[u32]>>, init: u32, index_base: u32, snap: Vec<(u32, Vec<u8>)>, texcheck: Vec<(u32, u32, u64, String)> },
    Present { regs: Vec<(u32, [u32; BLOCK])>, fb: u32, w: u32, h: u32 },
    /// The command processor stopped (guest code runs next): the backend's `sync`.
    Sync,
}

/// Render-thread time spent on commands (not waiting for them), in ns, and frames it presented:
/// busy / presents is its cost per frame, which a 60 fps frame must keep under ~16 ms.
pub static RENDER_BUSY_NS: AtomicU64 = AtomicU64::new(0);
pub static RENDER_PRESENTS: AtomicU64 = AtomicU64::new(0);
/// The guest frame the game thread is building (LMA_RT_TEXCHECK's log names it).
pub static GUEST_FRAME: AtomicU64 = AtomicU64::new(0);

struct Shared {
    /// Commands the render thread has finished.
    processed: AtomicU64,
    /// Commands whose guest memory writes have all landed.
    landed: AtomicU64,
    /// The newest presented image, for the guest thread to pick up.
    frame: Mutex<Option<Image>>,
    /// Writes to pages the reader can't reach (never allocated by the guest): the guest thread
    /// applies them.
    spill: Mutex<Vec<(u32, Vec<u8>)>>,
}

/// The render thread's view of guest memory.
struct ThreadMem<'a> {
    reader: &'a MemReader,
    spill: &'a Mutex<Vec<(u32, Vec<u8>)>>,
    /// The current draw's snapshots: reads inside one come from it, not from live memory.
    snap: Vec<(u32, Vec<u8>)>,
}

impl GuestMem for ThreadMem<'_> {
    fn read(&mut self, a: u32, out: &mut [u8]) {
        for (base, bytes) in &self.snap {
            if a >= *base && (a - base) as usize + out.len() <= bytes.len() {
                let o = (a - base) as usize;
                out.copy_from_slice(&bytes[o..o + out.len()]);
                return;
            }
        }
        self.reader.read(a, out);
    }
    fn write(&mut self, a: u32, data: &[u8]) {
        if !self.reader.write(a, data) {
            self.spill.lock().unwrap().push((a, data.to_vec()));
        }
    }
    fn write_watch(&mut self, a: u32, len: u32) -> Option<Vec<u32>> {
        // LMA_TEX_WATCH=0 turns it off (A/B, #6)
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var("LMA_TEX_WATCH").as_deref() != Ok("0")) {
            return None;
        }
        self.reader.write_watch(a, len)
    }
}

pub struct ThreadedBackend<B: RenderBackend + 'static> {
    inner: Arc<Mutex<B>>,
    shared: Arc<Shared>,
    tx: Option<mpsc::Sender<Cmd>>,
    thread: Option<std::thread::JoinHandle<()>>,
    name: &'static str,
    /// Commands sent so far.
    sent: u64,
    threaded: bool,
    /// The render thread has the registers (else the next draw sends them all).
    regs_sent: bool,
    /// Register blocks written since the last draw or present (`regs_dirty`).
    dirty: Vec<u64>,
    /// A shader may have changed since the last draw (`shaders_dirty`): only then are the
    /// draw's shader words compared with the ones the render thread has (~2,400 draws a frame
    /// each compared 1-2 KB twice).
    shaders_dirty: bool,
    /// LMA_RT_CHECK=1: `sent_regs` compared with Gpu's registers at every draw.
    check: bool,
    /// The registers as the render thread has them (what was sent).
    sent_regs: Vec<u32>,
    vs: Arc<[u32]>,
    ps: Arc<[u32]>,
    /// The previous present's command: at most one frame is in flight.
    last_present: u64,

    /// A debug capture is running (`start_debug` until `take_debug`).
    debug: bool,
    /// `sent` at the last forwarded `sync`.
    synced: u64,
    /// Resolves still landing: (command, guest physical [lo, hi) they may write).
    resolves: std::collections::VecDeque<(u64, u32, u32)>,
    /// Fast fences (real time only; LMA_FENCE_FAST=0 turns them off): draws carry snapshots of
    /// the memory they read, and fences wait only for resolves (their copies land in guest
    /// memory), not for every draw sent. In a match a GPU interrupt otherwise held the command
    /// processor ~40 ms a frame while the guest spun (#6).
    fast: bool,
    /// Resolve destinations that have landed once: later resolves there don't hold fences
    /// (the game reads a resolve's bytes a frame later at the earliest; Xenia's "fast" readback).
    landed_dests: crate::fasthash::HashSet<u32>,
    /// Vertex fetch constants per vertex shader (by its Arc's address).
    vs_fetch: crate::fasthash::HashMap<usize, Vec<u32>>,
    /// LMA_RT_TEXCHECK=1 (#205): each draw's textures (up to 1 MB) are hashed when it is sent and
    /// again when the render thread draws it; a difference (the game rewrote a texture the render
    /// thread hadn't read yet) is logged. Pixel shaders decoded once each, by their Arc's address.
    texcheck: Option<crate::fasthash::HashMap<usize, crate::shader::Shader>>,
}

/// Streams up to this size are snapshotted (dynamic data: immediate draws, sprites, text);
/// bigger ones are static meshes the game doesn't rewrite between fences.
const SNAP_MAX: u32 = 16 << 10;

impl<B: RenderBackend + 'static> ThreadedBackend<B> {
    pub fn new(inner: B, reader: MemReader) -> Self {
        let name = inner.name();
        let inner = Arc::new(Mutex::new(inner));
        let shared = Arc::new(Shared { processed: AtomicU64::new(0), landed: AtomicU64::new(0), frame: Mutex::new(None), spill: Mutex::new(Vec::new()) });
        let (tx, rx) = mpsc::channel();
        let (inner2, shared2) = (inner.clone(), shared.clone());
        let thread = std::thread::Builder::new().name("render".into()).spawn(move || render_loop(inner2, shared2, reader, rx)).expect("render thread");
        Self { inner, shared, tx: Some(tx), thread: Some(thread), name, sent: 0, threaded: false, regs_sent: false, dirty: vec![0; crate::gpu::REG_BLOCK_WORDS], shaders_dirty: true, check: std::env::var("LMA_RT_CHECK").is_ok_and(|v| v == "1"), sent_regs: Vec::new(), vs: Arc::from([]), ps: Arc::from([]), last_present: 0, debug: false, synced: 0, resolves: Default::default(), fast: std::env::var("LMA_FENCE_FAST").as_deref() != Ok("0"), landed_dests: Default::default(), vs_fetch: Default::default(), texcheck: (std::env::var("LMA_RT_TEXCHECK").as_deref() == Ok("1")).then(Default::default) }
    }

    /// Copies of the guest memory a draw reads: its DMA indices and its vertex streams up to
    /// `SNAP_MAX` bytes each (LMA_FENCE_FAST).
    fn snapshot(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut take = |lo: u32, len: u32, out: &mut Vec<(u32, Vec<u8>)>| {
            if len == 0 || len > SNAP_MAX {
                return;
            }
            let mut b = vec![0u8; len as usize];
            mem.read(lo, &mut b);
            out.push((lo, b));
        };
        if (init >> 6) & 3 == 0 {
            let size = if (init >> 11) & 1 == 1 { 4 } else { 2 };
            take(crate::gpu::phys(index_base), (init >> 16) * size, &mut out);
        }
        let key = self.vs.as_ptr() as usize;
        if !self.vs_fetch.contains_key(&key) {
            let consts = crate::shader::Shader::new(self.vs.to_vec()).vertex_fetch_consts();
            self.vs_fetch.insert(key, consts);
        }
        for &k in &self.vs_fetch[&key] {
            let (d0, d1) = (regs[FETCH_BASE + 2 * k as usize], regs[FETCH_BASE + 2 * k as usize + 1]);
            take(crate::gpu::phys(d0 & !3), ((d1 >> 2) & 0xFF_FFFF) * 4, &mut out);
        }
        out
    }

    /// Wait until the render thread has finished every command sent.
    fn drain(&self) {
        let _p = crate::prof::span(crate::prof::Cat::WaitDrain);
        while self.shared.processed.load(Ordering::Acquire) < self.sent {
            std::thread::yield_now();
        }
    }

    /// Wait until the render thread is idle with nothing left to write (state loads replace the
    /// pages it writes to).
    fn quiesce(&self) {
        self.drain();
        let _p = crate::prof::span(crate::prof::Cat::WaitDrain);
        while self.shared.landed.load(Ordering::Acquire) < self.sent {
            std::thread::yield_now();
        }
    }

    fn apply_spill(&self, mem: &mut dyn GuestMem) {
        let spill = std::mem::take(&mut *self.shared.spill.lock().unwrap());
        for (a, data) in spill {
            mem.write(a, &data);
        }
    }

    /// Run `f` on the backend from this thread, after everything sent before.
    fn inline<R>(&self, f: impl FnOnce(&mut B) -> R) -> R {
        self.drain();
        f(&mut self.inner.lock().unwrap())
    }

    /// The register blocks the render thread doesn't have yet: those Gpu marked as written
    /// (`regs_dirty`) whose values differ from what it was sent (`sent_regs`), or all of them
    /// after an inline stretch. In a match D3D rewrites ~13 blocks a draw, ~4 of them with new
    /// values: copying and sending all of them was a quarter of the command processor. With
    /// LMA_RT_CHECK=1 the whole register file is compared with what was sent.
    fn reg_deltas(&mut self, regs: &[u32]) -> Vec<(u32, [u32; BLOCK])> {
        let block = |i: usize| {
            let now = &regs[i * BLOCK..(i * BLOCK + BLOCK).min(regs.len())];
            let b: [u32; BLOCK] = now.try_into().unwrap_or_else(|_| {
                let mut b = [0; BLOCK];
                b[..now.len()].copy_from_slice(now);
                b
            });
            (i as u32, b)
        };
        let out: Vec<_> = if self.regs_sent {
            // sized once: growing it block by block was ~3.5% of the guest thread in a match
            let mut out = Vec::with_capacity(self.dirty.iter().map(|b| b.count_ones() as usize).sum());
            for (w, bits) in self.dirty.iter().enumerate() {
                let mut bits = *bits;
                while bits != 0 {
                    let i = w * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let at = i * BLOCK;
                    if at < regs.len() {
                        let n = BLOCK.min(regs.len() - at);
                        if self.sent_regs[at..at + n] != regs[at..at + n] {
                            self.sent_regs[at..at + n].copy_from_slice(&regs[at..at + n]);
                            out.push(block(i));
                        }
                    }
                }
            }
            out
        } else {
            self.regs_sent = true;
            self.sent_regs = regs.to_vec();
            (0..regs.len().div_ceil(BLOCK)).map(block).collect()
        };
        self.dirty.iter_mut().for_each(|d| *d = 0);
        if self.check {
            if let Some(i) = (0..regs.len()).find(|&i| self.sent_regs[i] != regs[i]) {
                panic!("LMA_RT_CHECK: register {i:#06x} changed without being marked dirty");
            }
        }
        out
    }

    fn send(&mut self, cmd: Cmd) {
        self.sent += 1;
        self.tx.as_ref().unwrap().send(cmd).expect("render thread stopped");
    }
}

/// The register blocks `deltas` writes, as `RenderBackend::regs_dirty` takes them.
fn dirty_blocks(deltas: &[(u32, [u32; BLOCK])]) -> [u64; crate::gpu::REG_BLOCK_WORDS] {
    let mut bits = [0u64; crate::gpu::REG_BLOCK_WORDS];
    for &(i, _) in deltas {
        bits[i as usize / 64] |= 1 << (i % 64);
    }
    bits
}

fn apply(regs: &mut [u32], deltas: Vec<(u32, [u32; BLOCK])>) {
    for (i, b) in deltas {
        let at = i as usize * BLOCK;
        let n = BLOCK.min(regs.len() - at);
        regs[at..at + n].copy_from_slice(&b[..n]);
    }
}

fn render_loop<B: RenderBackend>(inner: Arc<Mutex<B>>, shared: Arc<Shared>, reader: MemReader, rx: mpsc::Receiver<Cmd>) {
    let mut regs = vec![0u32; crate::gpu::REG_COUNT];
    let (mut vs, mut ps): (Arc<[u32]>, Arc<[u32]>) = (Arc::from([]), Arc::from([]));
    let mut seq = 0u64;
    // (command, backend ticket) still landing, oldest first
    let mut pending: std::collections::VecDeque<(u64, u64)> = Default::default();
    loop {
        let first = if pending.is_empty() { rx.recv().map_err(|_| RecvTimeoutError::Disconnected) } else { rx.recv_timeout(Duration::from_micros(500)) };
        let mut next = match first {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // busy time is counted per command: under load a batch never ends
        let mut busy = std::time::Instant::now();
        let count_busy = |busy: &mut std::time::Instant| {
            let now = std::time::Instant::now();
            let ns = (now - *busy).as_nanos() as u64;
            RENDER_BUSY_NS.fetch_add(ns, Ordering::Relaxed);
            crate::prof::add(crate::prof::Cat::RenderBusy, ns);
            *busy = now;
        };
        crate::zone!("render batch");
        let mut b = inner.lock().unwrap();
        let mut mem = ThreadMem { reader: &reader, spill: &shared.spill, snap: Vec::new() };
        // everything queued, then one check for finished copies
        while let Some(cmd) = next.take() {
            match cmd {
                Cmd::Draw { regs: d, vs: v, ps: p, init, index_base, snap, texcheck } => {
                    crate::prof::count(crate::prof::Cat::RenderDraws);
                    for (lo, hi, h, what) in texcheck {
                        let mut buf = vec![0u8; (hi - lo) as usize];
                        reader.read(lo, &mut buf);
                        if crate::raster::bytes_hash(&buf) != h {
                            eprintln!("LMA_RT_TEXCHECK: frame {} texture {lo:#010x} ({} bytes) changed between send and draw ({what})", RENDER_PRESENTS.load(Ordering::Relaxed), hi - lo);
                        }
                    }
                    b.regs_dirty(&dirty_blocks(&d));
                    apply(&mut regs, d);
                    mem.snap = snap;
                    if let Some(v) = v {
                        vs = v;
                    }
                    if let Some(p) = p {
                        ps = p;
                    }
                    b.draw(&regs, &mut mem, init, index_base, &vs, &ps);
                    mem.snap.clear();
                }
                Cmd::Present { regs: d, fb, w, h } => {
                    crate::frame_mark();
                    RENDER_PRESENTS.fetch_add(1, Ordering::Relaxed);
                    b.regs_dirty(&dirty_blocks(&d));
                    apply(&mut regs, d);
                    if let Some(img) = b.present(&regs, &mut mem, fb, w, h) {
                        *shared.frame.lock().unwrap() = Some(img);
                    }
                }
                Cmd::Sync => b.sync(&mut mem),
            }
            seq += 1;
            let t = b.write_ticket();
            match pending.back_mut() {
                Some(last) if last.1 == t => last.0 = seq,
                _ => pending.push_back((seq, t)),
            }
            shared.processed.store(seq, Ordering::Release);
            count_busy(&mut busy);
            next = rx.try_recv().ok();
        }
        while let Some(&(s, t)) = pending.front() {
            if t != 0 && !b.landed(&mut mem, t) {
                break;
            }
            shared.landed.store(s, Ordering::Release);
            pending.pop_front();
        }
        if pending.is_empty() {
            shared.landed.store(seq, Ordering::Release);
        }
        drop(b);
        count_busy(&mut busy);
    }
}

impl<B: RenderBackend + 'static> Drop for ThreadedBackend<B> {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl<B: RenderBackend + 'static> RenderBackend for ThreadedBackend<B> {
    fn name(&self) -> &'static str {
        self.name
    }

    fn draw(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, vs: &[u32], ps: &[u32]) {
        if !self.threaded {
            return self.inline(|b| b.draw(regs, mem, init, index_base, vs, ps));
        }
        let d = self.reg_deltas(regs);
        let changed = std::mem::take(&mut self.shaders_dirty);
        if self.check && !changed && (*self.vs != *vs || *self.ps != *ps) {
            panic!("LMA_RT_CHECK: a shader changed without shaders_dirty");
        }
        let v = (changed && *self.vs != *vs).then(|| Arc::<[u32]>::from(vs));
        let p = (changed && *self.ps != *ps).then(|| Arc::<[u32]>::from(ps));
        if let Some(v) = &v {
            self.vs = v.clone();
        }
        if let Some(p) = &p {
            self.ps = p.clone();
        }
        let snap = if self.fast { self.snapshot(regs, mem, init, index_base) } else { Vec::new() };
        let mut texcheck = Vec::new();
        if let Some(shaders) = &mut self.texcheck {
            let sh = shaders.entry(self.ps.as_ptr() as usize).or_insert_with(|| crate::shader::Shader::new(ps.to_vec()));
            for k in sh.texture_consts() {
                let b = FETCH_BASE + k as usize * 6;
                let t = crate::raster::TexInfo::from_fetch(&regs[b..b + 6]);
                let Some(&(lo, hi)) = crate::raster::texture_ranges(regs, sh, &(0..32).filter(|&c| c != k).collect::<Vec<_>>()).first() else { continue };
                if hi > lo && hi - lo <= 1 << 20 {
                    let mut buf = vec![0u8; (hi - lo) as usize];
                    mem.read(lo, &mut buf);
                    let what = format!("guest frame {} c{k} {}x{} fmt {} tiled {} ps {:016x}", GUEST_FRAME.load(Ordering::Relaxed), t.width, t.height, t.format, t.tiled, crate::raster::bytes_hash(bytemuck_words(ps)));
                    texcheck.push((lo, hi, crate::raster::bytes_hash(&buf), what));
                }
            }
        }
        self.send(Cmd::Draw { regs: d, vs: v, ps: p, init, index_base, snap, texcheck });
        if regs[RB_MODECONTROL] & 7 == 6 && regs[RB_COPY_CONTROL] >> 20 & 3 != 3 && regs[RB_COPY_DEST_BASE] != 0 {
            // a resolve: remember (generously) where its copy lands, for `complete_range`
            let pitch = (regs[RB_COPY_DEST_PITCH] & 0x3FFF).max(32).next_multiple_of(32);
            let rows = match (regs[RB_COPY_DEST_PITCH] >> 16) & 0x3FFF { 0 => 2048, h => h }.next_multiple_of(32);
            let lo = crate::gpu::phys(regs[RB_COPY_DEST_BASE]);
            self.resolves.push_back((self.sent, lo, lo.saturating_add(pitch * rows * 4)));
        }
    }

    fn present(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) -> Option<Image> {
        if !self.threaded {
            return self.inline(|b| b.present(regs, mem, fb, w, h));
        }
        // the newest finished frame (one frame behind at most); this one is drawn meanwhile
        let p = crate::prof::span(crate::prof::Cat::WaitPresent);
        while self.shared.processed.load(Ordering::Acquire) < self.last_present {
            std::thread::yield_now();
        }
        drop(p);
        let d = self.reg_deltas(regs);
        self.send(Cmd::Present { regs: d, fb, w, h });
        self.last_present = self.sent;
        self.shared.frame.lock().unwrap().take()
    }

    fn start_debug(&mut self) {
        self.debug = true;
        self.inline(|b| b.start_debug())
    }
    fn take_debug(&mut self) -> Option<Vec<String>> {
        // Gpu asks after every draw: answer without waiting for the render thread unless a
        // capture is running
        if !self.debug {
            return None;
        }
        self.debug = false;
        self.inline(|b| b.take_debug())
    }
    fn debugging(&self) -> bool {
        self.debug
    }
    fn take_textures(&mut self) -> Vec<(String, u32, u32, Vec<u8>)> {
        self.inline(|b| b.take_textures())
    }
    fn stats(&self) -> BackendStats {
        self.inner.lock().unwrap().stats()
    }
    fn targets(&self) -> (Vec<Vec4>, Vec<f32>) {
        self.quiesce();
        self.inner.lock().unwrap().targets()
    }
    fn set_targets(&mut self, edram: Vec<Vec4>, depth: Vec<f32>) {
        // a state load: the pages are replaced right after this, and nothing from before it may
        // reach the restored state
        self.quiesce();
        self.shared.spill.lock().unwrap().clear();
        *self.shared.frame.lock().unwrap() = None;
        self.resolves.clear();
        self.inner.lock().unwrap().set_targets(edram, depth)
    }

    fn sync(&mut self, mem: &mut dyn GuestMem) {
        if !self.threaded {
            return self.inline(|b| b.sync(mem));
        }
        self.apply_spill(mem); // copies land on the render thread
        if self.sent != self.synced {
            self.send(Cmd::Sync);
            self.synced = self.sent;
        }
    }

    fn fence(&mut self, mem: &mut dyn GuestMem) {
        self.quiesce();
        self.apply_spill(mem);
        self.inner.lock().unwrap().fence(mem);
    }

    fn complete_range(&mut self, mem: &mut dyn GuestMem, lo: u32, hi: u32) {
        if self.threaded {
            // wait only for the resolves still landing that may write [lo, hi)
            let landed = self.shared.landed.load(Ordering::Acquire);
            while self.resolves.front().is_some_and(|r| r.0 <= landed) {
                let r = self.resolves.pop_front().unwrap();
                self.landed_dests.insert(r.1);
            }
            if let Some(&(seq, _, _)) = self.resolves.iter().rev().find(|r| r.1 < hi && r.2 > lo) {
                let _p = crate::prof::span(crate::prof::Cat::WaitResolve);
                while self.shared.landed.load(Ordering::Acquire) < seq {
                    std::thread::yield_now();
                }
            }
            return self.apply_spill(mem);
        }
        self.quiesce();
        self.apply_spill(mem);
        self.inner.lock().unwrap().complete_range(mem, lo, hi);
    }

    fn set_async_fence(&mut self, on: bool) {
        // Gpu fences before turning them off, so nothing is in flight when going inline
        self.drain();
        self.inner.lock().unwrap().set_async_fence(on);
        self.threaded = on;
        self.regs_sent = false; // the render thread's registers are stale after inline draws
        (self.vs, self.ps) = (Arc::from([]), Arc::from([]));
        self.shaders_dirty = true; // compare again after the switch (a bind meanwhile already set it)
    }

    fn regs_dirty(&mut self, blocks: &[u64]) {
        for (d, b) in self.dirty.iter_mut().zip(blocks) {
            *d |= b;
        }
    }

    fn shaders_dirty(&mut self) {
        self.shaders_dirty = true;
    }

    fn write_ticket(&self) -> u64 {
        if !self.threaded {
            return self.inner.lock().unwrap().write_ticket();
        }
        let landed = self.shared.landed.load(Ordering::Acquire);
        if self.fast {
            return fast_ticket(&self.resolves, &self.landed_dests, self.sent, landed);
        }
        if landed >= self.sent { 0 } else { self.sent }
    }

    fn landed(&mut self, mem: &mut dyn GuestMem, ticket: u64) -> bool {
        if !self.threaded {
            return self.inline(|b| b.landed(mem, ticket));
        }
        self.apply_spill(mem);
        let t = crate::prof::IRQ_TICKET.load(Ordering::Relaxed);
        if t != 0 && self.shared.processed.load(Ordering::Acquire) >= t {
            crate::prof::end(crate::prof::Cat::IrqRender);
        }
        let landed = self.shared.landed.load(Ordering::Acquire);
        while self.resolves.front().is_some_and(|r| r.0 <= landed) {
            let r = self.resolves.pop_front().unwrap();
            self.landed_dests.insert(r.1);
        }
        landed >= ticket
    }
}

/// The fence ticket under fast fences: the resolve a fence must wait for, or 0.
/// - A fence right after a resolve to a small target (no draw between): the game reads that copy
///   back at once. It composes each player's shirt back in one 256x256 scratch target and builds
///   the shirt from it, so with the landed exemption below every shirt got the previous player's
///   (#166). Full-screen targets (end-of-frame copies, ~3,400 fences a match) keep the fast path;
///   this holds a few dozen fences per match.
/// - Otherwise only resolves write guest memory, and only a destination's first one holds fences.
fn fast_ticket(resolves: &std::collections::VecDeque<(u64, u32, u32)>, landed_dests: &crate::fasthash::HashSet<u32>, sent: u64, landed: u64) -> u64 {
    if let Some(&(seq, lo, hi)) = resolves.back() {
        if seq == sent && landed < seq && hi - lo < 0x10_0000 {
            return seq;
        }
    }
    resolves.iter().rev().find(|r| landed < r.0 && !landed_dests.contains(&r.1)).map_or(0, |r| r.0)
}

/// The wgpu backend on a render thread reading `mem` (see the module docs).
pub fn threaded<B: RenderBackend + 'static>(inner: B, mem: &Memory) -> ThreadedBackend<B> {
    ThreadedBackend::new(inner, mem.reader())
}

/// A shader's words as bytes (for hashing).
fn bytemuck_words(w: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding; any byte view of it is valid
    unsafe { std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 4) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A backend that records the shaders each draw arrives with.
    struct Recorder(Arc<Mutex<Vec<(u32, u32)>>>);

    impl RenderBackend for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn draw(&mut self, _regs: &[u32], _mem: &mut dyn GuestMem, _init: u32, _index_base: u32, vs: &[u32], ps: &[u32]) {
            self.0.lock().unwrap().push((vs.first().copied().unwrap_or(0), ps.first().copied().unwrap_or(0)));
        }
        fn present(&mut self, _regs: &[u32], _mem: &mut dyn GuestMem, _fb: u32, _w: u32, _h: u32) -> Option<Image> {
            None
        }
        fn start_debug(&mut self) {}
        fn take_debug(&mut self) -> Option<Vec<String>> {
            None
        }
        fn debugging(&self) -> bool {
            false
        }
        fn take_textures(&mut self) -> Vec<(String, u32, u32, Vec<u8>)> {
            Vec::new()
        }
        fn stats(&self) -> BackendStats {
            BackendStats::default()
        }
        fn targets(&self) -> (Vec<Vec4>, Vec<f32>) {
            (Vec::new(), Vec::new())
        }
        fn set_targets(&mut self, _edram: Vec<Vec4>, _depth: Vec<f32>) {}
    }

    /// What a draw sends of the registers (`reg_deltas`): everything the first time, then only
    /// blocks marked dirty whose values actually changed; marks are cleared after each draw.
    #[test]
    fn register_deltas() {
        let mem = Memory::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut b = ThreadedBackend::new(Recorder(seen), mem.reader());
        let mut regs = vec![0u32; crate::gpu::REG_COUNT];
        let blocks = crate::gpu::REG_COUNT / BLOCK;
        assert_eq!(b.reg_deltas(&regs).len(), blocks, "first draw: the whole register file");
        let mark = |b: &mut ThreadedBackend<Recorder>, block: usize| {
            let mut m = vec![0u64; crate::gpu::REG_BLOCK_WORDS];
            m[block / 64] |= 1 << (block % 64);
            b.regs_dirty(&m);
        };
        mark(&mut b, 3);
        assert!(b.reg_deltas(&regs).is_empty(), "marked but unchanged: nothing sent");
        regs[3 * BLOCK + 5] = 0xABCD;
        mark(&mut b, 3);
        let d = b.reg_deltas(&regs);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0, d[0].1[5]), (3, 0xABCD), "the changed block, whole, with its values");
        assert!(b.reg_deltas(&regs).is_empty(), "marks cleared after a draw");
        // two blocks in different mask words
        regs[70 * BLOCK] = 1;
        regs[200 * BLOCK + 63] = 2;
        mark(&mut b, 70);
        mark(&mut b, 200);
        let d: Vec<u32> = b.reg_deltas(&regs).iter().map(|x| x.0).collect();
        assert_eq!(d, vec![70, 200]);
    }

    /// Shaders reach the render thread when they may have changed (`shaders_dirty`), and again
    /// after a switch between inline and threaded drawing; between those, draws reuse them.
    #[test]
    fn shaders_follow_shaders_dirty() {
        let mut mem = Memory::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut b = ThreadedBackend::new(Recorder(seen.clone()), mem.reader());
        b.set_async_fence(true);
        let regs = vec![0u32; crate::gpu::REG_COUNT];
        let (vs1, ps1, vs2) = ([1u32, 9], [10u32, 9], [2u32, 9]);
        b.draw(&regs, &mut mem, 0, 0, &vs1, &ps1); // first draw: dirty from the start
        b.shaders_dirty();
        b.draw(&regs, &mut mem, 0, 0, &vs2, &ps1); // after a bind: the new vertex shader
        b.draw(&regs, &mut mem, 0, 0, &vs2, &ps1); // unchanged: reused
        // inline drawing meanwhile (deterministic or journaled stretches), with a bind in it: the
        // flag survives inline draws, so the first threaded draw afterwards sends the new shader
        b.set_async_fence(false);
        let vs3 = [3u32, 9];
        b.shaders_dirty();
        b.draw(&regs, &mut mem, 0, 0, &vs3, &ps1); // inline: straight to the backend
        b.set_async_fence(true);
        b.draw(&regs, &mut mem, 0, 0, &vs3, &ps1);
        b.draw(&regs, &mut mem, 0, 0, &vs3, &ps1);
        b.set_async_fence(false); // drains the render thread
        assert_eq!(*seen.lock().unwrap(), vec![(1, 10), (2, 10), (2, 10), (3, 10), (3, 10), (3, 10)]);
    }

    #[test]
    fn fast_fence_tickets() {
        let mut landed_dests = crate::fasthash::HashSet::default();
        landed_dests.insert(0x1000);
        landed_dests.insert(0x80_0000);
        let r = |v: &[(u64, u32, u32)]| v.iter().copied().collect::<std::collections::VecDeque<_>>();
        // the shirt scratch (256x256, landed before): the fence right after it waits for it (#166)
        assert_eq!(fast_ticket(&r(&[(10, 0x1000, 0x41000)]), &landed_dests, 10, 9), 10);
        // a draw sent after the resolve: the landed exemption applies again
        assert_eq!(fast_ticket(&r(&[(10, 0x1000, 0x41000)]), &landed_dests, 11, 9), 0);
        // a full-screen copy (3.6 MB) right before the fence keeps the fast path
        assert_eq!(fast_ticket(&r(&[(10, 0x80_0000, 0xB9_8000)]), &landed_dests, 10, 9), 0);
        // a destination resolved for the first time holds fences until it lands
        assert_eq!(fast_ticket(&r(&[(7, 0x2000, 0x42000), (10, 0x80_0000, 0xB9_8000)]), &landed_dests, 12, 6), 7);
        assert_eq!(fast_ticket(&r(&[(7, 0x2000, 0x42000)]), &landed_dests, 12, 7), 0);
    }

    /// A draw's reads inside one of its snapshots come from the snapshot (the memory as it was when
    /// the draw was sent: the game may have rewritten it since, LMA_FENCE_FAST); anything else,
    /// including a read only partly inside, comes from live memory.
    #[test]
    fn draws_read_their_snapshots() {
        let mut mem = Memory::new();
        let base = 0xA001_0000u32;
        mem.write(base, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let reader = mem.reader();
        let spill = Mutex::new(Vec::new());
        let mut tm = ThreadMem { reader: &reader, spill: &spill, snap: vec![(base, vec![9, 9, 9, 9])] };
        let mut b4 = [0u8; 4];
        tm.read(base, &mut b4);
        assert_eq!(b4, [9, 9, 9, 9], "inside the snapshot");
        let mut b2 = [0u8; 2];
        tm.read(base + 2, &mut b2);
        assert_eq!(b2, [9, 9]);
        tm.read(base + 2, &mut b4);
        assert_eq!(b4, [3, 4, 5, 6], "partly outside: live memory");
        tm.read(base + 4, &mut b4);
        assert_eq!(b4, [5, 6, 7, 8]);
        // writes (resolve copies landing) go to guest memory
        tm.write(base + 4, &[0xAA; 4]);
        tm.snap.clear();
        tm.read(base + 4, &mut b4);
        assert_eq!(b4, [0xAA; 4]);
        assert!(spill.lock().unwrap().is_empty());
    }
}
