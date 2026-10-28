//! LMA_FRAME_PROFILE=1 (#6): where the game thread's and the render thread's wall time goes,
//! executing or waiting (and on what). Counters are nanoseconds summed over the run; lma-app
//! prints their change per guest frame every 2 s. Off, a span costs one relaxed load.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub static ON: AtomicBool = AtomicBool::new(false);
/// The command processor runs on its own thread (LMA_CP_THREAD=1).
pub static CP_THREAD: AtomicBool = AtomicBool::new(false);

/// What a span measures (index into `NS`).
#[derive(Clone, Copy)]
pub enum Cat {
    /// Game thread: guest slices (guest code, kernel HLE and native hooks included).
    Slice,
    /// Game thread: kernel HLE calls (inside `Slice`).
    Kernel,
    /// The command processor (`Gpu::pump`), waits below included: on the game thread, or on its
    /// own thread with LMA_CP_THREAD=1.
    Cp,
    /// Game thread (LMA_CP_THREAD=1): waiting for the command-processor thread's run to finish.
    CpWait,
    /// Game thread: Present waits for the render thread to finish the previous frame.
    WaitPresent,
    /// Game thread: a read of resolve output waits for the copy to land.
    WaitResolve,
    /// Game thread: waits for the render thread to drain (inline calls, fences, state loads).
    WaitDrain,
    /// Game thread: real-time naps while every guest thread waits (on a timer, vblank, event).
    Sleep,
    /// Render thread: processing commands (`RENDER_BUSY_NS` measures the same).
    RenderBusy,
    /// Any thread: blocked in a wgpu device poll for the GPU (read-backs, present).
    GpuWait,
    /// The part of `GpuWait` reading the presented frame back.
    GpuWaitPresent,
    /// The part of `GpuWait` for textures over a resolve still on its way back.
    GpuWaitTex,
    /// The part of `GpuWait` for index or vertex data over a resolve still on its way back.
    GpuWaitVerts,
    /// Render thread: creating pipelines and shader modules (first use of a shader pair or state).
    RtPipeline,
    /// Render thread: decoding textures and uploading them (setup-cache misses that decode).
    RtTexture,
    /// Render thread: flushing batches (uploads, bind groups, encoding, submit).
    RtFlush,
    /// Render thread: landing finished resolves in guest memory.
    RtLand,
    /// Render thread: resolves (copies out of the render targets, clears).
    RtResolve,
    /// Render thread: presents (composing and reading back the front buffer).
    RtPresent,
    /// render thread: the CPU vertex transform per draw (`soft.transform`)
    RtTransform,
    /// render thread, GPU-VS draw path: pixel-shader/texture setup, the vertex window copy, constants, the record push
    RtPsSetup,
    RtVwin,
    RtConsts,
    RtRecord,
    /// Count: draws the render thread processed.
    RenderDraws,
    /// Count: batches flushed (render passes of draws), and bind groups created for them.
    Flushes,
    BindGroups,
    /// Count: draws whose texture setup was computed (not taken from the setup cache).
    SetupMiss,
    /// Count: cached setups reused with their redrawn render targets' new views (#304).
    SetupRefresh,
    /// Count and time: cached setups re-validated because the texture generation moved on since
    /// they were last used (`setup_still_valid`: the decoded-texture check hashes guest memory).
    SetupCheck,
    SetupCheckTime,
    /// Count: cached setups kept by the write-watch check alone (`setup_fast_valid`).
    SetupFast,
    /// Time: the decoded-texture part of the re-validations (`prepare_textures`).
    SetupCheckDecoded,
    /// Count: float-constant sets that were new (uploaded), of those looked up.
    FloatsNew,
    FloatsLookups,
    /// Bytes: batch buffers written to the GPU (vertices, indices, constants, vertex memory), and
    /// resolve output landed in guest memory.
    UploadBytes,
    LandBytes,
    /// Count: command-processor runs that returned at once (blocked by an interrupt).
    CpBlocked,
    /// Count: of those, blocked by an interrupt still waiting for the render thread's work (the
    /// rest wait for the guest's interrupt handler to return).
    CpBlockedGpu,
    /// Count: command-processor runs.
    CpRuns,
    /// Wall time a GPU interrupt waits for the render thread's earlier work to land.
    IrqDefer,
    /// The part of `IrqDefer` until the render thread has processed every earlier command (the
    /// rest waits for the GPU and the read-backs).
    IrqRender,
    /// Wall time a raised interrupt waits for its handler thread to start.
    IrqQueue,
    /// Wall time from the handler's start to its return (the command processor waits throughout).
    IrqHandler,
    /// Game thread: the XMA hardware decoding audio (LMA_AUDIO=1, #199).
    Xma,
}

/// One counter per `Cat` (the last variant decides; a hand-kept number fell behind in a merge).
pub const N: usize = Cat::Xma as usize + 1;

/// The render-thread ticket the interrupt in `IrqDefer` waits for.
pub static IRQ_TICKET: AtomicU64 = AtomicU64::new(0);
pub static NS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

/// Adds the time until it is dropped to its category.
pub struct Span(Cat, Option<Instant>);

#[inline(always)]
pub fn span(c: Cat) -> Span {
    Span(c, ON.load(Relaxed).then(Instant::now))
}

impl Drop for Span {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(t) = self.1 {
            NS[self.0 as usize].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
        }
    }
}

static START: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn now_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64 + 1
}

/// Starts a wall-time interval of `c` that `end` closes (from another call site or thread);
/// one interval at a time per category: a second `begin` before the `end` is ignored.
pub fn begin(c: Cat) {
    if ON.load(Relaxed) {
        let _ = START[c as usize].compare_exchange(0, now_ns(), Relaxed, Relaxed);
    }
}

pub fn end(c: Cat) {
    if ON.load(Relaxed) {
        let s = START[c as usize].swap(0, Relaxed);
        if s != 0 {
            NS[c as usize].fetch_add(now_ns() - s, Relaxed);
        }
    }
}

/// Adds `ns` to `c` (a span measured by hand).
#[inline(always)]
pub fn add(c: Cat, ns: u64) {
    if ON.load(Relaxed) {
        NS[c as usize].fetch_add(ns, Relaxed);
    }
}

#[inline(always)]
pub fn count(c: Cat) {
    if ON.load(Relaxed) {
        NS[c as usize].fetch_add(1, Relaxed);
    }
}

pub fn snapshot() -> [u64; N] {
    std::array::from_fn(|i| NS[i].load(Relaxed))
}

/// Per-frame slow-frame log (LMA_FRAME_PROFILE=1, threshold LMA_SLOW_MS, default 30 ms): when a
/// guest frame takes longer, where its time went on both threads.
pub fn frame_tick(frame: u64) {
    use std::sync::Mutex;
    static LAST: Mutex<Option<(Instant, [u64; N])>> = Mutex::new(None);
    if !ON.load(Relaxed) {
        return;
    }
    let now = (Instant::now(), snapshot());
    let mut last = LAST.lock().unwrap();
    if let Some((t, before)) = last.replace(now) {
        let ms = (now.0 - t).as_secs_f64() * 1e3;
        static LIMIT: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| std::env::var("LMA_SLOW_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(30.0));
        if ms > limit {
            let d = |c: Cat| (now.1[c as usize] - before[c as usize]) as f64 / 1e6;
            let n = |c: Cat| now.1[c as usize] - before[c as usize];
            eprintln!(
                "slow frame {frame}: {ms:.1} ms | guest {:.1}, CP {:.1} (present wait {:.1}, resolve wait {:.1}, drain {:.1}), sleep {:.1} | render busy {:.1} (GPU waits {:.1}: textures {:.1}, vertices {:.1}), {} draws, {} batches, {} setup misses | pipelines {:.1}, textures {:.1}, flushes {:.1}, landings {:.1}, resolves {:.1}, presents {:.1}",
                d(Cat::Slice) - d(Cat::Kernel), d(Cat::Cp), d(Cat::WaitPresent), d(Cat::WaitResolve), d(Cat::WaitDrain), d(Cat::Sleep),
                d(Cat::RenderBusy), d(Cat::GpuWait), d(Cat::GpuWaitTex), d(Cat::GpuWaitVerts), n(Cat::RenderDraws), n(Cat::Flushes), n(Cat::SetupMiss),
                d(Cat::RtPipeline), d(Cat::RtTexture), d(Cat::RtFlush), d(Cat::RtLand), d(Cat::RtResolve), d(Cat::RtPresent),
            );
            if crate::raster::tex_stats_on() {
                let s: Vec<String> = crate::raster::tex_stats_take().iter().take(6).map(|(f, n, px, ns)| format!("fmt {f}: {n} x, {:.0} K texels, {:.1} ms", *px as f64 / 1e3, *ns as f64 / 1e6)).collect();
                eprintln!("  texture decodes: {}", s.join("; "));
            }
        } else if crate::raster::tex_stats_on() {
            crate::raster::tex_stats_take();
        }
    }
}

/// One line: the change from `before` to `now` per guest frame, over `wall` seconds.
pub fn report(before: &[u64; N], now: &[u64; N], frames: u64, wall: f64) -> String {
    let d = |c: Cat| (now[c as usize] - before[c as usize]) as f64;
    let f = frames.max(1) as f64;
    let ms = |c: Cat| d(c) / f / 1e6;
    let frame = wall * 1e3 / f;
    // the command processor's share of the game thread: its waits for the CP thread, or all of it
    let cp_game = if CP_THREAD.load(Relaxed) { ms(Cat::CpWait) } else { ms(Cat::Cp) };
    let game_other = frame - ms(Cat::Slice) - cp_game - ms(Cat::Sleep) - ms(Cat::Xma);
    format!(
        "profile: {frames} guest frames, {frame:.1} ms/frame | game thread: guest code {:.1}, kernel {:.1}, CP {:.1} (CP thread wait {:.1}) \
         (waits: present {:.1}, resolve {:.1}, drain {:.1}), sleep {:.1}, XMA {:.1}, other {:.1} | render thread: busy {:.1} \
         (GPU waits {:.1}, of which present {:.1}, textures {:.1}, vertices {:.1}), idle {:.1}, CPU transform {:.1}, ps setup {:.1}, vwin {:.1}, consts {:.1}, record {:.1}, {:.0} draws in {:.0} batches ({:.0} bind groups), {:.2} us/draw, setup misses {:.0}% (refreshed {:.0}%, re-validated {:.0}% in {:.1} ms of which decoded {:.1} ms, kept by the write watch {:.0}%), new float sets {:.0}%, uploads {:.0} KB, landings {:.0} KB | CP runs {:.0}/frame, {:.0}% blocked ({:.0}% on the render thread) | \
         interrupts: render thread {:.1} (its commands {:.1}, then GPU/read-backs), queued {:.1}, handler {:.1}",
        ms(Cat::Slice) - ms(Cat::Kernel),
        ms(Cat::Kernel),
        ms(Cat::Cp),
        ms(Cat::CpWait),
        ms(Cat::WaitPresent),
        ms(Cat::WaitResolve),
        ms(Cat::WaitDrain),
        ms(Cat::Sleep),
        ms(Cat::Xma),
        game_other,
        ms(Cat::RenderBusy),
        ms(Cat::GpuWait),
        ms(Cat::GpuWaitPresent),
        ms(Cat::GpuWaitTex),
        ms(Cat::GpuWaitVerts),
        frame - ms(Cat::RenderBusy),
        ms(Cat::RtTransform),
        ms(Cat::RtPsSetup),
        ms(Cat::RtVwin),
        ms(Cat::RtConsts),
        ms(Cat::RtRecord),
        d(Cat::RenderDraws) / f,
        d(Cat::Flushes) / f,
        d(Cat::BindGroups) / f,
        d(Cat::RenderBusy) / 1e3 / d(Cat::RenderDraws).max(1.0),
        100.0 * d(Cat::SetupMiss) / d(Cat::RenderDraws).max(1.0),
        100.0 * d(Cat::SetupRefresh) / d(Cat::RenderDraws).max(1.0),
        100.0 * d(Cat::SetupCheck) / d(Cat::RenderDraws).max(1.0),
        ms(Cat::SetupCheckTime),
        ms(Cat::SetupCheckDecoded),
        100.0 * d(Cat::SetupFast) / d(Cat::RenderDraws).max(1.0),
        100.0 * d(Cat::FloatsNew) / d(Cat::FloatsLookups).max(1.0),
        d(Cat::UploadBytes) / f / 1e3,
        d(Cat::LandBytes) / f / 1e3,
        d(Cat::CpRuns) / f,
        100.0 * d(Cat::CpBlocked) / d(Cat::CpRuns).max(1.0),
        100.0 * d(Cat::CpBlockedGpu) / d(Cat::CpRuns).max(1.0),
        ms(Cat::IrqDefer),
        ms(Cat::IrqRender),
        ms(Cat::IrqQueue),
        ms(Cat::IrqHandler),
    )
}
