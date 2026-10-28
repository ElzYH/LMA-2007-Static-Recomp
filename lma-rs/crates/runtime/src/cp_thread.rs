//! LMA_CP_THREAD=1 (#6): the command processor runs on its own thread in live play, as the
//! console's GPU front end runs beside the CPU. Deterministic runs and recordings keep it inline.
//!
//! The game thread starts a command-processor run (`Gpu::pump`) at a slice boundary and
//! collects it at the first boundary after it finished, so the work overlaps guest code (a run
//! still going is not waited for: that boundary does no GPU work). Ordering as the game sees it
//! is the inline order, a few slices later: the run never writes guest memory itself. Its writes
//! (the registers' MMIO mirror, fences, scratch registers, render-thread spills) are queued and
//! applied by the game thread, in order, when it collects the run, before it delivers the run's
//! vblanks and interrupts. Reads see the run's own queued writes. The run reads the ring only up
//! to the write pointer the game thread published when it started the run.
//!
//! While a run is in progress the game thread doesn't touch `Machine::gpu`: every access outside
//! the slice boundary collects the run first (`Machine::cp_join`).

use crate::backend::{CpMem, GuestMem};
use crate::gpu::Gpu;
use crate::mem::MemReader;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

const IDLE: u8 = 0;
const RUN: u8 = 1;
const DONE: u8 = 2;
const QUIT: u8 = 3;

/// Queued writes, in order: (address, start, length) into one byte arena, indexed by the 4 KB
/// pages they touch (a read scans only the writes on its own pages: D3D's scratch-register fences
/// land next to the ring it reads). Kept allocated between runs.
#[derive(Default)]
struct Queued {
    writes: Vec<(u32, u32, u32)>,
    bytes: Vec<u8>,
    pages: crate::fasthash::HashMap<u32, Vec<u32>>,
}

impl Queued {
    fn push(&mut self, a: u32, data: &[u8]) {
        let i = self.writes.len() as u32;
        for p in a >> 12..=a.saturating_add(data.len() as u32 - 1) >> 12 {
            self.pages.entry(p).or_default().push(i);
        }
        self.writes.push((a, self.bytes.len() as u32, data.len() as u32));
        self.bytes.extend_from_slice(data);
    }
    fn get(&self, i: usize) -> (u32, &[u8]) {
        let (a, s, n) = self.writes[i];
        (a, &self.bytes[s as usize..(s + n) as usize])
    }
    fn overlay(&self, a: u32, out: &mut [u8]) {
        if self.writes.is_empty() || out.is_empty() {
            return;
        }
        let end = a.saturating_add(out.len() as u32);
        for p in a >> 12..=(end - 1) >> 12 {
            for &i in self.pages.get(&p).map_or(&[][..], |v| &v[..]) {
                let (w, bytes) = self.get(i as usize);
                let (s, e) = (w.max(a), w.saturating_add(bytes.len() as u32).min(end));
                if s < e {
                    out[(s - a) as usize..(e - a) as usize].copy_from_slice(&bytes[(s - w) as usize..(e - w) as usize]);
                }
            }
        }
    }
    fn clear(&mut self) {
        if self.writes.is_empty() {
            return;
        }
        self.writes.clear();
        self.bytes.clear();
        // the same few pages come back every run: empty their lists, keep the allocations
        self.pages.values_mut().for_each(Vec::clear);
    }
}

/// Guest memory as a command-processor run sees it: the guest's bytes, with the run's own writes
/// queued on top. The registers' MMIO mirror (thousands of writes a frame) is queued apart from
/// the rest (fences, scratch, spills), so reads of the ring and its buffers skip both.
pub struct CpView {
    reader: MemReader,
    mmio: Queued,
    other: Queued,
    /// Every write in order (the order they reach guest memory in).
    order: Vec<(bool, usize)>,
}

impl CpView {
    fn new(reader: MemReader) -> Self {
        Self { reader, mmio: Queued::default(), other: Queued::default(), order: Vec::new() }
    }
    /// The queued writes in the order they were made.
    fn drain(&mut self, mut apply: impl FnMut(u32, &[u8])) {
        for &(is_mmio, i) in &self.order {
            let (a, bytes) = if is_mmio { self.mmio.get(i) } else { self.other.get(i) };
            apply(a, bytes);
        }
        self.mmio.clear();
        self.other.clear();
        self.order.clear();
    }
}

const MMIO: std::ops::Range<u32> = crate::gpu::GPU_MMIO_BASE..crate::gpu::GPU_MMIO_BASE + 0x2_0000;

impl GuestMem for CpView {
    fn read(&mut self, a: u32, out: &mut [u8]) {
        self.reader.read(a, out);
        self.mmio.overlay(a, out);
        self.other.overlay(a, out);
    }
    fn write(&mut self, a: u32, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let is_mmio = MMIO.contains(&a);
        let q = if is_mmio { &mut self.mmio } else { &mut self.other };
        self.order.push((is_mmio, q.writes.len()));
        q.push(a, data);
    }
}

impl CpMem for CpView {}

struct Shared {
    state: AtomicU8,
    /// The run's Gpu (owned by the game thread's Machine) and the view: only the side whose turn
    /// `state` says it is touches them.
    gpu: UnsafeCell<*mut Gpu>,
    view: UnsafeCell<CpView>,
}

// SAFETY: `state` hands `gpu` and `view` between the two threads (Release on hand-over, Acquire
// on taking), and the Machine doesn't move or touch its Gpu while a run is in progress.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

pub struct CpThread {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// A run was started and not yet collected.
    pub busy: bool,
}

impl CpThread {
    pub fn new(reader: MemReader) -> Self {
        let shared = Arc::new(Shared { state: AtomicU8::new(IDLE), gpu: UnsafeCell::new(std::ptr::null_mut()), view: UnsafeCell::new(CpView::new(reader)) });
        let s = shared.clone();
        let thread = std::thread::Builder::new().name("command processor".into()).spawn(move || worker(s)).expect("command processor thread");
        Self { shared, thread: Some(thread), busy: false }
    }

    /// Start a run of `gpu` (which must stay put and untouched until `collect`).
    pub fn start(&mut self, gpu: &mut Gpu) {
        debug_assert!(!self.busy);
        // SAFETY: the worker is idle (nothing started or the last run collected)
        unsafe { *self.shared.gpu.get() = gpu };
        self.shared.state.store(RUN, Ordering::Release);
        // it may be napping after an idle stretch (vsync waits, real-time sleeps)
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
        self.busy = true;
    }

    /// The run in progress has finished (or none was started).
    pub fn ready(&self) -> bool {
        !self.busy || self.shared.state.load(Ordering::Acquire) == DONE
    }

    /// Wait for the run in progress and hand its queued writes to `apply`, in order.
    pub fn collect(&mut self, mut apply: impl FnMut(u32, &[u8])) {
        if !self.busy {
            return;
        }
        if self.shared.state.load(Ordering::Acquire) != DONE {
            let _p = crate::prof::span(crate::prof::Cat::CpWait);
            let mut spins = 0u32;
            while self.shared.state.load(Ordering::Acquire) != DONE {
                spins += 1;
                if spins < 4096 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
        // SAFETY: DONE: the worker has finished with the view until the next RUN
        let view = unsafe { &mut *self.shared.view.get() };
        view.drain(&mut apply);
        self.shared.state.store(IDLE, Ordering::Release);
        self.busy = false;
    }
}

impl Drop for CpThread {
    fn drop(&mut self) {
        self.collect(|_, _| {});
        self.shared.state.store(QUIT, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn worker(s: Arc<Shared>) {
    let mut idle = 0u32;
    loop {
        match s.state.load(Ordering::Acquire) {
            RUN => {
                idle = 0;
                // SAFETY: RUN: the game thread handed over the Gpu and the view until DONE
                let (gpu, view) = unsafe { (&mut **s.gpu.get(), &mut *s.view.get()) };
                gpu.pump(view);
                s.state.store(DONE, Ordering::Release);
            }
            QUIT => return,
            _ => {
                // runs come every ~15 us while the game plays: spin, then back off when it pauses
                idle = idle.saturating_add(1);
                if idle < 1 << 14 {
                    std::hint::spin_loop();
                } else if idle < 1 << 16 {
                    std::thread::yield_now();
                } else {
                    std::thread::park_timeout(std::time::Duration::from_micros(200));
                }
            }
        }
    }
}
