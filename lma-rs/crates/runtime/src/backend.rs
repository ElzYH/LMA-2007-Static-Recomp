//! Rendering backends. The PM4 command processor (`gpu.rs`) runs on the CPU and hands every
//! draw, resolve and present to a `RenderBackend`. `raster::Renderer` (software) is the
//! reference; a wgpu backend (#6) implements the same trait.

use crate::mem::Memory;
use crate::shader_exec::Vec4;

/// Guest memory as a backend reads and writes it: `Memory` itself on the guest thread, or (#95)
/// a render thread's view that reads through `guest::mem::MemReader` and hands its writes
/// (resolve copies landing) back to the guest thread.
pub trait GuestMem {
    fn read(&mut self, a: u32, out: &mut [u8]);
    fn write(&mut self, a: u32, data: &[u8]);
    /// Translated addresses of the 4 KB pages in `len` bytes at `a` written since the last query
    /// of each, or None when this memory can't tell (then callers re-hash as before).
    fn write_watch(&mut self, _a: u32, _len: u32) -> Option<Vec<u32>> {
        None
    }
}

impl GuestMem for Memory {
    fn read(&mut self, a: u32, out: &mut [u8]) {
        Memory::read(self, a, out)
    }
    fn write(&mut self, a: u32, data: &[u8]) {
        Memory::write(self, a, data)
    }
    fn write_watch(&mut self, a: u32, len: u32) -> Option<Vec<u32>> {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *OFF.get_or_init(|| std::env::var("LMA_TEX_WATCH").as_deref() == Ok("0")) {
            return None;
        }
        Memory::write_watch(self, a, len)
    }
}

/// What the command processor needs of guest memory: `Memory` on the game thread, or the
/// command-processor thread's view (`crate::cp_thread`). Big-endian words, as `Memory` has them.
pub trait CpMem: GuestMem {
    fn read32(&mut self, a: u32) -> u32 {
        let mut b = [0; 4];
        self.read(a, &mut b);
        u32::from_be_bytes(b)
    }
    fn write32(&mut self, a: u32, v: u32) {
        self.write(a, &v.to_be_bytes())
    }
    fn write16(&mut self, a: u32, v: u16) {
        self.write(a, &v.to_be_bytes())
    }
}

impl CpMem for Memory {
    fn read32(&mut self, a: u32) -> u32 {
        Memory::read32(self, a)
    }
    fn write32(&mut self, a: u32, v: u32) {
        Memory::write32(self, a, v)
    }
    fn write16(&mut self, a: u32, v: u16) {
        Memory::write16(self, a, v)
    }
}

/// Counters and host timings a backend reports (for `lma-cli` and the app status line).
#[derive(Clone, Debug, Default)]
pub struct BackendStats {
    /// Host time in (vertex transform, pixel raster incl. textures, resolve, present, texture decode).
    pub timing: [std::time::Duration; 5],
    pub pixels_drawn: u64,
    pub resolves: u64,
}

/// An RGBA8 image: (width, height, pixels).
pub type Image = (u32, u32, Vec<u8>);

pub trait RenderBackend: Send {
    /// Short name for logs ("software", "wgpu").
    fn name(&self) -> &'static str;

    /// Execute one draw packet (DRAW_INDX / DRAW_INDX_2) with the current register state.
    /// Resolves arrive here too (RB_MODECONTROL = copy).
    fn draw(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, vs: &[u32], ps: &[u32]);

    /// Convert the front buffer at `fb` (written by VdSwap) to RGBA8.
    fn present(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) -> Option<Image>;

    /// Debug capture: start collecting per-draw lines, or take what was collected.
    fn start_debug(&mut self);
    fn take_debug(&mut self) -> Option<Vec<String>>;
    fn debugging(&self) -> bool;

    /// Texture dumps made while debugging: (name, width, height, RGBA8).
    fn take_textures(&mut self) -> Vec<(String, u32, u32, Vec<u8>)>;

    fn stats(&self) -> BackendStats;

    /// EDRAM colour and depth for save states (backends that keep EDRAM on the GPU read it back).
    fn targets(&self) -> (Vec<Vec4>, Vec<f32>);
    fn set_targets(&mut self, edram: Vec<Vec4>, depth: Vec<f32>);

    /// Finish any guest memory writes still in flight (e.g. asynchronous resolve readbacks).
    /// Called when the command processor stops, before guest code can look at memory again.
    fn sync(&mut self, _mem: &mut dyn GuestMem) {}

    /// Everything the GPU wrote to guest memory so far must be visible now (blocking).
    fn fence(&mut self, _mem: &mut dyn GuestMem) {}

    /// The GPU writes submitted so far to guest physical [lo, hi) must be visible now (blocking
    /// only when one is still in flight).
    fn complete_range(&mut self, _mem: &mut dyn GuestMem, _lo: u32, _hi: u32) {}

    /// Asynchronous fences: a ticket for "every guest memory write submitted so far" (0 when
    /// nothing is in flight), and whether that ticket has landed; `landed` may write finished
    /// copies to memory but never blocks.
    /// Asynchronous fences on or off (Gpu decides; see Gpu::async_fence).
    fn set_async_fence(&mut self, _on: bool) {}

    /// The 64-register blocks written since the last `draw` or `present` (one bit each, see
    /// `gpu::REG_BLOCK_WORDS`), passed just before it.
    fn regs_dirty(&mut self, _blocks: &[u64]) {}

    /// The shaders the next `draw` passes may differ from the previous draw's (a shader was
    /// bound, a state loaded, skipped draws replayed). Until then they are the same slices.
    fn shaders_dirty(&mut self) {}

    fn write_ticket(&self) -> u64 {
        0
    }
    fn landed(&mut self, _mem: &mut dyn GuestMem, _ticket: u64) -> bool {
        true
    }
}
