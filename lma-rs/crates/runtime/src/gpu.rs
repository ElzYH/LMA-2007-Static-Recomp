//! Xenos command processor (PM4). Consumes the ring buffer the game's D3D runtime writes,
//! keeps the register file, performs memory/fence/scratch writebacks and raises interrupts.
//! Drawing is recorded (counted) here; the wgpu renderer will consume the same state.
//!
//! Semantics follow Xenia's command_processor (BSD-3-Clause).

use std::sync::Arc;
use crate::backend::CpMem;
use std::collections::{HashMap, VecDeque};

const REG_NAMES: &str = include_str!("../data/gpu_regs.txt");

/// Register index -> name (from Xenia's register table).
pub fn reg_name(index: usize) -> String {
    thread_local! {
        static NAMES: HashMap<usize, &'static str> = REG_NAMES
            .lines()
            .filter_map(|l| {
                let (i, n) = l.split_once(' ')?;
                Some((usize::from_str_radix(i.trim_start_matches("0x"), 16).ok()?, n))
            })
            .collect();
    }
    NAMES.with(|m| m.get(&index).map_or(format!("reg_{index:04x}"), |n| n.to_string()))
}

pub const GPU_MMIO_BASE: u32 = 0x7FC8_0000;
pub const REG_COUNT: usize = 0x6000;
/// Words of the changed-register bitmap: one bit per 64-register block.
pub const REG_BLOCK_WORDS: usize = REG_COUNT / 64 / 64;

const CP_RB_RPTR: usize = 0x01C4;
pub(crate) const CP_RB_WPTR: usize = 0x01C5;
const SCRATCH_UMSK: usize = 0x01DC;
const SCRATCH_ADDR: usize = 0x01DD;
const SCRATCH_REG0: usize = 0x0578;
const COHER_STATUS_HOST: usize = 0x0A31;
const RB_SAMPLE_COUNT_ADDR: usize = 0x2325;
const RB_MODECONTROL: usize = 0x2208;
/// Frames a resolve target counts as recurring (see the frame skip in DRAW_INDX).
const RESOLVE_RECENT: u64 = 8;
const RB_COPY_CONTROL: usize = 0x2318;
const RB_COPY_DEST_BASE: usize = 0x2319;
const RB_COPY_DEST_PITCH: usize = 0x231A;

/// A draw of a skipped frame, kept in case the frame must be rendered after all (`Gpu::skipped`).
struct SkippedDraw {
    /// Register blocks changed since the previous logged draw.
    blocks: Vec<(usize, [u32; 64])>,
    init: u32,
    base: u32,
    vs: Arc<Vec<u32>>,
    ps: Arc<Vec<u32>>,
}

/// The skipped frame so far: registers at its first draw, then its draws.
#[derive(Default)]
struct Skipped {
    start: Option<Vec<u32>>,
    draws: Vec<SkippedDraw>,
    /// Register blocks written since the last logged draw.
    dirty: [u64; REG_BLOCK_WORDS],
}
const D1MODE_VBLANK_STATUS: usize = 0x194D;

const PM4_ME_INIT: u32 = 0x48;
const PM4_NOP: u32 = 0x10;
const PM4_INTERRUPT: u32 = 0x54;
const PM4_XE_SWAP: u32 = 0x64;
const PM4_INDIRECT_BUFFER: u32 = 0x3F;
const PM4_INDIRECT_BUFFER_PFD: u32 = 0x37;
const PM4_WAIT_REG_MEM: u32 = 0x3C;
const PM4_REG_RMW: u32 = 0x21;
const PM4_REG_TO_MEM: u32 = 0x3E;
const PM4_MEM_WRITE: u32 = 0x3D;
const PM4_COND_WRITE: u32 = 0x45;
const PM4_EVENT_WRITE: u32 = 0x46;
const PM4_EVENT_WRITE_SHD: u32 = 0x58;
const PM4_EVENT_WRITE_EXT: u32 = 0x5A;
const PM4_EVENT_WRITE_ZPD: u32 = 0x5B;
const PM4_DRAW_INDX: u32 = 0x22;
/// `Gpu::deferred` address of an interrupt raised once its GPU work has landed.
const DEFERRED_INTERRUPT: u32 = u32::MAX;
const PM4_DRAW_INDX_2: u32 = 0x36;
/// Whether the tiled scene is drawn once when `LMA_SINGLE_PASS` is unset (#171): lma-cli sets it
/// before the GPU exists (headless and CI runs: frames on 13 states and the fingerprints are the
/// same with it, and the render thread does 55-75% of the work, #876); lma-app leaves it off until
/// the live window check, as #864 did for the resolve read-backs.
pub static SINGLE_PASS_DEFAULT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `LMA_SINGLE_PASS`'s mode: 0 = the tiled replay as the game issues it, 1 = single pass, 2 = drop
/// the later tiles but leave the first tile's scissor alone (debug). Unset = `default_on` -> 1.
pub fn single_pass_mode_of(v: Option<&str>, default_on: bool) -> u8 {
    match v.map(str::trim) {
        None => u8::from(default_on),
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

const PA_SC_WINDOW_SCISSOR_TL: usize = 0x2081;
const PA_SC_WINDOW_SCISSOR_BR: usize = 0x2082;
const PA_CL_VPORT_YSCALE: usize = 0x2111;
const PA_SC_WINDOW_OFFSET: usize = 0x2080;
const PM4_SET_CONSTANT: u32 = 0x2D;
const PM4_SET_CONSTANT2: u32 = 0x55;
const PM4_LOAD_ALU_CONSTANT: u32 = 0x2F;
const PM4_SET_SHADER_CONSTANTS: u32 = 0x56;
const PM4_IM_LOAD: u32 = 0x27;
const PM4_IM_LOAD_IMMEDIATE: u32 = 0x2B;

/// GPU physical address -> our guest virtual mapping of physical memory.
/// `w` as its little-endian bytes, without a copy (every supported host is little-endian).
pub(crate) fn words(w: &[u32]) -> &[u8] {
    const { assert!(cfg!(target_endian = "little")) };
    // SAFETY: u8 has no alignment or validity requirements; same memory, 4x the length
    unsafe { std::slice::from_raw_parts(w.as_ptr().cast::<u8>(), std::mem::size_of_val(w)) }
}

pub(crate) fn words_mut(w: &mut [u32]) -> &mut [u8] {
    const { assert!(cfg!(target_endian = "little")) };
    // SAFETY: as `words`; every byte pattern is a valid u32
    unsafe { std::slice::from_raw_parts_mut(w.as_mut_ptr().cast::<u8>(), std::mem::size_of_val(w)) }
}

/// `n` big-endian words of guest memory from `addr` into `out`, in one bulk read.
fn read_be(mem: &mut impl CpMem, addr: u32, n: usize, out: &mut Vec<u32>) {
    out.clear();
    out.resize(n, 0);
    mem.read(addr, words_mut(out));
    out.iter_mut().for_each(|w| *w = u32::from_be(*w));
}

pub fn phys(p: u32) -> u32 {
    0xA000_0000 | (p & 0x1FFF_FFFF)
}

/// Register index of SET_CONSTANT / LOAD_ALU_CONSTANT's `offset_type`: kind in bits 16-23
/// (ALU, fetch, bool, loop constants or plain registers), index in bits 0-10.
fn const_base(offset_type: u32) -> usize {
    let base = match (offset_type >> 16) & 0xFF {
        1 => 0x4800, // fetch
        2 => 0x4900, // bool
        3 => 0x4908, // loop
        4 => 0x2000, // registers
        _ => 0x4000, // ALU
    };
    base + (offset_type & 0x7FF) as usize
}

/// Xenos endian swap modes (low 2 bits of most GPU addresses).
fn gpu_swap(v: u32, endian: u32) -> u32 {
    match endian & 3 {
        1 => ((v >> 8) & 0x00FF_00FF) | ((v << 8) & 0xFF00_FF00),
        2 => v.swap_bytes(),
        3 => v.rotate_left(16),
        _ => v,
    }
}

fn match_ref(value: u32, reference: u32, wait_info: u32) -> bool {
    match wait_info & 7 {
        0 => false,
        1 => value < reference,
        2 => value <= reference,
        3 => value == reference,
        4 => value != reference,
        5 => value >= reference,
        6 => value > reference,
        _ => true,
    }
}

/// Frame-time summary of a run of swaps: (frames, average fps, 1% low fps, worst frame ms).
/// The 1% low is the frame rate at the 99th-percentile frame time.
pub fn frame_stats(swaps: &[std::time::Instant]) -> Option<(usize, f64, f64, f64)> {
    let mut dt: Vec<f64> = swaps.windows(2).map(|w| (w[1] - w[0]).as_secs_f64()).collect();
    if dt.is_empty() {
        return None;
    }
    let total: f64 = dt.iter().sum();
    dt.sort_by(|a, b| a.total_cmp(b));
    let p99 = dt[((dt.len() as f64 * 0.99).ceil() as usize).clamp(1, dt.len()) - 1];
    Some((dt.len(), dt.len() as f64 / total, 1.0 / p99, dt[dt.len() - 1] * 1000.0))
}

#[derive(Default, Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub packets: u64,
    pub draws: u64,
    pub frames: u64,
    pub ib_calls: u64,
    pub stalls: u64,
}

/// Command processor and EDRAM state for a save state.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct GpuState {
    regs: Vec<u32>,
    rb: (u32, u32, u32, u32),
    interrupt: (u32, u32),
    pending_interrupts: VecDeque<(u32, u32)>,
    counter: u32,
    stats: Stats,
    last_swap: Option<(u32, u32, u32)>,
    bins: (u64, u64),
    ib_stack: Vec<Reader>,
    pending_ib: Option<Reader>,
    blocked: bool,
    vs: Option<(u32, Vec<u32>)>,
    ps: Option<(u32, Vec<u32>)>,
    last_draw_regs: Vec<u32>,
    edram: Vec<[f32; 4]>,
    depth: Vec<f32>,
}

pub struct Gpu {
    pub regs: Vec<u32>,
    /// LMA_SEAM_LOG=2 (#526, the row-384 seam): per frame, a hash of each draw's vertex streams in the
    /// first tile pass, compared with the same draw in the second; a swap line names the frame when
    /// any differ (the game rewrote a buffer between the passes: the lower tile drew newer geometry).
    seam: Option<SeamLog>,
    pub rb_base: u32,
    pub rb_size: u32, // bytes
    pub rptr: u32,    // dword index
    pub writeback: u32,
    pub interrupt_cb: u32,
    pub interrupt_user: u32,
    pub pending_interrupts: VecDeque<(u32, u32)>, // (source, cpu)
    pub counter: u32,
    pub stats: Stats,
    /// Front buffer of the last XE_SWAP: (physical address, width, height).
    pub last_swap: Option<(u32, u32, u32)>,
    pub log: Vec<String>,
    history: VecDeque<(u32, u32, u32)>, // (address, header, depth)
    dumped: bool,
    bin_mask: u64,
    bin_select: u64,
    /// LMA_SINGLE_PASS=1 (#171): render the tiled scene once. The game replays one command buffer
    /// per EDRAM band (window offset moved, scissor clipped to the band, every band resolved in
    /// each pass); the first tile (window offset 0) is kept, with its band scissor widened to the
    /// viewport, and the later tiles' predicated draws, clears and resolves are dropped
    /// (`later_tile`). Every other packet still runs, so fences and events are untouched.
    single_pass: bool,
    /// `LMA_SINGLE_PASS=2` (debug): drop the later tiles but leave the first tile's scissor alone.
    single_pass_widen: bool,
    /// Predicated draws of later tile passes dropped so far (not in `Stats`: that struct is part of
    /// every save state, and a field added to it makes older states unreadable).
    pub pass_dropped: u64,
    /// LMA_SINGLE_PASS_LOG: the (scissor, viewport, window offset, select) tuples logged so far.
    pass_seen: Vec<(u32, u32, u32, u32, u32, u64)>,
    ib_stack: Vec<Reader>,
    pending_ib: Option<Reader>,
    /// Waiting for the guest interrupt handler before continuing the command stream.
    pub blocked: bool,
    /// Shader microcode bound by IM_LOAD / IM_LOAD_IMMEDIATE: (guest address or 0, dwords).
    /// The bound shaders: (guest address, code), shared with each draw instead of copied.
    pub vs: Option<(u32, Arc<Vec<u32>>)>,
    /// PM4_IM_LOAD's shader words, read in one go (reused).
    im_words: Vec<u32>,
    /// Bulk register writes: the words (reused) and their big-endian MMIO mirror.
    cp_words: Vec<u32>,
    cp_be: Vec<u32>,
    pub ps: Option<(u32, Arc<Vec<u32>>)>,
    /// Frame-capture: frame number to record, recorded lines, register snapshot at the last draw.
    pub capture_frame: Option<u64>,
    pub capture: Vec<String>,
    pub captured_shaders: Vec<(String, Vec<u32>)>,
    last_draw_regs: Vec<u32>,
    /// Where draws, resolves and presents go (software reference by default).
    pub renderer: Box<dyn crate::backend::RenderBackend>,
    /// Render draws in software (slow; on by default).
    pub render: bool,
    /// Rasterise only every Nth frame (frame skip); the guest still runs every frame.
    pub render_every: u64,
    /// Frame numbers whose front buffer should be kept, and the kept frames.
    pub dump_frames: Vec<u64>,
    pub frames_out: Vec<(u64, u32, u32, Vec<u8>)>,
    /// Present every frame into `latest_frame` (for a live window).
    pub present_all: bool,
    /// With present_all: convert only every Nth frame (the front buffer read is costly).
    pub present_every: u64,
    pub latest_frame: Option<(u64, u32, u32, Vec<u8>)>,
    /// Host time of every frame swap since the last `take`, for frame-time statistics.
    pub swap_times: Vec<std::time::Instant>,
    /// Asynchronous fences: fence writes wait (in order) for the GPU writes before them instead
    /// of the command processor blocking on them, as on the console. On in real time, off when
    /// the machine is deterministic (fence timing would depend on the host GPU); LMA_ASYNC_FENCE=0/1
    /// overrides. The machine sets `realtime` before every pump.
    pub realtime: bool,
    /// The ring's write pointer as published by the game thread when it started this run on the
    /// command-processor thread (`crate::cp_thread`); None: read it from guest memory.
    pub wptr_snapshot: Option<u32>,
    async_fence_env: Option<bool>,
    async_fence: bool,
    /// Asynchronous fences: (ticket, address, value) writes that land once the GPU work
    /// submitted before them has; address `DEFERRED_INTERRUPT` raises interrupt mask `value`.
    deferred: std::collections::VecDeque<(u64, u32, u32)>,
    /// Register blocks changed since the last draw or present (see `mark`).
    reg_dirty: [u64; REG_BLOCK_WORDS],
    /// Frame skip: the current skipped frame's draws, and whether this frame renders after all.
    skipped: Skipped,
    forced: bool,
    /// Each resolve target (dest, pitch): (first frame of its current run, last frame seen),
    /// rendered or skipped. A run continues while the gaps are at most 2 frames (double-buffered
    /// targets alternate).
    resolve_seen: HashMap<(u32, u32), (u64, u64)>,
    /// Write every distinct shader binary here (`LMA_SHADER_DUMP`).
    pub shader_dump: Option<std::path::PathBuf>,
}

impl Default for Gpu {
    fn default() -> Self {
        Self {
            regs: vec![0; REG_COUNT],
            seam: SeamLog::from_env(),
            rb_base: 0,
            rb_size: 0,
            rptr: 0,
            writeback: 0,
            interrupt_cb: 0,
            interrupt_user: 0,
            pending_interrupts: VecDeque::new(),
            counter: 0,
            stats: Stats::default(),
            last_swap: None,
            log: Vec::new(),
            history: VecDeque::new(),
            dumped: false,
            bin_mask: 0xFFFF_FFFF,
            bin_select: 0xFFFF_FFFF,
            single_pass: single_pass_mode_of(std::env::var("LMA_SINGLE_PASS").ok().as_deref(), SINGLE_PASS_DEFAULT.load(std::sync::atomic::Ordering::Relaxed)) != 0,
            single_pass_widen: single_pass_mode_of(std::env::var("LMA_SINGLE_PASS").ok().as_deref(), SINGLE_PASS_DEFAULT.load(std::sync::atomic::Ordering::Relaxed)) == 1,
            pass_dropped: 0,
            pass_seen: Vec::new(),
            ib_stack: Vec::new(),
            pending_ib: None,
            blocked: false,
            vs: None,
            im_words: Vec::new(),
            cp_words: Vec::new(),
            cp_be: Vec::new(),
            ps: None,
            capture_frame: None,
            capture: Vec::new(),
            captured_shaders: Vec::new(),
            last_draw_regs: vec![0; REG_COUNT],
            renderer: Box::new(crate::raster::Renderer::default()),
            render: true,
            dump_frames: Vec::new(),
            frames_out: Vec::new(),
            present_all: false,
            present_every: 4,
            render_every: 1,
            latest_frame: None,
            swap_times: Vec::new(),
            realtime: false,
            wptr_snapshot: None,
            async_fence_env: std::env::var("LMA_ASYNC_FENCE").ok().map(|v| v != "0"),
            async_fence: false,
            deferred: std::collections::VecDeque::new(),
            reg_dirty: [!0; REG_BLOCK_WORDS],
            skipped: Skipped::default(),
            forced: false,
            resolve_seen: HashMap::new(),
            shader_dump: std::env::var_os("LMA_SHADER_DUMP").map(Into::into),
        }
    }
}

/// Reads dwords from either the ring (wrapping) or a linear indirect buffer.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Reader {
    base: u32,
    size_dw: u32, // 0 = linear
    pos: u32,
    end: u32,
}

impl Reader {
    fn read(&mut self, mem: &mut impl CpMem) -> u32 {
        let idx = if self.size_dw != 0 { self.pos % self.size_dw } else { self.pos };
        self.pos += 1;
        mem.read32(self.base + idx * 4)
    }
    fn left(&self) -> u32 {
        self.end.saturating_sub(self.pos)
    }
    /// The next `n` words into `out`, in one bulk read unless they wrap around the ring.
    fn read_n(&mut self, mem: &mut impl CpMem, n: u32, out: &mut Vec<u32>) {
        let idx = if self.size_dw != 0 { self.pos % self.size_dw } else { self.pos };
        if self.size_dw == 0 || idx + n <= self.size_dw {
            read_be(mem, self.base + idx * 4, n as usize, out);
            self.pos += n;
        } else {
            out.clear();
            out.extend((0..n).map(|_| self.read(mem)));
        }
    }
}

enum Step {
    Ok,
    Stall,
    /// PM4_INTERRUPT: stop until the CPU-side handler has run (it is synchronous on hardware).
    Interrupt,
}

impl Gpu {
    pub fn save_state(&self) -> GpuState {
        let (edram, depth) = self.renderer.targets();
        GpuState {
            regs: self.regs.clone(),
            rb: (self.rb_base, self.rb_size, self.rptr, self.writeback),
            interrupt: (self.interrupt_cb, self.interrupt_user),
            pending_interrupts: self.pending_interrupts.clone(),
            counter: self.counter,
            stats: self.stats.clone(),
            last_swap: self.last_swap,
            bins: (self.bin_mask, self.bin_select),
            ib_stack: self.ib_stack.clone(),
            pending_ib: self.pending_ib.clone(),
            blocked: self.blocked,
            vs: self.vs.as_ref().map(|(a, c)| (*a, c.to_vec())),
            ps: self.ps.as_ref().map(|(a, c)| (*a, c.to_vec())),
            last_draw_regs: self.last_draw_regs.clone(),
            edram,
            depth,
        }
    }

    pub fn load_state(&mut self, s: GpuState) {
        self.regs = s.regs;
        self.reg_dirty = [!0; REG_BLOCK_WORDS];
        (self.rb_base, self.rb_size, self.rptr, self.writeback) = s.rb;
        (self.interrupt_cb, self.interrupt_user) = s.interrupt;
        self.pending_interrupts = s.pending_interrupts;
        self.counter = s.counter;
        self.stats = s.stats;
        self.last_swap = s.last_swap;
        (self.bin_mask, self.bin_select) = s.bins;
        self.ib_stack = s.ib_stack;
        self.pending_ib = s.pending_ib;
        self.blocked = s.blocked;
        // fence writes and interrupts still waiting belong to the timeline being replaced (a
        // state is saved after a full fence, so it has none): delivered now, an interrupt found
        // its data word as the restored memory has it (D3D's 0x0BADF00D placeholder, an assert)
        self.deferred.clear();
        self.vs = s.vs.map(|(a, c)| (a, Arc::new(c)));
        self.ps = s.ps.map(|(a, c)| (a, Arc::new(c)));
        self.renderer.shaders_dirty();
        self.last_draw_regs = s.last_draw_regs;
        self.renderer.set_targets(s.edram, s.depth);
    }

    fn store(&self, mem: &mut impl CpMem, addr: u32, value: u32) {
        let endian = addr & 3;
        let a = addr & !3;
        // Physical, like `load` and Xenia's TranslatePhysical: the guest reads it back at the same
        // address (0xC000_0000 + x translates to physical x too). Writes inside the CP writeback
        // window (0xC000_0000, 1 MB, set by D3D) were once redirected to 0x7F00_0000, which is
        // where the thread PCRs live: the one at boot overwrote the main thread's KTHREAD pointer,
        // giving it ThreadId 0 and letting it past D3D's device-ownership check (#60).
        // Xenia stores host-order (little-endian) after GpuSwap
        mem.write(phys(a), &gpu_swap(value, endian).to_le_bytes());
    }

    fn load(&self, mem: &mut impl CpMem, addr: u32) -> u32 {
        let mut b = [0u8; 4];
        mem.read(phys(addr & !3), &mut b);
        gpu_swap(u32::from_le_bytes(b), addr & 3)
    }

    /// Register `index` changed: its 64-register block goes to the backend with the next draw
    /// (`RenderBackend::regs_dirty`; the render thread copies only those).
    fn mark(&mut self, index: usize) {
        self.reg_dirty[index / 4096] |= 1 << ((index / 64) % 64);
        self.skipped.dirty[index / 4096] |= 1 << ((index / 64) % 64);
    }

    /// The draw about to run is a resolve into memory: its target (dest, pitch).
    fn resolve_target(&self) -> Option<(u32, u32)> {
        let r = &self.regs;
        (r[RB_MODECONTROL] & 7 == 6 && (r[RB_COPY_CONTROL] >> 20) & 3 != 3 && r[RB_COPY_DEST_BASE] != 0).then(|| (r[RB_COPY_DEST_BASE], r[RB_COPY_DEST_PITCH]))
    }

    /// Keep a skipped frame's draw: the register blocks changed since the previous one.
    fn log_skipped(&mut self, init: u32, base: u32, vs: Arc<Vec<u32>>, ps: Arc<Vec<u32>>) {
        let sk = &mut self.skipped;
        let mut blocks = Vec::new();
        if sk.start.is_none() {
            sk.start = Some(self.regs.clone());
        } else {
            for (w, bits) in sk.dirty.iter().enumerate() {
                let mut bits = *bits;
                while bits != 0 {
                    let i = w * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let mut blk = [0; 64];
                    blk.copy_from_slice(&self.regs[i * 64..i * 64 + 64]);
                    blocks.push((i, blk));
                }
            }
        }
        sk.dirty = [0; REG_BLOCK_WORDS];
        sk.draws.push(SkippedDraw { blocks, init, base, vs, ps });
    }

    /// Render the skipped frame's draws so far, with the registers each had.
    fn replay_skipped(&mut self, mem: &mut impl CpMem) {
        let sk = std::mem::take(&mut self.skipped);
        let Some(mut regs) = sk.start else { return };
        let all = [!0u64; REG_BLOCK_WORDS];
        for (n, d) in sk.draws.into_iter().enumerate() {
            let mut mask = [0u64; REG_BLOCK_WORDS];
            for (i, blk) in d.blocks {
                regs[i * 64..i * 64 + 64].copy_from_slice(&blk);
                mask[i / 64] |= 1 << (i % 64);
            }
            self.renderer.regs_dirty(if n == 0 { &all } else { &mask });
            self.renderer.shaders_dirty();
            self.renderer.draw(&regs, mem, d.init, d.base, &d.vs[..], &d.ps[..]);
        }
        self.reg_dirty = all; // the live registers differ from the replayed ones
        self.renderer.shaders_dirty(); // and so may the live shaders
    }

    /// Hand the backend the register blocks changed since the last draw or present.
    fn flush_dirty(&mut self) {
        self.renderer.regs_dirty(&self.reg_dirty);
        self.reg_dirty = [0; REG_BLOCK_WORDS];
    }

    pub fn write_register(&mut self, mem: &mut impl CpMem, index: usize, value: u32) {
        if index >= REG_COUNT {
            return;
        }
        self.regs[index] = value;
        self.mark(index);
        // the CPU reads GPU registers through MMIO; keep that view in sync
        mem.write32(GPU_MMIO_BASE + index as u32 * 4, value);
        if (SCRATCH_REG0..SCRATCH_REG0 + 8).contains(&index) {
            let n = index - SCRATCH_REG0;
            if (1 << n) & self.regs[SCRATCH_UMSK] != 0 {
                let addr = self.regs[SCRATCH_ADDR] + n as u32 * 4;
                mem.write32(phys(addr), value); // store_and_swap -> big-endian
            }
        } else if index == COHER_STATUS_HOST {
            self.regs[index] |= 0x8000_0000;
        }
    }

    /// `write_register` on consecutive registers from `base`, in bulk: SET_CONSTANT and
    /// LOAD_ALU_CONSTANT write hundreds a draw, and doing it register by register (dirty marks,
    /// MMIO mirror word by word) was a fifth of the command processor in a match.
    fn write_registers(&mut self, mem: &mut impl CpMem, base: usize, values: &[u32]) {
        let end = (base + values.len()).min(REG_COUNT);
        if base >= end {
            return;
        }
        let values = &values[..end - base];
        // registers with side effects keep the one-by-one path
        if (base < SCRATCH_REG0 + 8 && end > SCRATCH_REG0) || (base..end).contains(&COHER_STATUS_HOST) {
            for (i, &v) in values.iter().enumerate() {
                self.write_register(mem, base + i, v);
            }
            return;
        }
        self.regs[base..end].copy_from_slice(values);
        for block in base / 64..=(end - 1) / 64 {
            self.mark(block * 64);
        }
        self.cp_be.clear();
        self.cp_be.extend(values.iter().map(|v| v.to_be()));
        mem.write(GPU_MMIO_BASE + base as u32 * 4, words(&self.cp_be));
    }

    /// The next `n` words of `r` into consecutive registers from `base`. In one bulk read and
    /// three passes when they don't wrap around the ring and touch no register with side
    /// effects: the packet's bytes go to the MMIO mirror as they are (the mirror holds them
    /// big-endian, as the guest wrote them) and byte-swapped into the register file, instead of
    /// reading, swapping, copying, swapping back and writing (#6: the command processor's
    /// copies were ~5% of the game thread in a match).
    fn write_registers_from(&mut self, mem: &mut impl CpMem, r: &mut Reader, base: usize, n: u32) {
        let end = base + n as usize;
        let idx = if r.size_dw != 0 { r.pos % r.size_dw } else { r.pos };
        let linear = r.size_dw == 0 || idx + n <= r.size_dw;
        let side = (base < SCRATCH_REG0 + 8 && end > SCRATCH_REG0) || (base..end).contains(&COHER_STATUS_HOST);
        if linear && !side && base < end && end <= REG_COUNT {
            let mut raw = std::mem::take(&mut self.cp_be);
            raw.clear();
            raw.resize(n as usize, 0);
            mem.read(r.base + idx * 4, words_mut(&mut raw));
            r.pos += n;
            for (d, &w) in self.regs[base..end].iter_mut().zip(&raw) {
                *d = u32::from_be(w);
            }
            for block in base / 64..=(end - 1) / 64 {
                self.mark(block * 64);
            }
            mem.write(GPU_MMIO_BASE + base as u32 * 4, words(&raw));
            self.cp_be = raw;
            return;
        }
        let mut words = std::mem::take(&mut self.cp_words);
        r.read_n(mem, n, &mut words);
        self.write_registers(mem, base, &words);
        self.cp_words = words;
    }

    /// Registers whose MMIO reads are fixed (as in Xenia's GraphicsSystem::ReadRegister).
    fn refresh_fixed_registers(mem: &mut impl CpMem) {
        for (reg, v) in [(0x0F00u32, 0x0810_0748u32), (0x0F01, 0x0000_200E), (0x1951, 1), (0x1961, 0x0500_02D0)] {
            mem.write32(GPU_MMIO_BASE + reg * 4, v);
        }
    }

    /// Run the command stream (indirect buffers first, then the ring up to the write pointer).
    /// Make every GPU write to guest memory submitted so far visible, fence writes included
    /// (e.g. before a save state).
    pub fn fence(&mut self, mem: &mut impl CpMem) {
        self.renderer.fence(mem);
        while let Some((_, addr, v)) = self.deferred.pop_front() {
            self.land(mem, addr, v);
        }
    }

    /// A deferred fence write or interrupt, now that the GPU work before it has landed.
    fn land(&mut self, mem: &mut impl CpMem, addr: u32, v: u32) {
        if addr == DEFERRED_INTERRUPT {
            crate::prof::end(crate::prof::Cat::IrqRender);
            crate::prof::end(crate::prof::Cat::IrqDefer);
            crate::prof::begin(crate::prof::Cat::IrqQueue);
            self.blocked = true; // until its handler returns (a vsync handler may have unblocked)
            for n in 0..6 {
                if v & (1 << n) != 0 {
                    self.pending_interrupts.push_back((1, n));
                }
            }
        } else {
            self.store(mem, addr, v);
        }
    }

    /// The value at `addr` as the command processor sees it now: the newest fence write still
    /// deferred to it, else memory once any copy still in flight to it has landed.
    fn load_ordered(&mut self, mem: &mut impl CpMem, addr: u32) -> u32 {
        if let Some(&(_, a, v)) = self.deferred.iter().rev().find(|&&(_, a, _)| a != DEFERRED_INTERRUPT && phys(a & !3) == phys(addr & !3)) {
            return gpu_swap(gpu_swap(v, a & 3), addr & 3);
        }
        let p = phys(addr & !3);
        self.renderer.complete_range(mem, p, p + 4);
        self.load(mem, addr)
    }

    /// A fence write: with asynchronous fences it lands after the GPU writes submitted before it.
    fn fence_store(&mut self, mem: &mut impl CpMem, addr: u32, v: u32) {
        if self.async_fence {
            let ticket = self.renderer.write_ticket();
            if ticket != 0 || !self.deferred.is_empty() {
                let ticket = ticket.max(self.deferred.back().map_or(0, |d| d.0));
                if addr == DEFERRED_INTERRUPT {
                    crate::prof::begin(crate::prof::Cat::IrqDefer);
                    crate::prof::begin(crate::prof::Cat::IrqRender);
                    crate::prof::IRQ_TICKET.store(ticket, std::sync::atomic::Ordering::Relaxed);
                }
                self.deferred.push_back((ticket, addr, v));
                return;
            }
        }
        self.land(mem, addr, v);
    }

    /// Debug (LMA_FENCE_NAP_LOG): the deferred fence queue and the renderer's last readback.
    pub fn fence_debug(&self) -> String {
        format!("deferred={} front={:?} ticket={} blocked={} irq={} rptr={:x} wptr={:x} wb={:x} ibs={}", self.deferred.len(), self.deferred.front(), self.renderer.write_ticket(), self.blocked, self.pending_interrupts.len(), self.rptr, self.regs[CP_RB_WPTR], self.writeback, self.ib_stack.len())
    }

    /// Apply the deferred fence writes whose GPU work has finished (never blocks).
    fn drain_fences(&mut self, mem: &mut impl CpMem) {
        while let Some(&(ticket, addr, v)) = self.deferred.front() {
            if !self.renderer.landed(mem, ticket) {
                break;
            }
            self.deferred.pop_front();
            self.land(mem, addr, v);
        }
    }

    pub fn pump(&mut self, mem: &mut impl CpMem) {
        crate::zone!("gpu pump");
        let _p = crate::prof::span(crate::prof::Cat::Cp);
        crate::prof::count(crate::prof::Cat::CpRuns);
        let on = self.async_fence_env.unwrap_or(self.realtime);
        if on != self.async_fence {
            if !on {
                self.fence(mem); // leaving async mode: nothing may stay deferred
            }
            self.async_fence = on;
            self.renderer.set_async_fence(on);
        }
        self.drain_fences(mem);
        Self::refresh_fixed_registers(mem);
        // an interrupt still waiting for its GPU work keeps the command processor stopped, as
        // a delivered one does until its handler returns
        if self.rb_size == 0 || self.blocked || self.deferred.iter().any(|d| d.1 == DEFERRED_INTERRUPT) {
            crate::prof::count(crate::prof::Cat::CpBlocked);
            if !self.blocked && self.rb_size != 0 {
                crate::prof::count(crate::prof::Cat::CpBlockedGpu);
            }
            return;
        }
        let wptr = match self.wptr_snapshot.take() {
            Some(w) => w,
            None => mem.read32(GPU_MMIO_BASE + CP_RB_WPTR as u32 * 4),
        };
        self.regs[CP_RB_WPTR] = wptr;
        self.mark(CP_RB_WPTR);
        let size_dw = self.rb_size / 4;
        let wptr = wptr % size_dw;
        let mut budget = 200_000;
        while budget > 0 {
            budget -= 1;
            // finish the innermost indirect buffer first
            if let Some(mut r) = self.ib_stack.pop() {
                if r.left() == 0 {
                    continue;
                }
                let step = self.execute_packet(mem, &mut r);
                self.ib_stack.push(r);
                match step {
                    Step::Ok => self.push_pending_ib(),
                    Step::Stall => {
                        self.stats.stalls += 1;
                        break;
                    }
                    Step::Interrupt => {
                        self.blocked = true;
                        break;
                    }
                }
                continue;
            }
            if self.rptr == wptr {
                break;
            }
            let avail = (wptr + size_dw - self.rptr) % size_dw;
            // don't start a packet the CPU hasn't finished writing
            let header = mem.read32(self.rb_base + self.rptr * 4);
            let len = match header >> 30 {
                0 | 3 => ((header >> 16) & 0x3FFF) + 2,
                1 => 3,
                _ => 1,
            };
            if len > avail {
                break;
            }
            let mut r = Reader { base: self.rb_base, size_dw, pos: self.rptr, end: self.rptr + avail };
            match self.execute_packet(mem, &mut r) {
                Step::Ok => {
                    self.rptr = (self.rptr + len) % size_dw;
                    self.push_pending_ib();
                }
                Step::Stall => {
                    self.stats.stalls += 1;
                    break;
                }
                Step::Interrupt => {
                    self.rptr = (self.rptr + len) % size_dw;
                    self.blocked = true;
                    break;
                }
            }
        }
        // resolves may still be copying into guest memory: finish before guest code runs again
        // (with asynchronous fences: land what is done; the rest is covered by the fences)
        self.renderer.sync(mem);
        self.drain_fences(mem);
        self.regs[CP_RB_RPTR] = self.rptr;
        self.mark(CP_RB_RPTR);
        mem.write32(GPU_MMIO_BASE + CP_RB_RPTR as u32 * 4, self.rptr);
        if self.writeback != 0 {
            mem.write32(phys(self.writeback), self.rptr);
        }
    }

    fn push_pending_ib(&mut self) {
        if let Some(ib) = self.pending_ib.take() {
            if self.ib_stack.len() < 8 {
                self.stats.ib_calls += 1;
                self.ib_stack.push(ib);
            }
        }
    }

    fn execute_packet(&mut self, mem: &mut impl CpMem, r: &mut Reader) -> Step {
        let start = r.pos;
        let addr = r.base + (if r.size_dw != 0 { start % r.size_dw } else { start }) * 4;
        let header = r.read(mem);
        if self.history.len() == 40 {
            self.history.pop_front();
        }
        self.history.push_back((addr, header, self.ib_stack.len() as u32));
        self.stats.packets += 1;
        match header >> 30 {
            0 => {
                let count = ((header >> 16) & 0x3FFF) + 1;
                let base = (header & 0x7FFF) as usize;
                if (header >> 15) & 1 == 1 {
                    for _ in 0..count {
                        let v = r.read(mem);
                        self.write_register(mem, base, v);
                    }
                } else {
                    self.write_registers_from(mem, r, base, count);
                }
            }
            1 => {
                let (r1, r2) = ((header & 0x7FF) as usize, ((header >> 11) & 0x7FF) as usize);
                let (v1, v2) = (r.read(mem), r.read(mem));
                self.write_register(mem, r1, v1);
                self.write_register(mem, r2, v2);
            }
            2 => {}
            _ => {
                let op = (header >> 8) & 0x7F;
                let count = ((header >> 16) & 0x3FFF) + 1;
                let data_start = r.pos;
                // predicated packets run only when the current bin is selected
                if header & 1 == 1 && (self.bin_select & self.bin_mask == 0 || op == PM4_XE_SWAP) {
                    if self.capturing() {
                        self.capture.push(format!("skip predicated op {op:#04x} (select {:#x} mask {:#x})", self.bin_select, self.bin_mask));
                    }
                    r.pos = data_start + count;
                    return Step::Ok;
                }
                if self.capturing() && matches!(op, 0x50 | 0x51 | 0x60..=0x63 | PM4_INDIRECT_BUFFER) {
                    let v: Vec<String> = (0..count.min(3)).map(|i| format!("{:#x}", mem.read32(r.base + ((data_start + i) % r.size_dw.max(u32::MAX)) * 4))).collect();
                    self.capture.push(format!("op {op:#04x} {}{}", v.join(" "), if header & 1 == 1 { " (predicated)" } else { "" }));
                }
                match op {
                    0x60 => self.bin_mask = (self.bin_mask & !0xFFFF_FFFF) | r.read(mem) as u64,
                    0x61 => self.bin_mask = (self.bin_mask & 0xFFFF_FFFF) | ((r.read(mem) as u64) << 32),
                    0x62 => {
                        self.bin_select = (self.bin_select & !0xFFFF_FFFF) | r.read(mem) as u64;
                        self.seam_select();
                    }
                    0x63 => {
                        self.bin_select = (self.bin_select & 0xFFFF_FFFF) | ((r.read(mem) as u64) << 32);
                        self.seam_select();
                    }
                    0x50 | 0x51 => {
                        let hi = r.read(mem) as u64;
                        let lo = r.read(mem) as u64;
                        if op == 0x50 { self.bin_mask = (hi << 32) | lo } else { self.bin_select = (hi << 32) | lo; self.seam_select() }
                    }
                    0x3B | 0x5E | 0x26 | 0x53 => {} // INVALIDATE_STATE, CONTEXT_UPDATE, WAIT_FOR_IDLE, WAIT_IB_PFD_COMPLETE
                    PM4_INDIRECT_BUFFER | PM4_INDIRECT_BUFFER_PFD => {
                        let ptr = r.read(mem);
                        let len = r.read(mem) & 0xF_FFFF;
                        self.pending_ib = Some(Reader { base: phys(ptr), size_dw: 0, pos: 0, end: len });
                    }
                    PM4_WAIT_REG_MEM => {
                        let (info, poll, reference, mask) = (r.read(mem), r.read(mem), r.read(mem), r.read(mem));
                        let _wait = r.read(mem);
                        let value = if info & 0x10 != 0 {
                            self.load(mem, poll)
                        } else {
                            if poll as usize == COHER_STATUS_HOST {
                                self.regs[COHER_STATUS_HOST] &= 0x7FFF_FFFF; // we're always coherent
                                self.mark(COHER_STATUS_HOST);
                            }
                            self.regs.get(poll as usize).copied().unwrap_or(0)
                        };
                        if !match_ref(value & mask, reference, info) {
                            if self.stats.stalls == 0 || self.stats.stalls == 1000 {
                                self.log.push(format!(
                                    "WAIT_REG_MEM stall: info {info:#x} {} {poll:#x} value {value:#x} & mask {mask:#x} vs ref {reference:#x}",
                                    if info & 0x10 != 0 { "mem" } else { "reg" }
                                ));
                            }
                            r.pos = start;
                            return Step::Stall;
                        }
                    }
                    PM4_REG_RMW => {
                        let (info, and_mask, or_mask) = (r.read(mem), r.read(mem), r.read(mem));
                        let idx = (info & 0x1FFF) as usize;
                        let mut v = self.regs[idx];
                        v &= if info >> 31 & 1 == 1 { self.regs[(and_mask & 0x1FFF) as usize] } else { and_mask };
                        v |= if info >> 30 & 1 == 1 { self.regs[(or_mask & 0x1FFF) as usize] } else { or_mask };
                        self.write_register(mem, idx, v);
                    }
                    PM4_REG_TO_MEM => {
                        let (reg, addr) = (r.read(mem), r.read(mem));
                        let v = self.regs.get(reg as usize).copied().unwrap_or(0);
                        self.fence_store(mem, addr, v);
                    }
                    PM4_MEM_WRITE => {
                        let mut addr = r.read(mem);
                        for _ in 0..count - 1 {
                            let v = r.read(mem);
                            self.fence_store(mem, addr, v);
                            addr = addr.wrapping_add(4);
                        }
                    }
                    PM4_COND_WRITE => {
                        // With asynchronous fences this doesn't wait for the GPU (that was a quarter
                        // of team select's real time on the Mac): the poll reads memory as the
                        // command processor sees it, and the write lands in order behind the
                        // deferred ones.
                        let (info, poll, reference, mask, waddr, data) =
                            (r.read(mem), r.read(mem), r.read(mem), r.read(mem), r.read(mem), r.read(mem));
                        let value = if info & 0x10 != 0 { self.load_ordered(mem, poll) } else { self.regs.get(poll as usize).copied().unwrap_or(0) };
                        if match_ref(value & mask, reference, info) {
                            if info & 0x100 != 0 {
                                self.fence_store(mem, waddr, data);
                            } else {
                                self.write_register(mem, waddr as usize, data);
                            }
                        }
                    }
                    PM4_EVENT_WRITE_SHD => {
                        let (initiator, addr, value) = (r.read(mem), r.read(mem), r.read(mem));
                        let v = if initiator >> 31 & 1 == 1 { self.counter } else { value };
                        self.fence_store(mem, addr, v);
                    }
                    PM4_EVENT_WRITE_EXT => {
                        let _initiator = r.read(mem);
                        let addr = r.read(mem) & !3;
                        // screen extents (k8in16): min/max x, min/max y, min/max z
                        for (i, v) in [0u16, 8192 >> 3, 0, 8192 >> 3, 0, 1].iter().enumerate() {
                            mem.write16(phys(addr) + 2 * i as u32, *v);
                        }
                    }
                    PM4_EVENT_WRITE_ZPD => {
                        let _initiator = r.read(mem);
                        let report = self.regs[RB_SAMPLE_COUNT_ADDR];
                        if report != 0 {
                            // fake "everything visible" occlusion result
                            mem.write(phys(report), &[0; 16]);
                            mem.write32(phys(report) + 8, 100u32.swap_bytes());
                        }
                    }
                    PM4_INTERRUPT => {
                        let mask = r.read(mem);
                        if !self.dumped {
                            self.dumped = true;
                            self.log.push(format!("first PM4_INTERRUPT mask {mask:#x}; previous packets:"));
                            for (a, h, d) in &self.history {
                                let kind = match h >> 30 {
                                    0 => format!("type0 reg {:#06x} x{}", h & 0x7FFF, ((h >> 16) & 0x3FFF) + 1),
                                    3 => format!("type3 op {:#04x} count {}", (h >> 8) & 0x7F, ((h >> 16) & 0x3FFF) + 1),
                                    t => format!("type{t}"),
                                };
                                self.log.push(format!("  ib{d} {a:#010x}: {h:#010x} {kind}"));
                            }
                        }
                        // the guest's handler may read anything the GPU wrote before it: with
                        // asynchronous fences it is raised once that has landed (`drain_fences`)
                        self.fence_store(mem, DEFERRED_INTERRUPT, mask);
                        r.pos = data_start + count;
                        return Step::Interrupt;
                    }
                    PM4_XE_SWAP => {
                        if self.capturing() {
                            if let Some(d) = self.renderer.take_debug() {
                                self.capture.extend(d);
                            }
                            self.capture.push("---- XE_SWAP (end of frame)".into());
                        }
                        let _sig = r.read(mem);
                        let (fb, w, h) = (r.read(mem), r.read(mem), r.read(mem));
                        self.last_swap = Some((fb, w, h));
                        if self.capturing() {
                            let f: Vec<String> = (0..6).map(|i| format!("{:08x}", self.regs[0x4800 + i])).collect();
                            self.capture.push(format!("swap fb {fb:#010x} {w}x{h} fetch0 {}", f.join(" ")));
                        }
                        if self.present_all && self.stats.frames % self.present_every.max(1) == 0 {
                            self.flush_dirty();
                            if let Some((fw, fh, px)) = self.renderer.present(&self.regs, mem, fb, w, h) {
                                self.latest_frame = Some((self.stats.frames, fw, fh, px));
                            }
                        }
                        if self.dump_frames.contains(&self.stats.frames) {
                            self.flush_dirty();
                            if let Some((fw, fh, px)) = self.renderer.present(&self.regs, mem, fb, w, h) {
                                self.frames_out.push((self.stats.frames, fw, fh, px));
                            }
                        }
                        let frame = self.stats.frames;
                        if frame % 64 == 0 {
                            self.resolve_seen.retain(|_, f| frame - f.1 <= RESOLVE_RECENT);
                        }
                        self.skipped = Skipped::default();
                        self.forced = false;
                        self.stats.frames += 1;
                        self.seam_swap();
                        crate::threaded::GUEST_FRAME.store(self.stats.frames, std::sync::atomic::Ordering::Relaxed);
                        crate::prof::frame_tick(self.stats.frames);
                        self.swap_times.push(std::time::Instant::now());
                        self.counter = self.counter.wrapping_add(1);
                    }
                    PM4_DRAW_INDX | PM4_DRAW_INDX_2 => {
                        if header & 1 == 1 {
                            self.pass_log();
                        }
                        // LMA_SINGLE_PASS: a later tile's draws are dropped (the first tile drew the
                        // whole frame); its resolves and clears run with the window offset zeroed, so
                        // the rectangle names the EDRAM rows of the full-height surface (the resolve
                        // writes its destination relative to the rectangle's origin: tile 1's band
                        // texture still gets rows 0..336, read from EDRAM rows 384..720).
                        let (mut tile_woff, mut tile_scissor) = (None, None);
                        if header & 1 == 1 && self.single_pass {
                            let copy = self.regs[RB_MODECONTROL] & 7 == 6;
                            if self.later_tile() {
                                if !copy {
                                    self.pass_dropped += 1;
                                    r.pos = data_start + count;
                                    return Step::Ok;
                                }
                                tile_woff = Some(self.regs[PA_SC_WINDOW_OFFSET]);
                                self.regs[PA_SC_WINDOW_OFFSET] = 0;
                                self.mark(PA_SC_WINDOW_OFFSET);
                            } else if !copy && self.single_pass_widen {
                                // per draw: the runtime sets the scissor once per pass, and the pass's
                                // resolve of the other band must stay clipped to empty by the band
                                tile_scissor = self.widen_tile_scissor();
                            }
                        }
                        self.stats.draws += 1;
                        if op == PM4_DRAW_INDX {
                            let _viz = r.read(mem);
                        }
                        let init = r.read(mem);
                        let (mut base, mut size) = (0, 0);
                        if (init >> 6) & 3 == 0 {
                            base = r.read(mem);
                            size = r.read(mem);
                            // index buffer size dword: count in the low bits, endian swap in bits 30-31
                            self.regs[0x21FB] = size;
                            self.mark(0x21FB);
                        }
                        self.regs[0x21FC] = init;
                        self.mark(0x21FC);
                        if self.capturing() {
                            self.record_draw(init, base, size);
                        }
                        if self.capturing() && !self.renderer.debugging() {
                            self.renderer.start_debug();
                        }
                        if !self.capturing() {
                            if let Some(d) = self.renderer.take_debug() {
                                self.capture.extend(d);
                            }
                        }
                        let vs = self.vs.as_ref().map(|v| v.1.clone()).unwrap_or_default(); // Arc clones
                        let ps = self.ps.as_ref().map(|v| v.1.clone()).unwrap_or_default();
                        let target = self.resolve_target();
                        let frame = self.stats.frames;
                        // only a per-frame resolve target (resolved in every one of the last frames,
                        // allowing double-buffered gaps: shadow maps, bloom, the front buffer) may
                        // be skipped. Anything else is content the game builds and keeps, possibly
                        // over several frames through the same target, and may read back on the
                        // CPU: LMA builds the shirt backs that way (Xenia's compatibility report
                        // #914: "requires readback_resolve to render back of the shirts"), and a
                        // skipped resolve left them white.
                        let new_target = target.is_some_and(|t| {
                            let run = self.resolve_seen.get(&t).copied().filter(|&(_, last)| frame - last <= 2);
                            let first = run.map_or(frame, |(first, _)| first);
                            self.resolve_seen.insert(t, (first, frame));
                            frame - first < RESOLVE_RECENT
                        });
                        if self.render && !vs.is_empty() && !self.forced && self.stats.frames % self.render_every.max(1) != 0 {
                            // A skipped frame. Its resolves into textures the last rendered frame also
                            // made (shadow maps, bloom, the front buffer) are redone every frame, but
                            // a new one is content the game makes once and keeps (player faces and
                            // kits composited while a match loads): skipping it left them black. Then
                            // the frame is rendered after all, its draws so far replayed.
                            if new_target {
                                self.replay_skipped(mem);
                                self.forced = true;
                            } else {
                                self.log_skipped(init, base, vs.clone(), ps.clone());
                            }
                        }
                        if self.render && (self.forced || self.stats.frames % self.render_every.max(1) == 0) && !vs.is_empty() {
                            self.flush_dirty();
                            self.seam_draw(mem, &vs, header & 1 == 1);
                            self.renderer.draw(&self.regs, mem, init, base, &vs[..], &ps[..]);
                        }
                        if let Some(w) = tile_woff {
                            self.regs[PA_SC_WINDOW_OFFSET] = w;
                            self.mark(PA_SC_WINDOW_OFFSET);
                        }
                        if let Some(br) = tile_scissor {
                            self.regs[PA_SC_WINDOW_SCISSOR_BR] = br;
                            self.mark(PA_SC_WINDOW_SCISSOR_BR);
                        }
                    }
                    PM4_IM_LOAD => {
                        let addr_type = r.read(mem);
                        let start_size = r.read(mem);
                        let n = (start_size & 0xFFFF) as usize;
                        let addr = addr_type & !3;
                        // one bulk read; most binds rebind the shader already bound (no copy, no hash).
                        // Compared as slices (memcmp): an element-wise compare through a
                        // byte-swapping iterator was ~9% of the command processor.
                        read_be(mem, phys(addr), n, &mut self.im_words);
                        let kind = addr_type & 3;
                        let bound = if kind == 0 { &self.vs } else { &self.ps };
                        let same = bound.as_ref().is_some_and(|(a, c)| *a == addr && c[..] == self.im_words[..]);
                        if !same || self.capturing() {
                            let code = self.im_words.clone();
                            self.bind_shader(kind, addr, code);
                        }
                    }
                    PM4_IM_LOAD_IMMEDIATE => {
                        let kind = r.read(mem);
                        let n = r.read(mem) & 0xFFFF;
                        let code: Vec<u32> = (0..n).map(|_| r.read(mem)).collect();
                        self.bind_shader(kind, 0, code);
                    }
                    PM4_SET_CONSTANT | PM4_SET_CONSTANT2 => {
                        let base = const_base(r.read(mem));
                        self.write_registers_from(mem, r, base, count - 1);
                    }
                    PM4_LOAD_ALU_CONSTANT => {
                        // constants (or registers) copied from memory: how D3D uploads most shader
                        // constants; dropping it left e.g. the 3D models' pixel-shader alpha at 0
                        let addr = r.read(mem) & 0x3FFF_FFFF;
                        let offset_type = r.read(mem);
                        let dwords = r.read(mem) & 0xFFF;
                        let base = const_base(offset_type);
                        let mut words = std::mem::take(&mut self.cp_words);
                        if dwords > 0 && phys(addr + (dwords - 1) * 4) == phys(addr) + (dwords - 1) * 4 {
                            // one bulk read (the range doesn't wrap the physical window)
                            read_be(mem, phys(addr), dwords as usize, &mut words);
                        } else {
                            words.clear();
                            words.extend((0..dwords).map(|i| mem.read32(phys(addr + i * 4))));
                        }
                        self.write_registers(mem, base, &words);
                        self.cp_words = words;
                    }
                    PM4_SET_SHADER_CONSTANTS => {
                        // register index straight in the low 16 bits
                        let index = (r.read(mem) & 0xFFFF) as usize;
                        self.write_registers_from(mem, r, index, count - 1);
                    }
                    PM4_ME_INIT | PM4_NOP | PM4_EVENT_WRITE => {}
                    _ => {
                        if self.log.len() < 50 {
                            self.log.push(format!("unhandled PM4 type3 op {op:#04x} count {count}"));
                        }
                    }
                }
                // skip whatever the handler did not consume
                r.pos = data_start + count;
            }
        }
        Step::Ok
    }

    fn capturing(&self) -> bool {
        self.capture_frame == Some(self.stats.frames)
    }

    fn bind_shader(&mut self, kind: u32, addr: u32, code: Vec<u32>) {
        if self.capturing() || self.shader_dump.is_some() {
            self.note_shader(kind, addr, &code);
        }
        let code = Arc::new(code);
        if kind == 0 { self.vs = Some((addr, code)) } else { self.ps = Some((addr, code)) }
        self.renderer.shaders_dirty();
    }

    /// Capture / LMA_SHADER_DUMP bookkeeping for a bound shader (only when one of them is on:
    /// the hash and label cost every bind otherwise).
    fn note_shader(&mut self, kind: u32, addr: u32, code: &[u32]) {
        let hash = code.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &w| (h ^ w as u64).wrapping_mul(0x100_0000_01b3));
        let label = format!("{}_{:016x}", if kind == 0 { "vs" } else { "ps" }, hash);
        if self.capturing() {
            self.capture.push(format!("bind {label} ({} dwords) from {addr:#010x}", code.len()));
            if !self.captured_shaders.iter().any(|(l, _)| *l == label) {
                self.captured_shaders.push((label.clone(), code.to_vec()));
            }
        }
        if let Some(dir) = &self.shader_dump {
            // LMA_SHADER_DUMP: keep every distinct shader (for WGSL translation tests)
            let path = dir.join(format!("{label}.bin"));
            if !path.exists() {
                let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_be_bytes()).collect();
                let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(&path, bytes));
            }
        }
    }

    fn record_draw(&mut self, init: u32, base: u32, size: u32) {
        let prim = init & 0x3F;
        let source = ["dma", "immediate", "auto", "?"][((init >> 6) & 3) as usize];
        let index32 = (init >> 11) & 1 == 1;
        let count = init >> 16;
        let h = |c: &Option<(u32, Arc<Vec<u32>>)>| c.as_ref().map_or(0, |(_, code)| code.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &w| (h ^ w as u64).wrapping_mul(0x100_0000_01b3)));
        let mut line = format!("DRAW prim {prim} count {count} source {source} vs {:016x} ps {:016x}", h(&self.vs), h(&self.ps));
        if source == "dma" {
            line += &format!(" indices@{base:#010x} {} words {:#x}", if index32 { "u32" } else { "u16" }, size & 0xFF_FFFF);
        }
        self.capture.push(line);
        for i in (0x2000..0x2400).chain(0x4000..0x4A00) {
            if self.regs[i] != self.last_draw_regs[i] {
                self.capture.push(format!("    {:<28} = {:#010x}", reg_name(i), self.regs[i]));
            }
        }
        self.last_draw_regs.copy_from_slice(&self.regs);
    }

    /// Called at each vertical blank.
    pub fn vblank(&mut self) {
        self.regs[D1MODE_VBLANK_STATUS] |= 1;
        self.mark(D1MODE_VBLANK_STATUS);
        self.counter = self.counter.wrapping_add(1);
        if self.interrupt_cb != 0 {
            self.pending_interrupts.push_back((0, 2)); // vsync on cpu 2, like Xenia
        }
    }
}

/// LMA_SEAM_LOG=2 state (see `Gpu::seam`).
struct SeamLog {
    /// per bin-select value seen this frame, the predicated draws' vertex-stream hashes in order
    /// (the game runs the tiled part of the frame once per EDRAM tile with a different select;
    /// unpredicated draws run once and are not compared)
    groups: Vec<(u64, Vec<u64>)>,
    /// LMA_SEAM_LOG=3: also print every frame's group sizes
    verbose: bool,
    /// vertex fetch slots per vertex shader (by the shader's hash)
    slots: HashMap<u64, Vec<u32>>,
    /// frames logged so far (the line is printed for the first 200 seam frames, then every 50th)
    logged: u32,
}

impl SeamLog {
    fn new() -> Self {
        SeamLog { groups: Vec::new(), verbose: false, slots: HashMap::new(), logged: 0 }
    }
    fn from_env() -> Option<Self> {
        match std::env::var("LMA_SEAM_LOG").as_deref() {
            Ok("2") => Some(SeamLog::new()),
            Ok("3") => Some(SeamLog { verbose: true, ..SeamLog::new() }),
            _ => None,
        }
    }
}

impl Gpu {
    /// A bin select write: nothing to do, the draws carry the select they ran under.
    fn seam_select(&mut self) {}

    /// LMA_SINGLE_PASS: a predicated packet of a later tile, i.e. one the runtime replays with the
    /// window offset moved (tile 1 of the night match: select 0x8, offset (0,-384), scissor rows
    /// 384..720, its band resolved into its own 336-row texture; tile 0 has offset 0, select
    /// 0x80000002, rows 0..384 and a 384-row texture; the present composes the bands). Other
    /// predicated passes (the shadow map, the 384-row pass and the luminance targets under
    /// select-all or 0x20) keep offset 0 and their own viewport and are left alone.
    fn later_tile(&self) -> bool {
        self.regs[PA_SC_WINDOW_OFFSET] & 0x7FFF_7FFF != 0
    }

    /// LMA_SINGLE_PASS_LOG=1 (debug): each distinct (scissor, viewport, window offset, select) a
    /// predicated draw runs under, in any mode.
    fn pass_log(&mut self) {
        static LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*LOG.get_or_init(|| std::env::var_os("LMA_SINGLE_PASS_LOG").is_some()) {
            return;
        }
        let (tl, br) = (self.regs[PA_SC_WINDOW_SCISSOR_TL], self.regs[PA_SC_WINDOW_SCISSOR_BR]);
        let key = (tl, br, self.regs[PA_CL_VPORT_YSCALE], self.regs[PA_CL_VPORT_YSCALE + 1], self.regs[PA_SC_WINDOW_OFFSET], self.bin_select);
        if !self.pass_seen.contains(&key) {
            self.pass_seen.push(key);
            let woff = key.4;
            let (wox, woy) = (((woff & 0x7FFF) << 17) as i32 >> 17, (((woff >> 16) & 0x7FFF) << 17) as i32 >> 17);
            eprintln!("single-pass: scissor x {}..{} y {}..{} vport yscale {} yoffset {} woff ({wox},{woy}) select {:#x} mask {:#x} frame {}", tl & 0x7FFF, br & 0x7FFF, (tl >> 16) & 0x7FFF, (br >> 16) & 0x7FFF, f32::from_bits(key.2), f32::from_bits(key.3), key.5, self.bin_mask, self.stats.frames);
        }
    }

    /// LMA_SINGLE_PASS: a draw of the first tile whose window scissor starts at row 0 and stops
    /// short of the viewport (the top band) gets the viewport's height for this draw, so the one
    /// pass draws every band; returns the scissor word to restore after the draw (resolves keep the
    /// band: `raster::resolve_rect` clips to the scissor, and the pass's resolve of the other band
    /// must stay empty).
    fn widen_tile_scissor(&mut self) -> Option<u32> {
        let (tl, br) = (self.regs[PA_SC_WINDOW_SCISSOR_TL], self.regs[PA_SC_WINDOW_SCISSOR_BR]);
        let (y0, y1) = ((tl >> 16) & 0x7FFF, (br >> 16) & 0x7FFF);
        let vh = (2.0 * f32::from_bits(self.regs[PA_CL_VPORT_YSCALE]).abs()).round() as u32;
        if y0 == 0 && y1 > 0 && y1 < vh {
            self.regs[PA_SC_WINDOW_SCISSOR_BR] = (br & 0xFFFF) | (vh << 16);
            self.mark(PA_SC_WINDOW_SCISSOR_BR);
            return Some(br);
        }
        None
    }

    /// A rendered predicated draw: hash the first 4 KB of each vertex stream the shader reads and
    /// file it under the current bin select.
    fn seam_draw(&mut self, mem: &mut impl CpMem, vs: &Arc<Vec<u32>>, predicated: bool) {
        if !predicated {
            return;
        }
        let select = self.bin_select;
        let Some(s) = self.seam.as_mut() else { return };
        let key = crate::raster::hash(vs);
        let slots = s.slots.entry(key).or_insert_with(|| crate::shader::Shader::new(vs.to_vec()).vertex_fetch_consts());
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &k in slots.iter() {
            let d0 = self.regs[0x4800 + k as usize * 2];
            let d1 = self.regs[0x4800 + k as usize * 2 + 1];
            let base = phys(d0 & !3);
            let dwords = (d1 >> 2).min(1024);
            for i in 0..dwords {
                h = (h ^ mem.read32(base + i * 4) as u64).wrapping_mul(0x100_0000_01b3);
            }
            h = (h ^ ((d0 as u64) << 32) ^ d1 as u64).wrapping_mul(0x100_0000_01b3);
        }
        match s.groups.iter_mut().find(|g| g.0 == select) {
            Some(g) => g.1.push(h),
            None => s.groups.push((select, vec![h])),
        }
    }

    /// The swap: compare the two biggest tile groups draw by draw; one line when they disagree.
    fn seam_swap(&mut self) {
        let frame = self.stats.frames;
        let Some(s) = self.seam.as_mut() else { return };
        s.groups.sort_by_key(|g| std::cmp::Reverse(g.1.len()));
        let sizes: Vec<String> = s.groups.iter().map(|g| format!("{:#x}:{}", g.0, g.1.len())).collect();
        if s.groups.len() >= 2 {
            let (a, b) = (&s.groups[0].1, &s.groups[1].1);
            let n = a.len().min(b.len());
            let mismatch = (0..n).filter(|&i| a[i] != b[i]).count();
            if mismatch > 0 || s.verbose {
                s.logged += 1;
                if s.logged <= 300 || s.logged % 50 == 0 || s.verbose {
                    eprintln!("seam frame {frame}: {mismatch} of {n} tiled draws read different vertex data in the second tile (groups {})", sizes.join(" "));
                }
            }
        } else if s.verbose {
            eprintln!("seam frame {frame}: groups {}", sizes.join(" "));
        }
        s.groups.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LMA_SEAM_LOG=2 bookkeeping: predicated draws are filed under the bin select they ran with,
    /// the swap compares the two biggest tile groups draw by draw, and a stream the game rewrote
    /// between the tiles counts as a mismatch.
    #[test]
    fn seam_log_counts_rewritten_vertex_streams() {
        let mut g = Gpu::default();
        g.seam = Some(SeamLog::new());
        let mut mem = crate::mem::Memory::new();
        let exec_end = 1 | (2 << 12) | (0b0101 << 16);
        let vfetch = [(1 << 5) | (2 << 12) | (31 << 20) | (2 << 25), 0x688 | (0x39 << 16), 12 | (4 << 8)];
        let tfetch = [1 | (3 << 20), 0x688 | (1 << 12) | (1 << 14), 1 << 14];
        let words: Vec<u32> = [[exec_end, 2 << 12, 0], vfetch, tfetch].concat();
        let slot = crate::shader::Shader::new(words.clone()).vertex_fetch_consts()[0] as usize;
        let vs = Arc::new(words);
        g.regs[0x4800 + slot * 2] = 0x1000_0000;
        g.regs[0x4800 + slot * 2 + 1] = 16 << 2; // 16 dwords
        for i in 0..16 {
            mem.write32(phys(0x1000_0000 + i * 4), i); // the hash reads the stream through phys()
        }
        g.bin_select = 1; // tile 1
        g.seam_draw(&mut mem, &vs, false); // unpredicated: not compared
        g.seam_draw(&mut mem, &vs, true);
        g.seam_draw(&mut mem, &vs, true);
        g.bin_select = 2; // tile 2
        g.seam_draw(&mut mem, &vs, true); // same stream: equal
        mem.write32(phys(0x1000_0008), 0xdead); // the game rewrote the buffer
        g.seam_draw(&mut mem, &vs, true); // differs
        let s = g.seam.as_ref().unwrap();
        assert_eq!(s.groups.iter().map(|g| (g.0, g.1.len())).collect::<Vec<_>>(), vec![(1, 2), (2, 2)]);
        g.stats.frames = 7;
        g.seam_swap();
        let s = g.seam.as_ref().unwrap();
        assert_eq!((s.groups.len(), s.logged), (0, 1), "one mismatch logged, frame reset");
    }

    /// LMA_SINGLE_PASS: the first tile's band scissor (row 0 to short of the viewport) is widened to
    /// the viewport, other clip rectangles are kept, and a moved window offset marks a later tile.
    #[test]
    fn single_pass_widens_the_top_band_and_drops_later_tiles() {
        let mut g = Gpu::default();
        g.single_pass = true;
        g.regs[PA_CL_VPORT_YSCALE] = 360.0f32.to_bits();
        g.regs[PA_SC_WINDOW_SCISSOR_TL] = 0;
        g.regs[PA_SC_WINDOW_SCISSOR_BR] = (384 << 16) | 1280;
        assert_eq!(g.widen_tile_scissor(), Some((384 << 16) | 1280), "the band's word, to restore after the draw");
        assert_eq!(g.regs[PA_SC_WINDOW_SCISSOR_BR], (720 << 16) | 1280, "the band becomes the viewport");
        g.regs[PA_SC_WINDOW_SCISSOR_TL] = 100 << 16;
        g.regs[PA_SC_WINDOW_SCISSOR_BR] = (200 << 16) | 300;
        assert_eq!(g.widen_tile_scissor(), None);
        assert_eq!(g.regs[PA_SC_WINDOW_SCISSOR_BR], (200 << 16) | 300, "a clip rectangle below row 0 is kept");
        g.regs[PA_SC_WINDOW_SCISSOR_TL] = 0;
        g.regs[PA_SC_WINDOW_SCISSOR_BR] = (720 << 16) | 1280;
        assert_eq!(g.widen_tile_scissor(), None, "a full scissor is kept");
        assert!(!g.later_tile(), "window offset 0: the first tile");
        g.regs[PA_SC_WINDOW_OFFSET] = ((-384i32 as u32) & 0x7FFF) << 16; // (0,-384): tile 1
        assert!(g.later_tile());
    }

    /// `LMA_SINGLE_PASS` unset takes the binary's default; `0` always switches it off.
    #[test]
    fn single_pass_mode_follows_the_default_when_unset() {
        assert_eq!(single_pass_mode_of(None, true), 1, "lma-cli");
        assert_eq!(single_pass_mode_of(None, false), 0, "lma-app");
        assert_eq!(single_pass_mode_of(Some("0"), true), 0, "opt out");
        assert_eq!(single_pass_mode_of(Some("1"), false), 1, "opt in");
        assert_eq!(single_pass_mode_of(Some("2"), false), 2);
        assert_eq!(single_pass_mode_of(Some("junk"), true), 0);
    }

    /// LMA_SINGLE_PASS at the packet level: a predicated draw packet under a moved window offset is
    /// dropped (the reader moves past it, nothing is drawn, `pass_dropped` counts it); the same
    /// packet with the offset at 0 and an unpredicated one under the moved offset go through.
    #[test]
    fn single_pass_drops_a_later_tiles_predicated_draw_packet() {
        let mut g = Gpu::default();
        g.single_pass = true;
        g.single_pass_widen = true;
        g.render = false; // no renderer work: the packet's bookkeeping is what's tested
        let mut mem = crate::mem::Memory::new();
        // a type-3 DRAW_INDX_2 packet: header + 1 word (the init word; no index buffer)
        let packet = |predicated: u32| [(3 << 30) | (PM4_DRAW_INDX_2 << 8) | (0 << 16) | predicated, 0x0000_0004];
        let run = |g: &mut Gpu, mem: &mut crate::mem::Memory, words: [u32; 2]| {
            let base = 0x4000_2000u32;
            for (i, w) in words.iter().enumerate() {
                mem.write32(base + i as u32 * 4, *w);
            }
            let mut r = Reader { base, size_dw: 0, pos: 0, end: 2 };
            g.execute_packet(mem, &mut r);
            r.pos
        };
        g.regs[PA_SC_WINDOW_OFFSET] = ((-384i32 as u32) & 0x7FFF) << 16; // tile 1
        let (draws, dropped) = (g.stats.draws, g.pass_dropped);
        assert_eq!(run(&mut g, &mut mem, packet(1)), 2, "the reader moved past the dropped packet");
        assert_eq!((g.stats.draws, g.pass_dropped), (draws, dropped + 1), "dropped, not counted as a draw");
        assert_eq!(run(&mut g, &mut mem, packet(0)), 2);
        assert_eq!((g.stats.draws, g.pass_dropped), (draws + 1, dropped + 1), "an unpredicated draw runs");
        g.regs[PA_SC_WINDOW_OFFSET] = 0; // tile 0
        assert_eq!(run(&mut g, &mut mem, packet(1)), 2);
        assert_eq!((g.stats.draws, g.pass_dropped), (draws + 2, dropped + 1), "tile 0's predicated draw runs");
    }

    /// LMA_SINGLE_PASS: a later tile's resolve packet (RB_MODECONTROL copy mode) is not dropped; it
    /// runs and the window offset is back to the tile's value afterwards.
    #[test]
    fn single_pass_keeps_a_later_tiles_resolve_and_restores_the_offset() {
        let mut g = Gpu::default();
        g.single_pass = true;
        g.single_pass_widen = true;
        g.render = false;
        let mut mem = crate::mem::Memory::new();
        let base = 0x4000_3000u32;
        for (i, w) in [(3 << 30) | (PM4_DRAW_INDX_2 << 8) | 1, 0x0000_0004u32].iter().enumerate() {
            mem.write32(base + i as u32 * 4, *w);
        }
        let woff = ((-384i32 as u32) & 0x7FFF) << 16;
        g.regs[PA_SC_WINDOW_OFFSET] = woff;
        g.regs[RB_MODECONTROL] = 6; // copy mode: a resolve or clear
        let (draws, dropped) = (g.stats.draws, g.pass_dropped);
        let mut r = Reader { base, size_dw: 0, pos: 0, end: 2 };
        g.execute_packet(&mut mem, &mut r);
        assert_eq!((r.pos, g.stats.draws, g.pass_dropped), (2, draws + 1, dropped), "the resolve ran");
        assert_eq!(g.regs[PA_SC_WINDOW_OFFSET], woff, "the tile's offset is back after the packet");
    }

    /// Bulk register writes (SET_CONSTANT and friends): the register file gets the values, the
    /// MMIO mirror gets them as the guest reads them (big-endian in memory), the 64-register
    /// blocks they touch are marked dirty for the backend, and a write running past the end of
    /// the register file is cut there.
    #[test]
    fn bulk_register_writes() {
        let mut g = Gpu::default();
        let mut mem = crate::mem::Memory::new(); // explicit: the module's own imports may change
        g.reg_dirty = [0; REG_BLOCK_WORDS];
        let base = 0x2000;
        g.write_registers(&mut mem, base, &[0x1122_3344, 5, 0xFFFF_FFFF]);
        assert_eq!(&g.regs[base..base + 3], &[0x1122_3344, 5, 0xFFFF_FFFF]);
        assert_eq!(mem.read32(GPU_MMIO_BASE + base as u32 * 4), 0x1122_3344, "mirror, as a guest load sees it");
        assert_eq!(mem.read8(GPU_MMIO_BASE + base as u32 * 4), 0x11, "big-endian in memory");
        assert_eq!(mem.read32(GPU_MMIO_BASE + (base as u32 + 2) * 4), 0xFFFF_FFFF);
        let block = base / 64;
        assert_ne!(g.reg_dirty[block / 64] & (1 << (block % 64)), 0, "its block is dirty");
        assert_eq!(g.reg_dirty.iter().map(|w| w.count_ones()).sum::<u32>(), 1, "and only that one");
        // a run crossing a block edge marks both blocks
        g.reg_dirty = [0; REG_BLOCK_WORDS];
        g.write_registers(&mut mem, 0x203E, &[7, 8, 9]);
        assert_eq!(g.reg_dirty.iter().map(|w| w.count_ones()).sum::<u32>(), 2);
        assert_eq!(g.regs[0x2040], 9);
        // past the end of the register file: cut, no panic
        g.write_registers(&mut mem, REG_COUNT - 1, &[1, 2, 3]);
        assert_eq!(g.regs[REG_COUNT - 1], 1);
        g.write_registers(&mut mem, REG_COUNT + 5, &[1]);
    }

    /// Registers straight from a packet (SET_CONSTANT and friends): the linear path (raw bytes to
    /// the mirror, swapped into the register file) and the ring-wrapping one give the same
    /// registers, mirror and dirty blocks.
    #[test]
    fn registers_from_a_packet() {
        let vals = [0x1122_3344u32, 5, 0xFFFF_FFFF, 0x8000_0001];
        let run = |ring: bool| {
            let mut g = Gpu::default();
            let mut mem = crate::mem::Memory::new();
            g.reg_dirty = [0; REG_BLOCK_WORDS];
            // a 6-word ring with the packet starting at word 4 wraps after two words
            let (rbase, size, pos) = if ring { (0x4000_0000u32, 6u32, 4u32) } else { (0x4000_1000, 0, 0) };
            for (i, v) in vals.iter().enumerate() {
                let w = if ring { (pos + i as u32) % size } else { i as u32 };
                mem.write32(rbase + w * 4, *v);
            }
            let mut r = Reader { base: rbase, size_dw: size, pos, end: pos + 4 };
            g.write_registers_from(&mut mem, &mut r, 0x203E, 4);
            assert_eq!(r.pos, pos + 4, "ring {ring}: the reader moved past the packet");
            let mirror: Vec<u32> = (0..4).map(|i| mem.read32(GPU_MMIO_BASE + (0x203E + i) * 4)).collect();
            (g.regs[0x203E..0x2042].to_vec(), mirror, g.reg_dirty)
        };
        let (regs, mirror, dirty) = run(false);
        assert_eq!(regs, vals);
        assert_eq!(mirror, vals, "mirror, as a guest load sees it");
        assert_eq!(dirty.iter().map(|w| w.count_ones()).sum::<u32>(), 2, "both blocks of the run");
        assert_eq!(run(true), (regs, mirror, dirty), "wrapping the ring: the same result");
    }
}
