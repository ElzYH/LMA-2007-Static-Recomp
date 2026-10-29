//! Boots a decrypted XEX image on the reference interpreter and emulates the kernel/XAM
//! exports it imports. Guest threads are scheduled preemptively (see threads.rs).

use crate::cpu::{Cpu, Stop};
use crate::exports;
use crate::gpu::{self, Gpu};
use crate::mem::Memory;
use crate::threads::{Flow, GThread, State, Wait, OBJ_NOTIFICATION_EVENT, OBJ_SEMAPHORE, SLICE, THREAD_EXIT_MAGIC};
use crate::vfs::{self, Vfs};
use std::collections::{BTreeMap, HashMap};

pub const RETURN_MAGIC: u32 = 0xBCBC_BCB0;
pub const INTERRUPT_MAGIC: u32 = 0xBCBC_BCD0;
const VAR_BASE: u32 = 0x7F80_0000; // storage for imported kernel variables
const SMALL_BASE: u32 = 0x7FA0_0000; // kernel objects, TLS blocks
const LOCAL_XUID: u64 = 0xE000_0000_4C4D_4107;
const PROFILE_NAME: &str = "Player";

/// The local profile signed in on controller port `user` ("Player", then "Player 2".. with LMA_PADS).
fn profile_name(user: u32) -> String {
    if user == 0 { PROFILE_NAME.to_string() } else { format!("{PROFILE_NAME} {}", user + 1) }
}
const XN_SYS_UI: u32 = 0x0000_0009;
const XN_SYS_SIGNINCHANGED: u32 = 0x0000_000A;
const HDD_DEVICE_ID: u32 = 1;
const XCONTENT_DATA_SIZE: u32 = 0x134;
const HEAP_BASE: u32 = 0x4000_0000; // NtAllocateVirtualMemory / ExAllocatePool
const PHYS_BASE: u32 = 0xA000_0000; // MmAllocatePhysicalMemoryEx
/// How long a freed physical block waits before reuse (60 s of guest time;
/// 2 s was too short: sounds still went silent after the match load).
const PHYS_QUARANTINE: u64 = 60 * crate::threads::CLOCK_HZ;

pub struct Import {
    pub library: String,
    pub name: &'static str,
}

use crate::rec::BLOCK_BASE;

/// A guest thread's coroutine: resumed with the Machine, yields each slice's result.
type Coro = corosensei::Coroutine<*mut Machine, Option<Exit>, (), corosensei::stack::DefaultStack>;

/// Host stack per guest thread: reserved, committed by the OS as it is touched. Recursion in
/// recompiled code runs on it (native call depth is capped at `rec::MAX_DEPTH`).
const CORO_STACK: usize = 8 << 20;

fn new_coro() -> Coro {
    let stack = corosensei::stack::DefaultStack::new(CORO_STACK).expect("guest thread stack");
    corosensei::Coroutine::with_stack(stack, |y: &corosensei::Yielder<*mut Machine, Option<Exit>>, mut m: *mut Machine| loop {
        // SAFETY: `slice` hands over the only live access to the Machine for the duration of
        // this resume and doesn't touch it until the coroutine yields; no reference to it is
        // kept across the suspend
        let r = unsafe { (*m).run_slice() };
        m = y.suspend(r);
    })
}

pub struct Machine {
    /// Recompiled functions indexed by block leader (pc - 0x82000000) / 4.
    blocks: Vec<Option<crate::rec::FuncFn>>,
    /// Registers of the running thread (the others are parked in `threads`).
    pub cpu: Cpu,
    pub mem: Memory,
    pub threads: Vec<GThread>,
    pub cur: usize,
    /// Virtual time in guest instructions (all threads).
    pub clock: u64,
    /// Host time spent in (recompiled+interpreted code, HLE, GPU), and instructions interpreted.
    pub timing: [std::time::Duration; 3],
    pub interp_steps: u64,
    /// Guest instructions each thread (by index) has run in this process (diagnostic, not saved).
    pub thread_instr: Vec<u64>,
    /// Debug: sampled pcs that ran outside recompiled code.
    pub misses: Option<HashMap<u32, u64>>,
    pub(crate) handles: HashMap<u32, u32>,
    pub(crate) next_handle: u32,
    pub(crate) tls_size: u32,
    pub(crate) tls_init: Vec<u8>,
    thunks: HashMap<u32, Import>,
    /// Native replacements for guest functions (`crate::hooks`, LMA_HLE_D3D=1).
    hooks: HashMap<u32, crate::hooks::Hook>,
    /// Calls that reached each hooked address.
    pub hook_calls: HashMap<u32, u64>,
    /// Mod badges (`hooks::badge_set`, `hooks::badge_get`): club record index -> Badge.mng entry.
    pub badge_entries: HashMap<u16, u32>,
    /// Mod `income_scale` (`hooks::add_funds`): factor on the clubs' gate receipts, sponsorship and
    /// prize money; 1.0 leaves them as the game computes them.
    pub income_scale: f64,
    /// Mod `gate_scale`: the gate receipts' own factor (None: `income_scale`).
    pub gate_scale: Option<f64>,
    /// Mod `values_as_stored` (`hooks::valuation`): a player's value is his stored value.
    pub values_as_stored: bool,
    /// Each player's rating level in the mod's database (`hooks::rating_level`): the baseline his
    /// value moves from (`hooks::valuation`).
    pub value_base_levels: Vec<f32>,
    /// Mod wages.csv (`hooks::wage`): player id -> weekly wage in GBP and contract years left.
    pub wages: HashMap<u32, (u32, Option<u32>)>,
    /// Mod funds.csv (`hooks::funds`): club record index -> starting funds in GBP.
    pub funds: HashMap<u16, u32>,
    /// Guest words a mod rewrote in data the game reads (`ModSpace::data_words`: the sponsor offer
    /// table), written again after a state load: a state saved without the mod restores the old ones.
    pub after_load_writes: Vec<(u32, u32)>,
    /// Mod sponsors.csv (`hooks::sponsor_shirt_set`): club record index -> shirt and ground
    /// sponsorship in GBP per season.
    pub sponsors: HashMap<u16, (u32, u32)>,
    /// Mod wage_budgets.csv (`hooks::wage_budget_set`): club record index -> the least weekly wage
    /// budget in GBP.
    pub wage_budgets: HashMap<u16, u32>,
    /// GBP per stored unit of a sponsorship deal's weekly amount (`hooks::sponsor_*`; vanilla 10).
    pub sponsor_unit: u32,

    /// Mod squad_overflow.csv (`hooks::player_club`, #538): player id -> club record index, for
    /// players a club owns beyond its 32-id squad array.
    pub squad_overflow: HashMap<u16, u16>,
    /// `hooks::player_club`'s club uids by record index, found once each.
    pub squad_overflow_uids: HashMap<u16, u16>,
    /// `hooks::fence_watchdog` (LMA_FENCE_NAP, #312): per thread, the GPU read pointer at its last
    /// fence poll and how many polls it has stood still.
    pub fence_polls: HashMap<usize, (u32, u32)>,
    /// A hook asked for the current thread to sleep until this guest time before going on.
    pub nap_until: Option<u64>,
    /// With `nap_until`: wake early once one of these guest words differs from its value here
    /// (`Wait::GpuMoved`, the native fence wait).
    pub nap_watch: Option<([u32; 2], [u32; 2])>,
    /// LMA_AUDIO=1 (#199): the XAudio render driver (`crate::audio`).
    pub audio: crate::audio::Audio,
    /// LMA_CP_THREAD=1: the command processor's own thread, in live play (`crate::cp_thread`).
    cp: Option<crate::cp_thread::CpThread>,
    /// LMA_CP_THREAD: 0 off, 1 on in live play, 2 = "check": every run on the thread and waited
    /// for at once, in any mode (deterministic runs must stay bit-exact: tests the thread's
    /// memory view and write ordering).
    cp_thread_env: u8,
    /// `gpu.interrupt_cb != 0`, kept here for the scheduler (VdSetGraphicsInterruptCallback,
    /// state loads).
    interrupt_cb_set: bool,
    /// The interrupt handler returned and the command processor hasn't been told yet (it is
    /// told when no run is in progress, before the next interrupt is delivered).
    irq_returned: bool,
    /// LMA_COROUTINES=1 (#6, fast play build M1): each guest thread's slices run on its own
    /// coroutine stack (`slice`). The dev build switches only at slice boundaries, where that
    /// stack is empty, so this is bit-identical; the fast build will suspend mid-call instead.
    coros: Vec<Option<Coro>>,
    /// LMA_THREADS=N (#39, real time only): other Ready guest threads' recompiled code runs on
    /// N-1 host workers (`crate::par`); None when serial.
    #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
    par: Option<crate::par::Pool>,
    /// A worker's guest thread stopped (thread, why): reported by the next `run_inner` round.
    par_stop: Option<(usize, Stop)>,
    use_coros: bool,
    /// The frame LMA_SWAP_LOG last reported.
    swap_logged: u64,
    /// Real time: the audio callback's thread gets every other slice while it is ready.
    audio_priority: bool,
    /// The thread the round robin last picked (with the audio priority, the turn order skips
    /// the audio thread's extra slices).
    rr_last: usize,
    thunk_range: (u32, u32),
    pub(crate) heap: u32,
    /// XNotify messages waiting for the title: (id, param).
    notifications: std::collections::VecDeque<(u32, u32)>,
    /// NtAllocateVirtualMemory regions (base -> size) and committed 4 KB pages, so commits
    /// inside a reserved region keep their address and re-committed pages come back zeroed.
    vregions: BTreeMap<u32, u32>,
    committed: std::collections::HashSet<u32>,
    phys: u32,
    /// MmAllocatePhysicalMemoryEx blocks in use (address -> size) and freed ranges (start -> size),
    /// reused first-fit. Without reuse the bump pointer passed 0xA000_0000 + 512 MB after about a
    /// week of in-game days and new blocks aliased live ones through the 0xC0../0xE0.. mirrors (#60).
    phys_live: BTreeMap<u32, u32>,
    phys_free: BTreeMap<u32, u32>,
    /// Freed physical blocks (freed at guest time, address, size) wait `PHYS_QUARANTINE` before
    /// reuse (#199; on by default, LMA_PHYS_QUARANTINE=0 turns it off). The game keeps using some
    /// memory for a moment after
    /// freeing it (a sound's voice data while the pre-match screen loads textures); reused at once,
    /// that sound plays texture bytes.
    phys_quarantine: std::collections::VecDeque<(u64, u32, u32)>,
    quarantine: bool,
    small: u32,
    tls_slots: u32,
    pub calls: BTreeMap<&'static str, u64>,
    pub log: Vec<String>,
    image_base: u32,
    header_fields: HashMap<u32, u32>,
    pub vfs: Vfs,
    /// Optional PC histogram (sampled every 64 instructions) for profiling.
    pub pc_samples: Option<HashMap<(usize, u32), u64>>,
    pub gpu: Gpu,
    /// Debug: stop when any thread reaches this PC.
    pub break_at: Option<u32>,
    /// Debug: log every store to this address (pc, value).
    pub watch: Option<u32>,
    /// Debug: after `break_at` is hit, log changes to this GPR on that thread instead of stopping.
    pub reg_watch: Option<(usize, u64)>, // (register, instructions left)
    reg_watch_thread: Option<usize>,
    pub input: crate::input::Input,
    pub(crate) interrupt_thread: Option<usize>,
    /// Next guest ThreadId (KTHREAD+0x14C), never reused.
    pub(crate) next_tid: u32,
    next_vblank: u64,
    /// Real-time pacing (#56). Guest time (`now()`, 50 MHz ticks) follows the host clock unless
    /// `deterministic` (LMA_DETERMINISTIC=1): then it is the instruction count, bit-exact.
    pub deterministic: bool,
    rt_epoch: std::time::Instant,
    /// Guest time at `rt_epoch`.
    rt_offset: u64,
    /// Latest guest time handed out, so time never runs backwards.
    time_last: u64,
    slice_start: u64,
    rt_base: u64,
    /// Adaptive frame skip (real-time mode): render a guest frame only as often as the host can
    /// afford, so the game itself keeps running at its own frame rate and frame-bound animation
    /// stays at original speed. The live window turns it on; see `adapt_frame_skip`.
    pub adaptive_skip: bool,
    /// Real-time journal (#108): records what host timing decides (LMA_RECORD), or replays it.
    pub journal: Option<crate::journal::Journal>,
    /// Open XamContentCreateEnumerator handles: the packages to list and how many went out.
    content_enums: HashMap<u32, (Vec<crate::content::Package>, usize)>,
    skip: FrameSkip,
}

/// State of the adaptive frame skip (see `adapt_frame_skip`).
#[derive(Default)]
struct FrameSkip {
    on: bool,
    /// Render one guest frame out of `every`.
    every: usize,
    /// Current measuring window: start and guest frame count at the start.
    window: Option<(std::time::Instant, u64)>,
    /// Guest frame rate measured with each `every` (index), and when.
    fps: [f64; 7],
    seen: [Option<std::time::Instant>; 7],
}

#[derive(Debug)]
pub enum Exit {
    Returned,
    Limit,
    Stopped { thread: usize, stop: Stop },
    Terminated(&'static str),
    Deadlock,
    Breakpoint(u32),
}

impl Machine {
    pub fn new(x: &xex::Xex, game_dir: &std::path::Path, user_dir: &std::path::Path) -> Self {
        let mut mem = Memory::new();
        mem.write(x.image_base, &x.image);
        let mut thunks = HashMap::new();
        let mut var = VAR_BASE;
        for imp in x.imports() {
            let (name, is_var) = exports::lookup(&imp.library, imp.ordinal).unwrap_or(("unknown", false));
            match imp.kind {
                1 => {
                    thunks.insert(imp.address, Import { library: imp.library.clone(), name });
                }
                _ if is_var => {
                    // point the game's slot at zeroed storage for the variable
                    mem.write32(imp.address, var);
                    if name == "VdGpuClockInMHz" {
                        mem.write32(var, 500);
                    }
                    if name == "XboxKrnlVersion" {
                        mem.write(var, &[2, 0, 0x20, 0x77, 0x80, 0, 0, 0]); // 2.0.8311.0
                    }
                    var += 0x100;
                }
                _ => {}
            }
        }
        let header_fields = HashMap::from([(0x0001_0100, x.entry_point), (0x0001_0201, x.image_base)]);
        let mut m = Self {
            blocks: Vec::new(),
            timing: Default::default(),
            interp_steps: 0,
            thread_instr: Vec::new(),
            misses: None,
            cpu: Cpu::default(),
            mem,
            threads: Vec::new(),
            cur: 0,
            clock: 0,
            handles: HashMap::new(),
            next_handle: 0xF000_0000,
            tls_size: 0x100,
            tls_init: Vec::new(),
            thunk_range: (thunks.keys().copied().min().unwrap_or(1), thunks.keys().copied().max().unwrap_or(0)),
            thunks,
            hooks: crate::hooks::installed().into_iter().collect(),
            hook_calls: HashMap::new(),
            badge_entries: HashMap::new(),
            income_scale: 1.0,
            gate_scale: None,
            values_as_stored: false,
            value_base_levels: Vec::new(),
            wages: HashMap::new(),
            funds: HashMap::new(),
            after_load_writes: Vec::new(),
            sponsors: HashMap::new(),
            wage_budgets: HashMap::new(),
            sponsor_unit: 10,

            squad_overflow: HashMap::new(),
            squad_overflow_uids: HashMap::new(),
            fence_polls: HashMap::new(),
            nap_until: None,
            nap_watch: None,
            audio: crate::audio::Audio::new(),
            cp: None,
            cp_thread_env: match std::env::var("LMA_CP_THREAD").as_deref() {
                Ok("1") => 1,
                Ok("check") => 2,
                _ => 0,
            },
            interrupt_cb_set: false,
            irq_returned: false,
            coros: Vec::new(),
            #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
            par: None,
            par_stop: None,
            swap_logged: 0,
            use_coros: std::env::var("LMA_COROUTINES").as_deref() == Ok("1"),
            audio_priority: std::env::var("LMA_AUDIO_PRIORITY").as_deref() != Ok("0"),
            rr_last: 0,
            heap: HEAP_BASE,
            notifications: Default::default(),
            vregions: BTreeMap::new(),
            committed: Default::default(),
            phys: PHYS_BASE,
            phys_live: BTreeMap::new(),
            phys_free: BTreeMap::new(),
            phys_quarantine: Default::default(),
            quarantine: std::env::var("LMA_PHYS_QUARANTINE").as_deref() != Ok("0"),
            small: SMALL_BASE,
            tls_slots: 0,
            calls: BTreeMap::new(),
            log: Vec::new(),
            image_base: x.image_base,
            header_fields,
            vfs: Vfs::new(game_dir, user_dir),
            pc_samples: None,
            gpu: Gpu::default(),
            break_at: None,
            watch: None,
            reg_watch: None,
            reg_watch_thread: None,
            input: crate::input::Input::new(),
            interrupt_thread: None,
            next_tid: 1,
            next_vblank: crate::threads::CLOCK_HZ / 60,
            deterministic: std::env::var("LMA_DETERMINISTIC").is_ok_and(|v| v != "0"),
            rt_epoch: std::time::Instant::now(),
            rt_offset: 0,
            time_last: 0,
            slice_start: 0,
            adaptive_skip: false,
            journal: None,
            content_enums: HashMap::new(),
            skip: FrameSkip::default(),
            rt_base: 0,
        };
        m.spawn(x.entry_point, 0, 0, false, "main".into());
        m.cpu = std::mem::take(&mut m.threads[0].cpu);
        if m.audio.enabled {
            m.xma_init();
        }
        m
    }

    /// Audio on or off before the game starts, overriding LMA_AUDIO (lma-app defaults to on, #280).
    pub fn set_audio(&mut self, on: bool) {
        self.audio.enabled = on;
        if on && self.audio.xma_base == 0 {
            self.xma_init();
        }
    }

    /// LMA_AUDIO=1 (#199): the XMA context array, and its physical address in the audio hardware's
    /// register 0x1800, which the game reads at start-up to number the contexts it gets.
    fn xma_init(&mut self) {
        self.audio.xma_base = self.phys_alloc(crate::audio::XMA_CONTEXTS * 64, 0x1000);
        self.mem.write32(crate::audio::XMA_REGS + 0x1800, (self.audio.xma_base & 0x1FFF_FFFF).swap_bytes());
        crate::mmio::set_xma_trap(true);
    }

    pub(crate) fn alloc_small(&mut self, size: u32) -> u32 {
        let a = (self.small + 15) & !15;
        self.small = a + size;
        self.mem.write(a, &vec![0; size as usize]);
        a
    }

    pub(crate) fn alloc(next: &mut u32, size: u32, align: u32) -> u32 {
        let align = align.max(0x1000);
        let a = (*next + align - 1) & !(align - 1);
        *next = a + ((size + 0xFFF) & !0xFFF);
        a
    }

    /// Physical memory: reuse a freed range first-fit, else bump. Never hands out memory past the
    /// 512 MB of physical RAM (that would alias live blocks); returns 0 (allocation failure) instead.
    fn phys_alloc(&mut self, size: u32, align: u32) -> u32 {
        let size = (size.max(1) + 0xFFF) & !0xFFF;
        let align = align.max(0x1000);
        // blocks that have waited out their quarantine are free again
        if self.quarantine {
            let now = self.now();
            while let Some(&(t, a, s)) = self.phys_quarantine.front() {
                if now.saturating_sub(t) < PHYS_QUARANTINE {
                    break;
                }
                self.phys_quarantine.pop_front();
                self.phys_merge_free(a, s);
            }
        }
        let fit = self.phys_free.iter().find_map(|(&s, &len)| {
            let a = (s + align - 1) & !(align - 1);
            (a >= s && (a - s) as u64 + size as u64 <= len as u64).then_some((s, len, a))
        });
        let addr = if let Some((s, len, a)) = fit {
            self.phys_free.remove(&s);
            if a > s {
                self.phys_free.insert(s, a - s);
            }
            if a + size < s + len {
                self.phys_free.insert(a + size, s + len - (a + size));
            }
            a
        } else {
            let a = (self.phys + align - 1) & !(align - 1);
            if a as u64 + size as u64 > PHYS_BASE as u64 + 0x2000_0000 {
                // out of fresh memory: the oldest quarantined blocks first, one at a time
                if let Some((_, qa, qs)) = self.phys_quarantine.pop_front() {
                    self.phys_merge_free(qa, qs);
                    return self.phys_alloc(size, align);
                }
                eprintln!("runtime: physical memory exhausted: {size:#x} bytes requested, {} blocks live", self.phys_live.len());
                return 0;
            }
            self.phys = a + size;
            a
        };
        self.phys_live.insert(addr, size);
        // a new block reads as zero, also where an earlier one lived (as Xenia's physical heap,
        // which decommits freed pages): the game relies on it. Its sound thread copies a new
        // voice's XMA settings to the hardware context before the voice is set up, so a reused
        // block made the decoder play the old block's bytes (the pre-match screech, the stutter
        // loop and the broken analysis speech, #199)
        self.mem.write(addr, &vec![0u8; size as usize]);
        addr
    }

    /// A profile is signed in on each connected controller port (port 0 always; LMA_PADS adds more).
    fn local_user(&self, user: u32) -> bool {
        self.input.pads.get(user as usize).is_some_and(|p| p.is_some())
    }

    /// Free a physical block and merge it with neighbouring free ranges.
    fn phys_release(&mut self, addr: u32) -> bool {
        let Some(size) = self.phys_live.remove(&addr) else { return false };
        if self.quarantine {
            self.phys_quarantine.push_back((self.now(), addr, size));
        } else {
            self.phys_merge_free(addr, size);
        }
        true
    }

    /// Whether `[addr, addr + len)` lies inside one allocated physical block.
    fn phys_is_live(&self, addr: u32, len: u32) -> bool {
        self.phys_live.range(..=addr).next_back().is_some_and(|(&a, &s)| addr as u64 + len as u64 <= a as u64 + s as u64)
    }

    /// When the quarantined block holding `addr` was freed (`now()`), if it is one.
    fn phys_freed_at(&self, addr: u32) -> Option<u64> {
        self.phys_quarantine.iter().rev().find(|&&(_, a, s)| addr >= a && addr - a < s).map(|&(t, _, _)| t)
    }

    /// A free physical range back in the free list, merged with its neighbours.
    fn phys_merge_free(&mut self, addr: u32, size: u32) {
        let (mut start, mut end) = (addr, addr + size);
        if let Some((&s, &len)) = self.phys_free.range(..start).next_back() {
            if s + len == start {
                self.phys_free.remove(&s);
                start = s;
            }
        }
        if let Some(len) = self.phys_free.remove(&end) {
            end += len;
        }
        if end == self.phys {
            self.phys = start; // the top of the bump region is free again
        } else {
            self.phys_free.insert(start, end - start);
        }
    }

    /// OBJECT_ATTRIBUTES { root, ANSI_STRING* name, attributes } -> path string.
    fn object_path(&mut self, oa: u32) -> String {
        let name = self.mem.read32(oa + 4);
        self.ansi_string(name)
    }

    fn ansi_string(&mut self, p: u32) -> String {
        let len = self.mem.read16(p) as usize;
        let buf = self.mem.read32(p + 4);
        let mut b = vec![0u8; len];
        self.mem.read(buf, &mut b);
        String::from_utf8_lossy(&b).into_owned()
    }

    fn iosb(&mut self, iosb: u32, status: u32, info: u32) {
        if iosb != 0 {
            self.mem.write32(iosb, status);
            self.mem.write32(iosb + 4, info);
        }
    }

    /// FILE_NETWORK_OPEN_INFORMATION: 4 times, allocation size, end of file, attributes.
    fn write_network_info(&mut self, p: u32, size: u64, dir: bool) {
        for k in 0..4 {
            self.mem.write64(p + 8 * k, 0x01C8_0000_0000_0000);
        }
        self.mem.write64(p + 32, (size + 0xFFF) & !0xFFF);
        self.mem.write64(p + 40, size);
        self.mem.write32(p + 48, if dir { 0x10 } else { 0x80 });
    }

    // ---- scheduler -------------------------------------------------------------------
    /// Whether guest thread `t` is running on a host worker (#39).
    pub(crate) fn par_running(&self, t: usize) -> bool {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if let Some(p) = &self.par {
            return p.is_running(t);
        }
        let _ = t;
        false
    }

    /// One scheduling round's worker work (#39): take back the threads whose runs finished,
    /// report a stop, and hand other Ready threads on recompiled code to free workers. Starts the
    /// pool on first use with LMA_THREADS=N (N > 1), only in real time and not while journaling.
    fn par_round(&mut self) -> Option<Exit> {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        {
            static THREADS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            let n = *THREADS.get_or_init(|| std::env::var("LMA_THREADS").ok().and_then(|v| v.parse().ok()).unwrap_or(1));
            let allowed = n > 1 && !self.deterministic && self.journal.is_none() && !self.use_coros;
            if !allowed {
                if self.par.is_some() {
                    self.par_join();
                    self.par = None;
                    crate::rec::PARALLEL.store(false, std::sync::atomic::Ordering::SeqCst);
                }
                return None;
            }
            if self.par.is_none() {
                self.par = Some(crate::par::Pool::new(n - 1, &self.mem));
                crate::rec::PARALLEL.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            self.par_collect(None);
            if let Some((thread, stop)) = self.par_stop.take() {
                return Some(Exit::Stopped { thread, stop });
            }
            static BUDGET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
            let budget = *BUDGET.get_or_init(|| std::env::var("LMA_PAR_SLICE").ok().and_then(|v| v.parse().ok()).unwrap_or(500_000));
            for t in 0..self.threads.len() {
                let p = self.par.as_mut().unwrap();
                if !p.has_free() {
                    break;
                }
                if t == self.cur || self.threads[t].state != State::Ready || p.is_running(t) || crate::rec::block_at(self.threads[t].cpu.pc).is_none() {
                    continue;
                }
                let mut cpu = std::mem::take(&mut self.threads[t].cpu);
                cpu.reserve = None; // a context switch, as `resume`
                p.send(crate::par::Job { t, cpu, budget, rt_base: self.rt_base });
            }
        }
        None
    }

    /// Take back finished worker runs, waiting up to `wait` for one; whether any came back.
    fn par_collect(&mut self, wait: Option<std::time::Duration>) -> bool {
        let mut any = false;
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        {
            let mut wait = wait;
            while let Some(d) = self.par.as_mut().and_then(|p| p.recv(wait.take())) {
                any = true;
                self.threads[d.t].cpu = d.cpu;
                self.clock += d.used;
                if self.thread_instr.len() <= d.t {
                    self.thread_instr.resize(d.t + 1, 0);
                }
                self.thread_instr[d.t] += d.used;
                if let Some(stop) = d.stop {
                    self.par_stop.get_or_insert((d.t, stop));
                }
            }
        }
        let _ = wait;
        any
    }

    /// With workers running, wait up to `nap` for one to finish (instead of sleeping); whether
    /// there were workers to wait for.
    fn par_wait(&mut self, nap: std::time::Duration) -> bool {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        if self.par.as_ref().is_some_and(|p| p.in_flight > 0) {
            self.par_collect(Some(nap));
            return true;
        }
        std::thread::sleep(nap.min(std::time::Duration::from_millis(4)));
        let _ = nap;
        false
    }

    /// Wait for every worker run to finish and take its thread back (#39).
    pub(crate) fn par_join(&mut self) {
        #[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
        while self.par.as_ref().is_some_and(|p| p.in_flight > 0) {
            self.par_collect(Some(std::time::Duration::from_secs(1)));
        }
    }

    fn park(&mut self) {
        std::mem::swap(&mut self.cpu, &mut self.threads[self.cur].cpu);
    }
    fn resume(&mut self, i: usize) {
        self.cur = i;
        std::mem::swap(&mut self.cpu, &mut self.threads[i].cpu);
        self.cpu.reserve = None; // a context switch breaks lwarx/stwcx. reservations
    }

    /// Run until `limit` total guest instructions, the title exits, or something stops it.
    pub fn run(&mut self, limit: u64) -> Exit {
        let e = self.run_inner(limit);
        self.par_join();
        self.cp_join(); // callers see the GPU and guest memory with no command-processor run pending
        self.irq_return();
        e
    }

    /// LMA_CP_THREAD=1: wait for the command processor's run in progress, if any, and apply its
    /// guest-memory writes in order. Everything that touches `gpu` outside `service_gpu` calls
    /// this first.
    pub fn cp_join(&mut self) {
        if let Some(cp) = self.cp.as_mut() {
            let mem = &mut self.mem;
            cp.collect(|a, bytes| mem.write(a, bytes));
        }
    }

    fn run_inner(&mut self, limit: u64) -> Exit {
        let started = std::time::Instant::now();
        let frames0 = self.gpu.stats.frames;
        loop {
            if self.clock >= limit {
                return Exit::Limit;
            }
            if let Some(e) = self.par_round() {
                return e;
            }
            // one clock read per phase boundary (~2,000 slices a frame in a match: the reads were
            // ~2% of the game thread)
            let mut t0 = std::time::Instant::now();
            if self.threads[self.cur].state == State::Ready {
                let _p = crate::prof::span(crate::prof::Cat::Slice);
                let (t, c0) = (self.cur, self.clock);
                let r = self.slice();
                let t1 = std::time::Instant::now();
                self.timing[0] += t1 - t0;
                t0 = t1;
                if self.thread_instr.len() <= t {
                    self.thread_instr.resize(t + 1, 0);
                }
                self.thread_instr[t] += self.clock - c0;
                if let Some(e) = r {
                    return e;
                }
            }
            self.service_gpu(false);
            self.service_audio();
            self.timing[2] += t0.elapsed();
            // pick the next runnable thread, round robin
            self.park();
            loop {
                if self.wake() {
                    break;
                }
                // the GPU's interrupt callback as last set (read without waiting for a
                // command-processor run in progress)
                let vblank = self.interrupt_cb_set;
                let d = match (self.next_deadline(), vblank) {
                    (Some(d), true) => Some(d.min(self.next_vblank)),
                    (None, true) => Some(self.next_vblank),
                    (d, false) => d,
                };
                // the audio callback's next frame (LMA_AUDIO=1)
                let d = match (d, self.audio_deadline()) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                match d {
                    Some(d) => {
                        if self.deterministic {
                            self.clock = self.clock.max(d); // everyone sleeps: jump ahead
                        } else {
                            // real time (#56): wait on the host until guest time reaches the deadline/vblank,
                            // in short naps so input and the caller stay responsive
                            let now = self.sync_time();
                            if d > now {
                                if !self.journal.as_ref().is_some_and(|j| j.is_replay()) {
                                    let _p = crate::prof::span(crate::prof::Cat::Sleep);
                                    let nap = std::time::Duration::from_nanos(((d - now) * 20).min(4_000_000));
                                    // a worker's thread coming back may be runnable: wait for that instead
                                    if self.par_wait(nap) {
                                        continue;
                                    }
                                }
                                self.sync_time();
                            }
                            // back to the caller once the guest has presented a frame (a game
                            // napping in a fence wait, LMA_FENCE_NAP, executes too few
                            // instructions to reach `limit`: without this the window showed
                            // 20 fps, one frame per 50 ms budget), or after 50 ms anyway. Only with
                            // the nap hook installed: without it the loop runs as before.
                            if started.elapsed() > std::time::Duration::from_millis(50) || (self.gpu.stats.frames != frames0 && self.hooks.contains_key(&crate::hooks::FENCE_WATCHDOG)) {
                                self.resume(self.cur);
                                return Exit::Limit;
                            }
                        }
                        self.service_gpu(true);
                        self.service_audio();
                        if self.clock >= limit {
                            self.resume(self.cur);
                            return Exit::Limit;
                        }
                    }
                    None => {
                        // nothing to wait for here, but workers still running: their threads
                        if self.par_wait(std::time::Duration::from_millis(1)) {
                            continue;
                        }
                        self.resume(self.cur);
                        return Exit::Deadlock;
                    }
                }
            }
            // the audio callback every other slice while it is ready (real time): on the Xbox it
            // has a hardware thread of its own, and round robin among a busy game's threads left
            // its mix behind real time. Not strictly first: a callback spinning on a lock another
            // thread holds must not starve that thread. LMA_AUDIO_PRIORITY=0 turns it off.
            let next = self.pick_next();
            self.resume(next);
        }
    }

    /// The next thread to run (some thread is Ready): round robin after the current one, except
    /// that in real time the audio callback's thread gets every other slice while it is ready.
    fn pick_next(&mut self) -> usize {
        let n = self.threads.len();
        let ready = |i: usize| self.threads[i].state == State::Ready && !self.par_running(i);
        if self.deterministic || !self.audio_priority {
            return (1..=n).map(|k| (self.cur + k) % n).find(|&i| ready(i)).unwrap();
        }
        let audio = self.audio.thread.filter(|&t| ready(t));
        if let Some(t) = audio.filter(|&t| t != self.cur) {
            return t;
        }
        // the other threads take turns in their own order, from the last one that ran (going on
        // from the audio thread would always favour the threads right after it); the audio
        // thread again only if nothing else is ready
        let next = (1..=n).map(|k| (self.rr_last + k) % n).find(|&i| ready(i) && Some(i) != audio).or(audio).unwrap();
        if Some(next) != audio {
            self.rr_last = next;
        }
        next
    }

    /// Install a native hook at a guest function (`crate::hooks`), before `install_blocks`, which
    /// leaves hooked addresses out of the block table.
    pub fn hook(&mut self, addr: u32, f: crate::hooks::Hook) {
        debug_assert!(crate::hooks::ADDRESSES.contains(&addr), "{addr:#x} is not in guest::hooks::ADDRESSES");
        self.hooks.insert(addr, f);
    }

    /// Register recompiled functions (the generated `game-blocks` crate's `PARTS`).
    pub fn install_blocks(&mut self, parts: &[&[(u32, crate::rec::FuncFn)]]) -> usize {
        let mut n = 0;
        for list in parts {
            for &(pc, f) in *list {
                let i = (pc.wrapping_sub(BLOCK_BASE) >> 2) as usize;
                if pc >= BLOCK_BASE && i < (1 << 24) && !self.thunks.contains_key(&pc) && !self.hooks.contains_key(&pc) {
                    if self.blocks.len() <= i {
                        self.blocks.resize(i + 1, None);
                    }
                    self.blocks[i] = Some(f);
                    n += 1;
                }
            }
        }
        for (pc, f) in crate::hooks::native_blocks() {
            let i = (pc - BLOCK_BASE) as usize >> 2;
            if self.blocks.len() > i {
                self.blocks[i] = Some(f);
            }
        }
        // values_as_stored's valuation natively, the game's function behind it for the players
        // without a stored value (with LMA_NATIVE_MOD=hook the hook took the address instead)
        let i = (crate::hooks::VALUATION - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (self.values_as_stored && !self.hooks.contains_key(&crate::hooks::VALUATION), self.blocks.get(i).copied()) {
            *crate::hooks::VALUE_LEVELS.write().unwrap() = self.value_base_levels.clone();
            self.blocks[i] = Some(crate::hooks::valuation_block_for(game));
        }
        // squad_overflow.csv's club lookup natively, the game's lookup behind it for everyone else
        let i = (crate::hooks::PLAYER_CLUB - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (!self.squad_overflow.is_empty() && !self.hooks.contains_key(&crate::hooks::PLAYER_CLUB), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::player_club_block_for(game, &self.squad_overflow));
        }
        // wages.csv's getter natively, the game's getter behind it for every other answer (#653)
        let i = (crate::hooks::WAGE - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (!self.wages.is_empty() && !self.hooks.contains_key(&crate::hooks::WAGE), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::wage_block_for(game, &self.wages));
        }
        // the AI transfer search's filter rejects what it cheaply can natively, the game's function behind it (#39)
        let i = (crate::hooks::SEARCH_FILTER - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (crate::hooks::search_prefilter_on() && !self.hooks.contains_key(&crate::hooks::SEARCH_FILTER), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::search_filter_block_for(game));
        }
        // the league-phase European Cup (#703): its round-42 draw native, the game's draws behind it
        let i = (crate::hooks::EUROPEAN_CUP_DRAW - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (crate::hooks::LEAGUE_PHASE.load(std::sync::atomic::Ordering::Relaxed), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::european_cup_draw_block_for(game));
            if crate::hooks::LEAGUE_LIST.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                let list = self.alloc_small(0x408);
                crate::hooks::LEAGUE_LIST.store(list, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let i = (crate::hooks::GROUP_END - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (crate::hooks::LEAGUE_PHASE.load(std::sync::atomic::Ordering::Relaxed), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::group_end_block_for(game));
        }
        let i = (crate::hooks::STANDINGS_READ - BLOCK_BASE) as usize >> 2;
        if let (true, Some(Some(game))) = (crate::hooks::LEAGUE_PHASE.load(std::sync::atomic::Ordering::Relaxed), self.blocks.get(i).copied()) {
            self.blocks[i] = Some(crate::hooks::standings_read_block_for(game));
        }
        // mod `bench` 6-9 (#530): the substitution can bring on the first reserves after the
        // matchday 16; the lookup is native and the builder clears their stored slot images
        if crate::hooks::BENCH_EXTRA.load(std::sync::atomic::Ordering::Relaxed) > 0 {
            let i = (crate::hooks::SLOT_LOOKUP - BLOCK_BASE) as usize >> 2;
            if let Some(Some(_)) = self.blocks.get(i) {
                self.blocks[i] = Some(crate::hooks::slot_lookup_block_for());
            }
            let i = (crate::hooks::MATCH_BUILD - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::match_build_block_for(game));
            }
            // the slot getter answers 16.. with the reserves' virtual slots (the UI side, #530)
            let i = (crate::hooks::SLOT_GET - BLOCK_BASE) as usize >> 2;
            if let Some(Some(_)) = self.blocks.get(i) {
                if crate::hooks::VIRTUAL_SLOTS.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                    let v = self.alloc_small(2 * 4 * 0x400);
                    crate::hooks::VIRTUAL_SLOTS.store(v, std::sync::atomic::Ordering::Relaxed);
                }
                self.blocks[i] = Some(crate::hooks::slot_get_block_for());
            }
            // and a match-mode team screen shows the extra rows
            let i = (crate::hooks::TEAM_SCREEN_SETUP - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::team_screen_block_for(game));
            }
            // and its line drawer looks the scrolled rows up
            let i = (crate::hooks::TEAM_ROW_PLAYER - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::team_row_player_block_for(game));
            }
            let i = (crate::hooks::TEAM_ROW_STATUS - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::team_row_status_block_for(game));
            }
        }
        // formations.csv with more than 14 shapes: 14 and 15 have no position offsets in the record (#749)
        if guest::rec::FORMATION_COUNT.load(std::sync::atomic::Ordering::Relaxed) > guest::rec::FORMATION_COUNT_VANILLA {
            use crate::hooks::*;
            let addrs = [FORMATION_OFFSET_X_GET, FORMATION_OFFSET_X_SET, FORMATION_OFFSET_Y_GET, FORMATION_OFFSET_Y_SET, FORMATION_DIAGRAM_SLOT];
            let game: Vec<crate::rec::FuncFn> = addrs.iter().filter_map(|&a| self.blocks.get((a - BLOCK_BASE) as usize >> 2).copied().flatten()).collect();
            if let Ok(game) = <[crate::rec::FuncFn; 5]>::try_from(game) {
                for (a, f) in formation_offset_blocks_for(game) {
                    self.blocks[(a - BLOCK_BASE) as usize >> 2] = Some(f);
                }
            }
        }
        // LMA_NATIVE_AUDIO=1: the game's XACT calls mirrored to the host engine (#772)
        if crate::native_audio::enabled() {
            let game: std::collections::HashMap<u32, crate::rec::FuncFn> = crate::native_audio::HOOKS.iter().filter_map(|&a| self.blocks.get((a - BLOCK_BASE) as usize >> 2).copied().flatten().map(|f| (a, f))).collect();
            if game.len() == crate::native_audio::HOOKS.len() {
                crate::native_audio::init(self.vfs.game_dir(), self.vfs.user_dir());
                for (a, f) in crate::native_audio::blocks_for(game) {
                    self.blocks[(a - BLOCK_BASE) as usize >> 2] = Some(f);
                }
            } else {
                eprintln!("native audio: {} of {} XACT entry points found, off", game.len(), crate::native_audio::HOOKS.len());
            }
        }
        // mod `rating_wage_scale`: the wage from a player's rating x the scale (#704)
        if crate::hooks::RATING_WAGE_SCALE.load(std::sync::atomic::Ordering::Relaxed) != 1000 {
            let i = (crate::hooks::RATING_WAGE - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::rating_wage_block_for(game));
            }
        }
        // mod `ai_wage_offers`: wages rounded up to WAGE_MAX, AI offers at least the wage + 10%
        if crate::hooks::AI_WAGE_OFFERS.load(std::sync::atomic::Ordering::Relaxed) {
            let i = (crate::hooks::WAGE_ROUND - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::wage_round_block_for(game));
            }
            let i = (crate::hooks::AI_OFFER - BLOCK_BASE) as usize >> 2;
            if let Some(Some(game)) = self.blocks.get(i).copied() {
                self.blocks[i] = Some(crate::hooks::ai_offer_block_for(game));
            }
        }
        // recompiled functions call each other natively through this table (rec::call)
        crate::rec::set_table(Box::leak(self.blocks.clone().into_boxed_slice()));
        n
    }

    /// One slice of the current guest thread: directly, or on its coroutine (LMA_COROUTINES=1).
    fn slice(&mut self) -> Option<Exit> {
        if !self.use_coros {
            return self.run_slice();
        }
        let i = self.cur;
        if self.coros.len() <= i {
            self.coros.resize_with(i + 1, || None);
        }
        // taken out while it runs, so the Machine it is handed never aliases its own slot
        let mut co = self.coros[i].take().unwrap_or_else(new_coro);
        let me: *mut Machine = self;
        let r = match co.resume(me) {
            corosensei::CoroutineResult::Yield(r) => r,
            corosensei::CoroutineResult::Return(()) => unreachable!("guest thread coroutines never return"),
        };
        // a thread that finished in this slice keeps no coroutine (its stack unwinds here)
        if self.threads[i].state != State::Done {
            self.coros[i] = Some(co);
        }
        r
    }

    fn run_slice(&mut self) -> Option<Exit> {
        let fast = self.break_at.is_none() && self.watch.is_none() && self.reg_watch.is_none() && self.pc_samples.is_none();
        // LMA_SLICE=<instructions> (debug, #205): a different scheduling slice, to test whether
        // something depends on how guest threads interleave
        static SL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let slice = *SL.get_or_init(|| std::env::var("LMA_SLICE").ok().and_then(|v| v.parse().ok()).unwrap_or(SLICE));
        let end = self.clock + slice;
        if !self.deterministic {
            self.sync_time();
        }
        self.cpu.rt = !self.deterministic;
        self.cpu.rt_base = self.rt_base;
        self.cpu.slice_len = slice as i64;
        self.cpu.slice_end = end;
        while self.clock < end {
            let pc = self.cpu.pc;
            self.cpu.timebase = if self.deterministic { self.clock } else { self.rt_base + ((self.clock - self.slice_start) >> crate::rec::RT_SHIFT) };
            if fast {
                let i = (pc.wrapping_sub(BLOCK_BASE) >> 2) as usize;
                self.interp_steps += 1;
                if let Some(Some(_)) = self.blocks.get(i) {
                    self.interp_steps -= 1;
                } else if let Some(h) = self.misses.as_mut() {
                    if self.interp_steps & 255 == 0 {
                        *h.entry(pc).or_default() += 1;
                    }
                }
                if let Some(Some(f)) = self.blocks.get(i) {
                    let f = *f;
                    let budget = (end - self.clock) as i64;
                    let mut left = budget;
                    let r = f(&mut self.cpu, &mut self.mem, pc, &mut left);
                    self.clock += (budget - left).max(0) as u64;
                    match r {
                        Ok(next) => self.cpu.pc = next,
                        Err(stop) => return Some(Exit::Stopped { thread: self.cur, stop }),
                    }
                    continue;
                }
            }
            if pc == RETURN_MAGIC {
                return Some(Exit::Returned);
            }
            if pc == crate::audio::AUDIO_MAGIC {
                self.threads[self.cur].state = State::Suspended(1); // until the next audio frame
                return None;
            }
            if pc == INTERRUPT_MAGIC {
                self.threads[self.cur].state = State::Suspended(1);
                self.irq_returned = true;
                if !self.cp.as_ref().is_some_and(|cp| cp.busy) {
                    self.irq_return(); // else when the command processor's run is collected
                }
                return None;
            }
            if pc == THREAD_EXIT_MAGIC {
                let code = self.cpu.gpr[3] as u32;
                self.finish_thread(self.cur, code);
                return None;
            }
            // a hooked guest function: the native replacement, unless it declines
            if let Some(hook) = self.hooks.get(&pc).copied() {
                let handled = hook(self);
                if handled {
                    self.cpu.pc = self.cpu.lr as u32;
                }
                // a hook may put the thread to sleep (the fence nap, #312): it resumes here, at
                // the function (declined) or after it (handled), once the time has passed
                let watch = self.nap_watch.take();
                if let Some(d) = self.nap_until.take() {
                    if d > self.now() {
                        self.threads[self.cur].state = State::Waiting(match watch {
                            Some((words, last)) => Wait::GpuMoved { words, last, deadline: d },
                            None => Wait::Nap(d),
                        });
                        return None;
                    }
                }
                if handled {
                    continue;
                }
            }
            let maybe_thunk = pc >= self.thunk_range.0 && pc <= self.thunk_range.1;
            if let Some(imp) = if maybe_thunk { self.thunks.get(&pc) } else { None } {
                let name = imp.name;
                *self.calls.entry(name).or_default() += 1;
                let watched = self.watch.map(|w| (w, self.mem.read32(w)));
                let t0 = std::time::Instant::now();
                let _p = crate::prof::span(crate::prof::Cat::Kernel);
                let flow = self.hle(name);
                drop(_p);
                self.timing[1] += t0.elapsed();
                if let Some((w, before)) = watched {
                    let after = self.mem.read32(w);
                    if after != before {
                        self.log.push(format!("[{:>11}] t{} WATCH {w:#010x}: {before:#x} -> {after:#x} in kernel call {name}", self.clock, self.cur));
                    }
                }
                let flow = match flow {
                    Ok(f) => f,
                    Err(e) => return Some(e),
                };
                self.cpu.pc = self.cpu.lr as u32;
                match flow {
                    Flow::Continue => {}
                    Flow::Yield => return None,
                    Flow::ExitThread => {
                        let code = self.cpu.gpr[3] as u32;
                        self.finish_thread(self.cur, code);
                        return None;
                    }
                    Flow::Block(w) => match self.try_wait(&w) {
                        Some(status) => self.cpu.gpr[3] = status as u64,
                        None => {
                            self.threads[self.cur].state = State::Waiting(w);
                            return None;
                        }
                    },
                }
                continue;
            }
            #[cfg(feature = "rwatch")]
            {
                let hits = guest::mem::RWATCH_HITS.swap(0, std::sync::atomic::Ordering::Relaxed);
                if hits != 0 {
                    static PRINTED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                    if PRINTED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 300 {
                        println!("rwatch: {hits} load(s) of {:#010x} before pc {pc:#010x} thread {} clock {}", guest::mem::RWATCH.load(std::sync::atomic::Ordering::Relaxed), self.cur, self.clock);
                    }
                }
            }
            if self.clock & 63 == 0 {
                if let Some(h) = self.pc_samples.as_mut() {
                    *h.entry((self.cur, pc)).or_default() += 1;
                }
            }
            if self.break_at == Some(pc) {
                if self.reg_watch.is_some() {
                    self.break_at = None;
                    self.reg_watch_thread = Some(self.cur);
                } else {
                    return Some(Exit::Breakpoint(pc));
                }
            }
            if let (Some((reg, left)), Some(t)) = (self.reg_watch, self.reg_watch_thread) {
                if t == self.cur && left > 0 {
                    let before = self.cpu.gpr[reg];
                    let r1_before = self.cpu.gpr[1] as u32;
                    let slot = self.mem.read64(r1_before.wrapping_sub(0x30));
                    if let Err(stop) = self.cpu.step(&mut self.mem) {
                        return Some(Exit::Stopped { thread: self.cur, stop });
                    }
                    self.clock += 1;
                    let after = self.cpu.gpr[reg];
                    if after != before {
                        let w = self.mem.read32(pc);
                        self.log.push(format!("REGWATCH r{reg}: {before:#x} -> {after:#x} at pc {pc:#010x} ({w:08x} {}) r1 {:#x}", ppc::lookup(w).unwrap_or("?"), self.cpu.gpr[1]));
                        self.log.push(format!("    [r1-0x30 = {:#x}] = {slot:#x} at clock {}", r1_before.wrapping_sub(0x30), self.clock));
                    }
                    self.reg_watch = Some((reg, left - 1));
                    continue;
                }
            }
            if let Some(w) = self.watch {
                let before = self.mem.read32(w);
                if let Err(stop) = self.cpu.step(&mut self.mem) {
                    return Some(Exit::Stopped { thread: self.cur, stop });
                }
                let after = self.mem.read32(w);
                if after != before {
                    self.log.push(format!("[{:>11}] t{} WATCH {w:#010x}: {before:#x} -> {after:#x} at pc {pc:#010x} lr {:#010x}", self.clock, self.cur, self.cpu.lr));
                }
                self.clock += 1;
                continue;
            }
            if let Err(stop) = self.cpu.step(&mut self.mem) {
                return Some(Exit::Stopped { thread: self.cur, stop });
            }
            self.clock += 1;
        }
        None
    }

    /// Guest time in 50 MHz ticks (#56): the instruction count when deterministic, otherwise
    /// the current slice's real-time base plus instructions run since >> RT_SHIFT (the same formula
    /// the recompiled code uses through `rec::timebase_at`).
    pub fn now(&self) -> u64 {
        if self.deterministic {
            self.clock
        } else {
            self.rt_base + (self.clock.saturating_sub(self.slice_start) >> crate::rec::RT_SHIFT)
        }
    }

    /// Stop journaling: after a replay, real time carries on from the replayed guest time.
    pub fn end_journal(&mut self) {
        if self.journal.take().is_some_and(|j| j.is_replay()) {
            self.rt_offset = 0;
            self.rt_offset = self.now().saturating_sub(self.host_ticks());
        }
    }

    fn host_ticks(&self) -> u64 {
        self.rt_offset + self.rt_epoch.elapsed().as_nanos() as u64 / 20
    }

    /// Start a new real-time slice: base = host clock, never behind time already handed out.
    fn sync_time(&mut self) -> u64 {
        let host = self.host_ticks();
        let host = match self.journal.as_mut() {
            Some(j) => j.time(self.clock, host),
            None => host,
        };
        let t = host.max(self.now()).max(self.time_last);
        self.rt_base = t;
        self.slice_start = self.clock;
        self.time_last = t;
        t
    }

    /// Adaptive frame skip, called after every GPU pump. When rendering costs more than the guest
    /// frame it belongs to, the game's own loop (and everything tied to frames: menu flicker,
    /// scrolling text) slows down. Instead we rasterise/present only every Nth guest frame so the
    /// game keeps its natural frame rate. Every half second the guest frame rate is recorded for
    /// the current N; N becomes the smallest value within 7% of the best rate seen, and
    /// neighbouring values are re-measured once their reading is older than 5 s, so N follows the
    /// load both ways (every frame rendered whenever the host keeps up).
    fn adapt_frame_skip(&mut self) {
        // off by default (LMA_MAX_SKIP=2..6 allows skipping): since fast fences and the
        // asynchronous present (#183, #184) a match shows 31-52 fps on the Mac with every frame
        // drawn, while 1 in 2 showed half of a game rate that stayed at 20-25. (Before them, 1 in 2
        // kept the game at 29 fps and showed 14; 1 in 1 slowed the game to 18.)
        // read once: this runs every command-processor run (~1000 a frame), and the lookup was
        // 0.6% of the game thread in a match
        static MAX_EVERY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
        let max_every = *MAX_EVERY.get_or_init(|| std::env::var("LMA_MAX_SKIP").ok().and_then(|v| v.parse().ok()).unwrap_or(1).clamp(1, 6));
        let frames = self.gpu.stats.frames;
        let now = std::time::Instant::now();
        let k = &mut self.skip;
        if !k.on {
            *k = FrameSkip { on: true, every: 1, ..Default::default() };
        }
        let (start, start_frames) = *k.window.get_or_insert((now, frames));
        let secs = (now - start).as_secs_f64();
        if secs >= 0.5 {
            k.window = Some((now, frames));
            if secs < 2.0 {
                // longer gaps are pauses (loading, a state load): no reading
                let fps = (frames - start_frames) as f64 / secs;
                let e = k.every;
                let fresh = |s: &Option<std::time::Instant>| s.is_some_and(|t| (now - t).as_secs_f64() < 5.0);
                // average with the previous reading only while that one is fresh: a stale one (e.g.
                // the stall right after a state load) would hold N back for many probes (#56: menus
                // showed 60 guest fps but presented only every 2nd frame)
                k.fps[e] = if fresh(&k.seen[e]) { 0.5 * (k.fps[e] + fps) } else { fps };
                k.seen[e] = Some(now);
                let best = (1..=max_every).filter(|&i| fresh(&k.seen[i])).map(|i| k.fps[i]).fold(0.0, f64::max);
                let pick = (1..=max_every).find(|&i| fresh(&k.seen[i]) && k.fps[i] >= 0.93 * best).unwrap_or(e).min(max_every);
                // probe a neighbour whose reading is missing or stale: more skipping while the game
                // is below 57 fps, less skipping otherwise
                k.every = if k.fps[pick] < 57.0 && pick < max_every && !fresh(&k.seen[pick + 1]) {
                    pick + 1
                } else if pick > 1 && !fresh(&k.seen[pick - 1]) {
                    pick - 1
                } else {
                    pick
                };
                static SKIP_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                if *SKIP_LOG.get_or_init(|| std::env::var_os("LMA_SKIP_LOG").is_some()) {
                    eprintln!("frame skip: 1 in {e}: {fps:.0} fps; best {best:.0}; next 1 in {}", k.every);
                }
            }
        }
        // the GPU skips frame-aligned: draws and presents only on frames divisible by N
        self.gpu.render_every = k.every as u64;
        self.gpu.present_every = k.every as u64;
    }

    /// Run the GPU ring, raise vblanks on the virtual clock, deliver interrupts.
    /// LMA_AUDIO=1: when an audio frame is due and the callback isn't running, run it on the
    /// audio thread (pc = callback, r3 = its argument, returning to `AUDIO_MAGIC`).
    fn service_audio(&mut self) {
        let (Some((cb, arg)), Some(t)) = (self.audio.client, self.audio.thread) else { return };
        let now = self.now();
        let period = crate::audio::Audio::period(crate::threads::CLOCK_HZ);
        if now < self.audio.next_frame || self.threads[t].state != State::Suspended(1) {
            return;
        }
        // behind after a stall (a slow frame): catch up frame by frame, so nothing is lost; far
        // behind (a pause, loading): skip to now rather than calling back many times. "Far" is
        // ~0.5 s, what lma-app's output keeps in hand: cutscenes and replays stall the game
        // 100-280 ms, and the old limit (48 frames, 256 ms) dropped the sound of the longest.
        let late = now - self.audio.next_frame;
        self.audio.late_max = self.audio.late_max.max(late);
        self.audio.late_recent = self.audio.late_recent.max(late);
        if late > crate::audio::CATCH_UP_MAX * period {
            let n = late / period;
            self.audio.dropped += n;
            eprintln!("audio: {} ms behind after a stall: {n} frames skipped", late / (crate::threads::CLOCK_HZ / 1000));
            self.audio.next_frame = now + period;
        } else {
            // logged once per stall, not for every frame it catches up
            let behind = late > crate::threads::CLOCK_HZ / 10;
            if behind && !self.audio.catching_up {
                eprintln!("audio: {} ms behind after a stall: catching up", late / (crate::threads::CLOCK_HZ / 1000));
            }
            self.audio.catching_up = behind;
            self.audio.next_frame += period;
        }
        // LMA_NATIVE_AUDIO=2: the frame is the host engine's; the guest callback (its mixer, the XMA
        // hardware it drives) no longer runs
        if crate::native_audio::replaces_mixer() {
            if !crate::native_audio::device_pulls() {
                self.audio.submit(&[0u8; 6 * crate::audio::FRAME * 4]);
            }
            self.audio.calls += 1;
            return;
        }
        // the XMA hardware decodes ahead of the mix that reads its output
        let _p = crate::prof::span(crate::prof::Cat::Xma);
        self.service_xma();
        drop(_p);
        // the previous callback submitted nothing: a frame of silence in its place
        if self.audio.calls > 0 && self.audio.frames == self.audio.frames_at_call {
            self.audio.silence_fill();
        }
        self.audio.frames_at_call = self.audio.frames;
        let th = &mut self.threads[t].cpu;
        th.pc = cb;
        th.gpr[3] = arg as u64;
        th.lr = crate::audio::AUDIO_MAGIC as u64;
        self.threads[t].state = State::Ready;
        self.audio.calls += 1;
    }

    /// The XMA decoder hardware (#199): applies the lock (stop), clear and kick (start) registers
    /// the game wrote since the last audio frame, then runs every started context.
    fn service_xma(&mut self) {
        use crate::audio::{XMA_CONTEXTS, XMA_REGS};
        use crate::xma::{Context, Mem};
        if self.audio.xma_base == 0 {
            return;
        }
        if self.audio.xma_hw.len() < XMA_CONTEXTS as usize {
            self.audio.xma_hw.resize_with(XMA_CONTEXTS as usize, Default::default);
        }
        // LMA_XMA_DUMP=<dir>: every context's decoder input and output (also after a state load,
        // which brings its own contexts)
        if let Some(dir) = &self.audio.xma_dump_dir {
            for (i, ch) in self.audio.xma_hw.iter_mut().enumerate().filter(|(_, c)| c.dump_to.is_none()) {
                ch.dump_to = Some(dir.join(format!("ctx{i:03}")));
            }
        }
        // every store to the kick / lock / clear registers since the last frame, in order (the
        // game writes them one context at a time: as memory, only the last would show)
        for (ea, bits) in crate::mmio::take_xma_commands() {
            let (kind, base) = match ea - XMA_REGS {
                r @ 0x1940..0x1968 => ("kick", r - 0x1940),
                r @ 0x1A40..0x1A68 => ("lock", r - 0x1A40),
                r @ 0x1A80..0x1AA8 => ("clear", r - 0x1A80),
                _ => continue,
            };
            if self.audio.xma_log {
                self.log.push(format!("[{:>11}] xma {kind} word {}: {bits:#x}", self.clock, base / 4));
            }
            for b in (0..32).filter(|b| bits >> b & 1 != 0) {
                let Some(ch) = self.audio.xma_hw.get_mut((base / 4 * 32 + b) as usize) else { continue };
                match kind {
                    "kick" => (ch.enabled, ch.kicked) = (true, true),
                    "lock" => ch.enabled = false,
                    _ => ch.clear(),
                }
            }
        }
        // physical addresses, as the hardware sees them
        struct Phys<'a>(&'a mut Memory);
        impl Mem for Phys<'_> {
            fn read(&mut self, phys: u32, buf: &mut [u8]) {
                self.0.read(0xA000_0000 | phys, buf);
            }
            fn write(&mut self, phys: u32, buf: &[u8]) {
                self.0.write(0xA000_0000 | phys, buf);
            }
        }
        // LMA_XMA_LOG: every allocated context's words whenever they change (what the game sets up)
        if self.audio.xma_log {
            for i in 0..XMA_CONTEXTS {
                if !self.audio.xma_used[i as usize] {
                    continue;
                }
                let addr = self.audio.xma_context(i);
                let mut w = [0u32; 16];
                for (k, v) in w.iter_mut().enumerate() {
                    *v = self.mem.read32(addr + 4 * k as u32);
                }
                let ch = &mut self.audio.xma_hw[i as usize];
                if w != ch.logged {
                    ch.logged = w;
                    let c = crate::xma::Context::parse(&w);
                    self.log.push(format!("[{:>11}] xma {i} words: {:08x?} | in {:?} {:?} read {:#x}, out w{} r{} of {} valid {}, enabled {}", self.clock, &w[..10], c.input_valid, c.input_packets, c.input_read_offset, c.output_write, c.output_read, c.output_blocks, c.output_valid, ch.enabled));
                }
            }
        }
        for i in 0..XMA_CONTEXTS {
            if !(self.audio.xma_hw[i as usize].enabled || self.audio.xma_hw[i as usize].kicked) {
                continue;
            }
            let addr = self.audio.xma_context(i);
            let mut w = [0u32; 16];
            for (k, v) in w.iter_mut().enumerate() {
                *v = self.mem.read32(addr + 4 * k as u32);
            }
            let mut ctx = Context::parse(&w);
            // the buffers this context reads and writes should be live physical blocks: memory the
            // game has freed (or that is waiting out its quarantine) means a use after free (#199).
            // An output ring only counts while there is input to decode into it (the game frees a
            // finished voice's ring with the context still pointing there; nothing is written)
            let dead = [(ctx.input_valid[0], ctx.input_ptr[0], ctx.input_packets[0] * 2048, "input 0"), (ctx.input_valid[1], ctx.input_ptr[1], ctx.input_packets[1] * 2048, "input 1"), (ctx.output_valid && ctx.input_valid.iter().any(|&v| v), ctx.output_ptr, ctx.output_blocks * 256, "output")]
                .into_iter()
                .filter(|&(valid, ptr, len, _)| valid && len > 0 && !self.phys_is_live(PHYS_BASE | ptr, len))
                .map(|(_, ptr, len, what)| (what, ptr, len, self.phys_freed_at(PHYS_BASE | ptr)))
                .collect::<Vec<_>>();
            let now = self.now();
            let ch = &mut self.audio.xma_hw[i as usize];
            for &(what, ptr, len, freed) in &dead {
                ch.dead_reads += 1;
                if ch.dead_seen.insert(ptr) {
                    let when = freed.map_or("not a physical block".to_string(), |t| format!("freed {:.2} s ago", now.saturating_sub(t) as f64 / crate::threads::CLOCK_HZ as f64));
                    self.log.push(format!("[{:>11}] xma {i}: {what} at {ptr:#x}+{len:#x} is not live memory ({when})", self.clock));
                }
            }
            ch.ensure_decoder((if ctx.stereo { 2 } else { 1 }, crate::xma::rate_hz(ctx.sample_rate)), crate::xma::decoder);
            let before = ctx;
            let counts = |c: &crate::xma::Channel| (c.loop_jumps, c.resets, c.decode_errors);
            let (c0, stale) = (counts(ch), ch.check_reads(&ctx));
            if self.audio.xma_log && self.audio.calls % 1000 == 0 {
                self.log.push(format!("[{:>11}] xma {i}: in {:?} {:?} read {:#x} loop {}, out w{} r{} of {} valid {}, {} packets, peak {}", self.clock, ctx.input_valid, ctx.input_packets, ctx.input_read_offset, ctx.loop_count, ctx.output_write, ctx.output_read, ctx.output_blocks, ctx.output_valid, ch.packets, ch.peak));
            }
            if ch.work(&mut ctx, &mut Phys(&mut self.mem)) {
                if self.audio.xma_log && before.input_valid != ctx.input_valid {
                    self.log.push(format!("[{:>11}] xma {i}: inputs {:?} -> {:?} (packets {:?}, ptr {:x?}), loop {} {:#x}..{:#x}, out {}/{} of {}", self.clock, before.input_valid, ctx.input_valid, before.input_packets, before.input_ptr, before.loop_count, w[3] & 0x3FF_FFFF, w[4] & 0x3FF_FFFF, ctx.output_write, ctx.output_read, ctx.output_blocks));
                }
                ctx.store(&mut w);
                for (k, v) in w.iter().enumerate() {
                    self.mem.write32(addr + 4 * k as u32, *v);
                }
                self.audio.xma_updates += 1;
            }
            ch.note_ring(&ctx);
            let c1 = counts(ch);
            if self.audio.xma_log && (stale > 0 || c1 != c0) {
                self.log.push(format!("[{:>11}] xma {i}: {stale} stale blocks read, loop jumps +{}, resets +{}, decode errors +{}", self.clock, c1.0 - c0.0, c1.1 - c0.1, c1.2 - c0.2));
            }
        }
    }

    /// When the audio callback is next due (None without a client or with it running).
    fn audio_deadline(&self) -> Option<u64> {
        let t = self.audio.thread?;
        (self.audio.client.is_some() && self.threads[t].state == State::Suspended(1)).then_some(self.audio.next_frame)
    }

    fn service_gpu(&mut self, parked: bool) {
        // LMA_SWAP_LOG=1 (debug, #205): guest time and instruction clock at each frame swap
        // read once: service_gpu runs at every slice boundary, and with the variable unset
        // swap_logged never catches up, so getenv ran on every call (4.3% of day processing on
        // the PC)
        static SWAP_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if self.gpu.stats.frames != self.swap_logged && *SWAP_LOG.get_or_init(|| std::env::var_os("LMA_SWAP_LOG").is_some()) {
            self.swap_logged = self.gpu.stats.frames;
            eprintln!("swap {} t={} clock={}", self.gpu.stats.frames, self.now(), self.clock);
        }
        // asynchronous GPU fences only in real time, and not while journaling: their landing
        // times are the host's (#108)
        let realtime = !self.deterministic && self.journal.is_none();
        // LMA_CP_THREAD=1, live play only: the previous run's results now, the next run on the
        // command-processor thread during the next slice (`crate::cp_thread`)
        let check = self.cp_thread_env == 2;
        let threaded = (realtime && self.cp_thread_env == 1) || check;
        // a run still going: carry on with guest code, its results come at a later boundary
        // (in order: nothing else touches the GPU meanwhile)
        if threaded && self.cp.as_ref().is_some_and(|cp| !cp.ready()) {
            return;
        }
        self.cp_join();
        self.irq_return();
        if check {
            // the run on the thread, collected at once: the inline order exactly
            let cp = self.cp.get_or_insert_with(|| crate::cp_thread::CpThread::new(self.mem.reader()));
            self.gpu.realtime = realtime;
            cp.start(&mut self.gpu);
            self.cp_join();
        } else if !threaded {
            self.gpu.realtime = realtime;
            self.gpu.pump(&mut self.mem);
        }
        self.service_gpu_rest(parked);
        if threaded && !check {
            let cp = self.cp.get_or_insert_with(|| crate::cp_thread::CpThread::new(self.mem.reader()));
            crate::prof::CP_THREAD.store(true, std::sync::atomic::Ordering::Relaxed);
            self.gpu.realtime = realtime;
            self.gpu.wptr_snapshot = Some(self.mem.read32(gpu::GPU_MMIO_BASE + gpu::CP_RB_WPTR as u32 * 4));
            cp.start(&mut self.gpu);
        }
    }

    /// The interrupt handler has returned: the command processor may continue (unless more
    /// interrupts wait for delivery).
    fn irq_return(&mut self) {
        if std::mem::take(&mut self.irq_returned) && self.gpu.pending_interrupts.is_empty() {
            crate::prof::end(crate::prof::Cat::IrqHandler);
            self.gpu.blocked = false;
        }
    }

    /// `service_gpu` after the command processor: frame skip, vblanks, interrupt delivery.
    fn service_gpu_rest(&mut self, parked: bool) {
        if let Some(j) = self.journal.as_mut().filter(|j| j.is_replay()) {
            if let Some(every) = j.replay_skip(self.clock) {
                self.gpu.render_every = every;
                self.gpu.present_every = every;
            }
        } else if self.adaptive_skip && !self.deterministic {
            self.adapt_frame_skip();
            if let Some(j) = self.journal.as_mut() {
                j.record_skip(self.clock, self.gpu.render_every);
            }
        }
        let now = self.now();
        // LMA_VBLANK_DIV=<n> (#39, opt-in, default 1): a vblank every n 60 Hz periods, so a screen
        // that draws once per vblank (the day's processing screen) draws n times less often
        // LMA_VBLANK_HZ=<n> (experiment, #171 gap check): the vblank rate itself, default 60
        let period = crate::threads::CLOCK_HZ / vblank_hz() * vblank_div();
        if !self.deterministic && now > self.next_vblank + 2 * period {
            self.next_vblank = now - (now - self.next_vblank) % period; // fell behind: one vblank, resync
        }
        while now >= self.next_vblank {
            self.next_vblank += period;
            self.gpu.vblank();
        }
        let Some(t) = self.interrupt_thread else { return };
        if self.threads[t].state != State::Suspended(1) || (t == self.cur && !parked) {
            return;
        }
        if let Some((source, cpu)) = self.gpu.pending_interrupts.pop_front() {
            let th = &mut self.threads[t].cpu;
            th.pc = self.gpu.interrupt_cb;
            th.gpr[3] = source as u64;
            th.gpr[4] = self.gpu.interrupt_user as u64;
            th.gpr[1] = th.gpr[1].max(0); // keep its own stack
            th.lr = INTERRUPT_MAGIC as u64;
            let pcr = th.gpr[13] as u32;
            self.mem.write8(pcr + 0x10C, cpu as u8); // current processor number
            self.threads[t].state = State::Ready;
            crate::prof::end(crate::prof::Cat::IrqQueue);
            crate::prof::begin(crate::prof::Cat::IrqHandler);
        }
    }

    fn arg(&self, i: usize) -> u32 {
        self.cpu.gpr[3 + i] as u32
    }

    /// Finish an XOVERLAPPED request at once: result, length, extended error, then its event.
    /// XCONTENT_DATA: device id, content type, display name (UTF-16, 128), file name (42 chars).
    fn read_content_data(&mut self, p: u32) -> crate::content::Package {
        let kind = self.mem.read32(p + 4);
        let display: Vec<u16> = (0..128).map(|i| self.mem.read16(p + 8 + 2 * i)).take_while(|&c| c != 0).collect();
        let file = self.mem.cstr(p + 0x108, 42);
        crate::content::Package { kind, file, display: String::from_utf16_lossy(&display) }
    }

    fn write_content_data(&mut self, p: u32, c: &crate::content::Package) {
        self.mem.write(p, &[0; XCONTENT_DATA_SIZE as usize]);
        self.mem.write32(p, HDD_DEVICE_ID);
        self.mem.write32(p + 4, c.kind);
        for (i, u) in c.display.encode_utf16().take(127).enumerate() {
            self.mem.write16(p + 8 + 2 * i as u32, u);
        }
        let f = c.file.as_bytes();
        self.mem.write(p + 0x108, &f[..f.len().min(41)]);
    }

    fn complete_overlapped(&mut self, ov: u32, result: u32, len: u32) {
        if ov == 0 {
            return;
        }
        self.mem.write32(ov, result);
        self.mem.write32(ov + 4, len);
        self.mem.write32(ov + 0x18, result);
        let event = self.mem.read32(ov + 0xC);
        if event != 0 {
            if let Some(o) = self.object_of(event) {
                self.set_signal(o, 1);
            }
        }
    }

    fn ret(&mut self, v: u32) {
        self.cpu.gpr[3] = v as u64;
    }

    /// High-level emulation of one kernel/XAM export.
    fn hle(&mut self, name: &'static str) -> Result<Flow, Exit> {
        // calls that read or change another thread's state wait for the workers (#39)
        if name.contains("Thread") && !matches!(name, "KeDelayExecutionThread" | "KeGetCurrentThread" | "KeTlsGetValue") {
            self.par_join();
        }
        if matches!(name, "VdInitializeRingBuffer" | "VdEnableRingBufferRPtrWriteBack" | "VdSetGraphicsInterruptCallback") {
            self.cp_join(); // these set up the command processor
        }
        let mut args = [0u32; 10];
        for (i, slot) in args.iter_mut().enumerate() {
            *slot = if i < 8 {
                self.arg(i)
            } else {
                // arguments 9+ live in the caller's parameter area
                let sp = self.cpu.gpr[1] as u32;
                self.mem.read32(sp + 0x54 + 8 * (i as u32 - 8))
            };
        }
        let a = |i: usize| args[i];
        let mut note = None;
        let mut flow = Flow::Continue;
        match name {
            // ---- memory -----------------------------------------------------------------
            "NtAllocateVirtualMemory" => {
                // (PVOID* base, SIZE_T* size, type, protect): base 0 = new region; otherwise
                // reserve/commit pages at that address inside an earlier reservation.
                let (base_ptr, size_ptr, kind) = (a(0), a(1), a(2));
                let want = self.mem.read32(base_ptr);
                let size = self.mem.read32(size_ptr);
                let (addr, len) = if want == 0 {
                    let len = (size + 0xFFF) & !0xFFF;
                    let addr = Self::alloc(&mut self.heap, len, 0x10000);
                    self.vregions.insert(addr, len);
                    (addr, len)
                } else {
                    let start = want & !0xFFF;
                    (start, ((want + size + 0xFFF) & !0xFFF) - start)
                };
                if kind & 0x1000 != 0 {
                    // MEM_COMMIT: pages that are not committed yet read as zero
                    for page in (addr..addr + len).step_by(0x1000) {
                        if self.committed.insert(page) && want != 0 {
                            self.mem.write(page, &[0u8; 0x1000]);
                        }
                    }
                }
                self.mem.write32(base_ptr, addr);
                self.mem.write32(size_ptr, len);
                note = Some(format!("base {want:#x} size {size:#x} type {kind:#x} -> {addr:#010x}+{len:#x}"));
                self.ret(0);
            }
            "NtFreeVirtualMemory" => {
                // (PVOID* base, SIZE_T* size, type): MEM_DECOMMIT 0x4000 / MEM_RELEASE 0x8000
                let (base_ptr, size_ptr, kind) = (a(0), a(1), a(2));
                let base = self.mem.read32(base_ptr) & !0xFFF;
                let mut len = (self.mem.read32(size_ptr) + 0xFFF) & !0xFFF;
                if len == 0 {
                    len = self.vregions.get(&base).copied().unwrap_or(0);
                }
                for page in (base..base.saturating_add(len)).step_by(0x1000) {
                    self.committed.remove(&page);
                }
                if kind & 0x8000 != 0 {
                    self.vregions.remove(&base);
                }
                self.mem.write32(size_ptr, len);
                note = Some(format!("{base:#010x}+{len:#x} type {kind:#x}"));
                self.ret(0);
            }
            "MmFreePhysicalMemory" => {
                let addr = a(1);
                let freed = self.phys_release(addr);
                note = Some(format!("{addr:#010x} ({})", if freed { "freed" } else { "unknown block" }));
                self.ret(0);
            }
            "ExFreePool" => self.ret(0),
            "MmAllocatePhysicalMemoryEx" => {
                let (size, align) = (a(1), a(5));
                let addr = self.phys_alloc(size, align);
                note = Some(format!("{size:#x} -> {addr:#010x}"));
                self.ret(addr);
            }
            "ExAllocatePool" => {
                let addr = Self::alloc(&mut self.heap, a(0), 0x10);
                self.ret(addr);
            }
            "XamAlloc" => {
                let addr = Self::alloc(&mut self.heap, a(1), 0x10);
                self.mem.write32(a(2), addr);
                self.ret(0);
            }
            "MmGetPhysicalAddress" => {
                let v = a(0);
                let phys = if v >= 0xE000_0000 { (v - 0xE000_0000 + 0x1000) & 0x1FFF_FFFF } else { v & 0x1FFF_FFFF };
                self.ret(phys);
            }
            "MmQueryAddressProtect" => self.ret(4), // PAGE_READWRITE
            // ---- process / module -------------------------------------------------------
            "KeGetCurrentProcessType" => self.ret(1),
            "KeQueryPerformanceFrequency" => self.ret(crate::threads::CLOCK_HZ as u32),
            "KeQuerySystemTime" => {
                let t = self.system_time_100ns();
                self.mem.write64(a(0), t);
            }
            "XexGetModuleHandle" => {
                self.mem.write32(a(1), self.image_base);
                self.ret(0);
            }
            "RtlImageXexHeaderField" => {
                let v = self.header_fields.get(&a(1)).copied().unwrap_or(0);
                self.ret(v);
            }
            // ---- TLS (per thread) -------------------------------------------------------
            "KeTlsAlloc" => {
                self.tls_slots += 1;
                self.ret(self.tls_slots - 1);
            }
            "KeTlsGetValue" => {
                let v = self.threads[self.cur].tls.get(a(0) as usize).copied().unwrap_or(0);
                self.ret(v);
            }
            "KeTlsSetValue" => {
                let tls = &mut self.threads[self.cur].tls;
                let i = a(0) as usize;
                if tls.len() <= i {
                    tls.resize(i + 1, 0);
                }
                tls[i] = a(1);
                self.ret(1);
            }
            "KeTlsFree" => self.ret(1),
            // ---- threads ----------------------------------------------------------------
            "ExCreateThread" => {
                // (handle*, stack_size, thread_id*, xapi_startup, start, ctx, flags)
                let (xapi, start, ctx, flags) = (a(3), a(4), a(5), a(6));
                let suspended = flags & 1 != 0;
                let idx = if xapi != 0 {
                    self.spawn(xapi, start, ctx, suspended, format!("{start:#010x}"))
                } else {
                    self.spawn(start, ctx, 0, suspended, format!("{start:#010x}"))
                };
                let k = self.threads[idx].kthread;
                let h = self.new_handle(k);
                if a(0) != 0 {
                    self.mem.write32(a(0), h);
                }
                if a(2) != 0 {
                    self.mem.write32(a(2), idx as u32 + 1);
                }
                // the flags only with LMA_AUDIO=1, so audio-off kernel logs stay as the baselines
                let fl = if self.audio.enabled { format!(" flags {flags:#x}") } else { String::new() };
                note = Some(format!("thread {idx} start {start:#010x} ctx {ctx:#010x}{fl}{}", if suspended { " (suspended)" } else { "" }));
                // LMA_AUDIO=1 (#199, experiment): the processor the flags' affinity byte picks, as
                // the thread's current processor number (PCR+0x10C); the audio engine's worker
                // barrier waits for each processor's own byte. Off: every thread reports 0.
                if self.audio.enabled && flags >> 24 != 0 {
                    let pcr = self.threads[idx].cpu.gpr[13] as u32;
                    self.mem.write8(pcr + 0x10C, (flags >> 24).trailing_zeros() as u8);
                }
                self.ret(0);
            }
            "NtResumeThread" | "KeResumeThread" | "NtSuspendThread" => {
                let k = if name == "KeResumeThread" { Some(a(0)) } else { self.object_of(a(0)) };
                let idx = k.and_then(|k| self.thread_of_kthread(k));
                let mut prev = 0;
                if let Some(i) = idx {
                    let st = self.threads[i].state.clone();
                    if name == "NtSuspendThread" {
                        if st == State::Ready {
                            self.threads[i].state = State::Suspended(1);
                        }
                    } else if let State::Suspended(c) = st {
                        prev = c;
                        self.threads[i].state = if c <= 1 { State::Ready } else { State::Suspended(c - 1) };
                    }
                }
                if name != "KeResumeThread" && a(1) != 0 {
                    self.mem.write32(a(1), prev);
                }
                self.ret(if name == "KeResumeThread" { prev } else { 0 });
            }
            "ExTerminateThread" => flow = Flow::ExitThread,
            "ObReferenceObjectByHandle" => {
                let obj = self.object_of(a(0)).unwrap_or(0);
                if a(2) != 0 {
                    self.mem.write32(a(2), obj);
                }
                self.ret(if obj == 0 { 0xC000_0008 } else { 0 });
            }
            "KeSetAffinityThread" => {
                // (KTHREAD*, affinity mask, previous mask*)
                let t = self.thread_of_kthread(a(0));
                if self.audio.enabled {
                    note = Some(format!("thread {t:?} affinity {:#x}", a(1)));
                }
                if let (true, Some(i), true) = (self.audio.enabled, t, a(1) != 0) {
                    let pcr = self.threads[i].cpu.gpr[13] as u32;
                    self.mem.write8(pcr + 0x10C, a(1).trailing_zeros() as u8);
                }
                self.ret(0);
            }
            "ObDereferenceObject" | "KeSetBasePriorityThread" => self.ret(0),
            "KeDelayExecutionThread" => {
                let d = self.deadline(a(2)).unwrap_or(self.now());
                flow = if d <= self.now() { Flow::Yield } else { Flow::Block(Wait::Sleep(d)) };
            }
            // ---- events / semaphores ----------------------------------------------------
            "NtCreateEvent" => {
                // (handle*, obj_attr*, event_type, initial_state)
                let obj = self.new_object(a(2) as u8, a(3) as i32);
                let h = self.new_handle(obj);
                self.mem.write32(a(0), h);
                self.ret(0);
            }
            "NtSetEvent" | "KeSetEvent" => {
                let obj = if name == "KeSetEvent" { Some(a(0)) } else { self.object_of(a(0)) };
                let prev = obj.map_or(0, |o| self.signal_state(o));
                if let Some(o) = obj {
                    self.set_signal(o, 1);
                }
                if name == "NtSetEvent" && a(1) != 0 {
                    self.mem.write32(a(1), prev as u32);
                }
                self.ret(if name == "KeSetEvent" { prev as u32 } else { 0 });
            }
            "NtClearEvent" | "KeResetEvent" => {
                let obj = if name == "KeResetEvent" { Some(a(0)) } else { self.object_of(a(0)) };
                let prev = obj.map_or(0, |o| self.signal_state(o));
                if let Some(o) = obj {
                    self.set_signal(o, 0);
                }
                self.ret(if name == "KeResetEvent" { prev as u32 } else { 0 });
            }
            "KeInitializeSemaphore" => {
                // (sem*, count, limit)
                self.mem.write8(a(0), OBJ_SEMAPHORE);
                self.set_signal(a(0), a(1) as i32);
                self.mem.write32(a(0) + 16, a(2));
            }
            "KeReleaseSemaphore" => {
                // (sem*, increment, adjustment, wait)
                let prev = self.signal_state(a(0));
                self.set_signal(a(0), prev + a(2) as i32);
                self.ret(prev as u32);
            }
            "KeWaitForSingleObject" => {
                // (object*, reason, mode, alertable, timeout*)
                let deadline = self.deadline(a(4));
                flow = Flow::Block(Wait::Objects { ptrs: vec![a(0)], all: false, deadline });
            }
            "NtWaitForSingleObjectEx" => {
                // (handle, mode, alertable, timeout*)
                let deadline = self.deadline(a(3));
                match self.object_of(a(0)) {
                    Some(o) => flow = Flow::Block(Wait::Objects { ptrs: vec![o], all: false, deadline }),
                    None => self.ret(0xC000_0008),
                }
            }
            "KeWaitForMultipleObjects" | "NtWaitForMultipleObjectsEx" => {
                // Ke: (count, objects**, wait_type, reason, mode, alertable, timeout*, wait_blocks)
                // Nt: (count, handles*, wait_type, mode, alertable, timeout*)
                let count = a(0);
                let all = a(2) == 0;
                let timeout = if name.starts_with("Ke") { a(6) } else { a(5) };
                let deadline = self.deadline(timeout);
                let mut ptrs = Vec::new();
                for i in 0..count {
                    let v = self.mem.read32(a(1) + 4 * i);
                    let o = if name.starts_with("Ke") { Some(v) } else { self.object_of(v) };
                    ptrs.extend(o);
                }
                flow = Flow::Block(Wait::Objects { ptrs, all, deadline });
            }
            // ---- critical sections ------------------------------------------------------
            "RtlInitializeCriticalSection" => {
                self.mem.write(a(0), &[0; 28]);
                self.mem.write8(a(0), OBJ_NOTIFICATION_EVENT);
                self.mem.write32(a(0) + 16, u32::MAX); // lock count -1
            }
            "RtlEnterCriticalSection" => {
                let me = self.threads[self.cur].kthread;
                if !self.cs_try_enter(a(0), me) {
                    flow = Flow::Block(Wait::CritSec(a(0)));
                }
            }
            "RtlTryEnterCriticalSection" => {
                let me = self.threads[self.cur].kthread;
                let ok = self.cs_try_enter(a(0), me);
                self.ret(ok as u32);
            }
            "RtlLeaveCriticalSection" => self.cs_leave(a(0)),
            // ---- spin locks (previously no-ops: two threads could hold one; #60) ---------
            "KfAcquireSpinLock" | "KeAcquireSpinLockAtRaisedIrql" => {
                let me = self.threads[self.cur].kthread;
                if self.spin_try_acquire(a(0), me) {
                    self.ret(0); // previous IRQL
                } else {
                    flow = Flow::Block(Wait::SpinLock(a(0)));
                }
            }
            "KeTryToAcquireSpinLockAtRaisedIrql" => {
                let me = self.threads[self.cur].kthread;
                let ok = self.spin_try_acquire(a(0), me);
                self.ret(ok as u32);
            }
            "KfReleaseSpinLock" | "KeReleaseSpinLockFromRaisedIrql" => self.mem.write32(a(0), 0),
            // ---- strings / debug --------------------------------------------------------
            "RtlInitAnsiString" => {
                let (dst, src) = (a(0), a(1));
                let s = if src == 0 { String::new() } else { self.mem.cstr(src, 1024) };
                self.mem.write16(dst, s.len() as u16);
                self.mem.write16(dst + 2, s.len() as u16 + 1);
                self.mem.write32(dst + 4, src);
                note = Some(format!("\"{s}\""));
            }
            "DbgPrint" | "sprintf" | "_snprintf" | "_vsnprintf" => {
                // DbgPrint(fmt, ...), sprintf(buf, fmt, ...), _snprintf(buf, n, fmt, ...),
                // _vsnprintf(buf, n, fmt, va_list)
                let (buf, limit, fmt_i) = match name {
                    "DbgPrint" => (0, u32::MAX, 0),
                    "sprintf" => (a(0), u32::MAX, 1),
                    _ => (a(0), a(1), 2),
                };
                let text = if name == "_vsnprintf" {
                    let mut va = a(3);
                    crate::fmt::format(&mut self.mem, a(fmt_i), |m| {
                        let v = m.read64(va);
                        va += 8;
                        v
                    })
                } else {
                    // variadic: remaining GPR arguments, then the stack parameter area
                    let regs: Vec<u64> = (fmt_i + 1..8).map(|i| self.cpu.gpr[3 + i]).collect();
                    let sp = self.cpu.gpr[1] as u32;
                    let mut k = 0usize;
                    crate::fmt::format(&mut self.mem, a(fmt_i), |m| {
                        let v = if k < regs.len() { regs[k] } else { m.read64(sp + 0x54 - 4 + 8 * (k - regs.len()) as u32) };
                        k += 1;
                        v
                    })
                };
                if buf != 0 {
                    let mut bytes = text.clone().into_bytes();
                    let n = bytes.len().min(limit.saturating_sub(1) as usize);
                    bytes.truncate(n);
                    bytes.push(0);
                    self.mem.write(buf, &bytes);
                    self.ret(n as u32);
                } else {
                    self.ret(0);
                }
                note = Some(text.trim_end().to_string());
            }
            // ---- files ------------------------------------------------------------------
            "NtCreateFile" | "NtOpenFile" => {
                // NtCreateFile(handle*, access, obj_attr*, iosb*, alloc*, attrs, share, disposition, options)
                // NtOpenFile(handle*, access, obj_attr*, iosb*, share, options)
                let path = self.object_path(a(2));
                let write = a(1) & 0x4000_0116 != 0; // GENERIC_WRITE / FILE_WRITE_DATA / APPEND
                let disposition = if name == "NtCreateFile" { a(7) } else { 1 };
                let create = matches!(disposition, 2 | 3 | 5);
                let status = match self.vfs.open(&path, write, create) {
                    Ok(h) => {
                        self.mem.write32(a(0), h);
                        vfs::STATUS_SUCCESS
                    }
                    Err(e) => e,
                };
                self.iosb(a(3), status, 1);
                note = Some(format!("{path} -> {status:#x}"));
                self.ret(status);
            }
            "NtReadFile" | "NtWriteFile" => {
                // (handle, event, apc, apc_ctx, iosb*, buffer, length, offset*)
                let (h, event, iosb, buf, len, off_ptr) = (a(0), a(1), a(4), a(5), a(6), a(7));
                let off = (off_ptr != 0).then(|| self.mem.read64(off_ptr)).filter(|&o| o != u64::MAX - 1);
                let res = if name == "NtReadFile" {
                    let mut data = vec![0u8; len as usize];
                    self.vfs.read(h, &mut data, off).map(|n| {
                        self.mem.write(buf, &data[..n]);
                        n
                    })
                } else {
                    let mut data = vec![0u8; len as usize];
                    self.mem.read(buf, &mut data);
                    self.vfs.write(h, &data, off)
                };
                let (status, n) = match res {
                    Ok(n) => (vfs::STATUS_SUCCESS, n as u32),
                    Err(e) => (e, 0),
                };
                self.iosb(iosb, status, n);
                if let Some(o) = self.object_of(event) {
                    self.set_signal(o, 1); // I/O completes synchronously
                }
                let path = self.vfs.get(h).map(|f| f.guest_path.clone()).unwrap_or_default();
                // LMA_FILE_BUF=1 adds the guest buffer and the caller's return address, to find a file's
                // reader (LMA_WATCH on the buffer, the function at lr); off by default: the log's shape is
                // compared across runs
                static FILE_BUF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
                let buf_note = if *FILE_BUF.get_or_init(|| std::env::var_os("LMA_FILE_BUF").is_some()) {
                    let sp = self.cpu.gpr[1] as u32;
                    let chain: Vec<String> = crate::backchain::back_chain(&mut self.mem, sp, 6).iter().map(|a| format!("{a:#x}")).collect();
                    format!(" buf {buf:#x} lr {:#x} chain {}", self.cpu.lr, chain.join(","))
                } else { String::new() };
                note = Some(format!("{path} {len:#x}@{off:?} -> {n:#x} ({status:#x}){buf_note}"));
                self.ret(status);
            }
            "NtQueryInformationFile" => {
                // (handle, iosb*, info*, length, class)
                let (h, iosb, info, class) = (a(0), a(1), a(2), a(4));
                let status = match (self.vfs.size(h), class) {
                    (None, _) => vfs::STATUS_INVALID_HANDLE,
                    (Some((size, dir)), 5) => {
                        self.mem.write64(info, size);
                        self.mem.write64(info + 8, size);
                        self.mem.write32(info + 16, 1);
                        self.mem.write8(info + 20, 0);
                        self.mem.write8(info + 21, dir as u8);
                        0
                    }
                    (Some(_), 14) => {
                        let pos = self.vfs.get(h).map_or(0, |f| f.pos);
                        self.mem.write64(info, pos);
                        0
                    }
                    (Some((size, dir)), 34) => {
                        self.write_network_info(info, size, dir);
                        0
                    }
                    _ => 0,
                };
                self.iosb(iosb, status, 0);
                self.ret(status);
            }
            "NtSetInformationFile" => {
                let (h, iosb, info, class) = (a(0), a(1), a(2), a(4));
                if class == 14 {
                    let pos = self.mem.read64(info);
                    if let Some(f) = self.vfs.get(h) {
                        f.pos = pos;
                    }
                }
                self.iosb(iosb, 0, 0);
                self.ret(0);
            }
            "NtQueryFullAttributesFile" => {
                let path = self.object_path(a(0));
                let status = match self.vfs.stat(&path) {
                    Some((size, dir)) => {
                        self.write_network_info(a(1), size, dir);
                        0
                    }
                    None => vfs::STATUS_OBJECT_NAME_NOT_FOUND,
                };
                note = Some(format!("{path} -> {status:#x}"));
                self.ret(status);
            }
            "NtQueryDirectoryFile" => {
                // (handle, event, apc, apc_ctx, iosb*, info*, length, ANSI_STRING* mask, restart)
                let (h, iosb, info, len, mask_ptr, restart) = (a(0), a(4), a(5), a(6), a(7), a(8) != 0);
                let mask = if mask_ptr == 0 { String::new() } else { self.ansi_string(mask_ptr) };
                let status = match self.vfs.next_entry(h, &mask, restart) {
                    Ok(Some((fname, size, dir))) if 64 + fname.len() as u32 <= len => {
                        // FILE_DIRECTORY_INFORMATION
                        self.mem.write(info, &[0; 64]);
                        for k in 0..4 {
                            self.mem.write64(info + 8 + 8 * k, 0x01C8_0000_0000_0000);
                        }
                        self.mem.write64(info + 40, size);
                        self.mem.write64(info + 48, (size + 0xFFF) & !0xFFF);
                        self.mem.write32(info + 56, if dir { 0x10 } else { 0x80 });
                        self.mem.write32(info + 60, fname.len() as u32);
                        self.mem.write(info + 64, fname.as_bytes());
                        note = Some(format!("{mask:?} -> {fname}"));
                        0
                    }
                    Ok(Some(_)) => 0x8000_0005, // buffer overflow
                    Ok(None) => vfs::STATUS_NO_MORE_FILES,
                    Err(e) => e,
                };
                self.iosb(iosb, status, 0);
                self.ret(status);
            }
            "NtClose" => {
                if !self.vfs.close(a(0)) {
                    self.handles.remove(&a(0));
                }
                self.ret(0);
            }
            // ---- XAM / video -------------------------------------------------------------
            "XamInputGetState" => {
                // (user_index, flags, XINPUT_STATE*)
                match self.input.state_bytes(a(0) as usize & 3) {
                    Some(b) => {
                        self.mem.write(a(2), &b);
                        self.ret(0);
                    }
                    None => self.ret(0x48F), // ERROR_DEVICE_NOT_CONNECTED
                }
            }
            "XamInputGetCapabilities" => {
                // (user_index, flags, XINPUT_CAPABILITIES*): type gamepad, all buttons/axes
                if self.input.pads[a(0) as usize & 3].is_some() {
                    let p = a(2);
                    self.mem.write(p, &[0; 20]);
                    self.mem.write8(p, 1); // XINPUT_DEVTYPE_GAMEPAD
                    self.mem.write8(p + 1, 1); // subtype gamepad
                    self.mem.write16(p + 4, 0xF3FF);
                    self.mem.write8(p + 6, 0xFF);
                    self.mem.write8(p + 7, 0xFF);
                    for k in 0..4 {
                        self.mem.write16(p + 8 + 2 * k, 0x7FFF);
                    }
                    self.ret(0);
                } else {
                    self.ret(0x48F);
                }
            }
            "XamInputGetKeystrokeEx" => self.ret(0x10D2), // ERROR_EMPTY: no keystroke queued
            "ExGetXConfigSetting" if self.audio.enabled || std::env::var("LMA_XCONFIG").as_deref() == Ok("1") => {
                // (category, setting, buffer, size, required size*), as Xenia: user language =
                // English, video flags = widescreen. Unanswered (the default for now), the game reads
                // an unset language: menus fall back to English, but match sound is set up at about
                // -100 dB and the exhibition defaults differ (#199). On with LMA_AUDIO=1 or
                // LMA_XCONFIG=1 until the maintainer decides whether it becomes the default.
                let v = match (a(0), a(1)) {
                    (3, 9) => Some(1u32),
                    (3, 0xA) => Some(0x0004_0000),
                    _ => None,
                };
                note = Some(format!("category {:#x} setting {:#x} -> {v:?}", a(0), a(1)));
                if let (Some(v), true) = (v, a(3) >= 4) {
                    self.mem.write32(a(2), v);
                    if a(4) != 0 {
                        self.mem.write16(a(4), 4);
                    }
                }
                self.ret(if v.is_some() { 0 } else { 0xC000_00F0 }); // STATUS_INVALID_PARAMETER_2
            }
            "XGetVideoMode" => {
                // display 1280x720, widescreen, hi-def
                let p = a(0);
                self.mem.write32(p, 1280);
                self.mem.write32(p + 4, 720);
                self.mem.write32(p + 8, 0);
                self.mem.write32(p + 12, 1);
                self.mem.write32(p + 16, 1);
            }
            "VdInitializeRingBuffer" => {
                self.gpu.rb_base = gpu::phys(a(0));
                self.gpu.rb_size = 1 << (a(1) + 3);
                self.gpu.rptr = 0;
                note = Some(format!("{:#010x} size {:#x}", a(0), self.gpu.rb_size));
            }
            "VdEnableRingBufferRPtrWriteBack" => {
                self.gpu.writeback = gpu::phys(a(0));
                note = Some(format!("{:#010x}", a(0)));
            }
            // ---- audio (#199): the XAudio render driver, LMA_AUDIO=1 only (else they return 0) ----
            "XAudioRegisterRenderDriverClient" if self.audio.enabled => {
                // (callback record {callback, argument}, driver handle out)
                let (cb, arg) = (self.mem.read32(a(0)), self.mem.read32(a(0) + 4));
                self.audio.client = Some((cb, arg));
                if self.audio.thread.is_none() {
                    let t = self.spawn(crate::audio::AUDIO_MAGIC, 0, 0, true, "audio".into());
                    self.audio.thread = Some(t);
                }
                self.audio.next_frame = self.now() + crate::audio::Audio::period(crate::threads::CLOCK_HZ);
                self.mem.write32(a(1), crate::audio::DRIVER);
                note = Some(format!("callback {cb:#010x} arg {arg:#x}"));
                self.ret(0);
            }
            "XAudioUnregisterRenderDriverClient" if self.audio.enabled => {
                self.audio.client = None;
                self.ret(0);
            }
            "XAudioSubmitRenderDriverFrame" if self.audio.enabled && self.audio.xma_log && self.audio.frames % crate::audio::RATE.div_ceil(crate::audio::FRAME as u64) == 0 => {
                // LMA_XMA_LOG: a marker every second of submitted audio, to line dumps up with the log
                note = Some(format!("audio at {:.3} s", self.audio.frames as f64 * crate::audio::FRAME as f64 / crate::audio::RATE as f64));
                let mut buf = vec![0u8; 6 * crate::audio::FRAME * 4];
                self.mem.read(a(1), &mut buf);
                self.audio.submit(&buf);
                self.ret(0);
            }
            "XAudioSubmitRenderDriverFrame" if self.audio.enabled => {
                let mut buf = vec![0u8; 6 * crate::audio::FRAME * 4];
                self.mem.read(a(1), &mut buf);
                self.audio.submit(&buf);
                self.ret(0);
            }
            // XMA decoder contexts: 64-byte blocks in physical memory, one per call (the console's
            // audio hardware has 320). Decoding itself is step 2 of #199.
            "XMACreateContext" if self.audio.enabled => {
                if self.audio.xma_base == 0 {
                    self.xma_init(); // a state saved without audio
                }
                match self.audio.xma_alloc() {
                    Some(i) => {
                        let ctx = self.audio.xma_context(i);
                        self.mem.write(ctx, &[0; 64]);
                        self.mem.write32(a(0), ctx);
                        note = Some(format!("context {i} at {ctx:#010x}"));
                        self.ret(0);
                    }
                    None => self.ret(0xC000_0017), // STATUS_NO_MEMORY
                }
            }
            "XMAReleaseContext" if self.audio.enabled => {
                if a(0) >= self.audio.xma_base && a(0) < self.audio.xma_base + crate::audio::XMA_CONTEXTS * 64 {
                    let i = (a(0) - self.audio.xma_base) / 64;
                    self.audio.xma_free(i);
                    // a released context starts its next sound from scratch
                    if let Some(ch) = self.audio.xma_hw.get_mut(i as usize) {
                        ch.clear();
                    }
                }
                self.ret(0);
            }
            "XAudioGetVoiceCategoryVolume" if self.audio.enabled => {
                self.mem.write32(a(1), 1.0f32.to_bits()); // full volume, every category
                self.ret(0);
            }
            "XAudioGetVoiceCategoryVolumeChangeMask" if self.audio.enabled => {
                self.mem.write32(a(1), 0);
                self.ret(0);
            }
            "VdSetGraphicsInterruptCallback" => {
                self.gpu.interrupt_cb = a(0);
                self.gpu.interrupt_user = a(1);
                self.interrupt_cb_set = a(0) != 0;
                if self.interrupt_thread.is_none() {
                    let t = self.spawn(INTERRUPT_MAGIC, 0, 0, true, "gpu-interrupt".into());
                    self.interrupt_thread = Some(t);
                }
                note = Some(format!("cb {:#010x} user {:#010x}", a(0), a(1)));
            }
            "VdGetSystemCommandBuffer" => {
                self.mem.write32(a(0), 0xBEEF_0000);
                self.mem.write32(a(1), 0xBEEF_0001);
            }
            "VdIsHSIOTrainingSucceeded" | "VdInitializeEngines" => self.ret(1),
            "VdQueryVideoFlags" => self.ret(3), // widescreen | >= 1280 wide
            "VdPersistDisplay" => {
                if a(1) != 0 {
                    let p = self.alloc_small(64);
                    self.mem.write32(a(1), p);
                }
                self.ret(1);
            }
            "VdGetCurrentDisplayGamma" => {
                self.mem.write32(a(0), 2); // HDTV gamma curve
                self.mem.write32(a(1), 2.222_222_3f32.to_bits());
            }
            "VdInitializeScalerCommandBuffer" => {
                // fill the destination with type-2 NOPs, return its length in words
                // args 11 and 12 (dest*, count) are the 3rd/4th stack arguments
                let dest = self.mem.read32(self.cpu.gpr[1] as u32 + 0x54 + 8 * 2);
                let count = self.mem.read32(self.cpu.gpr[1] as u32 + 0x54 + 8 * 3);
                for i in 0..count.min(0x10000) {
                    self.mem.write32(dest + 4 * i, 0x8000_0000);
                }
                self.ret(count);
            }
            "VdQueryVideoMode" => {
                // X_VIDEO_MODE, 0x30 bytes: 1280x720 progressive, widescreen, HD, 60 Hz
                let p = a(0);
                let fields = [1280u32, 720, 0, 1, 1, 60f32.to_bits(), 1, 0x8A, 0x01, 0, 0, 0];
                for (i, v) in fields.iter().enumerate() {
                    self.mem.write32(p + 4 * i as u32, *v);
                }
            }
            "VdGetCurrentDisplayInformation" => {
                // X_DISPLAY_INFO, 0x58 bytes
                let p = a(0);
                self.mem.write(p, &[0; 0x58]);
                self.mem.write16(p, 1280);
                self.mem.write16(p + 2, 720);
                for (off, v) in [(0x40, 320u16), (0x42, 180), (0x44, 320), (0x46, 180), (0x48, 1280), (0x4A, 720), (0x56, 1280)] {
                    self.mem.write16(p + off, v);
                }
                self.mem.write32(p + 0x4C, 60f32.to_bits());
            }
            "VdSwap" => {
                // (buffer*, fetch*, unk, unk, unk, frontbuffer*, format*, colorspace*, width*, height*)
                let buf = a(0);
                let fb_virt = self.mem.read32(a(5));
                let fb = fb_virt & 0x1FFF_FFFF;
                let (w, h) = (self.mem.read32(a(8)), self.mem.read32(a(9)));
                self.mem.write(buf, &[0; 256]);
                let mut dw = Vec::with_capacity(64);
                dw.push((5 << 16) | 0x4800); // type0: 6 regs at SHADER_CONSTANT_FETCH_00_0
                for i in 0..6 {
                    let mut v = self.mem.read32(a(1) + 4 * i);
                    if i == 1 {
                        v = (v & 0xFFF) | (fb & !0xFFF);
                    }
                    dw.push(v);
                }
                dw.push((3 << 30) | (3 << 16) | (0x64 << 8)); // type3 XE_SWAP, 4 dwords
                dw.extend([0x5357_4150, fb, w, h]);
                while dw.len() < 64 {
                    dw.push(0x8000_0000); // type2 nop
                }
                for (i, v) in dw.iter().enumerate() {
                    self.mem.write32(buf + 4 * i as u32, *v);
                }
                note = Some(format!("front buffer {fb:#010x} {w}x{h}"));
            }
            // ---- one local profile ("Player") signed in on controller 1 ---------------------
            "XamUserGetSigninState" => self.ret(if self.local_user(a(0)) { 1 } else { 0 }), // SignedInLocally
            "XamUserGetXUID" => {
                if self.local_user(a(0)) {
                    self.mem.write64(a(2), LOCAL_XUID + a(0) as u64);
                    self.ret(0);
                } else {
                    self.ret(0x525); // ERROR_NO_SUCH_USER
                }
            }
            "XamUserGetName" => {
                // (user, char* buffer, size)
                if self.local_user(a(0)) {
                    let mut name = profile_name(a(0)).into_bytes();
                    name.push(0);
                    name.truncate(a(2) as usize);
                    self.mem.write(a(1), &name);
                    self.ret(0);
                } else {
                    self.ret(0x525);
                }
            }
            "XamUserGetSigninInfo" => {
                // (user, flags, XUSER_SIGNIN_INFO*): xuid, flags, state, guest, sponsor xuid, name[16]
                if self.local_user(a(0)) {
                    let p = a(2);
                    self.mem.write(p, &[0; 0x28]);
                    self.mem.write64(p, LOCAL_XUID + a(0) as u64);
                    self.mem.write32(p + 0xC, 1);
                    self.mem.write(p + 0x18, profile_name(a(0)).as_bytes());
                    self.ret(0);
                } else {
                    self.ret(0x525);
                }
            }
            "XamShowSigninUI" => {
                // the profile is already signed in: report the guide opening and closing
                self.notifications.push_back((XN_SYS_UI, 1));
                self.notifications.push_back((XN_SYS_UI, 0));
                self.notifications.push_back((XN_SYS_SIGNINCHANGED, 1));
                self.ret(0);
            }
            "XNotifyGetNext" => {
                // (listener, match_id, u32* id, u32* param) -> TRUE when a message was returned
                let want = a(1);
                let pos = self.notifications.iter().position(|&(id, _)| want == 0 || id == want);
                match pos.and_then(|i| self.notifications.remove(i)) {
                    Some((id, param)) => {
                        self.mem.write32(a(2), id);
                        self.mem.write32(a(3), param);
                        note = Some(format!("id {id:#x} param {param:#x}"));
                        self.ret(1);
                    }
                    None => self.ret(0),
                }
            }
            // ---- storage: one hard drive, saves live in userdata/ --------------------------
            "XamShowDeviceSelectorUI" => {
                // (user, content_type, flags, bytes_requested, u32* device_id, XOVERLAPPED*)
                self.mem.write32(a(4), HDD_DEVICE_ID);
                self.complete_overlapped(a(5), 0, 0);
                self.ret(if a(5) != 0 { 0x3E5 } else { 0 }); // ERROR_IO_PENDING when async
            }
            "XamContentGetDeviceData" => {
                // (device_id, XDEVICE_DATA*): id, type, total bytes, free bytes, name[28] (UTF-16)
                let p = a(1);
                self.mem.write(p, &[0; 0x50]);
                self.mem.write32(p, HDD_DEVICE_ID);
                self.mem.write32(p + 4, 1); // XCONTENTDEVICETYPE_HDD
                self.mem.write64(p + 8, 20 << 30);
                self.mem.write64(p + 16, 16 << 30);
                for (i, c) in "Hard Drive".encode_utf16().enumerate() {
                    self.mem.write16(p + 24 + 2 * i as u32, c);
                }
                self.ret(0);
            }
            "XamContentCreateEx" | "XamContentCreate" => {
                // (user, root name, XCONTENT_DATA*, flags, u32* disposition, u32* license,
                //  [cache size, content size,] XOVERLAPPED*): the package's files are plain files
                // under its root; remember the package so "load game" can list it
                let ov = if name == "XamContentCreateEx" { a(8) } else { a(6) };
                let root = self.mem.cstr(a(1), 64);
                let p = self.read_content_data(a(2));
                let index = self.vfs.resolve("content.idx").0;
                let exists = crate::content::load(&index).iter().any(|q| q.kind == p.kind && q.file.eq_ignore_ascii_case(&p.file));
                // each package gets its own folder, mounted at its root until XamContentClose
                let folder = crate::content::folder(&p.file);
                let user = self.vfs.user_dir().to_path_buf();
                if let Err(e) = crate::content::import_legacy(&user, &root, &p.file).and_then(|_| std::fs::create_dir_all(user.join(&folder))) {
                    eprintln!("content {folder}: {e}");
                }
                self.vfs.mount(&root, &folder);
                // creation dispositions: 1 create new, 2 create always, 4 open always (3 open existing,
                // 5 truncate); an existing package with files is listed too (a legacy save)
                let has_files = std::fs::read_dir(user.join(&folder)).is_ok_and(|mut d| d.next().is_some());
                if (matches!(a(3) & 0xF, 1 | 2 | 4) || has_files) && !p.file.is_empty() {
                    if let Err(e) = crate::content::add(&index, p.clone()) {
                        eprintln!("content index {}: {e}", index.display());
                    }
                }
                if a(4) != 0 {
                    self.mem.write32(a(4), if exists { 2 } else { 1 }); // XCONTENT opened / created
                }
                note = Some(format!("{root}: type {} file {:?} display {:?} flags {:#x}{} -> {folder}", p.kind, p.file, p.display, a(3), if exists { " (exists)" } else { "" }));
                self.complete_overlapped(ov, 0, 0);
                self.ret(if ov != 0 { 0x3E5 } else { 0 });
            }
            "XamContentGetCreator" => {
                // (user, XCONTENT_DATA*, BOOL* is_creator, XUID* creator, XOVERLAPPED*): every
                // package on the drive was made by the signed-in profile (else "save is damaged")
                if a(2) != 0 {
                    self.mem.write32(a(2), 1);
                }
                if a(3) != 0 {
                    self.mem.write64(a(3), LOCAL_XUID);
                }
                self.complete_overlapped(a(4), 0, 0);
                self.ret(if a(4) != 0 { 0x3E5 } else { 0 });
            }
            "XamContentClose" => {
                // (root name, XOVERLAPPED*)
                let root = self.mem.cstr(a(0), 64);
                self.vfs.unmount(&root);
                note = Some(root);
                self.complete_overlapped(a(1), 0, 0);
                self.ret(if a(1) != 0 { 0x3E5 } else { 0 });
            }
            "XamContentDelete" => {
                // (user, XCONTENT_DATA*, XOVERLAPPED*): forget the package (its files stay on disk)
                let p = self.read_content_data(a(1));
                let index = self.vfs.resolve("content.idx").0;
                let _ = crate::content::remove(&index, p.kind, &p.file);
                note = Some(format!("type {} file {:?}", p.kind, p.file));
                self.complete_overlapped(a(2), 0, 0);
                self.ret(if a(2) != 0 { 0x3E5 } else { 0 });
            }
            "XamContentCreateEnumerator" => {
                // (user, device_id, content_type, flags, items, u32* buffer_size, HANDLE* out)
                if a(5) != 0 {
                    self.mem.write32(a(5), a(4).max(1) * XCONTENT_DATA_SIZE);
                }
                let obj = self.new_object(OBJ_NOTIFICATION_EVENT, 1);
                let h = self.new_handle(obj);
                self.mem.write32(a(6), h);
                let kind = a(2);
                let user = self.vfs.user_dir().to_path_buf();
                let mut list = crate::content::load(&self.vfs.resolve("content.idx").0);
                list.extend(crate::content::legacy(&user, &list));
                list.retain(|p| kind == 0 || p.kind == kind);
                note = Some(format!("type {kind}: {} package(s)", list.len()));
                self.content_enums.insert(h, (list, 0));
                self.ret(0);
            }
            "XamEnumerate" => {
                // (handle, flags, buffer, size, u32* items_returned, XOVERLAPPED*)
                let (buf, size) = (a(2), a(3));
                let batch: Vec<_> = match self.content_enums.get_mut(&a(0)) {
                    Some((list, pos)) => {
                        let n = ((size / XCONTENT_DATA_SIZE) as usize).min(list.len() - *pos);
                        *pos += n;
                        list[*pos - n..*pos].to_vec()
                    }
                    None => Vec::new(),
                };
                for (i, p) in batch.iter().enumerate() {
                    self.write_content_data(buf + i as u32 * XCONTENT_DATA_SIZE, p);
                }
                if a(4) != 0 {
                    self.mem.write32(a(4), batch.len() as u32);
                }
                note = Some(format!("{} item(s)", batch.len()));
                let r = if batch.is_empty() { 0x12 } else { 0 }; // ERROR_NO_MORE_FILES
                if a(5) != 0 {
                    self.complete_overlapped(a(5), r, batch.len() as u32);
                    self.ret(0x3E5);
                } else {
                    self.ret(r);
                }
            }
            "XamGetOverlappedResult" => {
                // (XOVERLAPPED*, u32* result, wait)
                let ov = a(0);
                if a(1) != 0 {
                    let len = self.mem.read32(ov + 4);
                    self.mem.write32(a(1), len);
                }
                let r = self.mem.read32(ov);
                self.ret(r);
            }
            "XGetLanguage" => self.ret(1),
            "XGetGameRegion" => self.ret(0xFFFF),
            "XGetAVPack" => self.ret(6),
            "XamLoaderTerminateTitle" | "HalReturnToFirmware" => return Err(Exit::Terminated(name)),
            "KeBugCheck" | "KeBugCheckEx" => return Err(Exit::Terminated(name)),
            _ => self.ret(0),
        }
        if !matches!(name, "RtlEnterCriticalSection" | "RtlLeaveCriticalSection" | "RtlTryEnterCriticalSection") {
            let line = match note {
                Some(n) => format!("[{:>11}] t{} {name}: {n}", self.clock, self.cur),
                None => format!("[{:>11}] t{} {name}", self.clock, self.cur),
            };
            self.log.push(line);
        }
        Ok(flow)
    }
}

// ---- save states ---------------------------------------------------------------------------

/// Everything needed to resume the title exactly where it was. Host-only data (recompiled code,
/// import thunks, caches, logs) comes from the freshly booted machine the state is loaded into.
#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    magic: String,
    image_base: u32,
    cpu: Cpu,
    cur: usize,
    clock: u64,
    threads: Vec<GThread>,
    // ordered, so a save state's bytes don't depend on a hash map's per-process iteration order
    handles: BTreeMap<u32, u32>,
    next_handle: u32,
    tls: (u32, Vec<u8>, u32),
    allocators: (u32, u32, u32),
    notifications: std::collections::VecDeque<(u32, u32)>,
    vregions: BTreeMap<u32, u32>,
    committed: std::collections::BTreeSet<u32>,
    interrupt_thread: Option<usize>,
    next_vblank: u64,
    input: crate::input::Input,
    vfs: crate::vfs::VfsState,
    gpu: crate::gpu::GpuState,
    pages: Vec<(u32, Vec<u8>)>,
}

const SNAPSHOT_MAGIC: &str = "LMA2007-SAVESTATE-1";

/// A state file: the snapshot, then `tail` (`Machine::state_tail`), in one lz4 frame.
fn write_snapshot(path: &std::path::Path, snap: &Snapshot, tail: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = lz4_flex::frame::FrameEncoder::new(file);
    bincode::serialize_into(&mut enc, snap).map_err(std::io::Error::other)?;
    enc.write_all(tail)?;
    // flush explicitly: a dropped BufWriter swallows its last write error
    let mut file = enc.finish().map_err(std::io::Error::other)?;
    file.flush()?;
    file.get_ref().sync_all()
}

impl Machine {
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            magic: SNAPSHOT_MAGIC.into(),
            image_base: self.image_base,
            cpu: self.cpu.clone(),
            cur: self.cur,
            clock: self.clock,
            threads: self.threads.clone(),
            handles: self.handles.iter().map(|(&k, &v)| (k, v)).collect(),
            next_handle: self.next_handle,
            tls: (self.tls_size, self.tls_init.clone(), self.tls_slots),
            allocators: (self.heap, self.phys, self.small),
            notifications: self.notifications.clone(),
            vregions: self.vregions.clone(),
            committed: self.committed.iter().copied().collect(),
            interrupt_thread: self.interrupt_thread,
            next_vblank: self.next_vblank,
            input: self.input.clone(),
            vfs: self.vfs.save_state(),
            gpu: self.gpu.save_state(),
            pages: self.mem.snapshot_pages(),
        }
    }

    /// Write a save state (lz4-compressed). Returns the compressed size in bytes.
    pub fn save_state(&mut self, path: &std::path::Path) -> std::io::Result<u64> {
        self.par_join();
        self.cp_join();
        // GPU writes still in flight (resolve copies, deferred fence writes) belong in the state
        self.gpu.fence(&mut self.mem);
        let snap = self.snapshot();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // written next to the target and renamed over it only when complete: a save that fails
        // (or a process killed mid-write) never leaves a truncated state, nor destroys the last
        // good one (F5 overwrites the same quick state every time)
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        let done = self.write_state(&tmp, &snap).and_then(|_| std::fs::rename(&tmp, path));
        if done.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        done?;
        Ok(std::fs::metadata(path)?.len())
    }

    fn write_state(&self, path: &std::path::Path, snap: &Snapshot) -> std::io::Result<()> {
        write_snapshot(path, snap, &self.state_tail()?)
    }

    /// The fields appended after the snapshot, serialized: small, so a background save (the black
    /// box) takes them on the game thread and leaves only the pages' compression to its thread.
    fn state_tail(&self) -> std::io::Result<Vec<u8>> {
        let mut enc = Vec::new();
        // guest time (#56), appended so older builds and older states still load
        bincode::serialize_into(&mut enc, &self.now()).map_err(std::io::Error::other)?;
        // physical allocator state (#60), appended the same way
        // quarantined blocks are saved as free (the format stays; a loaded state reuses them)
        let mut free = self.phys_free.clone();
        free.extend(self.phys_quarantine.iter().map(|&(_, a, s)| (a, s)));
        bincode::serialize_into(&mut enc, &(&self.phys_live, &free)).map_err(std::io::Error::other)?;
        // mounted content packages (#121), appended the same way
        bincode::serialize_into(&mut enc, self.vfs.mounts()).map_err(std::io::Error::other)?;
        // the audio driver (#199): client, its thread, next frame, XMA contexts; None without one
        let audio = self.audio.client.map(|c| (c, self.audio.thread, self.audio.next_frame, self.audio.xma_base, &self.audio.xma_used));
        bincode::serialize_into(&mut enc, &audio).map_err(std::io::Error::other)?;
        // the XMA contexts the hardware is running (#199); decoders restart at the next packet
        let xma_on: Vec<bool> = self.audio.xma_hw.iter().map(|c| c.enabled).collect();
        bincode::serialize_into(&mut enc, &xma_on).map_err(std::io::Error::other)?;
        // the physical quarantine (#232 saves): blocks waiting out PHYS_QUARANTINE, with when they
        // were freed. They are in the free list above too (older builds reuse them at once); a
        // state that resumed with them free handed them out early and the match diverged.
        bincode::serialize_into(&mut enc, &self.phys_quarantine).map_err(std::io::Error::other)?;
        Ok(enc)
    }

    /// `save_state` with the file written on another thread (the black box saves one every few
    /// minutes; a full save stalls the game ~3 s, mostly lz4 and the disk). The game thread only
    /// copies the pages. The thread returns the state's size.
    pub fn save_state_background(&mut self, path: std::path::PathBuf) -> std::thread::JoinHandle<std::io::Result<u64>> {
        self.par_join();
        self.cp_join();
        self.gpu.fence(&mut self.mem);
        let snap = self.snapshot();
        let tail = self.state_tail();
        std::thread::spawn(move || {
            let tail = tail?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut tmp = path.as_os_str().to_owned();
            tmp.push(".tmp");
            let tmp = std::path::PathBuf::from(tmp);
            let done = write_snapshot(&tmp, &snap, &tail).and_then(|_| std::fs::rename(&tmp, &path));
            if done.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            done?;
            Ok(std::fs::metadata(&path)?.len())
        })
    }

    /// Resume from a save state written by `save_state` for the same default.xex.
    pub fn load_state(&mut self, path: &std::path::Path) -> std::io::Result<()> {
        self.par_join(); // memory is reset under them (#344 review)
        self.cp_join();
        let file = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut dec = lz4_flex::frame::FrameDecoder::new(file);
        let snap: Snapshot = bincode::deserialize_from(&mut dec).map_err(std::io::Error::other)?;
        // guest time (#56); states written before it have none: time = instruction count
        let time: u64 = bincode::deserialize_from(&mut dec).unwrap_or(snap.clock);
        // physical allocator state (#60); older states have none (their blocks can't be freed)
        let (phys_live, phys_free): (BTreeMap<u32, u32>, BTreeMap<u32, u32>) = bincode::deserialize_from(&mut dec).unwrap_or_default();
        // mounted content packages; older states have none
        let mounts: Vec<(String, String)> = bincode::deserialize_from(&mut dec).unwrap_or_default();
        // the audio driver (#199); older states and states saved without LMA_AUDIO have none
        type AudioState = Option<((u32, u32), Option<usize>, u64, u32, Vec<bool>)>;
        let audio: AudioState = bincode::deserialize_from(&mut dec).unwrap_or_default();
        let xma_on: Vec<bool> = bincode::deserialize_from(&mut dec).unwrap_or_default();
        // the physical quarantine; older states have none (their quarantined blocks are free)
        let quarantine: std::collections::VecDeque<(u64, u32, u32)> = bincode::deserialize_from(&mut dec).unwrap_or_default();
        if snap.magic != SNAPSHOT_MAGIC || snap.image_base != self.image_base {
            return Err(std::io::Error::other("not a save state for this game/build"));
        }
        self.cpu = snap.cpu;
        self.cur = snap.cur;
        self.clock = snap.clock;
        self.rt_epoch = std::time::Instant::now();
        self.rt_offset = time;
        self.rt_base = time;
        self.time_last = time;
        self.slice_start = snap.clock;
        self.threads = snap.threads;
        self.handles = snap.handles.into_iter().collect();
        self.next_handle = snap.next_handle;
        (self.tls_size, self.tls_init, self.tls_slots) = snap.tls;
        (self.heap, self.phys, self.small) = snap.allocators;
        (self.phys_live, self.phys_free) = (phys_live, phys_free);
        for &(_, a, s) in &quarantine {
            if self.phys_free.get(&a) == Some(&s) {
                self.phys_free.remove(&a);
            }
        }
        self.phys_quarantine = quarantine;
        self.notifications = snap.notifications;
        self.vregions = snap.vregions;
        self.committed = snap.committed.into_iter().collect();
        self.interrupt_thread = snap.interrupt_thread;
        self.next_vblank = snap.next_vblank;
        // A state saved in real time keeps its deadlines in guest time; under LMA_DETERMINISTIC
        // guest time is the instruction count, far ahead of it (the maintainer's 24-minute session: 70 B
        // vs 1.35 T), so every deadline would be long past and ~1.5 M vblanks would fire at once.
        // Shift them onto the instruction clock (and the other way for a deterministic state
        // loaded in real time, which is a no-op: there time = clock).
        // the audio driver only when it's on now too (else its calls return 0, as when saved without)
        self.audio.client = None;
        // the state's own XMA contexts, or none: contexts left from before the load would sit in
        // whatever the state's game keeps there (a state saved without audio has its own blocks
        // where this run put the context array; the game's data then overwrote the contexts, and
        // the decoder played it: #199). Without, the first XMACreateContext makes a fresh array.
        (self.audio.xma_base, self.audio.xma_used, self.audio.xma_hw) = (0, vec![false; crate::audio::XMA_CONTEXTS as usize], Vec::new());
        if let (true, Some((client, thread, next_frame, xma_base, xma_used))) = (self.audio.enabled, audio) {
            (self.audio.client, self.audio.thread, self.audio.next_frame) = (Some(client), thread, next_frame);
            (self.audio.xma_base, self.audio.xma_used) = (xma_base, xma_used);
            self.audio.xma_hw = xma_on.into_iter().map(|enabled| crate::xma::Channel { enabled, ..Default::default() }).collect();
        }
        if self.audio.enabled && self.audio.client.is_none() {
            // saved without audio: that session's sound engine never started, so nothing runs
            // XACT's work, and a screen that waits for a sound to be prepared (Football One's
            // Start Highlights) waits forever (#280)
            eprintln!("audio: this state was saved without audio (no XAudio client): its game runs no sound engine, and Football One highlights will hang (#280)");
        }
        if self.deterministic && time != snap.clock {
            let shift = |t: u64| t.wrapping_add(snap.clock).wrapping_sub(time);
            self.next_vblank = shift(self.next_vblank);
            self.audio.next_frame = shift(self.audio.next_frame);
            for th in &mut self.threads {
                match &mut th.state {
                    State::Waiting(Wait::Sleep(d)) | State::Waiting(Wait::Nap(d)) | State::Waiting(Wait::Objects { deadline: Some(d), .. }) | State::Waiting(Wait::GpuMoved { deadline: d, .. }) => *d = shift(*d),
                    _ => {}
                }
            }
            for q in &mut self.phys_quarantine {
                q.0 = shift(q.0);
            }
        }
        self.input = snap.input;
        self.vfs.set_mounts(mounts); // before reopening files: some may sit in a package
        self.vfs.load_state(snap.vfs)?;
        self.gpu.load_state(snap.gpu);
        self.interrupt_cb_set = self.gpu.interrupt_cb != 0;
        self.mem.restore_pages(snap.pages);
        // States saved before the GPU writeback fix (#60) have the main thread's KTHREAD pointer
        // (PCR+0x100) overwritten by a GPU write; every thread's PCR sits 0x1000 below its KTHREAD.
        for th in &self.threads {
            self.mem.write32(th.kthread - 0x1000 + 0x100, th.kthread);
        }
        // ThreadIds continue past the highest one in use (they aren't stored separately)
        self.next_tid = self.threads.iter().map(|t| self.mem.read32(t.kthread + 0x14C)).max().unwrap_or(0) + 1;
        for &(a, v) in &self.after_load_writes {
            self.mem.write32(a, v);
        }
        Ok(())
    }
}

/// `LMA_VBLANK_HZ=<n>` (#863, experiment for the render/sim decoupling check): the vblank rate,
/// default 60, clamped 10..480; `LMA_VBLANK_DIV` still multiplies the period.
fn vblank_hz() -> u64 {
    static HZ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *HZ.get_or_init(|| vblank_hz_of(std::env::var("LMA_VBLANK_HZ").ok().as_deref()))
}

/// `LMA_VBLANK_HZ`'s value: 60 unless set to a number, clamped to 10..480.
fn vblank_hz_of(var: Option<&str>) -> u64 {
    var.and_then(|v| v.trim().parse().ok()).unwrap_or(60).clamp(10, 480)
}

/// `LMA_VBLANK_DIV` (#39): vblanks every n 60 Hz periods, 1 (the console's rate) unless set, at most 8.
fn vblank_div() -> u64 {
    static DIV: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *DIV.get_or_init(|| std::env::var("LMA_VBLANK_DIV").ok().and_then(|v| v.parse().ok()).unwrap_or(1).clamp(1, 8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vblank_rate_defaults_to_60_and_clamps() {
        assert_eq!(vblank_hz_of(None), 60);
        assert_eq!(vblank_hz_of(Some("120")), 120);
        assert_eq!(vblank_hz_of(Some(" 90 ")), 90);
        assert_eq!(vblank_hz_of(Some("x")), 60, "not a number: the default");
        assert_eq!(vblank_hz_of(Some("1")), 10);
        assert_eq!(vblank_hz_of(Some("1000")), 480);
    }

    fn machine() -> Machine {
        let x = xex::Xex { module_flags: 0, entry_point: 0x8200_0000, image_base: 0x8200_0000, image: vec![0; 0x1000], libraries: vec![], used_devkit_key: false, resources: vec![] };
        let dir = std::env::temp_dir();
        Machine::new(&x, &dir, &dir)
    }

    /// A physical block reads as zero also where an earlier one lived (#199: the game copied a
    /// new voice's XMA settings before setting it up, and a dirty block started the decoder on
    /// garbage).
    #[test]
    fn reused_physical_memory_reads_as_zero() {
        let mut m = machine();
        m.quarantine = false; // reuse at once, so the same block comes back
        let a = m.phys_alloc(0x3000, 0x1000);
        assert_ne!(a, 0);
        m.mem.write(a, &[0xAA; 0x3000]);
        assert!(m.phys_release(a));
        let b = m.phys_alloc(0x2000, 0x1000);
        assert_eq!(b, a, "first fit reuses the freed block");
        let mut buf = vec![1u8; 0x2000];
        m.mem.read(b, &mut buf);
        assert!(buf.iter().all(|&v| v == 0));
    }

    /// Loading a state saved without audio drops this run's XMA contexts: the array sat where
    /// the state's game keeps its own data, which then overwrote the contexts (#199).
    #[test]
    fn state_without_audio_resets_xma_contexts() {
        let path = std::env::temp_dir().join(format!("lma-noaudio-{}.state", std::process::id()));
        let mut saved = machine();
        saved.audio.enabled = false;
        saved.save_state(&path).unwrap();
        let mut m = machine();
        m.audio.enabled = true;
        m.xma_init();
        m.audio.xma_used[3] = true;
        assert_ne!(m.audio.xma_base, 0);
        m.load_state(&path).unwrap();
        crate::mmio::set_xma_trap(false);
        std::fs::remove_file(&path).ok();
        assert_eq!(m.audio.xma_base, 0, "the next XMACreateContext makes a fresh array");
        assert!(m.audio.xma_used.iter().all(|&u| !u) && m.audio.xma_hw.is_empty());
    }

    /// A callback that submits nothing is filled with a frame of silence when the next one
    /// starts, so the output keeps the hardware's pace: frames submitted = callbacks.
    #[test]
    fn silent_callbacks_are_filled() {
        let mut m = machine();
        m.deterministic = true;
        m.audio.client = Some((0x8200_0000, 0));
        m.audio.thread = Some(0);
        let period = crate::audio::Audio::period(crate::threads::CLOCK_HZ);
        let frame = [0u8; 6 * crate::audio::FRAME * 4];
        // five callbacks: the game submits in the 1st, 2nd and 4th, nothing in the 3rd and 5th
        for (k, submits) in [true, true, false, true, false].into_iter().enumerate() {
            m.clock = period * (k as u64 + 1);
            m.threads[0].state = State::Suspended(1);
            m.service_audio();
            assert_eq!(m.threads[0].state, State::Ready, "callback {k} started");
            if submits {
                m.audio.submit(&frame);
            }
        }
        m.clock += period;
        m.threads[0].state = State::Suspended(1);
        m.service_audio(); // the 6th starts: the 5th is filled too
        assert_eq!(m.audio.silent_fills, 2);
        assert_eq!(m.audio.frames, 5, "one frame per finished callback");
        assert_eq!(m.audio.out.lock().unwrap().len(), 5 * crate::audio::FRAME);
    }

    /// In real time the audio callback's thread gets every other slice while it is ready; the
    /// others still take turns (none starves), and deterministic runs keep plain round robin.
    #[test]
    fn audio_thread_every_other_slice() {
        let mut m = machine();
        m.deterministic = false;
        m.audio_priority = true;
        let proto = m.threads[0].clone();
        while m.threads.len() < 4 {
            m.threads.push(proto.clone());
        }
        for t in &mut m.threads {
            t.state = State::Ready;
        }
        m.audio.thread = Some(2);
        let mut order = Vec::new();
        for _ in 0..8 {
            let next = m.pick_next();
            order.push(next);
            m.cur = next;
        }
        assert_eq!(order, vec![2, 1, 2, 3, 2, 0, 2, 1], "audio (2) every other slice, the rest in turn");
        // only the audio thread ready: it runs again
        for (i, t) in m.threads.iter_mut().enumerate() {
            t.state = if i == 2 { State::Ready } else { State::Suspended(1) };
        }
        m.cur = 2;
        assert_eq!(m.pick_next(), 2);
        for t in &mut m.threads {
            t.state = State::Ready;
        }
        m.deterministic = true;
        m.cur = 0;
        let rr: Vec<usize> = (0..4).map(|_| { let n = m.pick_next(); m.cur = n; n }).collect();
        assert_eq!(rr, vec![1, 2, 3, 0], "deterministic: round robin");
    }

    /// After a stall the audio callback catches up frame by frame up to ~0.5 s (a cutscene's
    /// 100-280 ms stall loses nothing); further behind it skips to now and counts what it dropped.
    #[test]
    fn audio_catches_up_after_a_stall() {
        let mut m = machine();
        m.deterministic = true;
        m.audio.client = Some((0x8200_0000, 0));
        m.audio.thread = Some(0);
        let period = crate::audio::Audio::period(crate::threads::CLOCK_HZ);
        let callbacks = |m: &mut Machine| {
            let mut n = 0;
            loop {
                m.threads[0].state = State::Suspended(1);
                m.service_audio();
                if m.threads[0].state != State::Ready {
                    return n;
                }
                n += 1;
            }
        };
        m.audio.next_frame = period;
        m.clock = period + 300 * crate::threads::CLOCK_HZ / 1000; // a 300 ms stall
        let n = callbacks(&mut m);
        assert_eq!(n as u64, 300 * crate::threads::CLOCK_HZ / 1000 / period + 1, "every frame called back");
        assert_eq!(m.audio.dropped, 0);
        m.clock += 2 * crate::threads::CLOCK_HZ; // 2 s: a pause
        assert_eq!(callbacks(&mut m), 1, "skips to now");
        assert!(m.audio.dropped >= 2 * crate::threads::CLOCK_HZ / period - 1, "{}", m.audio.dropped);
        assert!(m.audio.late_max >= 2 * crate::threads::CLOCK_HZ - 2 * period);
    }

    /// A running XMA context whose input buffer the game has freed is reported once (the
    /// freed-buffer invariant, #199); a live buffer isn't.
    #[test]
    fn xma_context_reading_freed_memory_is_reported() {
        let mut m = machine();
        m.audio.enabled = true;
        m.xma_init();
        let input = m.phys_alloc(0x800, 0x1000);
        let output = m.phys_alloc(0x1e00, 0x1000);
        // the game's words (as `Context::parse` reads them): one packet in buffer 0 (valid), a
        // 30-block stereo 48 kHz output ring (valid), the read offset after the packet header
        let mut w = [0u32; 16];
        w[0] = 1 | 1 << 20 | 30 << 22;
        w[1] = 3 << 27 | 1 << 29 | 1 << 31;
        w[2] = 32;
        w[5] = input & 0x1FFF_FFFF;
        w[7] = output & 0x1FFF_FFFF;
        let addr = m.audio.xma_context(0);
        for (k, v) in w.iter().enumerate() {
            m.mem.write32(addr + 4 * k as u32, *v);
        }
        m.audio.xma_hw.resize_with(crate::audio::XMA_CONTEXTS as usize, Default::default);
        m.audio.xma_hw[0].enabled = true;
        m.service_xma();
        assert!(!m.log.iter().any(|l| l.contains("not live")), "live buffers aren't reported");
        assert!(m.phys_release(input));
        // the game hands the (now freed) buffer to the context again
        for (k, v) in w.iter().enumerate() {
            m.mem.write32(addr + 4 * k as u32, *v);
        }
        m.service_xma();
        for (k, v) in w.iter().enumerate() {
            m.mem.write32(addr + 4 * k as u32, *v);
        }
        m.service_xma();
        crate::mmio::set_xma_trap(false);
        let hits: Vec<_> = m.log.iter().filter(|l| l.contains("not live")).collect();
        assert_eq!(hits.len(), 1, "reported once: {hits:?}");
        assert!(hits[0].contains("input 0") && hits[0].contains("freed"));
        assert!(m.audio.xma_hw[0].dead_reads >= 1);
    }

    /// With the quarantine on, a freed block isn't handed out again while fresh memory remains.
    #[test]
    fn quarantined_block_waits() {
        let mut m = machine();
        m.quarantine = true;
        let a = m.phys_alloc(0x1000, 0x1000);
        assert!(m.phys_release(a));
        let b = m.phys_alloc(0x1000, 0x1000);
        assert_ne!(b, a);
        assert!(!m.phys_is_live(a, 0x1000) && m.phys_is_live(b, 0x1000));
        assert!(m.phys_freed_at(a).is_some() && m.phys_freed_at(b).is_none());
    }

    /// A save state keeps the quarantine (#232 saves): a block freed before the save isn't
    /// handed out again after loading it, as in a run that never stopped. Resumed with it free,
    /// the match's later allocations moved and the round trip diverged.
    #[test]
    fn quarantine_survives_a_save_state() {
        let path = std::env::temp_dir().join(format!("lma-quarantine-{}.state", std::process::id()));
        let mut saved = machine();
        saved.quarantine = true;
        let a = saved.phys_alloc(0x1000, 0x1000);
        assert!(saved.phys_release(a));
        saved.save_state(&path).unwrap();
        let mut m = machine();
        m.quarantine = true;
        m.load_state(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(m.phys_quarantine, saved.phys_quarantine);
        assert!(m.phys_freed_at(a).is_some());
        assert_ne!(m.phys_alloc(0x1000, 0x1000), a, "still quarantined after the load");
    }

    /// A real-time state loaded under LMA_DETERMINISTIC=1: the quarantine's freed-at times move
    /// onto the instruction clock with the other deadlines, so a block freed just before the save
    /// still waits its full PHYS_QUARANTINE after the load (#291).
    #[test]
    fn quarantine_times_follow_a_deterministic_load() {
        let path = std::env::temp_dir().join(format!("lma-quarantine-rt-{}.state", std::process::id()));
        let mut saved = machine();
        saved.deterministic = false;
        saved.quarantine = true;
        (saved.clock, saved.slice_start, saved.rt_base) = (1_000, 1_000, 5_000_000); // now() = 5 M, clock = 1000
        let a = saved.phys_alloc(0x1000, 0x1000);
        assert!(saved.phys_release(a));
        assert_eq!(saved.phys_freed_at(a), Some(5_000_000));
        saved.save_state(&path).unwrap();
        let mut m = machine();
        m.deterministic = true;
        m.quarantine = true;
        m.load_state(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(m.phys_freed_at(a), Some(1_000), "freed at the state's own clock");
        assert_ne!(m.phys_alloc(0x1000, 0x1000), a, "and still quarantined");
    }

    /// A thread napping in a hook (`Wait::Nap`, #502) is saved like a sleeping one and its deadline
    /// is moved onto the loading machine's clock with the other deadlines, so it wakes after the
    /// same guest time, not at once or never.
    #[test]
    fn a_nap_deadline_follows_a_deterministic_load() {
        let path = std::env::temp_dir().join(format!("lma-nap-rt-{}.state", std::process::id()));
        let mut saved = machine();
        saved.deterministic = false;
        (saved.clock, saved.slice_start, saved.rt_base) = (1_000, 1_000, 5_000_000); // now() = 5 M, clock = 1000
        let t = saved.spawn(0x8200_0100, 0x40, 0, false, "nap".into());
        saved.threads[t].cpu.gpr[3] = 0x1234;
        saved.threads[t].state = State::Waiting(Wait::Nap(5_000_000 + 12_500));
        saved.save_state(&path).unwrap();
        let mut m = machine();
        m.deterministic = true;
        m.load_state(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(m.threads[t].state, State::Waiting(Wait::Nap(1_000 + 12_500)), "12,500 ticks from the state's own clock");
        assert_eq!(m.next_deadline(), Some(13_500));
        m.clock = 13_500;
        assert!(m.wake());
        assert_eq!((m.threads[t].state.clone(), m.threads[t].cpu.gpr[3]), (State::Ready, 0x1234), "woke with r3 intact");
    }

    /// Save, load into a fresh machine, save again: the two states are byte-identical, so nothing
    /// a state carries is lost or changed on the way back in. (Machine state a save leaves out
    /// altogether, as the quarantine was before #292, needs the round trip in tools/regress.py.)
    #[test]
    fn a_loaded_state_saves_the_same() {
        let dir = std::env::temp_dir();
        let (p1, p2) = (dir.join(format!("lma-rt1-{}.state", std::process::id())), dir.join(format!("lma-rt2-{}.state", std::process::id())));
        let mut a = machine();
        a.deterministic = true;
        a.quarantine = true;
        a.clock = 123_456_789;
        a.next_vblank = 123_500_000;
        let (x, y) = (a.phys_alloc(0x3000, 0x1000), a.phys_alloc(0x1000, 0x1000));
        a.phys_alloc(0x2000, 0x1000);
        assert!(a.phys_release(x) && a.phys_release(y));
        a.vregions.insert(0x4000_0000, 0x10000);
        a.committed.insert(0x4000_0000);
        a.notifications.push_back((9, 1));
        a.handles.insert(0x10, 0x7000_0000);
        a.next_handle = 0x14;
        a.save_state(&p1).unwrap();
        let mut b = machine();
        b.deterministic = true;
        b.quarantine = true;
        b.load_state(&p1).unwrap();
        b.save_state(&p2).unwrap();
        let (s1, s2) = (std::fs::read(&p1).unwrap(), std::fs::read(&p2).unwrap());
        std::fs::remove_file(&p1).ok();
        std::fs::remove_file(&p2).ok();
        assert!(s1 == s2, "the state changed on a save / load / save round trip");
    }

    /// `after_load_writes` (a mod's rewritten data, #582) are written again after a state load,
    /// over the words the state restored.
    #[test]
    fn a_state_load_rewrites_after_load_writes() {
        let p = std::env::temp_dir().join(format!("lma-alw-{}.state", std::process::id()));
        let mut a = machine();
        a.mem.write32(0x8200_0AF0, 40);
        a.save_state(&p).unwrap();
        let mut plain = machine();
        plain.load_state(&p).unwrap();
        assert_eq!(plain.mem.read32(0x8200_0AF0), 40, "the state restores the image word");
        let mut b = machine();
        b.after_load_writes = vec![(0x8200_0AF0, 40 * 24)];
        b.load_state(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(b.mem.read32(0x8200_0AF0), 40 * 24, "the mod's word, not the state's");
    }

    /// A profile is signed in on each connected controller port (#206): port 2 only once
    /// connected, with its own name and XUID.
    #[test]
    fn signed_in_per_connected_port() {
        let mut m = machine();
        let state = |m: &mut Machine, user: u64| {
            m.cpu.gpr[3] = user;
            m.hle("XamUserGetSigninState").unwrap();
            m.cpu.gpr[3] as u32
        };
        assert_eq!((state(&mut m, 0), state(&mut m, 1)), (1, 0));
        m.input.pads[1] = Some(crate::input::Pad::default());
        assert_eq!(state(&mut m, 1), 1);
        let buf = m.alloc_small(32);
        (m.cpu.gpr[3], m.cpu.gpr[4], m.cpu.gpr[5]) = (1, buf as u64, 32);
        m.hle("XamUserGetName").unwrap();
        assert_eq!(m.cpu.gpr[3], 0);
        let mut name = [0u8; 9];
        m.mem.read(buf, &mut name);
        assert_eq!(&name, b"Player 2\0");
        let xuid = m.alloc_small(8);
        for user in [0u64, 1] {
            (m.cpu.gpr[3], m.cpu.gpr[4], m.cpu.gpr[5]) = (user, 0, xuid as u64);
            m.hle("XamUserGetXUID").unwrap();
            assert_eq!(m.mem.read64(xuid), LOCAL_XUID + user);
        }
    }
}

/// `LMA_POKE="addr:hexbytes,..."`: writes guest memory once a state is loaded (hex address, big-endian
/// bytes as the guest stores them), to try a change of game data on a state: lma-cli and lma-app alike
/// (#697: a record's hair bits before team select).
impl Machine {
    pub fn apply_pokes(&mut self) -> std::result::Result<usize, String> {
        let Ok(spec) = std::env::var("LMA_POKE") else { return Ok(0) };
        let pokes = parse_poke(&spec)?;
        for (addr, bytes) in &pokes {
            self.mem.write(*addr, bytes);
            eprintln!("poked {} bytes at {addr:#x}", bytes.len());
        }
        Ok(pokes.len())
    }
}

/// `LMA_POKE`'s `addr:hexbytes,...`: a hex guest address (0x optional) and the bytes to write there.
pub fn parse_poke(spec: &str) -> std::result::Result<Vec<(u32, Vec<u8>)>, String> {
    spec.split(',')
        .filter(|s| !s.is_empty())
        .map(|item| {
            let (addr, hex) = item.split_once(':').ok_or_else(|| format!("LMA_POKE {item}: want addr:hexbytes"))?;
            let addr = u32::from_str_radix(addr.trim_start_matches("0x"), 16).map_err(|e| format!("LMA_POKE {item}: {e}"))?;
            if hex.len() % 2 != 0 {
                return Err(format!("LMA_POKE {item}: an odd number of hex digits"));
            }
            let bytes = (0..hex.len() / 2).map(|n| u8::from_str_radix(&hex[2 * n..2 * n + 2], 16)).collect::<Result<Vec<u8>, _>>().map_err(|e| format!("LMA_POKE {item}: {e}"))?;
            Ok((addr, bytes))
        })
        .collect()
}

#[cfg(test)]
mod poke_tests {
    use super::parse_poke;
    /// Addresses with or without 0x, bytes in order, and the malformed items refused.
    #[test]
    fn poke_specs_parse() {
        assert_eq!(parse_poke("0xa9f690e4:16a817fe,82000000:ff").unwrap(), vec![(0xa9f6_90e4, vec![0x16, 0xa8, 0x17, 0xfe]), (0x8200_0000, vec![0xff])]);
        assert!(parse_poke("82000000").is_err(), "no bytes");
        assert!(parse_poke("82000000:abc").is_err(), "odd digits");
        assert!(parse_poke("zz:00").is_err(), "bad address");
    }
}
