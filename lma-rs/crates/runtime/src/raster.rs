//! Software renderer for the Xenos command stream: EDRAM render targets, a triangle rasteriser
//! driven by the shader interpreter, resolves (EDRAM -> tiled textures) and front-buffer output.
//!
//! Register meanings follow Xenia (BSD-3-Clause). Correctness first; speed comes later (wgpu).

use crate::gpu::phys;
use crate::backend::GuestMem;
use crate::shader::{Shader, TextureFetch};
use crate::shader_exec::{Invocation, Outputs, ShaderEnv, Vec4};
use crate::fasthash::HashMap;
use std::sync::Arc;

/// Debug: trace every shaded fragment at one window pixel (LMA_PIXEL=x,y), per render target.
pub static PIXEL_TRACE: std::sync::Mutex<Option<(i32, i32, Vec<String>)>> = std::sync::Mutex::new(None);

// registers
const RB_SURFACE_INFO: usize = 0x2000;
const RB_COLOR_INFO: usize = 0x2001;
const RB_DEPTH_INFO: usize = 0x2002;
const RB_DEPTHCONTROL: usize = 0x2200;
const RB_DEPTH_CLEAR: usize = 0x231D;
const PA_SC_WINDOW_OFFSET: usize = 0x2080;
const PA_SC_WINDOW_SCISSOR_TL: usize = 0x2081;
const PA_SC_WINDOW_SCISSOR_BR: usize = 0x2082;
const PA_CL_VPORT_XSCALE: usize = 0x210F;
const PA_CL_VTE_CNTL: usize = 0x2206;
const PA_SU_VTX_CNTL: usize = 0x2302;

/// Screen-space shift from PA_SU_VTX_CNTL.PIX_CENTER. With 0 (the Direct3D 9 convention the
/// game uses) pixel centres sit on integer coordinates, so 2D quads are placed at -0.5 offsets
/// and their edges fall exactly on our sample points (x + 0.5). Shifting by half a pixel puts
/// them back on pixel edges: no fill-rule ties and texel-centred UVs.
pub(crate) fn pix_offset(regs: &[u32]) -> f32 {
    if regs[PA_SU_VTX_CNTL] & 1 == 0 { 0.5 } else { 0.0 }
}

/// A resolve's EDRAM rectangle [x0, y0, x1, y1]: its vertices in window coordinates (window
/// offset applied, like draws), clipped to the window scissor. With predicated tiling the game
/// replays one command buffer per screen band and resolves every band in each pass; the scissor
/// empties the resolves of the other bands (a match's depth target was overwritten with the
/// bottom band's rows), and trims bloom levels to their texture (320x184 -> 320x180).
pub(crate) fn resolve_rect(regs: &[u32], verts: &[Vtx]) -> [u32; 4] {
    let woff = regs[PA_SC_WINDOW_OFFSET];
    let (wox, woy) = (((woff & 0x7FFF) << 17) as i32 >> 17, (((woff >> 16) & 0x7FFF) << 17) as i32 >> 17);
    let pix = pix_offset(regs); // resolve rectangles ignore the pixel-centre shift
    let xs = verts.iter().map(|v| v.pos[0] - pix + wox as f32);
    let ys = verts.iter().map(|v| v.pos[1] - pix + woy as f32);
    // vertices sit on pixel edges (e.g. -0.5 .. 1279.5); round to whole pixels
    let x0 = xs.clone().fold(f32::MAX, f32::min).round().max(0.0) as u32;
    let x1 = xs.fold(f32::MIN, f32::max).round().max(0.0) as u32;
    let y0 = ys.clone().fold(f32::MAX, f32::min).round().max(0.0) as u32;
    let y1 = ys.fold(f32::MIN, f32::max).round().max(0.0) as u32;
    let s = RasterCtx::window_scissor(regs, wox, woy);
    let (x0, y0) = (x0.max(s[0] as u32), y0.max(s[1] as u32));
    [x0, y0, x1.min(s[2].max(0) as u32).max(x0), y1.min(s[3].max(0) as u32).max(y0)]
}
const RB_MODECONTROL: usize = 0x2208;
const RB_COLORCONTROL: usize = 0x2202;
const RB_BLENDCONTROL0: usize = 0x2201;
const RB_COLOR_MASK: usize = 0x2104;
const RB_ALPHA_REF: usize = 0x210E;
const RB_BLEND_RED: usize = 0x2105;
const PA_SU_POINT_SIZE: usize = 0x2280;
const PA_SU_SC_MODE_CNTL: usize = 0x2205;
const PA_SU_POLY_OFFSET_FRONT_SCALE: usize = 0x2380; // then FRONT_OFFSET, BACK_SCALE, BACK_OFFSET
const VGT_DMA_SIZE: usize = 0x21FB;
const RB_COPY_CONTROL: usize = 0x2318;
const RB_COPY_DEST_BASE: usize = 0x2319;
const RB_COPY_DEST_PITCH: usize = 0x231A;
const RB_COPY_DEST_INFO: usize = 0x231B;
const RB_COLOR_CLEAR: usize = 0x231E;
const FETCH_BASE: usize = 0x4800;
const FLOAT_BASE: usize = 0x4000;
const BOOL_BASE: usize = 0x4900;
const LOOP_BASE: usize = 0x4908;

const EDRAM_TILES: usize = 2048;
const TILE_W: usize = 80;
const TILE_H: usize = 16;

#[derive(Clone, Copy)]
pub(crate) struct Vtx {
    pub(crate) pos: [f32; 4],  // screen x, y, z, 1/w
    pub(crate) clip: [f32; 4], // shader output position (w already converted from 1/w if VTX_W0_FMT)
    pub(crate) interp: [Vec4; 16],
}

pub struct Renderer {
    pub(crate) edram: Vec<Vec4>,
    /// Depth for every EDRAM sample (stencil is not modelled).
    pub(crate) depth: Vec<f32>,
    /// Decoded shaders by code hash, shared: a draw takes an `Arc`, not a copy of the decoded
    /// instructions (the GPU path's CPU fallback, single-point sprites, cloned it per draw: #304)
    shaders: HashMap<u64, Arc<Shader>>,
    /// Last presented frame: (width, height, RGBA8).
    pub frame: Option<(u32, u32, Vec<u8>)>,
    pub pixels_drawn: u64,
    pub resolves: u64,
    /// Debug lines for the captured frame.
    pub debug: Option<Vec<String>>,
    /// Debug texture dumps: (name, width, height, RGBA8).
    pub textures_out: Vec<(String, u32, u32, Vec<u8>)>,
    /// Host time in (vertex transform, pixel raster incl. textures, resolve, present, texture decode).
    pub timing: [std::time::Duration; 5],
    /// Decoded textures: (hash of the raw bytes, generation last verified, texels, `page_gen`
    /// stamp when last verified).
    tex_cache: HashMap<TexKey, (u64, u64, Arc<Decoded>, u32)>,
    /// `tex_override::generation()` the cache was filled under: a change drops every entry (#694).
    override_gen: u64,
    /// Where guest memory has a write watch (`GuestMem::write_watch`): per 4 KB page, the stamp
    /// of the query that last saw it written. A texture none of whose pages has a newer stamp
    /// than its last verification wasn't written since, so its hash still holds (#6).
    page_gen: Vec<u32>,
    page_stamp: u32,
    /// Bumped at every present and resolve: cached textures are re-hashed once per generation.
    tex_gen: u64,
    /// Worker threads for big draws.
    pub threads: usize,
    /// Decode 8-bit unorm textures straight to RGBA8 (`Decoded::px8`): the wgpu backend uploads
    /// those bytes and never samples on the CPU (#304).
    pub(crate) rgba8: bool,
    /// DXT textures as their BC blocks and the guest's own mips (`Decoded::bc`, `bc_levels`): the
    /// wgpu backend uploads them compressed (LMA_TEX_BC=1 on an adapter with BC formats, #304).
    pub(crate) bc: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            edram: vec![[0.0; 4]; EDRAM_TILES * TILE_W * TILE_H],
            depth: vec![1.0; EDRAM_TILES * TILE_W * TILE_H],
            shaders: HashMap::default(),
            frame: None,
            pixels_drawn: 0,
            resolves: 0,
            debug: None,
            textures_out: Vec::new(),
            timing: Default::default(),
            tex_cache: HashMap::default(),
            override_gen: crate::tex_override::generation(),
            page_gen: Vec::new(),
            page_stamp: 0,
            tex_gen: 1,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16),
            rgba8: false,
            bc: false,
        }
    }
}

pub(crate) fn hash(code: &[u32]) -> u64 {
    code.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &w| (h ^ w as u64).wrapping_mul(0x100_0000_01b3))
}

fn gpu_swap(v: u32, endian: u32) -> u32 {
    match endian & 3 {
        1 => ((v >> 8) & 0x00FF_00FF) | ((v << 8) & 0xFF00_FF00),
        2 => v.swap_bytes(),
        3 => v.rotate_left(16),
        _ => v,
    }
}

/// Read a GPU dword from guest memory: little-endian load, then the Xenos endian swap.
fn gpu_read(mem: &mut dyn GuestMem, addr: u32, endian: u32) -> u32 {
    let mut b = [0u8; 4];
    mem.read(phys(addr), &mut b);
    gpu_swap(u32::from_le_bytes(b), endian)
}

fn gpu_write(mem: &mut dyn GuestMem, addr: u32, endian: u32, v: u32) {
    mem.write(phys(addr), &gpu_swap(v, endian).to_le_bytes());
}

/// The vertex indices of a draw: from the index buffer (DMA) or generated (auto).
pub(crate) fn draw_indices(regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32) -> Vec<u32> {
    let mut out = Vec::new();
    draw_indices_into(regs, mem, init, index_base, &mut out);
    out
}

/// `draw_indices` into `out` (cleared first), so a caller can keep one buffer for every draw
/// (a Vec per draw was ~2% of the wgpu render thread in a match, #304).
pub(crate) fn draw_indices_into(regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, out: &mut Vec<u32>) {
    out.clear();
    let count = init >> 16;
    match (init >> 6) & 3 {
        0 => {
            let swap = regs[VGT_DMA_SIZE] >> 30;
            let is32 = (init >> 11) & 1 == 1;
            // one read for the whole index buffer (per index, it was a tenth of the render
            // thread in a match), then each index as `gpu_read` returns it
            let lo = index_base & !3;
            let hi = index_base.wrapping_add(count * if is32 { 4 } else { 2 }).next_multiple_of(4);
            // the read buffer is reused across draws (a zeroed Vec per draw showed in the render
            // thread's allocations, #304); `mem.read` overwrites all of it
            thread_local! {
                static INDEX_BYTES: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
            }
            let mut buf = INDEX_BYTES.with_borrow_mut(std::mem::take);
            buf.resize(hi.wrapping_sub(lo) as usize, 0);
            mem.read(phys(lo), &mut buf);
            struct Give(Vec<u8>);
            impl Drop for Give {
                fn drop(&mut self) {
                    INDEX_BYTES.with_borrow_mut(|b| *b = std::mem::take(&mut self.0));
                }
            }
            let buf = Give(buf);
            let buf = &buf.0;
            // whole words, swapped as `gpu_swap` does, then the 16- or 32-bit indices in order
            // (per index through a closure, this was ~5% of the render thread in a match); one
            // loop per swap mode so each one vectorises
            if is32 && index_base & 3 != 0 {
                // unaligned 32-bit indices (never seen): byte offsets inside the swapped words
                let word = |a: u32| {
                    let o = (a - lo) as usize;
                    gpu_swap(u32::from_le_bytes(buf[o..o + 4].try_into().unwrap()), swap)
                };
                out.extend((0..count).map(|i| word(index_base + 4 * i)));
                return;
            }
            let skip = ((index_base - lo) / if is32 { 4 } else { 2 }) as usize;
            {
                fn decode(out: &mut Vec<u32>, buf: &[u8], is32: bool, skip: usize, count: usize, f: impl Fn(u32) -> u32) {
                    out.reserve(count + 1);
                    let mut words = buf.chunks_exact(4).map(|c| f(u32::from_le_bytes(c.try_into().unwrap())));
                    if is32 {
                        out.extend(words.skip(skip).take(count));
                    } else {
                        // `skip` is 0 or 1 (the buffer starts on the word holding the first
                        // index): an odd start takes the first word's high half alone, instead
                        // of decoding it whole and shifting the vector down (`drain`)
                        if skip == 1 {
                            if let Some(w) = words.next() {
                                out.push(w >> 16);
                            }
                        }
                        for w in words {
                            out.push(w & 0xFFFF);
                            out.push(w >> 16);
                        }
                        out.truncate(count);
                    }
                }
                let count = count as usize;
                match swap & 3 {
                    1 => decode(out, buf, is32, skip, count, |v| ((v >> 8) & 0x00FF_00FF) | ((v << 8) & 0xFF00_FF00)),
                    2 => decode(out, buf, is32, skip, count, u32::swap_bytes),
                    3 => decode(out, buf, is32, skip, count, |v| v.rotate_left(16)),
                    _ => decode(out, buf, is32, skip, count, |v| v),
                }
            }
        }
        _ => out.extend(0..count),
    }
}

/// Xenos 2D tiling (Xenia texture_address::Tiled2D): byte offset of block (x, y).
pub fn tiled_2d(x: i32, y: i32, pitch_aligned: u32, bpb_log2: u32) -> u32 {
    let outer = (((y >> 5) * (pitch_aligned >> 5) as i32) + (x >> 5)) << 6;
    let inner = (((y >> 1) & 7) << 3) | (x & 7);
    let oib = ((outer | inner) as u32) << bpb_log2;
    let bank = ((y >> 4) & 1) as u32;
    let pipe = (((x >> 3) & 3) ^ (((y >> 3) & 1) << 1)) as u32;
    let y_lsb = (y & 1) as u32;
    (y_lsb << 4) | (pipe << 6) | (bank << 11) | (oib & 0xF) | (((oib >> 4) & 1) << 5) | (((oib >> 5) & 7) << 8) | ((oib >> 8) << 12)
}

// ---------------------------------------------------------------- texture decoding
#[derive(Clone, Copy, Debug)]
pub struct TexInfo {
    pub base: u32,
    pub format: u32,
    pub endian: u32,
    pub tiled: bool,
    pub pitch: u32, // texels
    pub width: u32,
    pub height: u32,
    pub swizzle: u32,
    pub clamp: [u32; 2],
    /// Where level 0 starts inside its packed mip tail, in texels (see `packed_mip0_offset`).
    pub packed: (u32, u32),
}

impl TexInfo {
    pub fn from_fetch(d: &[u32]) -> Self {
        Self {
            base: (d[1] >> 12) << 12,
            format: d[1] & 0x3F,
            endian: (d[1] >> 6) & 3,
            tiled: d[0] >> 31 == 1,
            pitch: ((d[0] >> 22) & 0x1FF) * 32,
            width: (d[2] & 0x1FFF) + 1,
            height: ((d[2] >> 13) & 0x1FFF) + 1,
            swizzle: (d[3] >> 1) & 0xFFF,
            clamp: [(d[0] >> 10) & 7, (d[0] >> 13) & 7],
            // dword 5 bit 11: packed mips
            packed: if d.get(5).is_some_and(|w| w >> 11 & 1 == 1) { packed_mip0_offset((d[2] & 0x1FFF) + 1, ((d[2] >> 13) & 0x1FFF) + 1) } else { (0, 0) },
        }
    }
}

/// With packed mips, the levels whose smaller side is 16 texels or less share one tile, and
/// even level 0 then sits at an offset in it: 16 texels down for a wide texture, 16 across
/// otherwise (Xenia's GetPackedMipOffset for mip 0). E.g. a 512x4 DXT5 lighting ramp keeps its
/// level 0 at row 16; reading row 0 gave the mip tail's gaps (black lighting on the 3D players).
fn packed_mip0_offset(width: u32, height: u32) -> (u32, u32) {
    let (lw, lh) = (width.next_power_of_two().trailing_zeros(), height.next_power_of_two().trailing_zeros());
    if lw.min(lh) > 4 {
        (0, 0) // the shorter side is over 16: level 0 isn't packed
    } else if lw > lh {
        (0, 16)
    } else {
        (16, 0)
    }
}

/// (block width/height in texels, bytes per block log2)
fn block_info(format: u32) -> Option<(u32, u32)> {
    Some(match format {
        2 | 8 | 9 => (1, 0),
        3..=5 | 10 | 15 | 24 | 30 => (1, 1),
        6 | 7 | 14 | 16 | 17 | 22 | 23 | 25 | 31 | 33 | 36 => (1, 2),
        26 | 32 | 34 | 37 => (1, 3),
        18 => (4, 3),
        19 | 20 => (4, 4),
        35 | 38 => (1, 4),
        _ => return None,
    })
}

fn unorm(v: u32, bits: u32) -> f32 {
    v as f32 / ((1u32 << bits) - 1) as f32
}

fn rgb565(c: u16) -> Vec4 {
    let c = c as u32;
    [unorm((c >> 11) & 31, 5), unorm((c >> 5) & 63, 6), unorm(c & 31, 5), 1.0]
}

/// Decode one texel at integer coordinates (already clamped/wrapped).
fn texel(mem: &mut dyn GuestMem, t: &TexInfo, x: u32, y: u32) -> Vec4 {
    texel_with(|a| gpu_read(mem, a, t.endian), t, x, y)
}

/// Texel decode over any dword source (`read` gets the guest address, returns the swapped dword).
fn texel_with(mut read: impl FnMut(u32) -> u32, t: &TexInfo, x: u32, y: u32) -> Vec4 {
    let Some((bw, bpb_log2)) = block_info(t.format) else { return [1.0, 0.0, 1.0, 1.0] };
    let (x, y) = (x + t.packed.0, y + t.packed.1);
    let (bx, by) = (x / bw, y / bw);
    let pitch_blocks = (t.pitch.max(t.width) / bw).max(1);
    let offset = if t.tiled {
        tiled_2d(bx as i32, by as i32, (pitch_blocks + 31) & !31, bpb_log2)
    } else {
        (by * pitch_blocks + bx) << bpb_log2
    };
    let addr = t.base + offset;
    // canonical little-endian bytes after the endian swap
    let mut bytes = [0u8; 16];
    let n = (1u32 << bpb_log2).max(4);
    for k in (0..n).step_by(4) {
        let w = read((addr & !3) + k);
        bytes[k as usize..k as usize + 4].copy_from_slice(&w.to_le_bytes());
    }
    let sub = (addr & 3) as usize;
    let u16_at = |i: usize| u16::from_le_bytes([bytes[i], bytes[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes(bytes[i..i + 4].try_into().unwrap());
    let c: Vec4 = match t.format {
        2 | 8 | 9 => {
            let v = unorm(bytes[sub] as u32, 8);
            [v, v, v, v]
        }
        10 => {
            let v = u16_at(sub & 2);
            [unorm(v as u32 & 0xFF, 8), unorm((v as u32) >> 8, 8), 0.0, 1.0]
        }
        3 => {
            let v = u16_at(sub & 2) as u32;
            [unorm(v & 31, 5), unorm((v >> 5) & 31, 5), unorm((v >> 10) & 31, 5), unorm(v >> 15, 1)]
        }
        4 => {
            let v = u16_at(sub & 2);
            let c = rgb565(v);
            [c[2], c[1], c[0], 1.0] // x = low bits
        }
        15 => {
            let v = u16_at(sub & 2) as u32;
            [unorm(v & 15, 4), unorm((v >> 4) & 15, 4), unorm((v >> 8) & 15, 4), unorm(v >> 12, 4)]
        }
        6 | 14 => {
            let v = u32_at(0);
            [unorm(v & 0xFF, 8), unorm((v >> 8) & 0xFF, 8), unorm((v >> 16) & 0xFF, 8), unorm(v >> 24, 8)]
        }
        7 => {
            let v = u32_at(0);
            [unorm(v & 0x3FF, 10), unorm((v >> 10) & 0x3FF, 10), unorm((v >> 20) & 0x3FF, 10), unorm(v >> 30, 2)]
        }
        18..=20 => decode_dxt(&bytes, t.format, x % 4, y % 4),
        36 => [f32::from_bits(u32_at(0)), 0.0, 0.0, 1.0],
        22 => [((u32_at(0) >> 8) as f32 / 16_777_215.0); 4], // 24_8 depth as a float
        23 => {
            // 24_8_FLOAT: 20e4 float depth in the top 24 bits
            let v = u32_at(0) >> 8;
            let (e, m) = ((v >> 20) & 15, v & 0xF_FFFF);
            let d = if e == 0 { m as f32 / (1 << 20) as f32 * 2f32.powi(-14) } else { (1.0 + m as f32 / (1 << 20) as f32) * 2f32.powi(e as i32 - 15) };
            [d; 4]
        }
        _ => [1.0, 0.0, 1.0, 1.0],
    };
    apply_tex_swizzle(c, t.swizzle)
}

#[cfg(test)]
mod dxt_tests {
    use super::*;

    /// The block-at-a-time DXT decode gives exactly `texel_with`'s pixels: DXT1/3/5, tiled and
    /// linear, sizes that end inside a block, a block-aligned packed-mip offset, a swizzle, and
    /// a texture big enough to be split across threads.
    #[test]
    fn dxt_blocks_match_texel_decode() {
        let mut seed = 0x1234_5678_9ABC_DEF1u64;
        let buf: Vec<u8> = (0..1 << 20)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for (format, endian) in (18..=20).flat_map(|f| (0..4).map(move |e| (f, e))) {
            for tiled in [false, true] {
                for (w, h, packed, swizzle, threads) in [(37, 21, (0, 0), 0o3210, 4), (64, 64, (4, 8), 0o0123, 16), (5, 3, (0, 0), 0o5432, 1), (512, 300, (0, 4), 0o3210, 16)] {
                    let t = TexInfo { base: 0x1000, format, endian, tiled, pitch: 128, width: w, height: h, swizzle, clamp: [0, 0], packed };
                    let fast = decode_texture(&buf, &t, threads);
                    let read = |a: u32| {
                        let o = a.wrapping_sub(t.base) as usize;
                        buf.get(o..o + 4).map(|b| gpu_swap(u32::from_le_bytes(b.try_into().unwrap()), t.endian)).unwrap_or(0)
                    };
                    for y in 0..h {
                        for x in 0..w {
                            let want = texel_with(read, &t, x, y);
                            let got = fast[(y * w + x) as usize];
                            assert!(want.iter().zip(got).all(|(a, b)| a.to_bits() == b.to_bits()), "fmt {format} endian {endian} tiled {tiled} {w}x{h} packed {packed:?}: texel {x},{y}: {got:?} vs {want:?}");
                        }
                    }
                }
            }
        }
    }

    /// A whole block at once gives each texel's `decode_dxt` bits, for random blocks of DXT1/3/5
    /// (both DXT1 modes and both DXT5 alpha modes come up).
    #[test]
    fn dxt_block_decode_matches_per_texel() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..20_000 {
            let mut b = [0u8; 16];
            for x in &mut b {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                *x = seed as u8;
            }
            for format in 18..=20 {
                let block = decode_dxt_block(&b, format);
                for k in 0..16u32 {
                    let want = decode_dxt(&b, format, k % 4, k / 4);
                    assert!(want.iter().zip(block[k as usize]).all(|(a, c)| a.to_bits() == c.to_bits()), "fmt {format} block {b:02x?} texel {k}");
                }
            }
        }
    }

    /// The direct RGBA8 decode gives the bytes of the float decode converted afterwards, for
    /// every 8-bit unorm format: endians, tiled and linear, odd sizes, packed offsets (DXT's
    /// block path and its texel fallback), swizzles with constants, and thread splits.
    #[test]
    fn rgba8_decode_matches_float_decode_converted() {
        let mut seed = 0x0F1E_2D3C_4B5A_6978u64;
        let buf: Vec<u8> = (0..1 << 20)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        for format in UNORM8_FORMATS {
            for endian in 0..4 {
                for tiled in [false, true] {
                    for (w, h, packed, swizzle, threads) in [(37, 21, (0, 0), 0o3210, 4), (64, 64, (4, 8), 0o0123, 16), (5, 3, (1, 2), 0o5432, 1), (512, 300, (0, 4), 0o3210, 16), (300, 513, (0, 0), 0o4321, 3)] {
                        let t = TexInfo { base: 0x1000, format, endian, tiled, pitch: 512, width: w, height: h, swizzle, clamp: [0, 0], packed };
                        let want: Vec<u8> = decode_texture(&buf, &t, threads).iter().flat_map(|c| c.map(|v| (v * 255.0 + 0.5) as u8)).collect();
                        assert_eq!(decode_texture_rgba8(&buf, &t, threads), Some(want), "fmt {format} endian {endian} tiled {tiled} {w}x{h} packed {packed:?}");
                    }
                }
            }
        }
    }
}

fn apply_tex_swizzle(c: Vec4, swz: u32) -> Vec4 {
    std::array::from_fn(|i| match (swz >> (3 * i)) & 7 {
        s @ 0..=3 => c[s as usize],
        4 => 0.0,
        _ => 1.0,
    })
}

fn decode_dxt(b: &[u8; 16], format: u32, px: u32, py: u32) -> Vec4 {
    let (color, alpha_part) = if format == 18 { (&b[0..8], None) } else { (&b[8..16], Some(&b[0..8])) };
    let c0 = u16::from_le_bytes([color[0], color[1]]);
    let c1 = u16::from_le_bytes([color[2], color[3]]);
    let idx = (u32::from_le_bytes(color[4..8].try_into().unwrap()) >> (2 * (py * 4 + px))) & 3;
    let (a, bcol) = (rgb565(c0), rgb565(c1));
    let lerp = |t: f32| -> Vec4 { std::array::from_fn(|i| a[i] + (bcol[i] - a[i]) * t) };
    let mut out = match (idx, c0 > c1 || format != 18) {
        (0, _) => a,
        (1, _) => bcol,
        (2, true) => lerp(1.0 / 3.0),
        (3, true) => lerp(2.0 / 3.0),
        (2, false) => lerp(0.5),
        _ => [0.0, 0.0, 0.0, 0.0],
    };
    match (format, alpha_part) {
        (19, Some(ab)) => {
            let bits = u64::from_le_bytes(ab.try_into().unwrap());
            out[3] = ((bits >> (4 * (py * 4 + px))) & 15) as f32 / 15.0;
        }
        (20, Some(ab)) => {
            let (a0, a1) = (ab[0] as f32 / 255.0, ab[1] as f32 / 255.0);
            let bits = u64::from_le_bytes([ab[2], ab[3], ab[4], ab[5], ab[6], ab[7], 0, 0]);
            let i = ((bits >> (3 * (py * 4 + px))) & 7) as u32;
            out[3] = match i {
                0 => a0,
                1 => a1,
                i if a0 > a1 => ((8 - i) as f32 * a0 + (i - 1) as f32 * a1) / 7.0,
                6 => 0.0,
                7 => 1.0,
                i => ((6 - i) as f32 * a0 + (i - 1) as f32 * a1) / 5.0,
            };
        }
        _ => {}
    }
    out
}

/// A DXT block's 16 texels, row by row, each exactly `decode_dxt(b, format, px, py)` (same
/// expressions in the same order, so the same bits): the colour and alpha palettes are built once
/// per block instead of once per texel (#304: the first frames of a match decode ~33 M DXT texels).
fn decode_dxt_block(b: &[u8; 16], format: u32) -> [Vec4; 16] {
    let (color, alpha_part) = if format == 18 { (&b[0..8], None) } else { (&b[8..16], Some(&b[0..8])) };
    let c0 = u16::from_le_bytes([color[0], color[1]]);
    let c1 = u16::from_le_bytes([color[2], color[3]]);
    let idx = u32::from_le_bytes(color[4..8].try_into().unwrap());
    let (a, bcol) = (rgb565(c0), rgb565(c1));
    let lerp = |t: f32| -> Vec4 { std::array::from_fn(|i| a[i] + (bcol[i] - a[i]) * t) };
    let colors = if c0 > c1 || format != 18 { [a, bcol, lerp(1.0 / 3.0), lerp(2.0 / 3.0)] } else { [a, bcol, lerp(0.5), [0.0, 0.0, 0.0, 0.0]] };
    let mut out: [Vec4; 16] = std::array::from_fn(|k| colors[((idx >> (2 * k)) & 3) as usize]);
    match (format, alpha_part) {
        (19, Some(ab)) => {
            let bits = u64::from_le_bytes(ab.try_into().unwrap());
            for (k, o) in out.iter_mut().enumerate() {
                o[3] = ((bits >> (4 * k)) & 15) as f32 / 15.0;
            }
        }
        (20, Some(ab)) => {
            let (a0, a1) = (ab[0] as f32 / 255.0, ab[1] as f32 / 255.0);
            let alphas: [f32; 8] = std::array::from_fn(|i| match i as u32 {
                0 => a0,
                1 => a1,
                i if a0 > a1 => ((8 - i) as f32 * a0 + (i - 1) as f32 * a1) / 7.0,
                6 => 0.0,
                7 => 1.0,
                i => ((6 - i) as f32 * a0 + (i - 1) as f32 * a1) / 5.0,
            });
            let bits = u64::from_le_bytes([ab[2], ab[3], ab[4], ab[5], ab[6], ab[7], 0, 0]);
            for (k, o) in out.iter_mut().enumerate() {
                o[3] = alphas[((bits >> (3 * k)) & 7) as usize];
            }
        }
        _ => {}
    }
    out
}

fn wrap(c: i32, size: u32, mode: u32) -> u32 {
    let s = size as i32;
    match mode {
        0 => c.rem_euclid(s) as u32,
        1 => {
            let p = c.rem_euclid(2 * s);
            (if p >= s { 2 * s - 1 - p } else { p }) as u32
        }
        _ => c.clamp(0, s - 1) as u32,
    }
}

// ---------------------------------------------------------------- shader environment
struct Env<'a> {
    regs: &'a [u32],
    mem: &'a mut dyn GuestMem,
}

impl ShaderEnv for Env<'_> {
    fn float_const(&self, index: u32) -> Vec4 {
        let b = FLOAT_BASE + index as usize * 4;
        std::array::from_fn(|i| f32::from_bits(self.regs[b + i]))
    }
    fn bool_const(&self, index: u32) -> bool {
        (self.regs[BOOL_BASE + (index / 32) as usize] >> (index % 32)) & 1 == 1
    }
    fn loop_const(&self, index: u32) -> (u32, i32, i32) {
        let v = self.regs[LOOP_BASE + index as usize];
        (v & 0xFF, ((v >> 8) & 0xFF) as i32, ((v >> 16) as u8) as i8 as i32)
    }
    fn vertex_stream(&self, index: u32) -> (u32, u32) {
        let d0 = self.regs[FETCH_BASE + index as usize * 2];
        let d1 = self.regs[FETCH_BASE + index as usize * 2 + 1];
        (d0 & !3, d1 & 3)
    }
    fn read_dword(&mut self, addr: u32, endian: u32) -> u32 {
        gpu_read(self.mem, addr, endian)
    }
    fn sample(&mut self, f: &TextureFetch, coord: Vec4) -> Vec4 {
        let b = FETCH_BASE + f.const_index as usize * 6;
        let t = TexInfo::from_fetch(&self.regs[b..b + 6]);
        let (u, v) = if f.unnormalized { (coord[0], coord[1]) } else { (coord[0] * t.width as f32, coord[1] * t.height as f32) };
        if !linear_filter(f, self.regs[b + 3]) {
            let x = wrap(u.floor() as i32, t.width, t.clamp[0]);
            let y = wrap(v.floor() as i32, t.height, t.clamp[1]);
            return texel(self.mem, &t, x, y);
        }
        // bilinear filter on the base level
        let (fx, fy) = (u - 0.5, v - 0.5);
        let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let mut s = [0.0f32; 4];
        for (dx, dy, wgt) in [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)] {
            if wgt == 0.0 {
                continue;
            }
            let x = wrap(x0 + dx, t.width, t.clamp[0]);
            let y = wrap(y0 + dy, t.height, t.clamp[1]);
            let c = texel(self.mem, &t, x, y);
            for i in 0..4 {
                s[i] += c[i] * wgt;
            }
        }
        s
    }
}

// ---------------------------------------------------------------- parallel pixel pipeline
type TexKey = (u32, u32, u32, bool, u32, u32, u32, u32);

/// LMA_TEX_LOG=<file> (#166): one line per texture decoded from guest memory (the cache
/// generation, i.e. resolves/presents so far, address, size, format, hash of the guest bytes).
/// Runs of the same state compared line by line show a texture built from different data, such as
/// a shirt back composed from a read-back that hadn't landed yet.
/// `fetch`: the texture's six fetch dwords; the log line ends with the swizzle, the packed-mips
/// flag and dwords 4 and 5 (mip levels, mip address) for #304's BC upload survey.
fn tex_log(gen: u64, t: &TexInfo, hash: u64, d: &Decoded, fetch: &[u32]) {
    use std::io::Write;
    static LOG: std::sync::OnceLock<Option<std::sync::Mutex<std::io::LineWriter<std::fs::File>>>> = std::sync::OnceLock::new();
    let log = LOG.get_or_init(|| std::env::var_os("LMA_TEX_LOG").and_then(|p| std::fs::File::create(p).ok()).map(|f| std::sync::Mutex::new(std::io::LineWriter::new(f))));
    if let Some(l) = log {
        let _ = writeln!(l.lock().unwrap(), "{gen} {:#010x} {}x{} fmt {} {:016x} swz {:o} packed {} d4 {:08x} d5 {:08x}", t.base, t.width, t.height, t.format, hash, t.swizzle, t.packed != (0, 0) || fetch.get(5).is_some_and(|w| w >> 11 & 1 == 1), fetch.get(4).copied().unwrap_or(0), fetch.get(5).copied().unwrap_or(0));
        // LMA_TEX_DUMP=<dir>: also the decoded pixels, as <address>_<hash>.ppm, and the alpha as
        // grey in <address>_<hash>_a.pgm (the UI's white shapes only show there)
        if let Some(dir) = std::env::var_os("LMA_TEX_DUMP") {
            let mut out = format!("P6\n{} {}\n255\n", t.width, t.height).into_bytes();
            let bc8: Option<Vec<u8>> = d.bc.as_ref().map(|_| (0..(d.w * d.h) as usize).flat_map(|i| d.texel(i).map(|v| (v * 255.0 + 0.5) as u8)).collect());
            match bc8.as_ref().or(d.px8.as_ref()) {
                Some(b) => out.extend(b.chunks_exact(4).flat_map(|c| [c[0], c[1], c[2]])),
                None => out.extend(d.px.iter().flat_map(|c| [c[0], c[1], c[2]].map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8))),
            }
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{:08x}_{hash:016x}.ppm", t.base)), out);
            let mut alpha = format!("P5\n{} {}\n255\n", t.width, t.height).into_bytes();
            match bc8.as_ref().or(d.px8.as_ref()) {
                Some(b) => alpha.extend(b.chunks_exact(4).map(|c| c[3])),
                None => alpha.extend(d.px.iter().map(|c| (c[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8)),
            }
            let _ = std::fs::write(std::path::Path::new(&dir).join(format!("{:08x}_{hash:016x}_a.pgm", t.base)), alpha);
        }
    }
}

/// A texture decoded to RGBA floats (after the fetch-constant swizzle).
pub struct Decoded {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) px: Vec<Vec4>,
    /// Instead of `px` (left empty): the texels as RGBA8, `to_rgba8`'s bytes (`Renderer::rgba8`).
    pub(crate) px8: Option<Vec<u8>>,
    /// Instead of `px` and `px8`: a DXT texture's format (18-20) and its levels as plain BC1/2/3
    /// blocks, level 0 first (`bc_levels`; identity swizzle, so nothing is baked in).
    pub(crate) bc: Option<(u32, Vec<Vec<u8>>)>,
    /// Unique per decode (never reused, unlike an `Arc`'s address after it is freed): what a
    /// cached setup records to tell its texture is still the current decode (#304).
    pub(crate) id: u64,
}

impl Decoded {
    pub(crate) fn new(w: u32, h: u32, px: Vec<Vec4>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self { w, h, px, px8: None, bc: None, id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) }
    }
    pub(crate) fn new8(w: u32, h: u32, px8: Vec<u8>) -> Self {
        Self { px8: Some(px8), ..Self::new(w, h, Vec::new()) }
    }
    pub(crate) fn new_bc(w: u32, h: u32, format: u32, levels: Vec<Vec<u8>>) -> Self {
        Self { bc: Some((format, levels)), ..Self::new(w, h, Vec::new()) }
    }
    /// Texel `i` as floats (an RGBA8 decode's to the nearest 1/255; a BC texture's decoded from
    /// its block, as `decode_dxt` does).
    #[inline]
    pub(crate) fn texel(&self, i: usize) -> Vec4 {
        if let Some((format, levels)) = &self.bc {
            let (x, y) = (i as u32 % self.w, i as u32 / self.w);
            let bpb = if *format == 18 { 8 } else { 16 };
            let at = ((y / 4) * self.w.div_ceil(4) + x / 4) as usize * bpb;
            let mut b = [0u8; 16];
            b[..bpb].copy_from_slice(&levels[0][at..at + bpb]);
            return decode_dxt(&b, *format, x % 4, y % 4);
        }
        match &self.px8 {
            Some(b) => std::array::from_fn(|k| b[4 * i + k] as f32 / 255.0),
            None => self.px[i],
        }
    }
}

/// Texture formats whose decoded values are 8-bit (or narrower) unorm: k_8, 1_5_5_5, 5_6_5, 6_5_5,
/// 8_8_8_8, 8_A, 8_B, 8_8, 4_4_4_4 and DXT1/2_3/4_5.
pub(crate) const UNORM8_FORMATS: [u32; 12] = [2, 3, 4, 5, 6, 8, 9, 10, 15, 18, 19, 20];

/// Pixel-shader environment: registers plus pre-decoded textures and no guest memory access,
/// so it can run on several threads at once.
struct PixelEnv<'a> {
    regs: &'a [u32],
    tex: &'a [Option<Arc<Decoded>>],
}

impl ShaderEnv for PixelEnv<'_> {
    fn float_const(&self, index: u32) -> Vec4 {
        let b = FLOAT_BASE + index as usize * 4;
        std::array::from_fn(|i| f32::from_bits(self.regs[b + i]))
    }
    fn bool_const(&self, index: u32) -> bool {
        (self.regs[BOOL_BASE + (index / 32) as usize] >> (index % 32)) & 1 == 1
    }
    fn loop_const(&self, index: u32) -> (u32, i32, i32) {
        let v = self.regs[LOOP_BASE + index as usize];
        (v & 0xFF, ((v >> 8) & 0xFF) as i32, ((v >> 16) as u8) as i8 as i32)
    }
    fn vertex_stream(&self, index: u32) -> (u32, u32) {
        let d0 = self.regs[FETCH_BASE + index as usize * 2];
        let d1 = self.regs[FETCH_BASE + index as usize * 2 + 1];
        (d0 & !3, d1 & 3)
    }
    fn read_dword(&mut self, _addr: u32, _endian: u32) -> u32 {
        0 // vertex fetches from pixel shaders are not supported
    }
    fn sample(&mut self, f: &TextureFetch, coord: Vec4) -> Vec4 {
        let Some(Some(t)) = self.tex.get(f.const_index as usize) else { return [1.0, 0.0, 1.0, 1.0] };
        let d0 = self.regs[FETCH_BASE + f.const_index as usize * 6];
        let clamp = [(d0 >> 10) & 7, (d0 >> 13) & 7];
        let (u, v) = if f.unnormalized { (coord[0], coord[1]) } else { (coord[0] * t.w as f32, coord[1] * t.h as f32) };
        if !linear_filter(f, self.regs[FETCH_BASE + f.const_index as usize * 6 + 3]) {
            let x = wrap(u.floor() as i32, t.w, clamp[0]);
            let y = wrap(v.floor() as i32, t.h, clamp[1]);
            return t.texel((y * t.w + x) as usize);
        }
        // bilinear filter on the base level
        let (fx, fy) = (u - 0.5, v - 0.5);
        let (x0, y0) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let mut s = [0.0f32; 4];
        for (dx, dy, wgt) in [(0, 0, (1.0 - tx) * (1.0 - ty)), (1, 0, tx * (1.0 - ty)), (0, 1, (1.0 - tx) * ty), (1, 1, tx * ty)] {
            if wgt == 0.0 {
                continue;
            }
            let x = wrap(x0 + dx, t.w, clamp[0]);
            let y = wrap(y0 + dy, t.h, clamp[1]);
            let c = t.texel((y * t.w + x) as usize);
            for i in 0..4 {
                s[i] += c[i] * wgt;
            }
        }
        s
    }
}

/// Whether a texture fetch filters bilinearly: the instruction's mag filter (0 point, 1 linear,
/// 2 base map, 3 = the fetch constant's, dword 3 bits 19-20). Point sampling matters for depth
/// textures: a shadow map's taps must compare stored depths, not blends of them.
pub(crate) fn linear_filter(f: &TextureFetch, fetch_dword3: u32) -> bool {
    match f.mag_filter {
        0 => false,
        3 => (fetch_dword3 >> 19) & 3 != 0,
        _ => true,
    }
}

/// Render-state registers a draw's pixels need.
pub(crate) struct RasterCtx {
    pub(crate) surface_pitch: u32,
    pub(crate) color_base: u32,
    pub(crate) offset: [f32; 2],
    pub(crate) scissor: [i32; 4],
    pub(crate) blend: u32,
    pub(crate) mask: u32,
    pub(crate) cc: u32,
    pub(crate) alpha_ref: f32,
    pub(crate) constant: Vec4,
    pub(crate) depth_base: u32,
    /// RB_DEPTHCONTROL: z test, z write, compare function
    pub(crate) z_enable: bool,
    pub(crate) z_write: bool,
    pub(crate) z_func: u32,
    pub(crate) cull_front: bool,
    pub(crate) cull_back: bool,
    /// PA_SU_SC_MODE_CNTL.face: clockwise triangles are front-facing
    pub(crate) cw_front: bool,
    /// Polygon offset (depth bias) for front and back faces when enabled: (slope scale in
    /// 1/16 pixel units, constant offset in normalized depth), PA_SU_POLY_OFFSET_*
    pub(crate) poly_offset: [Option<(f32, f32)>; 2],
    pub(crate) vte: u32,
    pub(crate) vp: [f32; 6],
    pub(crate) pix_offset: f32,
    /// No colour output (depth pre-pass / shadow map): only depth is written.
    pub(crate) depth_only: bool,
}

/// Clip-space position -> screen (x, y, z, 1/w), as the transform stage does.
fn project(vte: u32, vp: &[f32; 6], pix: f32, c: Vec4) -> Vec4 {
    let mut p = c;
    let inv_w = if c[3] != 0.0 { 1.0 / c[3] } else { 1.0 };
    if vte & (1 << 8) == 0 {
        p[0] *= inv_w;
        p[1] *= inv_w;
    }
    if vte & (1 << 9) == 0 {
        p[2] *= inv_w;
    }
    for (bit, axis, k) in [(1, 0, 0), (4, 1, 2), (16, 2, 4)] {
        if vte & bit != 0 {
            p[axis] *= vp[k];
        }
        if vte & (bit << 1) != 0 {
            p[axis] += vp[k + 1];
        }
    }
    [p[0] + pix, p[1] + pix, p[2], inv_w]
}

/// Clip a triangle against the w > 0 plane (D3D near side) so vertices behind the camera never
/// reach the perspective divide; returns 0..2 triangles.
pub(crate) fn clip_near(ctx: &RasterCtx, t: [Vtx; 3]) -> Vec<[Vtx; 3]> {
    const EPS: f32 = 1e-5;
    // screen-space vertices (no divide) and well-formed triangles pass straight through
    if ctx.vte & (1 << 8) != 0 || t.iter().all(|v| v.clip[3] > EPS) {
        return vec![t];
    }
    if t.iter().all(|v| v.clip[3] <= EPS) {
        return Vec::new();
    }
    let mut poly: Vec<Vtx> = Vec::with_capacity(4);
    for i in 0..3 {
        let (a, b) = (&t[i], &t[(i + 1) % 3]);
        let (da, db) = (a.clip[3] - EPS, b.clip[3] - EPS);
        if da > 0.0 {
            poly.push(*a);
        }
        if (da > 0.0) != (db > 0.0) {
            let f = da / (da - db);
            let mut v = *a;
            for k in 0..4 {
                v.clip[k] = a.clip[k] + (b.clip[k] - a.clip[k]) * f;
            }
            for r in 0..16 {
                for k in 0..4 {
                    v.interp[r][k] = a.interp[r][k] + (b.interp[r][k] - a.interp[r][k]) * f;
                }
            }
            v.pos = project(ctx.vte, &ctx.vp, ctx.pix_offset, v.clip);
            poly.push(v);
        }
    }
    (1..poly.len().saturating_sub(1)).map(|i| [poly[0], poly[i], poly[i + 1]]).collect()
}

impl RasterCtx {
    /// The window scissor [x0, y0, x1, y1] in EDRAM pixels (x0 and y0 at least 0).
    fn window_scissor(regs: &[u32], wox: i32, woy: i32) -> [i32; 4] {
        let tl = regs[PA_SC_WINDOW_SCISSOR_TL];
        let br = regs[PA_SC_WINDOW_SCISSOR_BR];
        let (mut sx0, mut sy0) = ((tl & 0x7FFF) as i32, ((tl >> 16) & 0x7FFF) as i32);
        let (mut sx1, mut sy1) = ((br & 0x7FFF) as i32, ((br >> 16) & 0x7FFF) as i32);
        if tl >> 31 == 0 {
            // window scissor is relative to the window offset unless WINDOW_OFFSET_DISABLE
            sx0 += wox;
            sx1 += wox;
            sy0 += woy;
            sy1 += woy;
        }
        [sx0.max(0), sy0.max(0), sx1, sy1]
    }

    pub(crate) fn new(regs: &[u32]) -> Self {
        let woff = regs[PA_SC_WINDOW_OFFSET];
        let (wox, woy) = (((woff & 0x7FFF) << 17) as i32 >> 17, (((woff >> 16) & 0x7FFF) << 17) as i32 >> 17);
        let use_woff = regs[PA_SU_SC_MODE_CNTL] & (1 << 16) != 0;
        let [sx0, sy0, sx1, sy1] = Self::window_scissor(regs, wox, woy);
        Self {
            surface_pitch: (regs[RB_SURFACE_INFO] & 0x3FFF).max(1),
            color_base: regs[RB_COLOR_INFO] & 0xFFF,
            offset: if use_woff { [wox as f32, woy as f32] } else { [0.0, 0.0] },
            scissor: [sx0, sy0, sx1, sy1],
            blend: regs[RB_BLENDCONTROL0],
            mask: regs[RB_COLOR_MASK] & 0xF,
            cc: regs[RB_COLORCONTROL],
            alpha_ref: f32::from_bits(regs[RB_ALPHA_REF]),
            constant: std::array::from_fn(|i| f32::from_bits(regs[RB_BLEND_RED + i])),
            depth_base: regs[RB_DEPTH_INFO] & 0xFFF,
            z_enable: regs[RB_DEPTHCONTROL] & 2 != 0,
            z_write: regs[RB_DEPTHCONTROL] & 4 != 0,
            z_func: (regs[RB_DEPTHCONTROL] >> 4) & 7,
            cull_front: regs[PA_SU_SC_MODE_CNTL] & 1 != 0,
            cull_back: regs[PA_SU_SC_MODE_CNTL] & 2 != 0,
            cw_front: regs[PA_SU_SC_MODE_CNTL] & 4 != 0,
            poly_offset: std::array::from_fn(|i| {
                (regs[PA_SU_SC_MODE_CNTL] & (1 << (11 + i)) != 0).then(|| (f32::from_bits(regs[PA_SU_POLY_OFFSET_FRONT_SCALE + 2 * i]), f32::from_bits(regs[PA_SU_POLY_OFFSET_FRONT_SCALE + 2 * i + 1])))
            }),
            vte: regs[PA_CL_VTE_CNTL],
            vp: std::array::from_fn(|i| f32::from_bits(regs[PA_CL_VPORT_XSCALE + i])),
            pix_offset: pix_offset(regs),
            depth_only: false,
        }
    }
}

/// A texture's raw bytes in guest memory (whole tiled footprint).
/// Byte length of a texture's guest data, as `raw_bytes` reads it.
fn raw_len(t: &TexInfo) -> usize {
    let Some((bw, bpb_log2)) = block_info(t.format) else { return 0 };
    let pitch_blocks = ((t.pitch.max(t.width) / bw).max(1) + 31) & !31;
    let height_blocks = ((t.height + t.packed.1).div_ceil(bw) + 31) & !31;
    ((pitch_blocks * height_blocks) << bpb_log2).min(32 << 20) as usize
}

fn raw_bytes(mem: &mut dyn GuestMem, t: &TexInfo) -> Vec<u8> {
    let mut buf = vec![0u8; raw_len(t)];
    mem.read(phys(t.base), &mut buf);
    buf
}

/// Depth a resolve clears to: RB_DEPTH_CLEAR holds it in the top 24 bits (D24S8 unorm or D24FS8 float).
pub(crate) fn depth_clear_value(regs: &[u32]) -> f32 {
    let v = regs[RB_DEPTH_CLEAR] >> 8;
    if regs[RB_DEPTH_INFO] & (1 << 16) != 0 {
        let (e, m) = ((v >> 20) & 15, v & 0xF_FFFF);
        if e == 0 { m as f32 / (1 << 20) as f32 * 2f32.powi(-14) } else { (1.0 + m as f32 / (1 << 20) as f32) * 2f32.powi(e as i32 - 15) }
    } else {
        v as f32 / 0xFF_FFFF as f32
    }
}

/// Colour a resolve clears to: RB_COLOR_CLEAR is ARGB8 for 8888 targets.
pub(crate) fn color_clear_value(regs: &[u32]) -> Vec4 {
    let v = regs[RB_COLOR_CLEAR];
    [((v >> 16) & 0xFF) as f32 / 255.0, ((v >> 8) & 0xFF) as f32 / 255.0, (v & 0xFF) as f32 / 255.0, (v >> 24) as f32 / 255.0]
}

/// `bytes_hash` of a texture's guest data without copying it out: streamed through a 64 KB
/// buffer (a multiple of the hash's 32-byte step, so the result is the same).
fn tex_hash(mem: &mut dyn GuestMem, t: &TexInfo) -> u64 {
    let len = raw_len(t);
    let mut buf = vec![0u8; len.min(1 << 16)];
    let mut h = Hasher::default();
    let mut off = 0;
    while off < len {
        let n = (len - off).min(buf.len());
        mem.read(phys(t.base).wrapping_add(off as u32), &mut buf[..n]);
        h.update(&buf[..n]);
        off += n;
    }
    h.finish(len)
}

/// Guest physical byte ranges [lo, hi) the pixel shader's textures are read from, except the
/// fetch constants in `skip`.
pub(crate) fn texture_ranges(regs: &[u32], ps: &Shader, skip: &[u32]) -> Vec<(u32, u32)> {
    ps.texture_consts()
        .into_iter()
        .filter(|c| !skip.contains(c))
        .filter_map(|c| {
            let b = FETCH_BASE + c as usize * 6;
            let t = TexInfo::from_fetch(&regs[b..b + 6]);
            (t.width <= 4096 && t.height <= 4096).then(|| tex_span(&(t.base, t.format, t.endian, t.tiled, t.pitch, t.width, t.height, t.swizzle)))
        })
        .collect()
}

/// Guest physical byte range [lo, hi) a cached texture was decoded from (as `raw_bytes` reads it).
fn tex_span(k: &TexKey) -> (u32, u32) {
    let &(base, format, _, _, pitch, width, height, _) = k;
    let Some((bw, bpb_log2)) = block_info(format) else { return (0, 0) };
    let pitch_blocks = ((pitch.max(width) / bw).max(1) + 31) & !31;
    // + 16 rows: a packed level 0 may start 16 texels down (`packed_mip0_offset`)
    let height_blocks = ((height + 16).div_ceil(bw) + 31) & !31;
    let len = ((pitch_blocks * height_blocks) << bpb_log2).min(32 << 20);
    let lo = phys(base);
    (lo, lo.saturating_add(len))
}

/// Texture-cache content hash: four independent multiply chains over 32-byte steps (a single
/// chain is latency-bound), with the length folded in.
pub(crate) struct Hasher {
    lanes: [u64; 4],
    tail: Vec<u8>,
}

impl Default for Hasher {
    fn default() -> Self {
        Self { lanes: [Self::SEED, Self::SEED ^ 1, Self::SEED ^ 2, Self::SEED ^ 3], tail: Vec::new() }
    }
}

impl Hasher {
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const K: u64 = 0x9E37_79B9_7F4A_7C15;

    /// Feed bytes; all but the last call must pass a multiple of 32 bytes.
    pub(crate) fn update(&mut self, buf: &[u8]) {
        let mut steps = buf.chunks_exact(32);
        for c in &mut steps {
            for (k, lane) in self.lanes.iter_mut().enumerate() {
                let w = u64::from_le_bytes(c[8 * k..8 * k + 8].try_into().unwrap());
                *lane = (*lane ^ w).wrapping_mul(Self::K).rotate_left(29);
            }
        }
        self.tail.extend_from_slice(steps.remainder());
    }

    pub(crate) fn finish(mut self, len: usize) -> u64 {
        self.update(&[]);
        let mut h = (len as u64).wrapping_mul(Self::K);
        for (i, b) in self.tail.iter().enumerate() {
            h ^= (*b as u64) << (8 * (i % 8));
            h = h.wrapping_mul(Self::K);
        }
        for lane in self.lanes {
            h = (h ^ lane).wrapping_mul(Self::K).rotate_left(31);
        }
        h
    }
}

pub(crate) fn bytes_hash(buf: &[u8]) -> u64 {
    let mut h = Hasher::default();
    h.update(buf);
    h.finish(buf.len())
}

/// LMA_TEX_STATS=1 (#304 step 1): per texture format, the decodes since the last `tex_stats_take`
/// (count, texels, nanoseconds), printed with each slow frame (`prof`).
static TEX_STATS: [[std::sync::atomic::AtomicU64; 3]; 64] = [const { [const { std::sync::atomic::AtomicU64::new(0) }; 3] }; 64];

pub(crate) fn tex_stats_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LMA_TEX_STATS").as_deref() == Ok("1"))
}

fn tex_stats_add(format: u32, texels: u64, ns: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    let s = &TEX_STATS[format as usize & 63];
    s[0].fetch_add(1, Relaxed);
    s[1].fetch_add(texels, Relaxed);
    s[2].fetch_add(ns, Relaxed);
}

/// The decodes per format since the last call, busiest first: (format, count, texels, ns).
pub(crate) fn tex_stats_take() -> Vec<(u32, u64, u64, u64)> {
    use std::sync::atomic::Ordering::Relaxed;
    let mut out: Vec<(u32, u64, u64, u64)> = (0..64u32)
        .map(|f| {
            let s = &TEX_STATS[f as usize];
            (f, s[0].swap(0, Relaxed), s[1].swap(0, Relaxed), s[2].swap(0, Relaxed))
        })
        .filter(|e| e.1 > 0)
        .collect();
    out.sort_by_key(|e| std::cmp::Reverse(e.3));
    out
}

/// Decode a whole texture from its raw bytes, rows split across threads.
/// A DXT texture for a compressed upload (#304, `Renderer::bc`): level 0 and the guest's own mip
/// levels above the packed tail, each untiled to rows of blocks and endian-swapped, so the bytes
/// are plain BC1/BC2/BC3 (the same blocks `dxt_rows` decodes). Mip level l (from `fetch` dword 5's
/// address, up to dword 4's max level) is `next_pow2(size) >> l` texels, its pitch and height 32
/// blocks, tiled or linear (most of the match's DXT textures are linear; checked against a box
/// filter of the level above with LMA_TEX_BC_CHECK=1), the levels one after another on 4 KB
/// boundaries; from the packed level on (a side of 16 texels or less) the levels share one level
/// slot, each at `packed_mip_offset` (Xenia's GetGuestTextureLayout and GetPackedMipOffset).
/// None when the texture doesn't fit: not DXT, a swizzle other than the
/// identity (a view can't swizzle), level 0 inside a packed tail, off the block grid, or a mip
/// chain on a size that isn't a power of two (the GPU's levels would be other sizes).
pub(crate) fn bc_levels(mem: &mut dyn GuestMem, t: &TexInfo, fetch: &[u32]) -> Option<Vec<Vec<u8>>> {
    if !matches!(t.format, 18..=20) || t.swizzle != 0o3210 || t.packed != (0, 0) || t.width % 4 != 0 || t.height % 4 != 0 {
        return None;
    }
    let (_, bpb_log2) = block_info(t.format)?;
    let max_level = fetch.get(4).map_or(0, |d| (d >> 6) & 15);
    let packed = fetch.get(5).is_some_and(|d| d >> 11 & 1 == 1);
    let mip_base = fetch.get(5).map_or(0, |d| (d >> 12) << 12);
    if max_level > 0 && !(t.width.is_power_of_two() && t.height.is_power_of_two()) {
        return None;
    }
    let mut levels = vec![level_blocks(mem, t.base, (0, 0), t.width / 4, t.height / 4, (t.pitch.max(t.width) / 4).max(1), t.tiled, t.endian, bpb_log2)];
    // level 0 isn't in a tail (`t.packed` is (0, 0)), so the packed level is 1 or more
    let packed_level = if packed { log2_ceil(t.width.min(t.height)).saturating_sub(4) } else { u32::MAX };
    let (mut offset, mut tail) = (0u32, None);
    for l in 1..=max_level {
        if mip_base == 0 {
            break;
        }
        let (bw, bh) = ((t.width >> l).max(1).div_ceil(4), (t.height >> l).max(1).div_ceil(4));
        if l >= packed_level {
            let (base, pitch) = *tail.get_or_insert((mip_base + offset, ((t.width >> packed_level).max(1).div_ceil(4) + 31) & !31));
            let (ox, oy) = packed_mip_offset(t.width, t.height, l);
            levels.push(level_blocks(mem, base, (ox / 4, oy / 4), bw, bh, pitch, t.tiled, t.endian, bpb_log2));
            continue;
        }
        let pitch = (bw + 31) & !31;
        levels.push(level_blocks(mem, mip_base + offset, (0, 0), bw, bh, pitch, t.tiled, t.endian, bpb_log2));
        offset += ((pitch * ((bh + 31) & !31)) << bpb_log2).next_multiple_of(4096);
    }
    // LMA_TEX_BC_CHECK=1 (debug): each guest level against a 2x2 box filter of the one above,
    // mean absolute error per channel (a wrong mip address shows as noise, ~0.2 and up)
    if std::env::var_os("LMA_TEX_BC_CHECK").is_some() && levels.len() == 1 && max_level > 0 {
        eprintln!("bc check {:#010x} {}x{} fmt {}: no mips (max level {max_level}, mip base {mip_base:#x}, tiled {}, packed {packed})", t.base, t.width, t.height, t.format, t.tiled);
    }
    if std::env::var_os("LMA_TEX_BC_CHECK").is_some() && levels.len() > 1 {
        let dec: Vec<Decoded> = levels.iter().enumerate().map(|(l, b)| Decoded::new_bc((t.width >> l).max(1), (t.height >> l).max(1), t.format, vec![b.clone()])).collect();
        let errs: Vec<String> = (1..dec.len())
            .map(|l| {
                let (a, b) = (&dec[l - 1], &dec[l]);
                let mut e = 0.0f64;
                for y in 0..b.h {
                    for x in 0..b.w {
                        let s: Vec4 = std::array::from_fn(|c| [(0, 0), (1, 0), (0, 1), (1, 1)].iter().map(|(dx, dy)| a.texel(((2 * y + dy) * a.w + 2 * x + dx) as usize)[c]).sum::<f32>() / 4.0);
                        let v = b.texel((y * b.w + x) as usize);
                        e += (0..4).map(|c| (s[c] - v[c]).abs() as f64).sum::<f64>() / 4.0;
                    }
                }
                format!("L{l} {:.3}", e / (b.w * b.h) as f64)
            })
            .collect();
        eprintln!("bc check {:#010x} {}x{} fmt {} mips at {:#010x} mipfilter {} d4 {:08x}: {}", t.base, t.width, t.height, t.format, mip_base, fetch.get(3).map_or(9, |d| (d >> 23) & 3), fetch.get(4).copied().unwrap_or(0), errs.join(", "));
    }
    Some(levels)
}

/// Where packed mip level `mip` of a `width` x `height` texture sits in its tail, in texels
/// (Xenia's GetPackedMipOffset for 2D; `packed_mip0_offset` is its level 0).
fn packed_mip_offset(width: u32, height: u32, mip: u32) -> (u32, u32) {
    let (lw, lh) = (log2_ceil(width), log2_ceil(height));
    let size = lw.min(lh);
    if size > 4 + mip {
        return (0, 0);
    }
    let base = size.saturating_sub(4);
    let packed = mip - base;
    match (packed < 3, lw > lh) {
        (true, true) => (0, 16 >> packed),
        (true, false) => (16 >> packed, 0),
        (false, true) => ((1 << (lw - base)) >> (packed - 2), 0),
        (false, false) => (0, (1 << (lh - base)) >> (packed - 2)),
    }
}

fn log2_ceil(v: u32) -> u32 {
    32 - v.max(1).saturating_sub(1).leading_zeros()
}

/// One level's `bw` x `bh` blocks from guest `base`, starting at block `origin` (a packed tail's
/// level), rows of blocks in order, each block's words endian-swapped as `dxt_rows` reads them.
#[allow(clippy::too_many_arguments)]
fn level_blocks(mem: &mut dyn GuestMem, base: u32, origin: (u32, u32), bw: u32, bh: u32, pitch_blocks: u32, tiled: bool, endian: u32, bpb_log2: u32) -> Vec<u8> {
    let pitch_aligned = (pitch_blocks + 31) & !31;
    let rows = origin.1 + bh;
    let span = if tiled { pitch_aligned * ((rows + 31) & !31) } else { pitch_blocks * rows } << bpb_log2;
    let mut raw = vec![0u8; span as usize];
    mem.read(phys(base), &mut raw);
    let mut out = Vec::with_capacity(((bw * bh) << bpb_log2) as usize);
    for by in 0..bh {
        for bx in 0..bw {
            let (x, y) = (origin.0 + bx, origin.1 + by);
            let off = if tiled { tiled_2d(x as i32, y as i32, pitch_aligned, bpb_log2) } else { (y * pitch_blocks + x) << bpb_log2 };
            for k in (0..1u32 << bpb_log2).step_by(4) {
                let o = (off + k) as usize;
                let w = raw.get(o..o + 4).map_or(0, |b| gpu_swap(u32::from_le_bytes(b.try_into().unwrap()), endian));
                out.extend_from_slice(&w.to_le_bytes());
            }
        }
    }
    out
}

fn decode_texture(buf: &[u8], t: &TexInfo, threads: usize) -> Vec<Vec4> {
    let (w, h) = (t.width as usize, t.height as usize);
    let mut px = vec![[0.0f32; 4]; w * h];
    let read = |a: u32| {
        let o = a.wrapping_sub(t.base) as usize;
        buf.get(o..o + 4).map(|b| gpu_swap(u32::from_le_bytes(b.try_into().unwrap()), t.endian)).unwrap_or(0)
    };
    // DXT a block at a time when level 0 starts on a block edge (#304); chunks start on block rows
    let dxt = matches!(t.format, 18..=20) && t.packed.0 % 4 == 0 && t.packed.1 % 4 == 0;
    let rows = |chunk: &mut [Vec4], y0: usize| {
        if dxt {
            dxt_rows(&read, t, chunk, w, y0);
            return;
        }
        for (j, p) in chunk.iter_mut().enumerate() {
            *p = texel_with(read, t, (j % w) as u32, (y0 + j / w) as u32);
        }
    };
    // a thread per 64 K texels at most, and none below that: spawning ~16 OS threads for every
    // texture cost more than decoding the small ones (#304: hundreds of new textures per frame)
    let parts = (w * h).div_ceil(1 << 16).min(threads.max(1));
    if parts <= 1 {
        rows(&mut px, 0);
        return px;
    }
    let rows_per = h.div_ceil(parts).max(16).next_multiple_of(4);
    std::thread::scope(|s| {
        for (i, chunk) in px.chunks_mut(rows_per * w).enumerate() {
            let rows = &rows;
            s.spawn(move || rows(chunk, i * rows_per));
        }
    });
    px
}

/// `decode_texture` straight to RGBA8 (`v * 255 + 0.5`, truncated, as the wgpu upload converts),
/// four rows at a time through a small float buffer instead of a whole f32 image and a second
/// pass (#304); None as soon as a value is outside 0..=1, when the texture needs floats.
fn decode_texture_rgba8(buf: &[u8], t: &TexInfo, threads: usize) -> Option<Vec<u8>> {
    let (w, h) = (t.width as usize, t.height as usize);
    let mut out = vec![0u8; w * h * 4];
    if w == 0 {
        return Some(out);
    }
    let read = |a: u32| {
        let o = a.wrapping_sub(t.base) as usize;
        buf.get(o..o + 4).map(|b| gpu_swap(u32::from_le_bytes(b.try_into().unwrap()), t.endian)).unwrap_or(0)
    };
    let dxt = matches!(t.format, 18..=20) && t.packed.0 % 4 == 0 && t.packed.1 % 4 == 0;
    // `chunk` holds whole rows starting at row `y0` (a multiple of 4)
    let rows = |chunk: &mut [u8], y0: usize| -> bool {
        if dxt {
            // DXT straight to bytes through tables of the float decode's values (#304)
            dxt_rows_rgba8(&read, t, chunk, w, y0);
            return true;
        }
        if t.format == 6 {
            rgba8888_rows(&read, t, chunk, w, y0);
            return true;
        }
        let mut f = vec![[0.0f32; 4]; 4 * w];
        for (k, part) in chunk.chunks_mut(16 * w).enumerate() {
            let y = y0 + 4 * k;
            let f = &mut f[..part.len() / 4];
            if dxt {
                dxt_rows(&read, t, f, w, y);
            } else {
                for (j, p) in f.iter_mut().enumerate() {
                    *p = texel_with(read, t, (j % w) as u32, (y + j / w) as u32);
                }
            }
            for (c, o) in f.iter().zip(part.chunks_exact_mut(4)) {
                for i in 0..4 {
                    if !(0.0..=1.0).contains(&c[i]) {
                        return false;
                    }
                    o[i] = (c[i] * 255.0 + 0.5) as u8;
                }
            }
        }
        true
    };
    // threads as `decode_texture`
    let parts = (w * h).div_ceil(1 << 16).min(threads.max(1));
    if parts <= 1 {
        return rows(&mut out, 0).then_some(out);
    }
    let rows_per = h.div_ceil(parts).max(16).next_multiple_of(4);
    let ok = std::thread::scope(|s| {
        let handles: Vec<_> = out
            .chunks_mut(rows_per * w * 4)
            .enumerate()
            .map(|(i, chunk)| {
                let rows = &rows;
                s.spawn(move || rows(chunk, i * rows_per))
            })
            .collect();
        handles.into_iter().all(|j| j.join().unwrap())
    });
    ok.then_some(out)
}

/// Rows of an 8_8_8_8 texture straight to RGBA8, `chunk` starting at row `y0`: `texel_with`'s
/// address and its one dword per texel, the swizzle on the bytes. The same bytes as the float
/// decode converted (`b / 255 * 255 + 0.5` truncates back to `b`), without the 16-byte gather and
/// two float passes per texel (#304: kickoff's ~55 new 8_8_8_8 textures took 54-63 ms that way,
/// 14 ns a texel against DXT1's 3).
fn rgba8888_rows(read: &impl Fn(u32) -> u32, t: &TexInfo, chunk: &mut [u8], w: usize, y0: usize) {
    let pitch = t.pitch.max(t.width).max(1);
    let pitch_aligned = (pitch + 31) & !31;
    let swz: [u32; 4] = std::array::from_fn(|i| (t.swizzle >> (3 * i)) & 7);
    for (j, o) in chunk.chunks_exact_mut(4).enumerate() {
        let (x, y) = ((j % w) as u32 + t.packed.0, (y0 + j / w) as u32 + t.packed.1);
        let offset = if t.tiled { tiled_2d(x as i32, y as i32, pitch_aligned, 2) } else { (y * pitch + x) << 2 };
        let b = read(t.base + offset).to_le_bytes();
        for i in 0..4 {
            o[i] = match swz[i] {
                s @ 0..=3 => b[s as usize],
                4 => 0,
                _ => 255,
            };
        }
    }
}

/// Rows of a DXT texture, `chunk` starting at row `y0` (a multiple of 4), a 4x4 block at a time:
/// `texel_with`'s tiled address and byte reads once per block instead of once per texel, then the
/// same `decode_dxt` and swizzle for each of its texels, so the pixels are identical (#304: the
/// first frames of a match decoded ~33 M DXT texels, 16 reads and address computations each).
fn dxt_rows(read: &impl Fn(u32) -> u32, t: &TexInfo, chunk: &mut [Vec4], w: usize, y0: usize) {
    let Some((bw, bpb_log2)) = block_info(t.format) else {
        debug_assert!(false, "dxt_rows on format {} (not a block format)", t.format);
        return;
    };
    let pitch_blocks = (t.pitch.max(t.width) / bw).max(1);
    let rows = if w == 0 { 0 } else { chunk.len() / w };
    for row0 in (0..rows).step_by(4) {
        let by = (y0 + row0) as u32 / 4 + t.packed.1 / 4;
        for col0 in (0..w).step_by(4) {
            let bx = col0 as u32 / 4 + t.packed.0 / 4;
            let offset = if t.tiled {
                tiled_2d(bx as i32, by as i32, (pitch_blocks + 31) & !31, bpb_log2)
            } else {
                (by * pitch_blocks + bx) << bpb_log2
            };
            let addr = t.base + offset;
            let mut bytes = [0u8; 16];
            for k in (0..1u32 << bpb_log2).step_by(4) {
                let word = read((addr & !3) + k);
                bytes[k as usize..k as usize + 4].copy_from_slice(&word.to_le_bytes());
            }
            let texels = decode_dxt_block(&bytes, t.format);
            for py in 0..4.min(rows - row0) {
                for px in 0..4.min(w - col0) {
                    chunk[(row0 + py) * w + col0 + px] = apply_tex_swizzle(texels[py * 4 + px], t.swizzle);
                }
            }
        }
    }
}

/// `dxt_rows` straight to RGBA8 (#304: kickoff decodes hundreds of DXT textures on the render
/// thread). Every channel of a DXT texel is a function of its own endpoint bits and the index,
/// so `DxtTables` holds the float decode's values already converted (`v * 255 + 0.5`, truncated,
/// as `decode_texture_rgba8` converts them): the bytes are the same, without a float per texel.
fn dxt_rows_rgba8(read: &impl Fn(u32) -> u32, t: &TexInfo, out: &mut [u8], w: usize, y0: usize) {
    let Some((bw, bpb_log2)) = block_info(t.format) else { return };
    let tab = dxt_tables();
    let pitch_blocks = (t.pitch.max(t.width) / bw).max(1);
    let rows = if w == 0 { 0 } else { out.len() / (4 * w) };
    for row0 in (0..rows).step_by(4) {
        let by = (y0 + row0) as u32 / 4 + t.packed.1 / 4;
        for col0 in (0..w).step_by(4) {
            let bx = col0 as u32 / 4 + t.packed.0 / 4;
            let offset = if t.tiled { tiled_2d(bx as i32, by as i32, (pitch_blocks + 31) & !31, bpb_log2) } else { (by * pitch_blocks + bx) << bpb_log2 };
            let addr = t.base + offset;
            let mut bytes = [0u8; 16];
            for k in (0..1u32 << bpb_log2).step_by(4) {
                bytes[k as usize..k as usize + 4].copy_from_slice(&read((addr & !3) + k).to_le_bytes());
            }
            let texels = dxt_block_rgba8(tab, &bytes, t.format, t.swizzle);
            for py in 0..4.min(rows - row0) {
                for px in 0..4.min(w - col0) {
                    let o = 4 * ((row0 + py) * w + col0 + px);
                    out[o..o + 4].copy_from_slice(&texels[py * 4 + px]);
                }
            }
        }
    }
}

/// The float DXT decode's values as bytes: colour channels by (endpoint a, endpoint b) for 5 and
/// 6 bits, each [a, b, a+(b-a)/3, a+(b-a)*2/3, a+(b-a)/2]; DXT3's 4-bit alphas; DXT5's 8 alphas
/// by (a0, a1).
struct DxtTables {
    c5: Vec<[u8; 5]>,
    c6: Vec<[u8; 5]>,
    a4: [u8; 16],
    a8: Vec<[u8; 8]>,
}

fn dxt_tables() -> &'static DxtTables {
    static T: std::sync::OnceLock<DxtTables> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let byte = |v: f32| (v * 255.0 + 0.5) as u8;
        // the same expressions as `decode_dxt_block`'s
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let chan = |bits: u32| -> Vec<[u8; 5]> {
            let n = 1u32 << bits;
            (0..n * n)
                .map(|k| {
                    let (a, b) = (unorm(k / n, bits), unorm(k % n, bits));
                    [byte(a), byte(b), byte(lerp(a, b, 1.0 / 3.0)), byte(lerp(a, b, 2.0 / 3.0)), byte(lerp(a, b, 0.5))]
                })
                .collect()
        };
        let a8 = (0..256 * 256u32)
            .map(|k| {
                let (a0, a1) = ((k / 256) as f32 / 255.0, (k % 256) as f32 / 255.0);
                std::array::from_fn(|i| {
                    byte(match i as u32 {
                        0 => a0,
                        1 => a1,
                        i if a0 > a1 => ((8 - i) as f32 * a0 + (i - 1) as f32 * a1) / 7.0,
                        6 => 0.0,
                        7 => 1.0,
                        i => ((6 - i) as f32 * a0 + (i - 1) as f32 * a1) / 5.0,
                    })
                })
            })
            .collect();
        DxtTables { c5: chan(5), c6: chan(6), a4: std::array::from_fn(|v| byte(v as f32 / 15.0)), a8 }
    })
}

/// `decode_dxt_block` and the fetch swizzle, as bytes (`dxt_tables`).
fn dxt_block_rgba8(tab: &DxtTables, b: &[u8; 16], format: u32, swizzle: u32) -> [[u8; 4]; 16] {
    let color = if format == 18 { &b[0..8] } else { &b[8..16] };
    let c0 = u16::from_le_bytes([color[0], color[1]]) as usize;
    let c1 = u16::from_le_bytes([color[2], color[3]]) as usize;
    let idx = u32::from_le_bytes(color[4..8].try_into().unwrap());
    let four = c0 > c1 || format != 18;
    let (r, g, bl) = (&tab.c5[(c0 >> 11) * 32 + (c1 >> 11)], &tab.c6[((c0 >> 5) & 63) * 64 + ((c1 >> 5) & 63)], &tab.c5[(c0 & 31) * 32 + (c1 & 31)]);
    // palette entry -> table column: a, b, then 1/3 and 2/3, or 1/2 and transparent black
    let palette: [[u8; 4]; 4] = std::array::from_fn(|e| {
        let col = match (e, four) {
            (0, _) => 0,
            (1, _) => 1,
            (2, true) => 2,
            (3, true) => 3,
            (2, false) => 4,
            _ => return [0; 4],
        };
        [r[col], g[col], bl[col], 255]
    });
    let mut out: [[u8; 4]; 16] = std::array::from_fn(|k| palette[((idx >> (2 * k)) & 3) as usize]);
    match format {
        19 => {
            let bits = u64::from_le_bytes(b[0..8].try_into().unwrap());
            for (k, o) in out.iter_mut().enumerate() {
                o[3] = tab.a4[((bits >> (4 * k)) & 15) as usize];
            }
        }
        20 => {
            let alphas = &tab.a8[b[0] as usize * 256 + b[1] as usize];
            let bits = u64::from_le_bytes([b[2], b[3], b[4], b[5], b[6], b[7], 0, 0]);
            for (k, o) in out.iter_mut().enumerate() {
                o[3] = alphas[((bits >> (3 * k)) & 7) as usize];
            }
        }
        _ => {}
    }
    if swizzle != 0o3210 {
        for o in out.iter_mut() {
            let c = *o;
            *o = std::array::from_fn(|i| match (swizzle >> (3 * i)) & 7 {
                s @ 0..=3 => c[s as usize],
                4 => 0,
                _ => 255,
            });
        }
    }
    out
}

fn edram_index(color_base: u32, pitch: u32, x: u32, y: u32) -> usize {
    let tiles_per_row = pitch.div_ceil(TILE_W as u32).max(1);
    let tile = color_base + (y / TILE_H as u32) * tiles_per_row + x / TILE_W as u32;
    (tile as usize % EDRAM_TILES) * TILE_W * TILE_H + (y as usize % TILE_H) * TILE_W + x as usize % TILE_W
}

pub(crate) struct TriSetup {
    pub(crate) v: [Vtx; 3],
    pub(crate) p: [[f32; 2]; 3],
    pub(crate) area: f32,
    /// polygon offset added to this triangle's depth
    pub(crate) zbias: f32,
    pub(crate) minx: i32,
    pub(crate) maxx: i32,
    pub(crate) miny: i32,
    pub(crate) maxy: i32,
}

impl TriSetup {
    pub(crate) fn new(ctx: &RasterCtx, t: [Vtx; 3]) -> Option<Self> {
        let p: [[f32; 2]; 3] = std::array::from_fn(|i| [t[i].pos[0] + ctx.offset[0], t[i].pos[1] + ctx.offset[1]]);
        let area = (p[1][0] - p[0][0]) * (p[2][1] - p[0][1]) - (p[2][0] - p[0][0]) * (p[1][1] - p[0][1]);
        if area == 0.0 || !area.is_finite() {
            return None;
        }
        // screen y points down, so area > 0 is clockwise on screen
        let front = (area > 0.0) == ctx.cw_front;
        if (front && ctx.cull_front) || (!front && ctx.cull_back) {
            return None;
        }
        let [sx0, sy0, sx1, sy1] = ctx.scissor;
        let minx = p.iter().map(|q| q[0]).fold(f32::MAX, f32::min).floor().max(sx0 as f32) as i32;
        let maxx = p.iter().map(|q| q[0]).fold(f32::MIN, f32::max).ceil().min(sx1 as f32) as i32;
        let miny = p.iter().map(|q| q[1]).fold(f32::MAX, f32::min).floor().max(sy0 as f32) as i32;
        let maxy = p.iter().map(|q| q[1]).fold(f32::MIN, f32::max).ceil().min(sy1 as f32) as i32;
        if maxx <= minx || maxy <= miny || (maxx - minx) as i64 * ((maxy - miny) as i64) > 4_000_000 {
            return None;
        }
        let zbias = match ctx.poly_offset[!front as usize] {
            Some((scale, offset)) => {
                // depth plane slopes in screen space: the bias grows with the steepest one
                let (z0, z1, z2) = (t[0].pos[2], t[1].pos[2], t[2].pos[2]);
                let dzdx = ((z1 - z0) * (p[2][1] - p[0][1]) - (z2 - z0) * (p[1][1] - p[0][1])) / area;
                let dzdy = ((p[1][0] - p[0][0]) * (z2 - z0) - (p[2][0] - p[0][0]) * (z1 - z0)) / area;
                offset + scale / 16.0 * dzdx.abs().max(dzdy.abs())
            }
            None => 0.0,
        };
        Some(Self { v: t, p, area, zbias, minx, maxx, miny, maxy })
    }
}

/// EDRAM base pointer shared by the raster threads. Thread k of n owns the 16-row tile bands
/// with band % n == k, and a pixel's EDRAM slot depends only on its own (x, y), so no two
/// threads write the same element.
struct Shared<T>(*mut T, usize);
impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Shared<T> {}
unsafe impl<T> Send for Shared<T> {}
unsafe impl<T> Sync for Shared<T> {}

/// Shade and blend every pixel of `tris` that lies in the tile bands owned by thread `k` of `n`.
#[allow(clippy::too_many_arguments)]
fn raster_rows(ctx: &RasterCtx, regs: &[u32], ps: &Shader, tex: &[Option<Arc<Decoded>>], tris: &[TriSetup], edram: Shared<Vec4>, depth: Shared<f32>, k: usize, n: usize) -> u64 {
    let mut env = PixelEnv { regs, tex };
    let mut drawn = 0u64;
    let blend = ctx.blend;
    let mut inv;
    for t in tris {
        let (p, area) = (&t.p, t.area);
        for y in t.miny..t.maxy {
            if n > 1 && (y as usize / TILE_H) % n != k {
                continue;
            }
            let py = y as f32 + 0.5;
            for x in t.minx..t.maxx {
                let px = x as f32 + 0.5;
                let w0 = (p[1][0] - px) * (p[2][1] - py) - (p[2][0] - px) * (p[1][1] - py);
                let w1 = (p[2][0] - px) * (p[0][1] - py) - (p[0][0] - px) * (p[2][1] - py);
                let w2 = area - w0 - w1;
                let inside = if area > 0.0 { w0 >= 0.0 && w1 >= 0.0 && w2 >= 0.0 } else { w0 <= 0.0 && w1 <= 0.0 && w2 <= 0.0 };
                if !inside {
                    continue;
                }
                let (b0, b1, b2) = (w0 / area, w1 / area, w2 / area);
                // depth: screen z is linear in screen space
                let z = b0 * t.v[0].pos[2] + b1 * t.v[1].pos[2] + b2 * t.v[2].pos[2] + t.zbias;
                let didx = edram_index(ctx.depth_base, ctx.surface_pitch, x as u32, y as u32);
                if ctx.z_enable && didx < depth.1 {
                    // SAFETY: in bounds; only this thread touches this tile band
                    let d = unsafe { *depth.0.add(didx) };
                    let pass = match ctx.z_func {
                        0 => false,
                        1 => z < d,
                        2 => z == d,
                        3 => z <= d,
                        4 => z > d,
                        5 => z != d,
                        6 => z >= d,
                        _ => true,
                    };
                    if !pass {
                        continue;
                    }
                }
                // attributes: perspective-correct (weights scaled by 1/w)
                let (q0, q1, q2) = (b0 * t.v[0].pos[3], b1 * t.v[1].pos[3], b2 * t.v[2].pos[3]);
                let qs = q0 + q1 + q2;
                let (c0, c1, c2) = if qs.is_finite() && qs != 0.0 { (q0 / qs, q1 / qs, q2 / qs) } else { (b0, b1, b2) };
                inv = Invocation::new();
                for r in 0..16 {
                    inv.r[r] = std::array::from_fn(|c| c0 * t.v[0].interp[r][c] + c1 * t.v[1].interp[r][c] + c2 * t.v[2].interp[r][c]);
                }
                let traced = PIXEL_TRACE.lock().ok().and_then(|g| g.as_ref().map(|t| (t.0, t.1))) == Some((x, y));
                let inputs = if traced { Some((inv.r[0], inv.r[1], inv.r[2], inv.r[3])) } else { None };
                if !ps.code.is_empty() {
                    ps.run(&mut inv, &mut env, true);
                }
                if let Some(i) = inputs {
                    if let Ok(mut g) = PIXEL_TRACE.lock() {
                        if let Some(t) = g.as_mut() {
                            t.2.push(format!(
                                "px ({x},{y}) rt base {} pitch {} z {z:.4} ps {:016x} depth_only {} in r0 {:?} r1 {:?} r2 {:?} r3 {:?} -> {:?} killed {} tex {:?}",
                                ctx.color_base, ctx.surface_pitch, hash(&ps.code), ctx.depth_only, i.0, i.1, i.2, i.3, inv.out.colors[0], inv.out.killed,
                                tex.iter().enumerate().filter_map(|(k, t)| t.as_ref().map(|t| (k, t.w, t.h))).collect::<Vec<_>>()
                            ));
                        }
                    }
                }
                if inv.out.killed {
                    continue;
                }
                let src = inv.out.colors[0];
                if ctx.depth_only {
                    if ctx.z_enable && ctx.z_write && didx < depth.1 {
                        // SAFETY: in bounds; only this thread touches this tile band
                        unsafe { *depth.0.add(didx) = z };
                    }
                    drawn += 1;
                    continue;
                }
                // alpha test
                if ctx.cc & 8 != 0 {
                    // compared as the 8-bit values the render target stores (see shader_wgsl)
                    let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round();
                    let (s3, a) = (q(src[3]), q(ctx.alpha_ref));
                    let pass = match ctx.cc & 7 {
                        0 => false,
                        1 => s3 < a,
                        2 => s3 == a,
                        3 => s3 <= a,
                        4 => s3 > a,
                        5 => s3 != a,
                        6 => s3 >= a,
                        _ => true,
                    };
                    if !pass {
                        continue;
                    }
                }
                let idx = edram_index(ctx.color_base, ctx.surface_pitch, x as u32, y as u32);
                if idx >= edram.1 {
                    continue;
                }
                // SAFETY: idx is in bounds, and only this thread writes pixels of this tile band.
                let slot = unsafe { &mut *edram.0.add(idx) };
                let dst = *slot;
                let mut out = dst;
                for i in 0..4 {
                    let (sf, df, op) = if i < 3 { (blend & 31, (blend >> 8) & 31, (blend >> 5) & 7) } else { ((blend >> 16) & 31, (blend >> 24) & 31, (blend >> 21) & 7) };
                    let v = combine(op, src[i] * blend_factor(sf, src, dst, ctx.constant, i), dst[i] * blend_factor(df, src, dst, ctx.constant, i));
                    if ctx.mask & (1 << i) != 0 {
                        out[i] = v.clamp(0.0, 1.0);
                    }
                }
                *slot = out;
                if ctx.z_enable && ctx.z_write && didx < depth.1 {
                    // SAFETY: as above
                    unsafe { *depth.0.add(didx) = z };
                }
                drawn += 1;
            }
        }
    }
    drawn
}

// ---------------------------------------------------------------- blending
fn blend_factor(f: u32, src: Vec4, dst: Vec4, constant: Vec4, i: usize) -> f32 {
    match f {
        0 => 0.0,
        1 => 1.0,
        4 => src[i],
        5 => 1.0 - src[i],
        6 => src[3],
        7 => 1.0 - src[3],
        8 => dst[i],
        9 => 1.0 - dst[i],
        10 => dst[3],
        11 => 1.0 - dst[3],
        12 => constant[i],
        13 => 1.0 - constant[i],
        14 => constant[3],
        15 => 1.0 - constant[3],
        16 => src[3].min(1.0 - dst[3]),
        _ => 1.0,
    }
}

fn combine(op: u32, s: f32, d: f32) -> f32 {
    match op {
        1 => s - d,
        2 => s.min(d),
        3 => s.max(d),
        4 => d - s,
        _ => s + d,
    }
}

impl Renderer {
    /// EDRAM colour and depth, for save states.
    pub(crate) fn targets(&self) -> (Vec<Vec4>, Vec<f32>) {
        (self.edram.clone(), self.depth.clone())
    }

    pub(crate) fn set_targets(&mut self, edram: Vec<Vec4>, depth: Vec<f32>) {
        if edram.len() == self.edram.len() && depth.len() == self.depth.len() {
            self.edram = edram;
            self.depth = depth;
        }
        self.tex_gen += 1;
    }

    pub(crate) fn shader(&mut self, code: &[u32]) -> Arc<Shader> {
        let h = hash(code);
        self.shaders.entry(h).or_insert_with(|| Arc::new(Shader::new(code.to_vec()))).clone()
    }

    pub(crate) fn edram_index(&self, color_base: u32, pitch: u32, x: u32, y: u32) -> usize {
        let tiles_per_row = pitch.div_ceil(TILE_W as u32).max(1);
        let tile = color_base + (y / TILE_H as u32) * tiles_per_row + x / TILE_W as u32;
        (tile as usize % EDRAM_TILES) * TILE_W * TILE_H + (y as usize % TILE_H) * TILE_W + x as usize % TILE_W
    }

    /// Run the vertex shader for `count` vertices of a draw.
    pub(crate) fn transform(&mut self, regs: &[u32], mem: &mut dyn GuestMem, vs_code: &[u32], indices: &[u32]) -> Vec<Vtx> {
        let vs = self.shader(vs_code);
        let vte = regs[PA_CL_VTE_CNTL];
        let vp: [f32; 6] = std::array::from_fn(|i| f32::from_bits(regs[PA_CL_VPORT_XSCALE + i]));
        let pix = pix_offset(regs);
        let mut out = Vec::with_capacity(indices.len());
        for &index in indices {
            let mut inv = Invocation::new();
            inv.r[0] = [index as f32, 0.0, 0.0, 0.0];
            vs.run(&mut inv, &mut Env { regs, mem }, false);
            if out.is_empty() {
                if let Some(d) = self.debug.as_mut() {
                    d.push(format!("    vs {:016x} ({} dwords, {} cf) head {:08x?}", hash(vs_code), vs_code.len(), vs.cf.len(), &vs_code[..6.min(vs_code.len())]));
                    d.push(format!("    vs first vertex: r0 {:?} r1 {:?} r2 {:?} pos {:?}", inv.r[0], inv.r[1], inv.r[2], inv.out.position));
                }
            }
            let o: &Outputs = &inv.out;
            let mut p = o.position;
            // VTX_W0_FMT set: the shader wrote W itself; clear: it wrote 1/W
            let w = if vte & (1 << 10) != 0 { p[3] } else if p[3] != 0.0 { 1.0 / p[3] } else { 1.0 };
            let inv_w = if w != 0.0 { 1.0 / w } else { 1.0 };
            if vte & (1 << 8) == 0 {
                p[0] *= inv_w;
                p[1] *= inv_w;
            }
            if vte & (1 << 9) == 0 {
                p[2] *= inv_w;
            }
            // viewport: enabled scale/offset per axis
            if vte & 1 != 0 {
                p[0] *= vp[0];
            }
            if vte & 2 != 0 {
                p[0] += vp[1];
            }
            if vte & 4 != 0 {
                p[1] *= vp[2];
            }
            if vte & 8 != 0 {
                p[1] += vp[3];
            }
            if vte & 16 != 0 {
                p[2] *= vp[4];
            }
            if vte & 32 != 0 {
                p[2] += vp[5];
            }
            p[0] += pix;
            p[1] += pix;
            let mut v = Vtx { pos: [p[0], p[1], p[2], inv_w], clip: [o.position[0], o.position[1], o.position[2], w], interp: o.interpolators };
            if o.point_size != 0.0 {
                v.interp[15][0] = o.point_size; // stash for point sprites
            }
            out.push(v);
        }
        out
    }

    /// Execute one draw packet.
    pub fn draw(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, vs: &[u32], ps: &[u32]) {
        let prim = init & 0x3F;
        let indices = draw_indices(regs, mem, init, index_base);
        let mode = regs[RB_MODECONTROL] & 7;
        let t0 = std::time::Instant::now();
        let verts = self.transform(regs, mem, vs, &indices);
        self.timing[0] += t0.elapsed();
        let before = self.pixels_drawn;
        if mode == 6 {
            let t0 = std::time::Instant::now();
            self.resolve(regs, mem, &verts);
            self.timing[2] += t0.elapsed();
            return;
        }
        if mode != 4 && mode != 5 {
            return; // ignored
        }
        let depth_only = mode == 5 || ps.is_empty();
        let tris = assemble(prim, &verts, regs);
        let ntris = tris.len();
        if self.debug.is_some() && self.textures_out.len() < 8 {
            let t = TexInfo::from_fetch(&regs[FETCH_BASE..FETCH_BASE + 6]);
            if t.base != 0 && t.width > 1 && t.width <= 2048 && t.height <= 2048 {
                let mut px = Vec::with_capacity((t.width * t.height * 4) as usize);
                for y in 0..t.height {
                    for x in 0..t.width {
                        px.extend(texel(mem, &t, x, y).map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8));
                    }
                }
                let name = format!("tex_{:08x}_fmt{}_{}x{}", t.base, t.format, t.width, t.height);
                if let Some(d) = self.debug.as_mut() {
                    d.push(format!("    texture0 {t:?}"));
                }
                self.textures_out.push((name, t.width, t.height, px));
            }
        }
        let ps = self.shader(ps);
        let t0 = std::time::Instant::now();
        let t1 = std::time::Instant::now();
        let tex = self.prepare_textures(regs, mem, &ps, &[]);
        self.timing[4] += t1.elapsed();
        self.raster_draw(regs, &ps, &tex, tris, depth_only);
        self.timing[1] += t0.elapsed();
        if let Some(d) = self.debug.as_mut() {
            let pos: Vec<String> = verts.iter().take(4).map(|v| format!("({:.1},{:.1})", v.pos[0], v.pos[1])).collect();
            let c0 = verts.first().map(|v| v.interp[0]).unwrap_or_default();
            for k in [0usize, 95] {
                let (d0, d1) = (regs[FETCH_BASE + k * 2], regs[FETCH_BASE + k * 2 + 1]);
                let a = d0 & !3;
                let words: Vec<String> = (0..6).map(|i| format!("{:08x}", gpu_read(mem, a + 4 * i, d1 & 3))).collect();
                d.push(format!("    vfetch const {k}: {d0:08x} {d1:08x} -> data {}", words.join(" ")));
            }
            let f = |r: usize| f32::from_bits(regs[r]);
            let (imin, imax) = indices.iter().fold((u32::MAX, 0), |(lo, hi), &i| (lo.min(i), hi.max(i)));
            d.push(format!(
                "    indices: src {} {}-bit swap {} n {} min {imin} max {imax} first {:?} | zero-pos verts {}",
                (init >> 6) & 3, if (init >> 11) & 1 == 1 { 32 } else { 16 }, regs[VGT_DMA_SIZE] >> 30, indices.len(),
                &indices[..indices.len().min(12)], verts.iter().filter(|v| v.clip[3].abs() < 1e-6).count()
            ));
            d.push(format!(
                "    state: vte {:#x} vport scale/off x {} {} y {} {} z {} {} | woff {:#x} sc_mode {:#x} | depth {:#x} info {:#x} | mask {:#x} blend {:#x} | surface {:#x} color_info {:#x}",
                regs[PA_CL_VTE_CNTL], f(PA_CL_VPORT_XSCALE), f(PA_CL_VPORT_XSCALE + 1), f(PA_CL_VPORT_XSCALE + 2), f(PA_CL_VPORT_XSCALE + 3),
                f(PA_CL_VPORT_XSCALE + 4), f(PA_CL_VPORT_XSCALE + 5), regs[PA_SC_WINDOW_OFFSET], regs[PA_SU_SC_MODE_CNTL],
                regs[RB_DEPTHCONTROL], regs[RB_DEPTH_INFO], regs[RB_COLOR_MASK], regs[RB_BLENDCONTROL0], regs[RB_SURFACE_INFO], regs[RB_COLOR_INFO]
            ));
            d.push(format!(
                "  draw prim {prim} n {} tris {ntris} -> {} px; rt base {} pitch {} fmt {}; verts {} interp0 {:?}",
                verts.len(), self.pixels_drawn - before, regs[RB_COLOR_INFO] & 0xFFF, regs[RB_SURFACE_INFO] & 0x3FFF,
                (regs[RB_COLOR_INFO] >> 16) & 0xF, pos.join(" "), c0
            ));
        }
    }

    /// Decode every texture the pixel shader can sample (cached until the next resolve/present).
    /// Decode (or reuse) the textures `ps` samples, except the fetch constants in `skip`.
    pub(crate) fn prepare_textures(&mut self, regs: &[u32], mem: &mut dyn GuestMem, ps: &Shader, skip: &[u32]) -> Vec<Option<Arc<Decoded>>> {
        // a new override table (#694): every texture decodes again, so cached setups see new ids
        let override_gen = crate::tex_override::generation();
        if override_gen != self.override_gen {
            self.override_gen = override_gen;
            self.tex_cache.clear();
        }
        let overrides = crate::tex_override::any();
        let mut out = vec![None; 32];
        for c in ps.texture_consts().into_iter().filter(|c| !skip.contains(c)) {
            let b = FETCH_BASE + c as usize * 6;
            let t = TexInfo::from_fetch(&regs[b..b + 6]);
            if t.width > 4096 || t.height > 4096 {
                continue;
            }
            if let Some(d) = self.debug.as_mut() {
                d.push(format!("    ps tex c{c}: {:08x?} -> {t:?}", &regs[b..b + 6]));
            }
            let key = (t.base, t.format, t.endian, t.tiled, t.pitch, t.width, t.height, t.swizzle);
            let gen = self.tex_gen;
            let cached = self.tex_cache.get(&key).map(|e| (e.1, e.3));
            // pages written since the last look: query (and reset) before reading, so a write
            // during the read is seen next time
            let watched = match cached {
                Some((checked, _)) if checked == gen => None,
                _ => self.watch(mem, &t),
            };
            let fresh = match self.tex_cache.get_mut(&key) {
                Some((_, checked, d, _)) if *checked == gen => Some(d.clone()),
                Some((_, checked, d, stamp)) if watched.is_some_and(|newest| newest <= *stamp) => {
                    *checked = gen;
                    Some(d.clone())
                }
                Some((h, checked, d, stamp)) => {
                    let now = tex_hash(mem, &t);
                    (now == *h).then(|| {
                        *checked = gen;
                        *stamp = self.page_stamp;
                        d.clone()
                    })
                }
                None => None,
            };
            let d = match fresh {
                Some(d) => d,
                None => {
                    let t0 = tex_stats_on().then(std::time::Instant::now);
                    let raw = raw_bytes(mem, &t);
                    let hash = bytes_hash(&raw);
                    // a replacement image for this texture (#694): the texels as drawn, RGBA8
                    let replaced = if overrides { crate::tex_override::lookup(hash, t.width, t.height) } else { None };
                    let bc = if self.bc && replaced.is_none() { bc_levels(mem, &t, &regs[b..b + 6]) } else { None };
                    let d = Arc::new(match (replaced, bc) {
                        (Some(px8), _) => Decoded::new8(t.width, t.height, (*px8).clone()),
                        (None, Some(levels)) => Decoded::new_bc(t.width, t.height, t.format, levels),
                        (None, None) => {
                            let px8 = if self.rgba8 && UNORM8_FORMATS.contains(&t.format) { decode_texture_rgba8(&raw, &t, self.threads) } else { None };
                            match px8 {
                                Some(b) => Decoded::new8(t.width, t.height, b),
                                None => Decoded::new(t.width, t.height, decode_texture(&raw, &t, self.threads)),
                            }
                        }
                    });
                    if let Some(t0) = t0 {
                        tex_stats_add(t.format, t.width as u64 * t.height as u64, t0.elapsed().as_nanos() as u64);
                    }
                    self.tex_cache.insert(key, (hash, gen, d.clone(), self.page_stamp));
                    tex_log(gen, &t, hash, &d, &regs[b..b + 6]);
                    d
                }
            };
            out[c as usize] = Some(d);
        }
        out
    }

    /// The newest page stamp in texture `t`'s bytes after recording the pages written since the
    /// last query, or None when guest memory has no write watch.
    fn watch(&mut self, mem: &mut dyn GuestMem, t: &TexInfo) -> Option<u32> {
        self.range_stamp(mem, phys(t.base), raw_len(t) as u32)
    }

    /// The newest page stamp in guest physical [lo, lo + len) after recording the pages written
    /// since the last query (the stamps are shared by everyone who asks, so one caller's query
    /// never hides a write from another), or None when guest memory has no write watch.
    pub(crate) fn range_stamp(&mut self, mem: &mut dyn GuestMem, lo: u32, len: u32) -> Option<u32> {
        let written = mem.write_watch(lo, len)?;
        if self.page_gen.is_empty() {
            self.page_gen = vec![0; 1 << 20];
        }
        if !written.is_empty() {
            self.page_stamp += 1;
            for p in written {
                self.page_gen[(p >> 12) as usize] = self.page_stamp;
            }
        }
        let t0 = guest::mem::Memory::translate(lo) >> 12;
        let t1 = guest::mem::Memory::translate(lo.wrapping_add(len.max(1) - 1)) >> 12;
        Some(if t1 >= t0 { self.page_gen[t0 as usize..=t1 as usize].iter().copied().max().unwrap_or(0) } else { u32::MAX })
    }

    /// Rasterise a draw's triangles, splitting the rows across CPU cores for big draws.
    fn raster_draw(&mut self, regs: &[u32], ps: &Shader, tex: &[Option<Arc<Decoded>>], tris: Vec<[Vtx; 3]>, depth_only: bool) {
        let mut ctx = RasterCtx::new(regs);
        ctx.depth_only = depth_only;
        let setups: Vec<TriSetup> = tris.into_iter().flat_map(|t| clip_near(&ctx, t)).filter_map(|t| TriSetup::new(&ctx, t)).collect();
        if setups.is_empty() {
            return;
        }
        let work: i64 = setups.iter().map(|t| ((t.maxx - t.minx) as i64) * ((t.maxy - t.miny) as i64)).sum();
        let edram = Shared(self.edram.as_mut_ptr(), self.edram.len());
        let depth = Shared(self.depth.as_mut_ptr(), self.depth.len());
        let threads = if work < 16_384 { 1 } else { self.threads.max(1) };
        let drawn: u64 = if threads == 1 {
            raster_rows(&ctx, regs, ps, tex, &setups, edram, depth, 0, 1)
        } else {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..threads)
                    .map(|k| {
                        let (ctx, setups) = (&ctx, &setups);
                        s.spawn(move || raster_rows(ctx, regs, ps, tex, setups, edram, depth, k, threads))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap_or(0)).sum()
            })
        };
        self.pixels_drawn += drawn;
    }

    /// Copy (and optionally clear) an EDRAM rectangle into a tiled texture in memory.
    pub(crate) fn resolve(&mut self, regs: &[u32], mem: &mut dyn GuestMem, verts: &[Vtx]) {
        self.resolves += 1;
        let [x0, y0, x1, y1] = resolve_rect(regs, verts);
        let surface_pitch = (regs[RB_SURFACE_INFO] & 0x3FFF).max(1);
        let color_base = regs[RB_COLOR_INFO] & 0xFFF;
        let ctl = regs[RB_COPY_CONTROL];
        let dest = regs[RB_COPY_DEST_BASE];
        let dpitch = regs[RB_COPY_DEST_PITCH] & 0x3FFF;
        let dheight = (regs[RB_COPY_DEST_PITCH] >> 16) & 0x3FFF;
        let dinfo = regs[RB_COPY_DEST_INFO];
        let endian = dinfo & 7;
        let fmt = (dinfo >> 7) & 0x3F;
        // D3D moves RB_COPY_DEST_BASE instead of using a destination offset
        let (dox, doy) = (0i32, 0i32);
        let copy_cmd = (ctl >> 20) & 3;
        if let Some(d) = self.debug.as_mut() {
            d.push(format!(
                "  resolve rect ({x0},{y0})-({x1},{y1}) from base {color_base} pitch {surface_pitch} -> {dest:#010x} pitch {dpitch} fmt {fmt} endian {endian} cmd {copy_cmd} clear {} clear value {:#010x}",
                ctl & (1 << 8) != 0, regs[RB_COLOR_CLEAR]
            ));
        }
        let from_depth = ctl & 7 == 4; // copy_src_select: 0-3 colour targets, 4 depth
        if copy_cmd != 3 && dest != 0 && x1 > x0 && y1 > y0 && from_depth {
            // depth -> 24_8 texels (unorm depth in the top 24 bits, stencil 0)
            let depth_base = regs[RB_DEPTH_INFO] & 0xFFF;
            let pitch_aligned = (dpitch + 31) & !31;
            for y in y0..y1 {
                for x in x0..x1 {
                    let (tx, ty) = (x - x0, y - y0);
                    if tx >= dpitch || (dheight != 0 && ty >= dheight) {
                        continue;
                    }
                    let z = self.depth[self.edram_index(depth_base, surface_pitch, x, y)];
                    // min: 16777215.5 rounds to 2^24 in f32, and 1.0 << 8 would wrap to 0 ("nearest")
                    let word = ((z.clamp(0.0, 1.0) * 16_777_215.0 + 0.5) as u32).min(0xFF_FFFF) << 8;
                    gpu_write(mem, dest + tiled_2d(tx as i32, ty as i32, pitch_aligned, 2), endian, word);
                }
            }
        } else if copy_cmd != 3 && dest != 0 && x1 > x0 && y1 > y0 && fmt == 6 {
            let pitch_aligned = (dpitch + 31) & !31;
            for y in y0..y1 {
                for x in x0..x1 {
                    let mut c = self.edram[self.edram_index(color_base, surface_pitch, x, y)];
                    if dinfo & (1 << 24) != 0 {
                        c.swap(0, 2); // copy_dest_swap: exchange red and blue
                    }
                    let b = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
                    let word = b(c[0]) | (b(c[1]) << 8) | (b(c[2]) << 16) | (b(c[3]) << 24);
                    let (tx, ty) = (x as i32 - x0 as i32 + dox, y as i32 - y0 as i32 + doy);
                    // never write outside the destination texture
                    if tx < 0 || ty < 0 || tx as u32 >= dpitch || (dheight != 0 && ty as u32 >= dheight) {
                        continue;
                    }
                    let off = tiled_2d(tx, ty, pitch_aligned, 2);
                    gpu_write(mem, dest + off, endian, word);
                }
            }
        }
        // Only cached textures under the written range can have changed: those are re-hashed on
        // their next use. (Textures the CPU writes
        // are re-verified once per frame, at present.) Re-verifying every texture after every
        // resolve re-hashed each one ~8 times a frame.
        if copy_cmd != 3 && dest != 0 && x1 > x0 && y1 > y0 && (from_depth || fmt == 6) {
            let rows = if dheight != 0 { dheight } else { y1 - y0 };
            let len = ((dpitch + 31) & !31) * ((rows + 31) & !31) * 4;
            self.invalidate_textures(phys(dest), len);
        }
        if let Some(d) = self.debug.as_mut() {
            let (sx, sy) = (300u32.max(x0), 200u32.max(y0));
            let e = self.edram[{
                let tiles_per_row = surface_pitch.div_ceil(TILE_W as u32).max(1);
                let tile = color_base + (sy / TILE_H as u32) * tiles_per_row + sx / TILE_W as u32;
                (tile as usize % EDRAM_TILES) * TILE_W * TILE_H + (sy as usize % TILE_H) * TILE_W + sx as usize % TILE_W
            }];
            let off = tiled_2d((sx - x0) as i32, (sy - y0) as i32, (dpitch + 31) & !31, 2);
            let back = gpu_read(mem, dest + off, endian);
            d.push(format!("    resolve check ({sx},{sy}): edram {e:?} -> dest word {back:#010x} (dheight {dheight})"));
        }
        if ctl & (1 << 9) != 0 {
            let z = depth_clear_value(regs);
            let depth_base = regs[RB_DEPTH_INFO] & 0xFFF;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = self.edram_index(depth_base, surface_pitch, x, y);
                    self.depth[i] = z;
                }
            }
        }
        if ctl & (1 << 8) != 0 {
            let c = color_clear_value(regs);
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = self.edram_index(color_base, surface_pitch, x, y);
                    self.edram[i] = c;
                }
            }
        }
    }

    /// A resolve wrote guest bytes [lo, lo + len): cached textures there are re-hashed on their
    /// next use (and reused when the bytes didn't change).
    pub(crate) fn invalidate_textures(&mut self, lo: u32, len: u32) {
        let hi = lo.saturating_add(len);
        for (k, entry) in self.tex_cache.iter_mut() {
            let (tl, th) = tex_span(k);
            if th > lo && tl < hi {
                entry.1 = 0;
            }
        }
    }

    /// A frame was presented without `present` (the GPU backend presents itself): the CPU may
    /// have uploaded new texture data this frame, so cached textures are re-verified.
    /// Decoded textures held (#746: the cache never evicts; each keeps its GPU copy alive).
    pub(crate) fn tex_cache_len(&self) -> usize {
        self.tex_cache.len()
    }

    pub(crate) fn new_frame(&mut self) {
        self.tex_gen += 1;
    }

    /// Read the front buffer described by texture fetch constant 0 (written by VdSwap).
    pub fn present(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) {
        let t0 = std::time::Instant::now();
        self.tex_gen += 1; // the CPU may have uploaded new texture data this frame
        self.present_inner(regs, mem, fb, w, h);
        self.timing[3] += t0.elapsed();
    }

    fn present_inner(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) {
        let mut t = TexInfo::from_fetch(&regs[FETCH_BASE..FETCH_BASE + 6]);
        t.base = fb & !0xFFF;
        if t.width < 2 {
            t.width = w;
            t.height = h;
        }
        let (w, h) = (w.min(4096), h.min(4096));
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        let mut put = |c: Vec4| {
            rgba.extend(c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8).iter().take(3));
            rgba.push(255);
        };
        if w <= t.width && h <= t.height {
            // one bulk read and the multithreaded texture decode (same texel decoder), instead of
            // a guest memory read per pixel
            let px = decode_texture(&raw_bytes(mem, &t), &t, self.threads);
            for y in 0..h as usize {
                for &c in &px[y * t.width as usize..][..w as usize] {
                    put(c);
                }
            }
        } else {
            for y in 0..h {
                for x in 0..w {
                    put(texel(mem, &t, x, y));
                }
            }
        }
        self.frame = Some((w, h, rgba));
    }
}

/// Primitive assembly into screen-space triangles.
pub(crate) fn assemble(prim: u32, v: &[Vtx], regs: &[u32]) -> Vec<[Vtx; 3]> {
    let mut out = Vec::new();
    match prim {
        4 => {
            for c in v.chunks_exact(3) {
                out.push([c[0], c[1], c[2]]);
            }
        }
        5 => {
            for i in 1..v.len().saturating_sub(1) {
                out.push([v[0], v[i], v[i + 1]]);
            }
        }
        6 => {
            for i in 0..v.len().saturating_sub(2) {
                if i % 2 == 0 { out.push([v[i], v[i + 1], v[i + 2]]) } else { out.push([v[i + 1], v[i], v[i + 2]]) }
            }
        }
        8 => {
            for c in v.chunks_exact(3) {
                // rectangle from three corners: the corner opposite the longest edge is shared
                let d = |a: &Vtx, b: &Vtx| (a.pos[0] - b.pos[0]).powi(2) + (a.pos[1] - b.pos[1]).powi(2);
                let (d01, d12, d02) = (d(&c[0], &c[1]), d(&c[1], &c[2]), d(&c[0], &c[2]));
                let (corner, a, b) = if d12 >= d01 && d12 >= d02 { (0, 1, 2) } else if d02 >= d01 { (1, 0, 2) } else { (2, 0, 1) };
                let mut fourth = c[a];
                for k in 0..4 {
                    fourth.pos[k] = c[a].pos[k] + c[b].pos[k] - c[corner].pos[k];
                }
                for i in 0..16 {
                    for k in 0..4 {
                        fourth.interp[i][k] = c[a].interp[i][k] + c[b].interp[i][k] - c[corner].interp[i][k];
                    }
                }
                out.push([c[corner], c[a], c[b]]);
                out.push([fourth, c[b], c[a]]);
            }
        }
        13 => {
            for c in v.chunks_exact(4) {
                out.push([c[0], c[1], c[2]]);
                out.push([c[0], c[2], c[3]]);
            }
        }
        1 => {
            let reg = regs[PA_SU_POINT_SIZE];
            for p in v {
                let size = if p.interp[15][0] > 0.0 { p.interp[15][0] } else { ((reg & 0xFFFF) as f32) / 8.0 * 2.0 };
                let hs = size / 2.0;
                let mk = |dx: f32, dy: f32, u: f32, vv: f32| {
                    let mut q = *p;
                    q.pos[0] += dx;
                    q.pos[1] += dy;
                    let _ = (u, vv); // sprite texcoord generation (SQ point params) not modelled yet
                    q
                };
                let (a, b, c, d) = (mk(-hs, -hs, 0.0, 0.0), mk(hs, -hs, 1.0, 0.0), mk(hs, hs, 1.0, 1.0), mk(-hs, hs, 0.0, 1.0));
                out.push([a, b, c]);
                out.push([a, c, d]);
            }
        }
        _ => {}
    }
    out
}

impl crate::backend::RenderBackend for Renderer {
    fn name(&self) -> &'static str {
        "software"
    }
    fn draw(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, vs: &[u32], ps: &[u32]) {
        Renderer::draw(self, regs, mem, init, index_base, vs, ps)
    }
    fn present(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) -> Option<crate::backend::Image> {
        Renderer::present(self, regs, mem, fb, w, h);
        self.frame.take()
    }
    fn start_debug(&mut self) {
        self.debug = Some(Vec::new());
    }
    fn take_debug(&mut self) -> Option<Vec<String>> {
        self.debug.take()
    }
    fn debugging(&self) -> bool {
        self.debug.is_some()
    }
    fn take_textures(&mut self) -> Vec<(String, u32, u32, Vec<u8>)> {
        std::mem::take(&mut self.textures_out)
    }
    fn stats(&self) -> crate::backend::BackendStats {
        crate::backend::BackendStats { timing: self.timing, pixels_drawn: self.pixels_drawn, resolves: self.resolves }
    }
    fn targets(&self) -> (Vec<Vec4>, Vec<f32>) {
        Renderer::targets(self)
    }
    fn set_targets(&mut self, edram: Vec<Vec4>, depth: Vec<f32>) {
        Renderer::set_targets(self, edram, depth)
    }
}

#[cfg(test)]
mod tests {
    /// `cargo test --release -p runtime --lib dxt_rgba8_speed -- --ignored --nocapture`: the
    /// byte decode against the float decode and conversion it replaces (one thread, 1024x1024).
    #[test]
    #[ignore]
    fn dxt_rgba8_speed() {
        for format in [18u32, 20] {
            let bpb = if format == 18 { 8 } else { 16 };
            let buf: Vec<u8> = (0..1024 * 1024 / 16 * bpb).map(|i| (i as u32).wrapping_mul(2654435761) as u8).collect();
            let t = super::TexInfo { base: 0, format, endian: 0, tiled: false, pitch: 1024, width: 1024, height: 1024, swizzle: 0o3210, packed: (0, 0), clamp: [0, 0] };
            let time = |f: &dyn Fn()| {
                let s = std::time::Instant::now();
                for _ in 0..5 {
                    f();
                }
                s.elapsed().as_secs_f64() / 5.0 * 1000.0
            };
            let new = time(&|| {
                std::hint::black_box(super::decode_texture_rgba8(&buf, &t, 1));
            });
            let old = time(&|| {
                let px = super::decode_texture(&buf, &t, 1);
                std::hint::black_box(px.iter().flat_map(|c| c.map(|v| (v * 255.0 + 0.5) as u8)).collect::<Vec<u8>>());
            });
            println!("format {format}: bytes {new:.2} ms, float + convert {old:.2} ms");
        }
    }

    /// The table decode gives the float decode's bytes for every DXT format and swizzle:
    /// random blocks, plus every colour endpoint pair through both palette modes.
    #[test]
    fn dxt_bytes_match_the_float_decode() {
        let tab = super::dxt_tables();
        let to8 = |c: super::Vec4| -> [u8; 4] { std::array::from_fn(|i| (c[i] * 255.0 + 0.5) as u8) };
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let swizzles = [0o3210, 0o0123, 0o5432, 0o4210, 0o3333, 0o0000];
        let check = |b: &[u8; 16], format: u32, swz: u32| {
            let want: Vec<[u8; 4]> = super::decode_dxt_block(b, format).iter().map(|c| to8(super::apply_tex_swizzle(*c, swz))).collect();
            assert_eq!(super::dxt_block_rgba8(tab, b, format, swz).to_vec(), want, "format {format} swizzle {swz:o} block {b:02x?}");
        };
        for _ in 0..200_000 {
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&next().to_le_bytes());
            b[8..].copy_from_slice(&next().to_le_bytes());
            for format in 18..=20 {
                check(&b, format, swizzles[(next() % 6) as usize]);
            }
        }
        // every endpoint pair, every index (0xE4 = indices 0,1,2,3 in each row)
        for c0 in (0..=0xFFFFu32).step_by(7) {
            for c1 in [0u32, 0xFFFF, c0 ^ 0x0821, (c0 * 31) & 0xFFFF] {
                let mut b = [0u8; 16];
                b[0..2].copy_from_slice(&(c0 as u16).to_le_bytes());
                b[2..4].copy_from_slice(&(c1 as u16).to_le_bytes());
                b[4..8].copy_from_slice(&[0xE4; 4]);
                check(&b, 18, 0o3210);
            }
        }
        // every DXT5 alpha endpoint pair
        for a in 0..65536u32 {
            let mut b = [0u8; 16];
            b[0] = (a >> 8) as u8;
            b[1] = a as u8;
            b[2..8].copy_from_slice(&[0x88, 0xC6, 0xFA, 0x88, 0xC6, 0xFA]); // indices 0..7
            check(&b, 20, 0o3210);
        }
    }


    #[test]
    fn packed_mip0_offsets() {
        assert_eq!(packed_mip0_offset(512, 4), (0, 16)); // wide: level 0 16 rows down (team-select lighting ramp)
        assert_eq!(packed_mip0_offset(4, 512), (16, 0)); // tall: 16 across
        assert_eq!(packed_mip0_offset(16, 16), (16, 0)); // square counts as tall
        assert_eq!(packed_mip0_offset(64, 32), (0, 0)); // shorter side over 16: not packed
    }

    use super::*;

    /// Guest memory for tests: a flat byte array (physical and virtual addresses alike).
    struct Flat(Vec<u8>);
    impl GuestMem for Flat {
        fn read(&mut self, a: u32, out: &mut [u8]) {
            let a = (a & 0xFFFF) as usize;
            out.copy_from_slice(&self.0[a..a + out.len()]);
        }
        fn write(&mut self, a: u32, data: &[u8]) {
            let a = (a & 0xFFFF) as usize;
            self.0[a..a + data.len()].copy_from_slice(data);
        }
    }

    /// `bc_levels` (#304, LMA_TEX_BC): level 0's blocks decode to exactly `decode_texture`'s texels
    /// (DXT1/3/5, every endian, tiled), level 1 comes from the mip address as a 32x32 texture
    /// there, and level 2 from its place in the packed tail after it.
    #[test]
    fn bc_levels_hold_the_decoded_texels() {
        let mut mem = Flat((0..1u32 << 16).map(|i| (i.wrapping_mul(2654435761) >> 11) as u8).collect());
        for format in 18..=20 {
            for endian in 0..4 {
                let t = TexInfo { base: 0x1000, format, endian, tiled: true, pitch: 64, width: 64, height: 64, swizzle: 0o3210, clamp: [0, 0], packed: (0, 0) };
                let fetch = [0, 0, 0, 0, 6 << 6, 0x6000 | (1 << 11) | (2 << 9)];
                let levels = bc_levels(&mut mem, &t, &fetch).expect("a tiled 64x64 DXT texture");
                assert_eq!(levels.len(), 7, "64 down to 1; 16 and smaller from the packed tail");
                // level 2 (16x16) is the tail's first: 16 texels across in the slot after level 1
                // (0x6000 + 32 x 32 blocks), so texels 16..32 of a 32x32 texture there
                let tail = TexInfo { base: 0x6000 + (1024 << if format == 18 { 3 } else { 4 }), width: 32, height: 32, pitch: 32, ..t };
                let mut raw = vec![0u8; raw_len(&tail)];
                mem.read(tail.base, &mut raw);
                let want = decode_texture(&raw, &tail, 1);
                let d = Decoded::new_bc(16, 16, format, vec![levels[2].clone()]);
                for i in 0..256usize {
                    let w = want[(i / 16) * 32 + 16 + i % 16];
                    assert!(d.texel(i).iter().zip(w).all(|(a, b)| a.to_bits() == b.to_bits()), "fmt {format} endian {endian} tail level 2 texel {i}");
                }
                for (l, (base, size)) in [(0x1000u32, 64u32), (0x6000, 32)].into_iter().enumerate() {
                    let tl = TexInfo { base, width: size, height: size, pitch: size, ..t };
                    let mut raw = vec![0u8; raw_len(&tl)];
                    mem.read(base, &mut raw);
                    let want = decode_texture(&raw, &tl, 1);
                    let d = Decoded::new_bc(size, size, format, vec![levels[l].clone()]);
                    for (i, w) in want.iter().enumerate() {
                        assert!(d.texel(i).iter().zip(w).all(|(a, b)| a.to_bits() == b.to_bits()), "fmt {format} endian {endian} level {l} texel {i}");
                    }
                }
                assert!(bc_levels(&mut mem, &TexInfo { swizzle: 0o0123, ..t }, &fetch).is_none(), "a swizzle a view can't do");
                assert!(bc_levels(&mut mem, &TexInfo { width: 48, ..t }, &fetch).is_none(), "mips on a size that isn't a power of two");
            }
        }
    }

    #[test]
    fn index_decode_matches_per_index_reads() {
        let mut mem = Flat((0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect());
        for swap in 0..4u32 {
            for is32 in [false, true] {
                for base in [0x100u32, 0x102, 0x104, 0x101, 0x103] {
                    for count in [0u32, 1, 2, 3, 7, 64] {
                        let mut regs = vec![0u32; 0x6000];
                        regs[VGT_DMA_SIZE] = swap << 30;
                        let init = (count << 16) | ((is32 as u32) << 11);
                        let got = draw_indices(&regs, &mut mem, init, base);
                        // the reference: each index from its own word, as before
                        let word = |m: &mut Flat, a: u32| {
                            let mut b = [0u8; 4];
                            m.read(a, &mut b);
                            gpu_swap(u32::from_le_bytes(b), swap)
                        };
                        let want: Vec<u32> = (0..count)
                            .map(|i| {
                                if is32 {
                                    word(&mut mem, base + 4 * i)
                                } else {
                                    let w = word(&mut mem, (base + 2 * i) & !3);
                                    if (base + 2 * i) & 2 == 0 { w & 0xFFFF } else { w >> 16 }
                                }
                            })
                            .collect();
                        assert_eq!(got, want, "swap {swap} is32 {is32} base {base:#x} count {count}");
                    }
                }
            }
        }
    }

    #[test]
    fn chunked_hash_matches_whole_buffer() {
        for len in [0usize, 31, 32, 1000, (1 << 16) * 3 + 40] {
            let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let mut h = Hasher::default();
            for c in data.chunks(1 << 16) {
                h.update(c);
            }
            assert_eq!(h.finish(len), bytes_hash(&data), "{len}");
        }
        // content and length both matter
        assert_ne!(bytes_hash(&[1u8; 64]), bytes_hash(&[2u8; 64]));
        assert_ne!(bytes_hash(&[0u8; 64]), bytes_hash(&[0u8; 96]));
    }
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    /// Guest memory with a write watch: `pending` holds the (translated) 4 KB pages written since
    /// they were last queried; a query returns those in its range and resets them, as the
    /// Windows flat map does.
    struct Watched {
        pending: Vec<u32>,
    }

    impl GuestMem for Watched {
        fn read(&mut self, _a: u32, out: &mut [u8]) {
            out.fill(0);
        }
        fn write(&mut self, _a: u32, _data: &[u8]) {}
        fn write_watch(&mut self, a: u32, len: u32) -> Option<Vec<u32>> {
            let lo = guest::mem::Memory::translate(a);
            let (hit, keep): (Vec<u32>, Vec<u32>) = self.pending.iter().partition(|&&p| p >= lo & !0xFFF && p < lo + len);
            self.pending = keep;
            Some(hit)
        }
    }

    /// 64x64 8888: 16 KB, four pages from `base`.
    fn tex(base: u32) -> TexInfo {
        TexInfo { base, format: 6, endian: 0, tiled: false, pitch: 64, width: 64, height: 64, swizzle: 0, clamp: [0, 0], packed: (0, 0) }
    }

    /// `Renderer::watch` (#246): the newest page stamp in a texture's bytes, so a texture can tell
    /// whether anything was written since it was last verified, even when another texture's
    /// query took the write-watch bits of a page they share.
    #[test]
    fn texture_write_watch_stamps() {
        let _serial = crate::tex_override::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut r = Renderer::default();
        let (a, b) = (tex(0x1000_0000), tex(0x1000_2000)); // b's first two pages are a's last two
        let page = |o: u32| 0xB000_0000 + o;
        let mut m = Watched { pending: vec![page(0x1000)] };
        let first = r.watch(&mut m, &a).unwrap();
        assert!(first > 0, "a page written before the first look");
        assert_eq!(r.watch(&mut m, &a), Some(first), "nothing new");
        // a page outside the texture doesn't count
        m.pending.push(page(0x8000));
        assert_eq!(r.watch(&mut m, &a), Some(first));
        // a write to a page both textures cover, seen first by b's query (which resets it)...
        let b_before = r.watch(&mut m, &b).unwrap();
        m.pending.push(page(0x3000));
        let b_after = r.watch(&mut m, &b).unwrap();
        assert!(b_after > b_before);
        // ...still shows in a's next look: its stamp is newer than what a verified
        assert!(r.watch(&mut m, &a).unwrap() > first, "a write taken by another texture's query is not lost");
        // without a write watch: None, so callers hash as before
        struct Plain;
        impl GuestMem for Plain {
            fn read(&mut self, _a: u32, out: &mut [u8]) {
                out.fill(0);
            }
            fn write(&mut self, _a: u32, _data: &[u8]) {}
        }
        assert_eq!(r.watch(&mut Plain, &a), None);
    }
}

#[cfg(test)]
mod tex_cache_tests {
    use super::*;

    /// Guest memory that counts the bytes read, with an optional write watch (as in watch_tests).
    struct Counting {
        watch: bool,
        pending: Vec<u32>,
        read: usize,
    }

    impl GuestMem for Counting {
        fn read(&mut self, _a: u32, out: &mut [u8]) {
            self.read += out.len();
            out.fill(0x5A);
        }
        fn write(&mut self, _a: u32, _data: &[u8]) {}
        fn write_watch(&mut self, a: u32, len: u32) -> Option<Vec<u32>> {
            if !self.watch {
                return None;
            }
            let lo = guest::mem::Memory::translate(a);
            let (hit, keep): (Vec<u32>, Vec<u32>) = self.pending.iter().partition(|&&p| p >= lo & !0xFFF && p < lo + len);
            self.pending = keep;
            Some(hit)
        }
    }

    /// The hand-assembled pixel shader of shader.rs's tests: one 2D texture fetch from constant 3.
    fn shader() -> Shader {
        let exec_end = 1 | (2 << 12) | (0b0101 << 16);
        let cf = [exec_end, 2 << 12, 0];
        let vfetch = [(1 << 5) | (2 << 12) | (31 << 20) | (2 << 25), 0x688 | (0x39 << 16), 12 | (4 << 8)];
        let tfetch = [1 | (3 << 20), 0x688 | (1 << 12) | (1 << 14), 1 << 14];
        Shader::new([cf, vfetch, tfetch].concat())
    }

    /// Registers with fetch constant 3 = a linear 64x64 8888 texture at 0x10000000 (16 KB).
    fn regs() -> Vec<u32> {
        let mut r = vec![0u32; crate::gpu::REG_COUNT];
        let d = [2 << 22, 0x1000_0000 | 6, 63 | (63 << 13), 0, 0, 0];
        r[FETCH_BASE + 3 * 6..FETCH_BASE + 3 * 6 + 6].copy_from_slice(&d);
        r
    }

    /// The decode cache with a write watch (#246): a texture nobody wrote isn't read again in a
    /// new frame; a written one is re-hashed (and reused when its bytes are the same); without a
    /// write watch every new frame re-hashes, as before.
    #[test]
    fn unwritten_textures_skip_the_rehash() {
        let _serial = crate::tex_override::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (sh, regs) = (shader(), regs());
        let mut r = Renderer::default();
        let mut m = Counting { watch: true, pending: Vec::new(), read: 0 };
        let first = r.prepare_textures(&regs, &mut m, &sh, &[])[3].clone().expect("decoded");
        assert_eq!(m.read, 16 << 10, "decoded once");
        r.prepare_textures(&regs, &mut m, &sh, &[]);
        assert_eq!(m.read, 16 << 10, "same frame: cached");
        r.new_frame();
        r.prepare_textures(&regs, &mut m, &sh, &[]);
        assert_eq!(m.read, 16 << 10, "new frame, nothing written: no re-hash");
        m.pending.push(0xB000_1000);
        r.new_frame();
        let again = r.prepare_textures(&regs, &mut m, &sh, &[])[3].clone().unwrap();
        assert_eq!(m.read, 32 << 10, "written: re-hashed");
        assert!(Arc::ptr_eq(&first, &again), "same bytes: the decode is reused");
        // no write watch: every new frame re-hashes
        let mut plain = Counting { watch: false, pending: Vec::new(), read: 0 };
        let mut r = Renderer::default();
        r.prepare_textures(&regs, &mut plain, &sh, &[]);
        r.new_frame();
        r.prepare_textures(&regs, &mut plain, &sh, &[]);
        assert_eq!(plain.read, 32 << 10);
    }
}
