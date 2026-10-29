//! wgpu rendering backend (#6, phase 1): pixels on the GPU, vertices on the CPU.
//!
//! The expensive part of the software renderer is the pixel pipeline (shading, texturing,
//! blending, depth). This backend runs exactly that on the GPU (D3D12 / Vulkan / Metal through
//! wgpu) and keeps everything else from `raster::Renderer`:
//! - vertex shaders, primitive assembly, near-plane clipping and culling stay on the CPU (cheap,
//!   already correct); the transformed vertices go to a pass-through vertex shader;
//! - pixel shaders are translated to WGSL (`shader_wgsl`), textures are decoded by the software
//!   decoder (and its cache) and uploaded as RGBA32F;
//! - every EDRAM surface (colour base + pitch, depth base + pitch) is a GPU texture; a resolve
//!   reads the touched rectangle back into the software EDRAM and runs the software resolve, so
//!   tiling, formats and clears stay exactly as before; cleared rectangles are uploaded again.
//!
//! The software renderer stays the reference: `lma-cli compare --backend wgpu` checks this one.

use crate::backend::{BackendStats, GuestMem, Image, RenderBackend};
use crate::raster::{self, Decoded, RasterCtx, Renderer, TriSetup, Vtx};
use crate::shader::Shader;
use crate::shader_exec::Vec4;
use crate::fasthash::HashMap;
use std::sync::Arc;

// registers (same as raster.rs)
const RB_SURFACE_INFO: usize = 0x2000;
const RB_COLOR_INFO: usize = 0x2001;
const RB_DEPTH_INFO: usize = 0x2002;
const RB_MODECONTROL: usize = 0x2208;
const RB_COPY_CONTROL: usize = 0x2318;
const RB_COPY_DEST_BASE: usize = 0x2319;
const RB_COPY_DEST_PITCH: usize = 0x231A;
const RB_COPY_DEST_INFO: usize = 0x231B;
const FETCH_BASE: usize = 0x4800;
const FLOAT_BASE: usize = 0x4000;
const BOOL_BASE: usize = 0x4900;
const LOOP_BASE: usize = 0x4908;

/// Every EDRAM surface becomes a texture this big (enough for 1280x720 plus tiling offsets).
const SURFACE: u32 = 2048;
pub use crate::gpu::res_scale;

/// A guest-pixel rectangle (x, y, w, h) on a surface at resolution scale `k`.
fn scaled_rect(x: u32, y: u32, w: u32, h: u32, k: u32) -> [u32; 4] {
    [x * k, y * k, w * k, h * k]
}

/// The fetch constants a setup binds to a resolve kept on the GPU (ids with bit 63, see
/// `ps_setup_textures`), as a bit mask: at resolution scale n those textures have n times the
/// guest's size, which an unnormalized fetch must divide out (`Consts::scaled`). 0 at 1x.
fn scaled_mask(setup: &Setup) -> u32 {
    if res_scale() == 1 {
        return 0;
    }
    setup.1.iter().filter(|&&(k, id, _)| id >> 63 == 1 && k < 32).fold(0, |m, &(k, _, _)| m | 1 << k)
}

/// Flushes whose upload buffers are kept apart (see `WgpuBackend::upload_ring`).
const UPLOAD_RING: usize = 3;
/// Front-buffer read-backs in flight at most (`WgpuBackend::present_pending`), the only bound on
/// how far the GPU runs behind; LMA_PRESENT_QUEUE=<n> overrides. At 3 it drifted 3-4 frames
/// behind, and a fence held for a small resolve (the luminance pass, `threaded::fast_ticket`) then
/// waited for that whole backlog: 60-130 ms frames 20-30 times a minute in a night match (#6).
const PRESENT_QUEUE: usize = 1;
/// Whether resolves skip their read-back when `LMA_SKIP_LAND` is unset (#864): lma-cli sets it
/// before the GPU starts (headless and CI runs read no landed resolve: identical frames and
/// fingerprints on 12 states through a whole match, deterministic wall time halved); lma-app
/// leaves it off until a play build has been tested live (the maintainer, 28 Oct).
pub static SKIP_LAND_DEFAULT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `LMA_SKIP_LAND=<hex dest,...>|all|0`: resolve destinations that keep their GPU copy only, with
/// no read-back into guest memory; "all" skips every one, "0" (or "none") lands every one; unset
/// = [`SKIP_LAND_DEFAULT`].
fn skip_land(dest: u32) -> bool {
    static SKIP: std::sync::OnceLock<(bool, Vec<u32>)> = std::sync::OnceLock::new();
    let (all, list) = SKIP.get_or_init(|| {
        skip_land_list(std::env::var("LMA_SKIP_LAND").ok().as_deref(), SKIP_LAND_DEFAULT.load(std::sync::atomic::Ordering::Relaxed))
    });
    *all || list.contains(&dest)
}

/// `LMA_SKIP_LAND`'s value -> (all, destinations): hex with or without `0x`, spaces allowed;
/// `None` (unset) = `default_all`.
fn skip_land_list(v: Option<&str>, default_all: bool) -> (bool, Vec<u32>) {
    let Some(v) = v else { return (default_all, vec![]) };
    let v = v.trim();
    if v == "0" || v == "none" {
        return (false, vec![]);
    }
    (v == "all", v.split(',').filter_map(|d| u32::from_str_radix(d.trim().trim_start_matches("0x"), 16).ok()).collect())
}

/// Pending resolve read-backs before a resolve waits for them (#746; `LMA_READBACK_CAP`, 0 = no cap).
fn readback_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| std::env::var("LMA_READBACK_CAP").ok().and_then(|v| v.parse().ok()).unwrap_or(32))
}

fn present_queue() -> usize {
    static DEPTH: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *DEPTH.get_or_init(|| std::env::var("LMA_PRESENT_QUEUE").ok().and_then(|v| v.parse().ok()).unwrap_or(PRESENT_QUEUE))
}
/// LMA_PRESENT_JOIN=1: the present waits for the encoder thread after flushing the frame's last
/// batch, before it submits the compose. The batches are submitted from the encoder thread and the
/// compose from the machine thread, so without the wait the compose can reach the GPU first and show
/// the lower EDRAM tile (rows 384..720, drawn and resolved last) from before its draws: the maintainer's video
/// of 13 Oct shows one-frame bands with a straight top edge over the lower pitch (~12 in 29 s of a
/// match, #540), which no frame dump catches (a dump's read-back orders the GPU work itself). #526
/// tried this on 11 Oct and was closed for want of a capture.
fn present_join() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LMA_PRESENT_JOIN").as_deref() == Ok("1"))
}
/// Surface-key bit for 2x multisampled surfaces (EDRAM bases are 12-bit): a base used with and
/// without MSAA gets separate textures.
const MSAA2: u32 = 0x1000;

fn key_samples(key_base: u32) -> u32 {
    if key_base & MSAA2 != 0 { 2 } else { 1 }
}
/// Floats per vertex: clip position + 16 interpolators.
const VERTEX_FLOATS: usize = 4 + 16 * 4;

const PASSTHROUGH_VS: &str = r#"
struct In {
    @location(0) pos: vec4<f32>,
    @location(1) i0: vec4<f32>, @location(2) i1: vec4<f32>, @location(3) i2: vec4<f32>, @location(4) i3: vec4<f32>,
    @location(5) i4: vec4<f32>, @location(6) i5: vec4<f32>, @location(7) i6: vec4<f32>, @location(8) i7: vec4<f32>,
    @location(9) i8: vec4<f32>, @location(10) i9: vec4<f32>, @location(11) i10: vec4<f32>, @location(12) i11: vec4<f32>,
    @location(13) i12: vec4<f32>, @location(14) i13: vec4<f32>, @location(15) i14: vec4<f32>, @location(16) i15: vec4<f32>,
};
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(16) @interpolate(flat) di: u32,
    @location(0) i0: vec4<f32>, @location(1) i1: vec4<f32>, @location(2) i2: vec4<f32>, @location(3) i3: vec4<f32>,
    @location(4) i4: vec4<f32>, @location(5) i5: vec4<f32>, @location(6) i6: vec4<f32>, @location(7) i7: vec4<f32>,
    @location(8) i8: vec4<f32>, @location(9) i9: vec4<f32>, @location(10) i10: vec4<f32>, @location(11) i11: vec4<f32>,
    @location(12) i12: vec4<f32>, @location(13) i13: vec4<f32>, @location(14) i14: vec4<f32>, @location(15) i15: vec4<f32>,
};
@vertex fn main(v: In, @builtin(instance_index) ii: u32) -> Out {
    var o: Out;
    o.pos = v.pos;
    o.di = ii; // this draw's constants record (see shader_wgsl)
    o.i0 = v.i0; o.i1 = v.i1; o.i2 = v.i2; o.i3 = v.i3; o.i4 = v.i4; o.i5 = v.i5; o.i6 = v.i6; o.i7 = v.i7;
    o.i8 = v.i8; o.i9 = v.i9; o.i10 = v.i10; o.i11 = v.i11; o.i12 = v.i12; o.i13 = v.i13; o.i14 = v.i14; o.i15 = v.i15;
    return o;
}
"#;

/// Resolve on the GPU: EDRAM surface rectangle -> tiled, endian-swapped guest texels (k_8_8_8_8
/// colour, or depth as 24_8), as raster::Renderer::resolve writes them. `dst` starts as the
/// current guest bytes, so texels outside the rectangle keep their values.
const RESOLVE_CS: &str = r#"
struct P { x0: u32, y0: u32, x1: u32, y1: u32, dpitch: u32, dheight: u32, pitch_aligned: u32, endian: u32, swap_rb: u32, scale: u32, _b: u32, _c: u32 };
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var src: texture_2d<f32>;
@group(0) @binding(2) var<storage, read_write> dst: array<u32>;
@group(0) @binding(3) var src_depth: texture_depth_2d;
@group(0) @binding(4) var decoded_out: texture_storage_2d<rgba32float, write>;
fn gpu_swap(v: u32, endian: u32) -> u32 {
    switch endian & 3u {
        case 1u: { return ((v >> 8u) & 0x00FF00FFu) | ((v << 8u) & 0xFF00FF00u); }
        case 2u: { return ((v >> 24u) & 0xFFu) | ((v >> 8u) & 0xFF00u) | ((v << 8u) & 0xFF0000u) | (v << 24u); }
        case 3u: { return (v >> 16u) | (v << 16u); }
        default: { return v; }
    }
}
// raster::tiled_2d for 4-byte texels
fn tiled_2d(x: u32, y: u32, pitch_aligned: u32) -> u32 {
    let outer = (((y >> 5u) * (pitch_aligned >> 5u)) + (x >> 5u)) << 6u;
    let inner = (((y >> 1u) & 7u) << 3u) | (x & 7u);
    let oib = (outer | inner) << 2u;
    let bank = (y >> 4u) & 1u;
    let pipe = ((x >> 3u) & 3u) ^ (((y >> 3u) & 1u) << 1u);
    return ((y & 1u) << 4u) | (pipe << 6u) | (bank << 11u) | (oib & 0xFu) | (((oib >> 4u) & 1u) << 5u) | (((oib >> 5u) & 7u) << 8u) | ((oib >> 8u) << 12u);
}
fn dst_index(id: vec3<u32>) -> i32 {
    let x = p.x0 + id.x;
    let y = p.y0 + id.y;
    if (x >= p.x1 || y >= p.y1 || id.x >= p.dpitch || (p.dheight != 0u && id.y >= p.dheight)) { return -1; }
    let i = tiled_2d(id.x, id.y, p.pitch_aligned) / 4u;
    if (i >= arrayLength(&dst)) { return -1; }
    return i32(i);
}
@compute @workgroup_size(8, 8) fn color(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = dst_index(id);
    if (i < 0) { return; }
    // at resolution scale s the surface has s x s texels per guest pixel: guest memory gets the
    // top-left one, the texture kept on the GPU all of them (`res_scale`)
    let s = p.scale;
    let d = textureDimensions(decoded_out);
    for (var sy = 0u; sy < s; sy++) {
        for (var sx = 0u; sx < s; sx++) {
            var c = textureLoad(src, vec2<i32>(i32((p.x0 + id.x) * s + sx), i32((p.y0 + id.y) * s + sy)), 0);
            if (p.swap_rb != 0u) { c = vec4<f32>(c.z, c.y, c.x, c.w); } // copy_dest_swap
            let word = pack4x8unorm(c);
            if (sx == 0u && sy == 0u) { dst[i] = gpu_swap(word, p.endian); }
            // what raster::texel_with decodes from these bytes (k_8_8_8_8: byte / 255 per channel)
            let o = vec2<u32>(id.x * s + sx, id.y * s + sy);
            if (o.x < d.x && o.y < d.y) {
                textureStore(decoded_out, vec2<i32>(o), vec4<f32>(f32(word & 0xFFu), f32((word >> 8u) & 0xFFu), f32((word >> 16u) & 0xFFu), f32(word >> 24u)) / 255.0);
            }
        }
    }
}
@compute @workgroup_size(8, 8) fn depth(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = dst_index(id);
    if (i < 0) { return; }
    let s = p.scale;
    let d = textureDimensions(decoded_out);
    for (var sy = 0u; sy < s; sy++) {
        for (var sx = 0u; sx < s; sx++) {
            let z = textureLoad(src_depth, vec2<i32>(i32((p.x0 + id.x) * s + sx), i32((p.y0 + id.y) * s + sy)), 0);
            let q = min(u32(clamp(z, 0.0, 1.0) * 16777215.0 + 0.5), 16777215u); // 1.0 rounds to 2^24 in f32
            if (sx == 0u && sy == 0u) { dst[i] = gpu_swap(q << 8u, p.endian); }
            // what raster::texel_with decodes from these bytes (k_24_8: depth / 0xFFFFFF in every channel)
            let o = vec2<u32>(id.x * s + sx, id.y * s + sy);
            if (o.x < d.x && o.y < d.y) {
                textureStore(decoded_out, vec2<i32>(o), vec4<f32>(f32(q) / 16777215.0));
            }
        }
    }
}
"#;

/// One mip level from the level above it: 2x2 box filter (`texture_view`).
const MIP_CS: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<FORMAT, write>;
@compute @workgroup_size(8, 8) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let d = textureDimensions(dst);
    if (id.x >= d.x || id.y >= d.y) { return; }
    let m = vec2<i32>(textureDimensions(src)) - 1;
    let p = vec2<i32>(id.xy * 2u);
    let c = textureLoad(src, min(p, m), 0) + textureLoad(src, min(p + vec2<i32>(1, 0), m), 0)
        + textureLoad(src, min(p + vec2<i32>(0, 1), m), 0) + textureLoad(src, min(p + vec2<i32>(1, 1), m), 0);
    textureStore(dst, vec2<i32>(id.xy), c * 0.25);
}
"#;

/// A resolved texture with a texture fetch swizzle applied (raster::apply_tex_swizzle), for
/// fetch constants that read the resolve as e.g. BGRA.
const SWIZZLE_CS: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var dst: texture_storage_2d<rgba32float, write>;
@group(0) @binding(2) var<uniform> swz: vec4<u32>;
fn pick(c: vec4<f32>, s: u32) -> f32 {
    switch s {
        case 0u: { return c.x; }
        case 1u: { return c.y; }
        case 2u: { return c.z; }
        case 3u: { return c.w; }
        case 4u: { return 0.0; }
        default: { return 1.0; }
    }
}
@compute @workgroup_size(8, 8) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let d = textureDimensions(dst);
    if (id.x >= d.x || id.y >= d.y) { return; }
    let c = textureLoad(src, vec2<i32>(i32(id.x), i32(id.y)), 0);
    let s = swz.x;
    textureStore(dst, vec2<i32>(i32(id.x), i32(id.y)), vec4<f32>(pick(c, s & 7u), pick(c, (s >> 3u) & 7u), pick(c, (s >> 6u) & 7u), pick(c, (s >> 9u) & 7u)));
}
"#;

/// RESOLVE_CS reading 2x multisampled surfaces: colour averages the samples (as the console's
/// resolve does), depth takes sample 0. Each pattern must still be in RESOLVE_CS.
fn resolve_cs_msaa() -> String {
    [
        ("var src: texture_2d<f32>;", "var src: texture_multisampled_2d<f32>;"),
        ("var src_depth: texture_depth_2d;", "var src_depth: texture_depth_multisampled_2d;"),
        (
            "var c = textureLoad(src, vec2<i32>(i32((p.x0 + id.x) * s + sx), i32((p.y0 + id.y) * s + sy)), 0);",
            "let q = vec2<i32>(i32((p.x0 + id.x) * s + sx), i32((p.y0 + id.y) * s + sy));\n            var c = (textureLoad(src, q, 0) + textureLoad(src, q, 1)) * 0.5;",
        ),
    ]
    .iter()
    .fold(RESOLVE_CS.to_string(), |src, (from, to)| {
        assert!(src.contains(from), "resolve_cs_msaa: {from:?} is not in RESOLVE_CS");
        src.replace(from, to)
    })
}

/// Present from the GPU: the front buffer's resolves (decoded values, see RESOLVE_CS; the game
/// may resolve it in several parts, each a band of rows) with fetch constant 0's swizzle, packed
/// to RGBA8 exactly as raster::Renderer::present does (truncating, alpha 255), one u32 per pixel.
const PRESENT_CS: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<u32>;
@group(0) @binding(2) var<uniform> p: vec4<u32>; // w, rows in this part, swizzle, first row
fn pick(c: vec4<f32>, s: u32) -> f32 {
    switch s {
        case 0u: { return c.x; }
        case 1u: { return c.y; }
        case 2u: { return c.z; }
        case 3u: { return c.w; }
        case 4u: { return 0.0; }
        default: { return 1.0; }
    }
}
fn byte(v: f32) -> u32 { return u32(clamp(v, 0.0, 1.0) * 255.0); }
@compute @workgroup_size(8, 8) fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= p.x || id.y >= p.y) { return; }
    let c = textureLoad(src, vec2<i32>(i32(id.x), i32(id.y)), 0);
    let s = p.z;
    dst[(p.w + id.y) * p.x + id.x] = byte(pick(c, s & 7u)) | (byte(pick(c, (s >> 3u) & 7u)) << 8u) | (byte(pick(c, (s >> 6u) & 7u)) << 16u) | (255u << 24u);
}
"#;

use crate::raster::UNORM8_FORMATS;

/// Textures whose mip generation shares one command buffer (`generate_mips`, #304).
const MIP_BATCH: u32 = 16;

/// Colour clear: a scissored triangle with the clear colour as a vertex attribute.
const COLOR_FILL: &str = r#"
struct V { @builtin(position) p: vec4<f32>, @location(0) c: vec4<f32> };
@vertex fn vs(@location(0) p: vec4<f32>, @location(1) c: vec4<f32>) -> V { var o: V; o.p = p; o.c = c; return o; }
@fragment fn fs(v: V) -> @location(0) vec4<f32> { return v.c; }
"#;

/// Fragment shader for depth-only passes (no pixel shader bound): writes nothing.
const DEPTH_ONLY_PS: &str = "@fragment fn main() -> @location(0) vec4<f32> { return vec4<f32>(0.0); }";

type PsModule = (wgpu::ShaderModule, Vec<u32>, Arc<Shader>, Option<Vec<u32>>);
type VsModule = (wgpu::ShaderModule, Vec<u32>, Option<Vec<u32>>, u16);

/// A pixel shader's module, its texture fetch constants, the decoded shader and its float
/// constant slots.
fn make_ps(device: &wgpu::Device, code: &[u32]) -> PsModule {
    let shader = Arc::new(Shader::new(code.to_vec()));
    let t = crate::shader_wgsl::translate(&shader, crate::shader_wgsl::Stage::Pixel);
    // an unnormalized fetch divides by the bound texture's size, n times the guest's on a resolve
    // kept on the GPU at resolution scale n: log the shaders that have one (#797)
    if res_scale() > 1 && t.wgsl.contains("textureDimensions(tex") {
        eprintln!("LMA_RES_SCALE: pixel shader {:016x} has an unnormalized texture fetch", fast_hash_u32(code));
    }
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("xenos ps"), source: wgpu::ShaderSource::Wgsl(t.wgsl.into()) });
    (module, t.textures, shader, t.const_slots)
}

/// A vertex shader's module, its vertex fetch constants, its float constant slots and the
/// interpolants it outputs. `max_interp`: Some(n) trims its interface to the interpolants it
/// exports, at most n of them (16-attribute GPUs, `Trim`); None keeps all 16.
fn make_vs(device: &wgpu::Device, code: &[u32], max_interp: Option<u32>) -> VsModule {
    let shader = Shader::new(code.to_vec());
    let mut t = crate::shader_wgsl::translate_with(&shader, crate::shader_wgsl::Stage::Vertex, max_interp.map(|_| 0xFFFF));
    if let Some(n) = max_interp.filter(|&n| t.interp.count_ones() > n) {
        let allow = first_bits(t.interp, n);
        eprintln!("wgpu: a vertex shader exports {} interpolants, the GPU takes {n}: drawn with {allow:#06x} of {:#06x}", t.interp.count_ones(), t.interp);
        t = crate::shader_wgsl::translate_with(&shader, crate::shader_wgsl::Stage::Vertex, Some(allow));
    }
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("xenos vs"), source: wgpu::ShaderSource::Wgsl(t.wgsl.into()) });
    let interp = if max_interp.is_some() { t.interp } else { 0xFFFF };
    (module, t.vertex_consts, t.const_slots, interp)
}

/// The lowest `n` set bits of `mask`.
fn first_bits(mask: u16, n: u32) -> u16 {
    let mut out = 0u16;
    for d in 0..16 {
        if mask & (1 << d) != 0 && out.count_ones() < n {
            out |= 1 << d;
        }
    }
    out
}

/// The passthrough vertex shader with a trimmed interface (`Trim`): the position and `mask`'s
/// interpolants in, the draw index and them out, at `shader_wgsl::interp_location`.
fn passthrough_vs_trimmed(mask: u16) -> String {
    use std::fmt::Write;
    let loc = |d| crate::shader_wgsl::interp_location(mask, d);
    let (mut fin, mut fout, mut copy) = (String::from("@location(0) pos: vec4<f32>"), String::from("@builtin(position) pos: vec4<f32>, @location(0) @interpolate(flat) di: u32"), String::new());
    for d in (0..16).filter(|d| mask & (1 << d) != 0) {
        let _ = write!(fin, ", @location({}) i{d}: vec4<f32>", loc(d));
        let _ = write!(fout, ", @location({}) i{d}: vec4<f32>", loc(d));
        let _ = write!(copy, " o.i{d} = v.i{d};");
    }
    format!("struct In {{ {fin} }};\nstruct Out {{ {fout} }};\n@vertex fn main(v: In, @builtin(instance_index) ii: u32) -> Out {{\n    var o: Out;\n    o.pos = v.pos;\n    o.di = ii;{copy}\n    return o;\n}}\n")
}

/// The render pipeline for `key` with vertex module `vs` (the passthrough one for CPU-transformed
/// vertices) and pixel module `ps` (the depth-only one for none).
fn make_pipeline(device: &wgpu::Device, key: PipelineKey, vs: &wgpu::ShaderModule, ps: &wgpu::ShaderModule, trimmed: Option<u16>) -> wgpu::RenderPipeline {
    // the passthrough's vertex: position, then the 16 interpolants; trimmed (`Trim`), only the
    // position and `trimmed`'s interpolants are attributes
    let attrs: Vec<wgpu::VertexAttribute> = match trimmed {
        None => (0..17).map(|i| wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16 * i as u64, shader_location: i }).collect(),
        Some(mask) => std::iter::once(wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 0 })
            .chain((0..16).filter(|d| mask & (1 << d) != 0).map(|d| wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16 * (1 + d) as u64, shader_location: crate::shader_wgsl::interp_location(mask, d) }))
            .collect(),
    };
    let passthrough = [wgpu::VertexBufferLayout { array_stride: (VERTEX_FLOATS * 4) as u64, step_mode: wgpu::VertexStepMode::Vertex, attributes: &attrs }];
    let blend = blend_state(key.blend);
    let mut mask = wgpu::ColorWrites::empty();
    for (bit, w) in [(1, wgpu::ColorWrites::RED), (2, wgpu::ColorWrites::GREEN), (4, wgpu::ColorWrites::BLUE), (8, wgpu::ColorWrites::ALPHA)] {
        if key.mask & bit != 0 {
            mask |= w;
        }
    }
    let (z_enable, z_write, z_func) = (key.depth & 1 != 0, key.depth & 2 != 0, key.depth >> 2);
    let compare = if !z_enable {
        wgpu::CompareFunction::Always
    } else {
        [
            wgpu::CompareFunction::Never,
            wgpu::CompareFunction::Less,
            wgpu::CompareFunction::Equal,
            wgpu::CompareFunction::LessEqual,
            wgpu::CompareFunction::Greater,
            wgpu::CompareFunction::NotEqual,
            wgpu::CompareFunction::GreaterEqual,
            wgpu::CompareFunction::Always,
        ][z_func as usize & 7]
    };
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("xenos draw"),
        layout: None,
        vertex: wgpu::VertexState {
            module: vs,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            buffers: if key.vs == 0 { &passthrough } else { &[] },
        },
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            // WebGPU judges winding in framebuffer coordinates, y down like the Xenos screen:
            // PA_SU_SC_MODE_CNTL.face (clockwise is front) maps straight across. (Flipped,
            // back-face culling dropped the match pitch: black but for its line markings.)
            front_face: if key.cull & 4 != 0 { wgpu::FrontFace::Cw } else { wgpu::FrontFace::Ccw },
            cull_mode: match key.cull & 3 {
                1 => Some(wgpu::Face::Front),
                2 => Some(wgpu::Face::Back),
                _ => None,
            },
            ..Default::default()
        },
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(z_enable && z_write),
            depth_compare: Some(compare),
            stencil: Default::default(),
            // one bias for both faces (the game sets them alike); see `poly_offset`
            bias: wgpu::DepthBiasState { constant: 0, slope_scale: f32::from_bits(key.bias) / 16.0, clamp: 0.0 },
        }),
        multisample: wgpu::MultisampleState { count: key.samples, ..Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: ps,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState { format: key.format.wgpu(), blend: Some(blend), write_mask: mask })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Pipelines built in earlier sessions, built again ahead of use (#304: a match's first frames
/// spent 40-120 ms each creating shaders and pipelines). `LMA_PIPELINE_CACHE=<file>` lists every
/// shader (its microcode) and pipeline key the backend creates; at the next start a background
/// thread translates and builds them all on the same device, and the render thread picks them
/// up as they finish. Only rendering is affected: the same pipelines, built earlier.
struct Prewarm {
    log: Option<std::io::BufWriter<std::fs::File>>,
    /// Shaders and pipelines already in the file (each is written once).
    shaders: std::collections::HashSet<u64>,
    pipes: std::collections::HashSet<PipelineKey>,
    rx: Option<std::sync::mpsc::Receiver<Warm>>,
}

enum Warm {
    Ps(u64, PsModule),
    Vs(u64, VsModule),
    Pipe(PipelineKey, wgpu::RenderPipeline),
}

/// First line of the cache file: bump when `PipelineKey` or the file's lines change.
const PREWARM_HEADER: &str = "lma-rs wgpu pipelines 1";

impl PipelineKey {
    fn to_line(self) -> String {
        format!("pipe {:x} {:x} {} {} {} {} {} {} {}", self.vs, self.ps, self.cull, self.format as u32, self.blend, self.mask, self.depth, self.bias, self.samples)
    }
    fn from_line(rest: &str) -> Option<Self> {
        let mut it = rest.split(' ');
        let vs = u64::from_str_radix(it.next()?, 16).ok()?;
        let ps = u64::from_str_radix(it.next()?, 16).ok()?;
        let mut n = || it.next()?.parse::<u32>().ok();
        let (cull, format, blend, mask, depth, bias, samples) = (n()?, n()?, n()?, n()?, n()?, n()?, n()?);
        let format = [Fmt::Rgba8, Fmt::Rgba16F].get(format as usize).copied()?;
        Some(PipelineKey { vs, cull, ps, format, blend, mask, depth, bias, samples, imask: 0 })
    }
}

impl Prewarm {
    fn off() -> Self {
        Prewarm { log: None, shaders: Default::default(), pipes: Default::default(), rx: None }
    }

    /// Read the cache file, start building what it lists, and keep it open to add new entries.
    fn start(path: &std::path::Path, device: &wgpu::Device, passthrough: &wgpu::ShaderModule, depth_only: &wgpu::ShaderModule, msaa2: bool) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let (current, ps, vs, pipes) = Self::read(&text);
        let mut w = Prewarm::off();
        w.shaders = ps.iter().chain(&vs).map(|(h, _)| *h).collect();
        w.pipes = pipes.iter().copied().collect();
        let file = if current { std::fs::OpenOptions::new().append(true).open(path) } else { std::fs::write(path, format!("{PREWARM_HEADER}\n")).and_then(|_| std::fs::OpenOptions::new().append(true).open(path)) };
        match file {
            Ok(f) => w.log = Some(std::io::BufWriter::new(f)),
            Err(e) => eprintln!("LMA_PIPELINE_CACHE {}: {e}", path.display()),
        }
        if pipes.is_empty() {
            return w;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (device, passthrough, depth_only) = (device.clone(), passthrough.clone(), depth_only.clone());
        let started = std::thread::Builder::new().name("lma pipeline prewarm".into()).spawn(move || {
            let t0 = std::time::Instant::now();
            let mut psm: HashMap<u64, wgpu::ShaderModule> = HashMap::default();
            let mut vsm: HashMap<u64, wgpu::ShaderModule> = HashMap::default();
            for (h, code) in ps {
                let m = make_ps(&device, &code);
                psm.insert(h, m.0.clone());
                if tx.send(Warm::Ps(h, m)).is_err() {
                    return;
                }
            }
            for (h, code) in vs {
                let m = make_vs(&device, &code, None);
                vsm.insert(h, m.0.clone());
                if tx.send(Warm::Vs(h, m)).is_err() {
                    return;
                }
            }
            let mut built = 0;
            for key in pipes {
                let v = if key.vs == 0 { Some(&passthrough) } else { vsm.get(&key.vs) };
                let p = if key.ps == 0 { Some(&depth_only) } else { psm.get(&key.ps) };
                let (Some(v), Some(p)) = (v, p) else { continue };
                if key.samples > 1 && !msaa2 {
                    continue;
                }
                if tx.send(Warm::Pipe(key, make_pipeline(&device, key, v, p, None))).is_err() {
                    return;
                }
                built += 1;
            }
            eprintln!("pipeline prewarm: {} shaders, {built} pipelines in {:.2} s", psm.len() + vsm.len(), t0.elapsed().as_secs_f64());
        });
        if started.is_ok() {
            w.rx = Some(rx);
        }
        w
    }

    /// The cache file's pixel shaders, vertex shaders and pipeline keys, and whether its header is
    /// this build's (if not, it's started afresh). Lines that don't parse, and shaders whose code
    /// doesn't hash to their key, are skipped.
    #[allow(clippy::type_complexity)]
    fn read(text: &str) -> (bool, Vec<(u64, Vec<u32>)>, Vec<(u64, Vec<u32>)>, Vec<PipelineKey>) {
        let mut lines = text.lines();
        let current = lines.next() == Some(PREWARM_HEADER);
        let (mut ps, mut vs, mut pipes) = (Vec::new(), Vec::new(), Vec::new());
        if current {
            for l in lines {
                let words = |rest: &str| -> Option<(u64, Vec<u32>)> {
                    let mut it = rest.split(' ');
                    let h = u64::from_str_radix(it.next()?, 16).ok()?;
                    let code: Option<Vec<u32>> = it.map(|w| u32::from_str_radix(w, 16).ok()).collect();
                    code.filter(|c| raster::hash(c) == h).map(|c| (h, c))
                };
                if let Some(rest) = l.strip_prefix("ps ") {
                    ps.extend(words(rest));
                } else if let Some(rest) = l.strip_prefix("vs ") {
                    vs.extend(words(rest));
                } else if let Some(rest) = l.strip_prefix("pipe ") {
                    pipes.extend(PipelineKey::from_line(rest));
                }
            }
        }
        (current, ps, vs, pipes)
    }

    fn note_shader(&mut self, stage: &str, h: u64, code: &[u32]) {
        if let Some(log) = self.log.as_mut().filter(|_| self.shaders.insert(h)) {
            use std::io::Write;
            let words: Vec<String> = code.iter().map(|w| format!("{w:x}")).collect();
            let _ = writeln!(log, "{stage} {h:x} {}", words.join(" "));
        }
    }

    fn note_pipeline(&mut self, key: PipelineKey) {
        if let Some(log) = self.log.as_mut().filter(|_| self.pipes.insert(key)) {
            use std::io::Write;
            let _ = writeln!(log, "{}", key.to_line());
            let _ = log.flush();
        }
    }
}

/// Trimmed stage interfaces (#6, phones): every pipeline carries only the interpolants its vertex
/// stage writes (`shader_wgsl::translate_with`), so a GPU with 16 vertex attributes or inter-stage
/// variables (common on phones; the 360's full set is 16 interpolants and the draw index) still
/// takes the GPU backend. Exact (the interpolants left out arrive as zero in the full interface),
/// unless a vertex shader exports more interpolants than `max_interp`: it keeps its lowest ones,
/// with a log line (#752 review).
#[derive(Clone, Copy, Debug)]
struct Trim {
    /// Interpolants a pipeline can carry: the device's inter-stage variables and the passthrough's
    /// vertex attributes, less one (the draw index, the position).
    max_interp: u32,
}

impl Trim {
    fn for_limits(limits: &wgpu::Limits, forced: bool) -> Option<Self> {
        let max_interp = limits.max_inter_stage_shader_variables.min(limits.max_vertex_attributes).saturating_sub(1).min(16);
        (forced || max_interp < 16).then_some(Trim { max_interp })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    /// Translated vertex shader, or 0 for the passthrough shader (CPU-transformed vertices).
    vs: u64,
    /// GPU vertex shader only: cull front (1), cull back (2), clockwise front (4).
    cull: u32,
    ps: u64,
    format: Fmt,
    blend: u32,
    mask: u32,
    depth: u32, // z_enable | z_write << 1 | func << 2
    /// Polygon-offset slope scale (f32 bits, 1/16 pixel units) for GPU vertex shaders; the
    /// constant part goes through `Consts::z_offset`, and CPU-transformed vertices carry both.
    bias: u32,
    /// Sample count of the surfaces (1, or 2 for MSAA).
    samples: u32,
    /// Trimmed interfaces (`Trim`), CPU-transformed vertices: the interpolants any vertex of the
    /// draw carries (non-zero). 0 otherwise (the vertex shader's own mask applies).
    imask: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Fmt {
    Rgba8 = 0,
    Rgba16F = 1,
}

impl Fmt {
    fn wgpu(self) -> wgpu::TextureFormat {
        match self {
            Fmt::Rgba8 => wgpu::TextureFormat::Rgba8Unorm,
            Fmt::Rgba16F => wgpu::TextureFormat::Rgba16Float,
        }
    }
    fn bytes(self) -> u32 {
        match self {
            Fmt::Rgba8 => 4,
            Fmt::Rgba16F => 8,
        }
    }
    /// RB_COLOR_INFO.color_format: 0/1 = 8888 (and gamma); the rest get 16-bit float.
    fn from_color_info(info: u32) -> Self {
        if (info >> 16) & 0xF <= 1 { Fmt::Rgba8 } else { Fmt::Rgba16F }
    }
}

struct Surface {
    color: wgpu::Texture,
    view: wgpu::TextureView,
    format: Fmt,
}

struct DepthSurface {
    tex: wgpu::Texture,
    view: wgpu::TextureView,
}

pub struct WgpuBackend {
    soft: Renderer,
    device: wgpu::Device,
    queue: wgpu::Queue,
    float_filterable: bool,
    /// Mip-generation pipelines by texture format (`generate_mips`).
    mip_pipes: HashMap<wgpu::TextureFormat, wgpu::ComputePipeline>,
    /// Mip generation for textures created since the last submit, all in one encoder
    /// (`generate_mips`, submitted by `submit_mips` ahead of anything that may sample them, #304).
    mip_enc: Option<wgpu::CommandEncoder>,
    /// Textures recorded in `mip_enc`: submitted every `MIP_BATCH`, so one encoder never holds a
    /// whole kickoff's worth of passes.
    mip_count: u32,
    adapter_name: String,
    surfaces: HashMap<(u32, u32), Surface>,     // (colour base tile, pitch)
    depths: HashMap<(u32, u32), DepthSurface>,  // (depth base tile, pitch)
    vs: wgpu::ShaderModule,
    ps_modules: HashMap<u64, PsModule>, // hash -> (module, texture consts, decoded, float const slots)
    vs_modules: HashMap<u64, VsModule>, // hash -> (module, vertex fetch consts, float const slots, interpolants)
    /// Trimmed stage interfaces (#6, phones): Some with the device's limits when it takes fewer
    /// than 17 vertex attributes or inter-stage variables (or `LMA_INTERP_TRIM=1`).
    trim: Option<Trim>,
    /// `trim`: pixel shaders by (hash, interpolant mask), passthrough shaders by mask.
    ps_trimmed: HashMap<(u64, u16), wgpu::ShaderModule>,
    pass_trimmed: HashMap<u16, wgpu::ShaderModule>,
    /// Shaders and pipelines of earlier sessions, built ahead of use (`LMA_PIPELINE_CACHE`).
    prewarm: Prewarm,
    /// Landings are tracked by the write watch rather than hashed (`land`), until guest memory
    /// turns out to have none (`land_watch_default`; LMA_LAND_HASH=1 hashes, =0 watches).
    land_watch: bool,
    /// Per stage (vertex, pixel): a float constant was written since the previous draw, from
    /// `regs_dirty`; None when nothing said (every draw then compares its constants).
    floats_written: Option<[bool; 2]>,
    /// Each draw's indices, decoded into one buffer kept across draws (`draw`).
    index_scratch: Vec<u32>,
    /// Strips and fans converted to a triangle list (`draw_gpu_vs`), kept across draws.
    tri_scratch: Vec<u32>,
    depth_only_ps: wgpu::ShaderModule,
    depth_fill: wgpu::RenderPipeline,
    /// The same for 2x multisampled depth surfaces.
    depth_fill2: wgpu::RenderPipeline,
    /// 2x MSAA surfaces are supported (and LMA_MSAA isn't 0).
    msaa2: bool,
    /// Resolve pipelines reading 2x multisampled surfaces.
    resolve_color2: wgpu::ComputePipeline,
    resolve_depth2: wgpu::ComputePipeline,
    depth_upload: wgpu::RenderPipeline,
    pipelines: HashMap<PipelineKey, wgpu::RenderPipeline>,
    textures: HashMap<usize, (Arc<Decoded>, wgpu::TextureView)>,
    samplers: HashMap<(u32, u32, bool), wgpu::Sampler>, // (clamp u, clamp v, bilinear)
    stats: BackendStats,
    /// Draws recorded since the last flush, all into the same colour + depth surfaces.
    batch: Batch,
    /// Upload buffers kept across flushes, one per flush holding all six parts (vertices,
    /// indices, consts, fetch, vertex memory, float constants), in `UPLOAD_RING` slots used in
    /// turn: creating and freeing six buffers per flush was ~6% of the render thread in a match,
    /// and a staging buffer per part a quarter of the writes (#6).
    uploads: Uploads,
    /// Batches encoded and submitted on their own thread (`Encoder`; off with LMA_RT_ENCODER=0).
    encoder: Option<Encoder>,
    /// Reused buffer for `alias_valid`'s streamed full-hash check.
    hash_chunk: Vec<u8>,
    /// Run vertex shaders on the GPU (default; LMA_GPU_VS=0 transforms on the CPU instead).
    gpu_vs: bool,
    /// Resolve on the GPU (default; LMA_GPU_RESOLVE=0 copies through the software EDRAM).
    gpu_resolve: bool,
    resolve_color: wgpu::ComputePipeline,
    resolve_depth: wgpu::ComputePipeline,
    color_fill: HashMap<(Fmt, u32), wgpu::RenderPipeline>,
    color_fill_module: wgpu::ShaderModule,
    /// Resolve readbacks submitted but not yet written to guest memory, oldest first.
    pending: Vec<Readback>,
    next_readback: u64,
    /// `ps_setup` results of this (run, tex_gen), by pixel shader and its fetch constants.
    #[allow(clippy::type_complexity)]
    /// Pixel-shader setups by (shader, fetch constants), with the (run, tex_gen) they were built
    /// or last checked at, and `next_gen` when built (a resolve with a newer gen replaced one).
    setup_cache: HashMap<Vec<u32>, CachedSetup>,
    /// `tex_override::generation()` the setups were built under: a new override set (the pad
    /// glyphs following the device) drops them all, since the guest bytes they re-validate
    /// against haven't changed (#694: glyphs swapped one by one, at random times).
    setup_override_gen: u64,
    /// The last vertex and pixel shader code seen and its hash (see `vs_module`).
    last_vs: (Vec<u32>, u64),
    last_ps: (Vec<u32>, u64),
    /// Scratch for the setup cache key (no allocation per draw).
    setup_key: Vec<u32>,
    /// The previous draw's setup: (pixel shader, generation, setup) and its key.
    last_setup: Option<(u64, (u64, u64), Setup)>,
    last_key: Vec<u32>,
    setup_gen: (u64, u64),
    /// Bumped whenever a resolve is made or lands (texture sources may have changed).
    tex_gen: u64,
    /// LMA_GPU_TIMING=1 (see `GpuTimer`).
    timer: std::cell::RefCell<Option<GpuTimer>>,
    /// Colour resolves of this command-processor run, by guest physical destination: the
    /// texels as the texture decoder would read them back, so draws can sample them directly.
    resolved: HashMap<u32, Resolved>,
    /// LMA_RESOLVE_USE=1 census: resolves made this frame and last frame (dest, content), and the
    /// contents a draw sampled since.
    res_made: Vec<(u32, u64)>,
    res_prev: Vec<(u32, u64)>,
    res_sampled: std::collections::HashSet<u64>,
    swizzle_pipe: wgpu::ComputePipeline,
    /// Asynchronous fences (set by the Gpu, see Gpu::fence_store).
    async_fence: bool,
    present_pipe: wgpu::ComputePipeline,
    /// Real time: front-buffer read-backs still on their way (buffer, mapped flag, w, h), oldest
    /// first, at most `present_queue()`. A present shows the newest one that has landed instead of
    /// waiting for the GPU (#6: waiting was a quarter of the render thread in a match); one
    /// still on its way is shown late, never dropped.
    present_pending: std::collections::VecDeque<(wgpu::Buffer, Arc<std::sync::atomic::AtomicBool>, u32, u32, wgpu::Buffer)>,
    /// Present buffers (bytes, composed frame, read-back) kept for the next frames: a fresh pair
    /// of frame-sized buffers per present (host-visible memory allocated, mapped and zeroed) was
    /// most of the present's cost on the render thread (#304).
    present_bufs: Vec<(u64, wgpu::Buffer, wgpu::Buffer)>,
    /// Resolve read-back buffers (MAP_READ) given back after landing, unmapped, for later resolves
    /// of the same size (#746: one new buffer per resolve churned GBs of GPU memory a minute).
    readback_bufs: Vec<wgpu::Buffer>,
    /// k_8_8_8_8 colour resolves since the last present, by guest physical destination (kept
    /// apart from `resolved`, whose entries end with each command-processor run): the front
    /// buffer is presented from here when it is one of them.
    /// This frame's colour resolves by destination: (view, endian, pitch aligned, w, h, order). The
    /// order (`next_gen` at the resolve) lets the present compose them as memory would hold them:
    /// a later resolve over the same rows wins, whatever its start row (#540).
    frame_resolves: HashMap<u32, (wgpu::TextureView, u32, u32, u32, u32, u64)>,
    /// Command-processor runs so far (guest code may change memory between two).
    run: u64,
    /// Frames presented so far.
    frame: u64,
    /// Textures put together from several resolves on the GPU (`composed_view`), by base address.
    composed: HashMap<u32, Composed>,
    /// Generation stamp of the next entry in `resolved`.
    next_gen: u64,
}

/// A texture copied together from resolves that cover it: the bands of a target resolved in
/// several parts, or the top-left corner of a slightly larger resolve.
struct Composed {
    /// (width, height, format, endian, pitch aligned) of the texture.
    desc: (u32, u32, u32, u32, u32),
    /// (address, generation) of every resolve copied in: valid while all of them still are.
    parts: Vec<(u32, u64)>,
    view: wgpu::TextureView,
    /// Copies with a non-identity fetch swizzle applied, by swizzle.
    swizzled: HashMap<u32, wgpu::TextureView>,
}

/// A k_8_8_8_8 colour resolve kept on the GPU (Rgba32Float, exactly the decoded values).
struct Resolved {
    /// Texture format a fetch must use to read it: 6 (k_8_8_8_8, colour) or 22 (k_24_8, depth).
    format: u32,
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    /// The GPU texture's identity: a new value when a texture was created, kept when a resolve of
    /// the same size reuses the previous one in place (its identity view stays valid, so setups
    /// sampling it need no refresh and their bind groups can live on).
    gen: u64,
    /// Unique per resolve: the texels changed. Swizzled copies and composed textures made from
    /// this resolve are stale once it moves on.
    content: u64,
    /// The guest frame of the resolve that made or last reused the texture.
    made_frame: u64,
    /// Copies with a non-identity fetch swizzle applied, by swizzle.
    swizzled: HashMap<u32, wgpu::TextureView>,
    endian: u32,
    pitch_aligned: u32,
    w: u32,
    h: u32,
    /// Once its readback has landed: (bytes, hash of them, hash of `sample` of them). Guest code
    /// may change that memory afterwards, so the copy stays usable only while they match.
    landed: Option<(u32, u64, u64)>,
    /// Where guest memory has a write watch: the newest page stamp of its bytes right after they
    /// landed (`Renderer::range_stamp`). Valid while no page there gets a newer one: no hashes.
    land_stamp: Option<u32>,
    /// Command-processor run in which memory was last checked (no guest code runs within one).
    checked_run: u64,
    /// Frame in which it was last checked in full (other runs check `sample`).
    checked_frame: u64,
}

/// The bytes of a `len`-byte resolve that runs within a frame compare (`alias_valid`): the first
/// 64 of every KB, as (offset, length) pairs.
fn sample(len: usize, mut f: impl FnMut(usize, usize)) {
    for off in (0..len).step_by(1024) {
        f(off, 64.min(len - off));
    }
}

/// A resolve's copy on its way back: `len` bytes for guest physical address `dest`.
struct Readback {
    /// Submission order, for asynchronous fence tickets.
    id: u64,
    /// Set by map_async's callback once the copy can be read.
    ready: Arc<std::sync::atomic::AtomicBool>,
    read: wgpu::Buffer,
    /// The bytes on the GPU (what `read` copies), for draws that fetch vertices from them.
    src: wgpu::Buffer,
    dest: u32,
    len: usize,
}

/// A draw's textures: the (fetch constant, view, sampler) bindings, and their identities for
/// bind-group reuse: (fetch const, texture id, clamp). Shared by every draw with the same set
/// (cloning the vectors per draw was most of the render thread's allocations).
type Setup = Arc<(Vec<(u32, wgpu::TextureView, wgpu::Sampler)>, Vec<(u32, u64, u32)>)>;

/// A cached setup's revalidation (`WgpuBackend::setup_still_valid`).
enum Check {
    Valid,
    /// Valid with these slots' views replaced (their render targets were drawn again).
    Refresh(Vec<(u32, wgpu::TextureView)>),
    Invalid,
}

/// A cached pixel-shader setup (`WgpuBackend::setup_cache`).
struct CachedSetup {
    setup: Setup,
    /// The (run, tex_gen) it was built or last checked at.
    gen: (u64, u64),
    /// `next_gen` when built (a resolve with a newer gen replaced one).
    built: u64,
    /// Its decoded textures' guest ranges (lo, hi, base) and the newest write-watch page stamp
    /// over them when built: unchanged stamps and no resolve at a base mean the decodes are
    /// still current, without `prepare_textures` (`setup_fast_valid`). None without a write
    /// watch, or when the setup samples a resolve (those take the full check).
    watch: Option<SetupWatch>,
}

/// What a cached setup depends on, for `setup_fast_valid`: its decoded textures' guest ranges
/// (lo, hi, base) with the newest write-watch page stamp over them when built, and the resolves
/// it aliases with what each slot's view followed then (`alias_follows`) and the slot's id.
#[derive(Clone)]
struct SetupWatch {
    ranges: Vec<(u32, u32, u32)>,
    stamp: u32,
    aliases: Vec<(u32, u64, u64)>,
}

/// A `Setup` compared and hashed by its texture identities only (views can't be compared).
struct SetupIds(Setup);

impl PartialEq for SetupIds {
    fn eq(&self, o: &Self) -> bool {
        Arc::ptr_eq(&self.0, &o.0) || self.0 .1 == o.0 .1
    }
}

impl Eq for SetupIds {}

impl std::hash::Hash for SetupIds {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.0 .1.hash(h)
    }
}

/// One recorded draw: everything needed to replay it inside a shared render pass.
struct Rec {
    key: PipelineKey,
    setup: Setup,
    /// This draw's record in `Batch::consts` (the instance index it is drawn with); None when no
    /// shader reads constants.
    di: Option<u32>,
    /// The vertex shader fetches vertices: bind `fetch_arr` and `vmem`.
    fetches: bool,
    /// A shader reads float constants: bind `fvals`.
    uses_f: bool,
    /// Indices into `Batch::indices` (GPU vertex shader) rather than vertices in `Batch::verts`.
    indexed: bool,
    first: u32,
    count: u32,
    scissor: [u32; 4],
    blend: [f32; 4],
}

/// Draws batched into one command buffer and one render pass, so a frame costs a handful of
/// submits instead of one per draw. Buffers are uploaded once per flush.
#[derive(Default)]
struct Batch {
    target: Option<((u32, u32), (u32, u32))>, // (colour key, depth key)
    draws: Vec<Rec>,
    verts: Vec<u8>,
    indices: Vec<u32>,
    /// One `Consts` record per distinct set of draw constants (shader_wgsl's struct).
    consts: Vec<u8>,
    consts_by_hash: HashMap<u64, u32>,
    /// Vertex fetch constants, 192 words per distinct set.
    fetch: Vec<u32>,
    fetch_by_hash: HashMap<u64, u32>,
    /// Float constants (vec4 = 4 words), each draw's vertex and pixel shader runs interned.
    fvals: Vec<u32>,
    fvals_by_hash: HashMap<u64, (u32, u32)>, // hash -> (offset, words)
    /// Scratch for gathering a shader's float constants (no allocation per draw).
    scratch: Vec<u32>,
    /// Per stage (vertex, pixel): the previous draw's interned float constants as (slot list
    /// identity, word offset, words). Consecutive draws mostly repeat them: one compare
    /// instead of hashing a stage's 1024 words (a tenth of the render thread).
    fvals_last: [Option<(usize, u32, u32)>; 2],
    /// Per stage: the previous draw's whole constant block (relative addressing) as (chunk table
    /// offset, the 16 chunk offsets), all in vec4 units.
    fbank_last: [Option<(u32, [u32; 16])>; 2],
    /// Per stage: no float constant written since the previous draw (`WgpuBackend::regs_dirty`),
    /// so its `fvals_last`/`fbank_last` still hold this draw's constants: no gathering, no compare.
    floats_clean: [bool; 2],
    /// Vertex memory windows (words); the newest is reused when the same bytes come again.
    vmem: Vec<u32>,
    /// Vertex memory windows already in `vmem`: (guest base, bytes) -> (word offset, sample hash).
    vmem_windows: HashMap<(u32, u32), (u32, u64)>,
    /// Index runs already in `indices`, by source (index base, VGT_DRAW_INITIATOR, primitive):
    /// (first, count). Most of a match's indices repeat an earlier draw's in the batch (the same
    /// mesh drawn again), so a run is reused when its words are equal.
    idx_by_src: HashMap<(u32, u32, u32), (u32, u32)>,
    /// Vertex memory still on its way back from a resolve: copied on the GPU into the uploaded
    /// vertex memory at flush, (source, source byte offset, vmem byte offset, bytes).
    vmem_patches: Vec<(wgpu::Buffer, u64, u64, u64)>,
}

impl Batch {
    /// An empty batch that keeps this one's allocations (regrowing them every flush was a tenth
    /// of the render thread in a match).
    fn recycle(mut self) -> Batch {
        self.target = None;
        self.draws.clear();
        self.verts.clear();
        self.indices.clear();
        self.consts.clear();
        self.consts_by_hash.clear();
        self.fetch.clear();
        self.fetch_by_hash.clear();
        self.fvals.clear();
        self.fvals_by_hash.clear();
        self.fvals_last = [None; 2];
        self.fbank_last = [None; 2];
        self.floats_clean = [false; 2];
        self.vmem.clear();
        self.vmem_windows.clear();
        self.vmem_patches.clear();
        self.idx_by_src.clear();
        self
    }

    /// Index of `rec` in `pool` (records of `rec.len()` items), appending it if it is new.
    fn intern<T: Copy + PartialEq>(pool: &mut Vec<T>, by_hash: &mut HashMap<u64, u32>, hash: u64, rec: &[T]) -> u32 {
        let n = rec.len();
        if let Some(&i) = by_hash.get(&hash) {
            if pool[i as usize * n..(i as usize + 1) * n] == *rec {
                return i;
            }
        }
        let i = (pool.len() / n) as u32;
        pool.extend_from_slice(rec);
        by_hash.entry(hash).or_insert(i);
        i
    }

    /// The float constants a shader reads (`slots`, or its stage's 256 from `base` when None),
    /// interned; returns their offset in vec4 units (for a whole block, its chunk table's).
    fn put_floats(&mut self, regs: &[u32], base: usize, slots: Option<&[u32]>) -> u32 {
        let stage = (base != 0) as usize;
        let Some(list) = slots else { return self.put_float_block(regs, base) };
        let id = list.as_ptr() as usize ^ list.len();
        if let (true, Some((last_id, off, _))) = (self.floats_clean[stage], self.fvals_last[stage]) {
            if last_id == id {
                return off / 4;
            }
        }
        self.scratch.clear();
        for &n in list {
            self.scratch.extend_from_slice(&regs[FLOAT_BASE + n as usize * 4..FLOAT_BASE + n as usize * 4 + 4]);
        }
        if self.scratch.is_empty() {
            return 0;
        }
        let words = std::mem::take(&mut self.scratch);
        let off = match self.fvals_last[stage] {
            Some((last_id, off, len)) if last_id == id && len as usize == words.len() && self.fvals[off as usize..(off + len) as usize] == *words => off / 4,
            _ => self.intern_floats(&words),
        };
        self.fvals_last[stage] = Some((id, off * 4, words.len() as u32));
        self.scratch = words;
        off
    }

    /// A stage's whole 256-constant block (shaders that address constants relatively) in 16
    /// chunks of 16 interned separately, behind a table of their offsets (`fb` in the shader):
    /// a match's skinned draws change a few constants each, and uploading every draw's whole
    /// 4 KB block was ~7 MB a frame. Returns the table's offset.
    fn put_float_block(&mut self, regs: &[u32], base: usize) -> u32 {
        let stage = (base != 0) as usize;
        let last = self.fbank_last[stage];
        if let (true, Some((t, _))) = (self.floats_clean[stage], last) {
            return t;
        }
        let block = &regs[FLOAT_BASE + base * 4..FLOAT_BASE + (base + 256) * 4];
        let mut table = [0u32; 16];
        let mut same = last.is_some();
        for (k, chunk) in block.chunks_exact(64).enumerate() {
            // mostly the previous draw's chunk: one compare, no hash
            if let Some((_, prev)) = last {
                let o = prev[k] as usize * 4;
                if self.fvals[o..o + 64] == *chunk {
                    table[k] = prev[k];
                    continue;
                }
            }
            same = false;
            table[k] = self.intern_floats(chunk);
        }
        if let (true, Some((t, _))) = (same, last) {
            return t;
        }
        let t = self.intern_floats(&table.map(|o| (o as f32).to_bits()));
        self.fbank_last[stage] = Some((t, table));
        t
    }

    /// `words` (whole vec4s) in `fvals`, shared with an equal earlier run; its offset in vec4s.
    fn intern_floats(&mut self, words: &[u32]) -> u32 {
        crate::prof::count(crate::prof::Cat::FloatsLookups);
        let hash = fast_hash_u32(words);
        if let Some(&(off, len)) = self.fvals_by_hash.get(&hash) {
            if len as usize == words.len() && self.fvals[off as usize..(off + len) as usize] == *words {
                return off / 4;
            }
        }
        crate::prof::count(crate::prof::Cat::FloatsNew);
        let off = self.fvals.len() as u32;
        self.fvals.extend_from_slice(words);
        self.fvals_by_hash.entry(hash).or_insert((off, words.len() as u32));
        off / 4
    }
}

/// LMA_GPU_TIMING=1: GPU time per pass category, from timestamp queries around every pass,
/// read back once a frame (blocking, so only for profiling) and logged every 60 frames.
const GPU_CATS: [&str; 7] = ["draws", "resolve", "resolve clear", "depth clear", "depth upload", "present", "other"];
const GPU_QUERIES: u32 = 4096;

struct GpuTimer {
    set: wgpu::QuerySet,
    buf: wgpu::Buffer,
    read: wgpu::Buffer,
    /// Category of each pair of queries written this frame.
    cats: Vec<usize>,
    period_ns: f64,
    sums: [f64; 7],
    counts: [u64; 7],
    frames: u64,
}

impl GpuTimer {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Self {
        let size = GPU_QUERIES as u64 * 8;
        Self {
            set: device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("gpu timing"), ty: wgpu::QueryType::Timestamp, count: GPU_QUERIES }),
            buf: device.create_buffer(&wgpu::BufferDescriptor { label: Some("gpu timing"), size, usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false }),
            read: device.create_buffer(&wgpu::BufferDescriptor { label: Some("gpu timing read"), size, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
            cats: Vec::new(),
            period_ns: queue.get_timestamp_period() as f64,
            sums: [0.0; 7],
            counts: [0; 7],
            frames: 0,
        }
    }

    /// Two queries for a pass of category `cat` (None once this frame's are used up).
    fn take(&mut self, cat: usize) -> Option<(wgpu::QuerySet, u32)> {
        let i = self.cats.len() as u32 * 2;
        (i + 2 <= GPU_QUERIES).then(|| {
            self.cats.push(cat);
            (self.set.clone(), i)
        })
    }

    fn end_frame(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let n = self.cats.len() as u32 * 2;
        if n == 0 {
            return;
        }
        let mut enc = device.create_command_encoder(&Default::default());
        enc.resolve_query_set(&self.set, 0..n, &self.buf, 0);
        enc.copy_buffer_to_buffer(&self.buf, 0, &self.read, 0, n as u64 * 8);
        queue.submit([enc.finish()]);
        self.read.slice(..n as u64 * 8).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        {
            let data = self.read.slice(..n as u64 * 8).get_mapped_range();
            let t: Vec<u64> = data.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect();
            for (k, &cat) in self.cats.iter().enumerate() {
                let (a, b) = (t[2 * k], t[2 * k + 1]);
                let ns = b.wrapping_sub(a) as f64 * self.period_ns;
                if b >= a && ns < 1e9 {
                    self.sums[cat] += ns;
                    self.counts[cat] += 1;
                }
            }
        }
        self.read.unmap();
        self.cats.clear();
        self.frames += 1;
        if self.frames == 60 {
            let total: f64 = self.sums.iter().sum::<f64>() / 60.0 / 1e6;
            let parts: Vec<String> = (0..7)
                .filter(|&c| self.counts[c] > 0)
                .map(|c| format!("{} {:.2} ms ({:.0} passes)", GPU_CATS[c], self.sums[c] / 60.0 / 1e6, self.counts[c] as f64 / 60.0))
                .collect();
            eprintln!("gpu timing per frame: total {total:.2} ms; {}", parts.join(", "));
            (self.sums, self.counts, self.frames) = ([0.0; 7], [0; 7], 0);
        }
    }
}

/// Size of shader_wgsl's `Consts` struct (also its array stride).
const CONSTS_SIZE: usize = 384;

use crate::gpu::{words, words_mut};

/// LMA_SETUP_LOG: why cached setups were judged invalid (pending overlap, composed, no resolve, alias stale, layout, decoded-now-resolved, textures changed)
static RESOLVE_REUSE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
static FAST_REASON: [std::sync::atomic::AtomicU64; 7] = [const { std::sync::atomic::AtomicU64::new(0) }; 7];
static INVALID_REASON: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// Presented frames' pixel buffers, reused between the renderer and the window (#304: a fresh
/// 3.7 MB `to_vec` per frame and the window dropping the previous one cost ~2,000 page faults a
/// frame on the M4 Max, ~110k a second). `take_frame_buf` hands out a cleared buffer, the window
/// gives it back with `give_frame_buf` once it has copied the pixels into its own image.
static FRAME_BUFS: std::sync::Mutex<Vec<Vec<u8>>> = std::sync::Mutex::new(Vec::new());
/// LMA_SHARED_GPU=1 (#6): presented frames stay on the GPU. lma-app turns this on when the window
/// shares the renderer's device (`create_context`); `present_gpu` then publishes each composed
/// frame's buffer (`GPU_FRAME`) instead of reading it back, and the window copies it into its
/// screen texture on the same queue, after the compose.
pub static SHARED_PRESENT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Read the next presents back after all (lma-app, while a script screenshot waits for one).
pub static READBACK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The newest composed frame on the GPU: (w x h RGBA8 rows, w, h), until the window takes it.
pub static GPU_FRAME: std::sync::Mutex<Option<(wgpu::Buffer, u32, u32)>> = std::sync::Mutex::new(None);
/// Composed-frame buffers the window has copied from (or that were superseded), for reuse.
static GPU_FRAME_BUFS: std::sync::Mutex<Vec<wgpu::Buffer>> = std::sync::Mutex::new(Vec::new());

/// A composed-frame buffer back from the window: its copy is submitted, and the next compose
/// into it is queued after that copy.
pub fn give_gpu_frame_buf(b: wgpu::Buffer) {
    let mut bufs = GPU_FRAME_BUFS.lock().unwrap();
    if bufs.len() < 4 {
        bufs.push(b);
    }
}

fn take_gpu_frame_buf(device: &wgpu::Device, len: u64) -> wgpu::Buffer {
    let mut bufs = GPU_FRAME_BUFS.lock().unwrap();
    match bufs.iter().position(|b| b.size() == len) {
        Some(i) => bufs.swap_remove(i),
        None => device.create_buffer(&wgpu::BufferDescriptor { label: Some("present"), size: len, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false }),
    }
}

pub fn take_frame_buf() -> Vec<u8> {
    FRAME_BUFS.lock().map(|mut p| p.pop()).ok().flatten().unwrap_or_default()
}
pub fn give_frame_buf(mut v: Vec<u8>) {
    v.clear();
    if let Ok(mut p) = FRAME_BUFS.lock() {
        if p.len() < 4 {
            p.push(v);
        }
    }
}


/// Hash of 64 bytes of every 4 KB of guest memory [addr, addr + len), and its last 64.
/// A flush's upload buffers, one per slot used in turn (see `encode_batch`).
struct Uploads {
    ring: Vec<Option<wgpu::Buffer>>,
    slot: usize,
}

/// Encode and submit one batch of draws into `color`/`depth` (`WgpuBackend::flush`), on the
/// render thread or the encoder thread (LMA_RT_ENCODER); returns the batch emptied, for reuse.
#[allow(clippy::too_many_arguments)]
fn encode_batch(device: &wgpu::Device, queue: &wgpu::Queue, up: &mut Uploads, batch: Batch, pipelines: &HashMap<PipelineKey, wgpu::RenderPipeline>, color_view: &wgpu::TextureView, depth_view: &wgpu::TextureView, tw: Option<(wgpu::QuerySet, u32)>) -> Batch {
        // this flush's slot of the upload ring: a buffer is reused when big enough (the GPU has
        // long finished the flush that used this slot before)
        let slot = up.slot;
        up.slot = (slot + 1) % UPLOAD_RING;
        // the parts at offsets aligned for storage bindings (256 is the largest alignment a device
        // may require), written through one staging buffer
        let parts: [&[u8]; 6] = [
            &batch.verts,
            words(&batch.indices),
            &batch.consts,
            words(if batch.fetch.is_empty() { &[0; 192] } else { &batch.fetch }),
            words(if batch.vmem.is_empty() { &[0] } else { &batch.vmem }),
            words(if batch.fvals.is_empty() { &[0; 4] } else { &batch.fvals }),
        ];
        let mut at = [(0u64, 0u64); 6]; // (offset, bytes rounded up to 4)
        let mut total = 0;
        for (a, p) in at.iter_mut().zip(parts) {
            *a = (total, (p.len() as u64).next_multiple_of(4));
            total += (p.len() as u64).next_multiple_of(256);
        }
        let buf = match up.ring[slot].take() {
            Some(b) if b.size() >= total => b,
            _ => device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("batch uploads"),
                size: total.next_power_of_two().max(1 << 16),
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::INDEX | wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }),
        };
        if let Some(mut view) = queue.write_buffer_with(&buf, 0, wgpu::BufferSize::new(total).unwrap()) {
            for (&(off, _), p) in at.iter().zip(parts) {
                view.slice(off as usize..off as usize + p.len()).copy_from_slice(p);
                crate::prof::add(crate::prof::Cat::UploadBytes, p.len() as u64);
            }
        }
        up.ring[slot] = Some(buf.clone());
        let [vbuf, ibuf, cbuf, fbuf, mbuf, vbuf_f] = at.map(|(off, len)| (len > 0).then(|| (buf.clone(), off, len)));
        // a binding of the used part of the upload buffer
        fn part(b: &(wgpu::Buffer, u64, u64)) -> wgpu::BindingResource<'_> {
            wgpu::BindingResource::Buffer(wgpu::BufferBinding { buffer: &b.0, offset: b.1, size: wgpu::BufferSize::new(b.2) })
        }
        // one bind group per (pipeline, textures) for the whole batch: draws differ only in the
        // instance index that selects their constants record
        // LMA_ENCODE_LOG=1 (debug, #6): where encode_batch's time goes, every 300th batch
        static ENC_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        static ENC_N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let enc_log = *ENC_LOG.get_or_init(|| std::env::var_os("LMA_ENCODE_LOG").is_some());
        let t_groups = std::time::Instant::now();
        let mut groups: HashMap<(PipelineKey, SetupIds), usize> = HashMap::default();
        let mut bind_groups: Vec<wgpu::BindGroup> = Vec::new();
        let mut draw_group: Vec<Option<usize>> = Vec::with_capacity(batch.draws.len());
        for r in &batch.draws {
            if r.di.is_none() {
                draw_group.push(None);
                continue;
            }
            let (Some(cbuf), Some(fbuf), Some(mbuf), Some(vbuf_f)) = (&cbuf, &fbuf, &mbuf, &vbuf_f) else {
                draw_group.push(None);
                continue;
            };
            let g = *groups.entry((r.key, SetupIds(r.setup.clone()))).or_insert_with(|| {
                let mut entries = vec![wgpu::BindGroupEntry { binding: 0, resource: part(cbuf) }];
                if r.fetches {
                    entries.push(wgpu::BindGroupEntry { binding: 1, resource: part(fbuf) });
                    entries.push(wgpu::BindGroupEntry { binding: 2, resource: part(mbuf) });
                }
                if r.uses_f {
                    entries.push(wgpu::BindGroupEntry { binding: 3, resource: part(vbuf_f) });
                }
                for (k, view, sampler) in &r.setup.0 {
                    entries.push(wgpu::BindGroupEntry { binding: 16 + 2 * k, resource: wgpu::BindingResource::TextureView(view) });
                    entries.push(wgpu::BindGroupEntry { binding: 17 + 2 * k, resource: wgpu::BindingResource::Sampler(sampler) });
                }
                // every translated shader reads its constants record (the PS for the alpha test,
                // the VS for the viewport), so the auto layout has binding 0, plus fetch/vmem when
                // the VS fetches vertices and one texture/sampler pair per PS fetch constant
                crate::prof::count(crate::prof::Cat::BindGroups);
                let layout = pipelines[&r.key].get_bind_group_layout(0);
                bind_groups.push(device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &layout, entries: &entries }));
                bind_groups.len() - 1
            });
            draw_group.push(Some(g));
        }
        let groups_ms = t_groups.elapsed().as_secs_f64() * 1e3;
        let t_pass = std::time::Instant::now();
        let mut enc = device.create_command_encoder(&Default::default());
        if let Some((m, base, _)) = &mbuf {
            for (src, so, vo, n) in &batch.vmem_patches {
                enc.copy_buffer_to_buffer(src, *so, m, base + *vo, *n);
            }
        }
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("draws"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: color_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::RenderPassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some((v, off, len)) = &vbuf {
                pass.set_vertex_buffer(0, v.slice(*off..off + len));
            }
            if let Some((i, off, len)) = &ibuf {
                pass.set_index_buffer(i.slice(*off..off + len), wgpu::IndexFormat::Uint32);
            }
            let (mut last_key, mut last_group) = (None, None);
            // scissor and blend constant only when they change (each is a recorded command)
            let (mut last_scissor, mut last_blend) = (None, None);
            for (r, g) in batch.draws.iter().zip(&draw_group) {
                if last_key != Some(r.key) {
                    pass.set_pipeline(&pipelines[&r.key]);
                    last_key = Some(r.key);
                    last_group = None; // a new pipeline needs its group bound again
                }
                if let Some(g) = *g {
                    if last_group != Some(g) {
                        pass.set_bind_group(0, &bind_groups[g], &[]);
                        last_group = Some(g);
                    }
                }
                if last_scissor != Some(r.scissor) {
                    let [x, y, w, h] = r.scissor;
                    pass.set_scissor_rect(x, y, w, h);
                    last_scissor = Some(r.scissor);
                }
                if last_blend != Some(r.blend) {
                    let c = r.blend;
                    pass.set_blend_constant(wgpu::Color { r: c[0] as f64, g: c[1] as f64, b: c[2] as f64, a: c[3] as f64 });
                    last_blend = Some(r.blend);
                }
                // the instance index selects the draw's constants record (shader_wgsl `di`)
                let di = r.di.unwrap_or(0);
                if r.indexed {
                    pass.draw_indexed(r.first..r.first + r.count, 0, di..di + 1);
                } else {
                    pass.draw(r.first..r.first + r.count, di..di + 1);
                }
            }
        }
        let pass_ms = t_pass.elapsed().as_secs_f64() * 1e3;
        let t_submit = std::time::Instant::now();
        queue.submit([enc.finish()]);
        if enc_log && ENC_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 300 == 0 {
            eprintln!("encode: {} draws, {} bind groups: groups {groups_ms:.2} ms, pass {pass_ms:.2} ms, submit {:.2} ms", batch.draws.len(), bind_groups.len(), t_submit.elapsed().as_secs_f64() * 1e3);
        }
        drop((bind_groups, draw_group, groups));
        batch.recycle()
}

/// Work for the encoder thread (LMA_RT_ENCODER=1), done in order.
enum Job {
    /// A batch of draws to encode and submit (`encode_batch`).
    Flush { batch: Batch, pipelines: HashMap<PipelineKey, wgpu::RenderPipeline>, color: wgpu::TextureView, depth: wgpu::TextureView },
    /// A command buffer the render thread encoded, then read-back buffers to map once it ran
    /// (each flag set when its copy can be read).
    Submit { cmd: wgpu::CommandBuffer, maps: Vec<(wgpu::Buffer, Arc<std::sync::atomic::AtomicBool>)> },
}

/// The encoder thread (#6; LMA_RT_ENCODER=0 turns it off): batches are encoded and submitted on their own thread while the render
/// thread prepares the next ones. Every submit goes through it, so the GPU sees the same order;
/// the render thread waits for it (`join`) before waiting on the GPU or writing a resource an
/// earlier batch may still read.
struct Encoder {
    tx: Option<std::sync::mpsc::Sender<Job>>,
    /// Batches back for reuse (their allocations).
    back: std::sync::mpsc::Receiver<Batch>,
    sent: u64,
    done: Arc<std::sync::atomic::AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Encoder {
    fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let (back_tx, back) = std::sync::mpsc::channel();
        let done = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done2 = done.clone();
        let thread = std::thread::Builder::new()
            .name("gpu encoder".into())
            .spawn(move || {
                let mut up = Uploads { ring: (0..UPLOAD_RING).map(|_| None).collect(), slot: 0 };
                while let Ok(job) = rx.recv() {
                    match job {
                        Job::Flush { batch, pipelines, color, depth } => {
                            let b = encode_batch(&device, &queue, &mut up, batch, &pipelines, &color, &depth, None);
                            let _ = back_tx.send(b);
                        }
                        Job::Submit { cmd, maps } => {
                            queue.submit([cmd]);
                            for (buf, flag) in maps {
                                buf.slice(..).map_async(wgpu::MapMode::Read, move |r| flag.store(r.is_ok(), std::sync::atomic::Ordering::Release));
                            }
                        }
                    }
                    done2.fetch_add(1, std::sync::atomic::Ordering::Release);
                }
            })
            .expect("gpu encoder thread");
        Self { tx: Some(tx), back, sent: 0, done, thread: Some(thread) }
    }

    fn send(&mut self, job: Job) {
        self.sent += 1;
        let _ = self.tx.as_ref().expect("encoder running").send(job);
    }

    /// Wait until everything sent has been submitted.
    fn join(&self) {
        let mut spins = 0u32;
        while self.done.load(std::sync::atomic::Ordering::Acquire) < self.sent {
            spins += 1;
            if spins < 2048 { std::hint::spin_loop() } else { std::thread::yield_now() }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn vmem_sample(mem: &mut dyn GuestMem, addr: u32, len: u32) -> u64 {
    let mut buf = [0u8; 64];
    let mut h = 0x9E37_79B9_7F4A_7C15u64;
    let mut at = |off: u32, h: &mut u64| {
        let n = 64.min(len - off) as usize;
        mem.read(addr + off, &mut buf[..n]);
        // each chunk of up to 8 bytes as a big-endian number (a byte-wise fold was a third of
        // the render backend's own time in a match)
        let word = |c: &[u8]| if c.len() == 8 { u64::from_be_bytes(c.try_into().unwrap()) } else { c.iter().fold(0u64, |a, &x| a << 8 | x as u64) };
        *h = fast_hash(buf[..n].chunks(8).map(word).chain([*h]));
    };
    for off in (0..len).step_by(4096) {
        at(off, &mut h);
    }
    if len > 64 {
        at(len - 64, &mut h);
    }
    h
}

fn fast_hash(words: impl Iterator<Item = u64>) -> u64 {
    words.fold(0x9E37_79B9_7F4A_7C15, |h, w| (h ^ w).wrapping_mul(0x100_0000_01B3).rotate_left(29))
}

/// `fast_hash` for u32 words, two per mix in four independent lanes (a single chain of multiplies
/// over a draw's ~700 float-constant words was most of `put_floats`).
fn fast_hash_u32(words: &[u32]) -> u64 {
    let mix = |h: u64, w: u64| (h ^ w).wrapping_mul(0x100_0000_01B3).rotate_left(29);
    let mut lanes = [0x9E37_79B9_7F4A_7C15u64, 0x243F_6A88_85A3_08D3, 0x1319_8A2E_0370_7344, 0xA409_3822_299F_31D0];
    let mut chunks = words.chunks_exact(8);
    for c in &mut chunks {
        for (l, pair) in lanes.iter_mut().zip(c.chunks_exact(2)) {
            *l = mix(*l, pair[0] as u64 | (pair[1] as u64) << 32);
        }
    }
    let tail = chunks.remainder().iter().fold(words.len() as u64, |h, &w| mix(h, w as u64));
    lanes.iter().fold(tail, |h, &l| mix(h, l))
}

/// Largest vertex memory window uploaded for one draw; bigger ones use the CPU transform.
const MAX_VMEM: u32 = 16 << 20;

/// Guest primitive -> triangle-list indices for the GPU vertex shader, with the same triangle
/// order and winding as raster::assemble. None: primitives that need the CPU (rect lists,
/// points, lines).
/// Strips and fans are written into `out` (the caller's buffer, kept across draws: a Vec per
/// draw was ~8% of the render thread on the night match, #304).
fn triangle_list<'a>(prim: u32, idx: &'a [u32], out: &'a mut Vec<u32>) -> Option<&'a [u32]> {
    let n = idx.len();
    if prim == 4 {
        // already a list: no copy (most of a match's ~3 M indices per frame)
        return Some(&idx[..n / 3 * 3]);
    }
    out.clear();
    out.reserve(n * 3);
    match prim {
        5 => {
            if let Some((&first, rest)) = idx.split_first() {
                for w in rest.windows(2) {
                    out.extend_from_slice(&[first, w[0], w[1]]);
                }
            }
        }
        6 => {
            // pairs of triangles: (a, b, c) then (c', b', d) flipped, as the strip alternates
            for (i, w) in idx.windows(3).enumerate() {
                if i % 2 == 0 { out.extend_from_slice(&[w[0], w[1], w[2]]) } else { out.extend_from_slice(&[w[1], w[0], w[2]]) }
            }
        }
        13 => {
            for c in idx.chunks_exact(4) {
                out.extend([c[0], c[1], c[2], c[0], c[2], c[3]]);
            }
        }
        _ => return None,
    }
    Some(out)
}

impl Drop for WgpuBackend {
    fn drop(&mut self) {
        // LMA_SETUP_LOG=1: the resolve reuse counters once more at the end (headless runs present no
        // frames, so the per-300-frames print never fires there)
        if std::env::var_os("LMA_SETUP_LOG").is_some() {
            let rr: Vec<u64> = RESOLVE_REUSE.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).collect();
            eprintln!("at exit: resolves [new texture, reused in place, same dest other size, same dest same frame (refused)]: {rr:?}");
        }
    }
}


/// The GPU the backend renders on: instance, adapter, and a device and queue with the optional
/// features the backend uses where the adapter has them (float filtering, 2x MSAA formats, BC
/// textures, timestamps with LMA_GPU_TIMING=1). lma-app creates it before the window so the
/// window's renderer (bevy) runs on the same device (#6: a presented frame without the CPU
/// read-back); `WgpuBackend::new` makes its own.
#[derive(Clone)]
pub struct GpuContext {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

pub fn create_context() -> Result<GpuContext, String> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapter = futures_lite::future::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        ..Default::default()
    }))
    .map_err(|e| format!("no GPU adapter: {e}"))?;
    let limits = adapter.limits();
    // fewer than 17 (the 16 interpolants and the draw index) trims the stage interfaces (`Trim`)
    if limits.max_vertex_attributes < 8 || limits.max_inter_stage_shader_variables < 8 {
        return Err(format!("GPU limits too small: {} vertex attributes, {} inter-stage variables", limits.max_vertex_attributes, limits.max_inter_stage_shader_variables));
    }
    let float_filterable = adapter.features().contains(wgpu::Features::FLOAT32_FILTERABLE);
    // 2x MSAA needs every surface format to support it (optional in WebGPU); LMA_MSAA=0 turns
    // it off
    let msaa2 = std::env::var("LMA_MSAA").as_deref() != Ok("0")
        && adapter.features().contains(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
        && [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Rgba16Float, wgpu::TextureFormat::Depth32Float]
            .iter()
            .all(|&f| adapter.get_texture_format_features(f).flags.sample_count_supported(2));
    let mut features = wgpu::Features::empty();
    if float_filterable {
        features |= wgpu::Features::FLOAT32_FILTERABLE;
    }
    if msaa2 {
        features |= wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES;
    }
    let gpu_timing = std::env::var("LMA_GPU_TIMING").as_deref() == Ok("1") && adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    if gpu_timing {
        features |= wgpu::Features::TIMESTAMP_QUERY;
    }
    // DXT textures uploaded as their BC blocks with the guest's own mips (#304; on by default
    // since the Metal and Vulkan gates: in play 55 -> 60 fps, lows 30 -> 53 on the Mac, the
    // same picture); LMA_TEX_BC=0 goes back to the RGBA8 decode, as does an adapter without BC
    let bc = std::env::var("LMA_TEX_BC").as_deref() != Ok("0") && adapter.features().contains(wgpu::Features::TEXTURE_COMPRESSION_BC);
    if bc {
        features |= wgpu::Features::TEXTURE_COMPRESSION_BC;
    }
    let (device, queue) = futures_lite::future::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("lma gpu backend"),
        required_features: features,
        required_limits: limits,
        ..Default::default()
    }))
    .map_err(|e| format!("GPU device: {e}"))?;
    Ok(GpuContext { instance, adapter, device, queue })
}

impl WgpuBackend {
    /// Open the default GPU. Fails when no adapter supports what the backend needs.
    pub fn new() -> Result<Self, String> {
        Self::from_context(create_context()?)
    }

    /// The backend on an existing device (lma-app shares one with the window's renderer, #6).
    /// Its optional features follow what the device was created with (`create_context`).
    pub fn from_context(ctx: GpuContext) -> Result<Self, String> {
        let GpuContext { adapter, device, queue, .. } = ctx;
        let has = |f: wgpu::Features| device.features().contains(f);
        let float_filterable = has(wgpu::Features::FLOAT32_FILTERABLE);
        let msaa2 = std::env::var("LMA_MSAA").as_deref() != Ok("0")
            && has(wgpu::Features::TEXTURE_ADAPTER_SPECIFIC_FORMAT_FEATURES)
            && [wgpu::TextureFormat::Rgba8Unorm, wgpu::TextureFormat::Rgba16Float, wgpu::TextureFormat::Depth32Float]
                .iter()
                .all(|&f| adapter.get_texture_format_features(f).flags.sample_count_supported(2));
        let gpu_timing = std::env::var("LMA_GPU_TIMING").as_deref() == Ok("1") && has(wgpu::Features::TIMESTAMP_QUERY);
        let bc = std::env::var("LMA_TEX_BC").as_deref() != Ok("0") && has(wgpu::Features::TEXTURE_COMPRESSION_BC);
        let timer = gpu_timing.then(|| GpuTimer::new(&device, &queue));
        let info = adapter.get_info();
        let module = |label: &str, src: &str| {
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) })
        };
        let vs = module("passthrough vs", PASSTHROUGH_VS);
        let depth_only_ps = module("depth-only ps", DEPTH_ONLY_PS);
        let fill_vs = module("depth fill", "@vertex fn main(@location(0) p: vec4<f32>) -> @builtin(position) vec4<f32> { return p; }");
        let upload_ps = module(
            "depth upload",
            "@group(0) @binding(0) var src: texture_2d<f32>;
             @fragment fn main(@builtin(position) p: vec4<f32>) -> @builtin(frag_depth) f32 { return textureLoad(src, vec2<i32>(p.xy), 0).x; }",
        );
        let depth_upload = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("depth upload"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &fill_vs,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 0 }],
                }],
            },
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState { module: &upload_ps, entry_point: Some("main"), compilation_options: Default::default(), targets: &[] }),
            multiview_mask: None,
            cache: None,
        });
        let depth_fill_n = |count: u32| device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("depth fill"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &fill_vs,
                entry_point: Some("main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 16,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 0 }],
                }],
            },
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count, ..Default::default() },
            fragment: None,
            multiview_mask: None,
            cache: None,
        });
        let (depth_fill, depth_fill2) = (depth_fill_n(1), depth_fill_n(if msaa2 { 2 } else { 1 }));
        let resolve_cs = module("resolve", RESOLVE_CS);
        let resolve_cs2 = module("resolve", &resolve_cs_msaa());
        let (resolve_color2, resolve_depth2) = (
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("resolve 2x"), layout: None, module: &resolve_cs2, entry_point: Some("color"), compilation_options: Default::default(), cache: None }),
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("resolve 2x"), layout: None, module: &resolve_cs2, entry_point: Some("depth"), compilation_options: Default::default(), cache: None }),
        );
        let compute = |entry: &str| {
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("resolve"), layout: None, module: &resolve_cs, entry_point: Some(entry), compilation_options: Default::default(), cache: None })
        };
        let (resolve_color, resolve_depth) = (compute("color"), compute("depth"));
        let color_fill_module = module("colour fill", COLOR_FILL);
        let swizzle_module = module("swizzle", SWIZZLE_CS);
        let present_module = module("present", PRESENT_CS);
        let present_pipe = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("present"), layout: None, module: &present_module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
        let swizzle_pipe = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("swizzle"), layout: None, module: &swizzle_module, entry_point: Some("main"), compilation_options: Default::default(), cache: None });
        // on unless LMA_RT_ENCODER=0, and not with LMA_GPU_TIMING (its queries are the render
        // thread's)
        let encoder = (std::env::var("LMA_RT_ENCODER").as_deref() != Ok("0") && !gpu_timing).then(|| Encoder::new(device.clone(), queue.clone()));
        // 8-bit textures decode straight to the RGBA8 this backend uploads (#304)
        let mut soft = Renderer::default();
        soft.rgba8 = true;
        soft.bc = bc;
        let trim = Trim::for_limits(&device.limits(), std::env::var("LMA_INTERP_TRIM").as_deref() == Ok("1"));
        if let Some(t) = trim {
            eprintln!("wgpu: trimmed stage interfaces, up to {} interpolants a pipeline", t.max_interp);
        }
        // the prewarm cache lists full-interface pipelines: off when trimming
        let prewarm = match std::env::var_os("LMA_PIPELINE_CACHE").filter(|p| !p.is_empty() && trim.is_none()) {
            Some(p) => Prewarm::start(std::path::Path::new(&p), &device, &vs, &depth_only_ps, msaa2),
            None => Prewarm::off(),
        };
        Ok(Self {
            soft,
            device,
            queue,
            float_filterable,
            mip_pipes: HashMap::default(),
            mip_enc: None,
            mip_count: 0,
            adapter_name: format!("{} ({:?})", info.name, info.backend),
            surfaces: HashMap::default(),
            depths: HashMap::default(),
            vs,
            ps_modules: HashMap::default(),
            vs_modules: HashMap::default(),
            trim,
            ps_trimmed: HashMap::default(),
            pass_trimmed: HashMap::default(),
            prewarm,
            // landings stay hashed on Apple (LMA_LAND_HASH=0 watches them): a fault marks all four 4 KB
            // subpages of a 16 KB host page, so a landed band whose edge shares a host page with guest
            // writes was dropped and decoded again every frame (13 K re-decodes in 40 s, -13 fps, #355)
            land_watch: std::env::var("LMA_LAND_HASH").as_deref() == Ok("0"),
            floats_written: None,
            index_scratch: Vec::new(),
            tri_scratch: Vec::new(),
            depth_only_ps,
            depth_fill,
            depth_fill2,
            msaa2,
            resolve_color2,
            resolve_depth2,
            depth_upload,
            pipelines: HashMap::default(),
            textures: HashMap::default(),
            samplers: HashMap::default(),
            stats: BackendStats::default(),
            batch: Batch::default(),
            uploads: Uploads { ring: (0..UPLOAD_RING).map(|_| None).collect(), slot: 0 },
            encoder,
            hash_chunk: Vec::new(),
            gpu_vs: std::env::var("LMA_GPU_VS").as_deref() != Ok("0"),
            gpu_resolve: std::env::var("LMA_GPU_RESOLVE").as_deref() != Ok("0"),
            resolve_color,
            resolve_depth,
            color_fill: HashMap::default(),
            color_fill_module,
            pending: Vec::new(),
            next_readback: 1,
            setup_cache: HashMap::default(),
            setup_override_gen: crate::tex_override::generation(),
            setup_key: Vec::new(),
            last_setup: None,
            last_key: Vec::new(),
            last_vs: (Vec::new(), raster::hash(&[])),
            last_ps: (Vec::new(), raster::hash(&[])),
            setup_gen: (u64::MAX, 0),
            tex_gen: 0,
            timer: std::cell::RefCell::new(timer),
            resolved: HashMap::default(),
            res_made: Vec::new(),
            res_prev: Vec::new(),
            res_sampled: Default::default(),
            swizzle_pipe,
            async_fence: false,
            present_pipe,
            present_pending: Default::default(),
            present_bufs: Vec::new(),
            readback_bufs: Vec::new(),
            frame_resolves: HashMap::default(),
            run: 0,
            frame: 0,
            composed: HashMap::default(),
            next_gen: 1,
        })
    }

    pub fn adapter_name(&self) -> &str {
        &self.adapter_name
    }

    // ---- EDRAM surfaces ----------------------------------------------------------------------

    /// The surface-key base for an EDRAM base under the current registers: RB_SURFACE_INFO bits
    /// 16-17 select MSAA, and the main 3D pass of a match is 2x (the console resolves average the
    /// samples, which smooths edges). 4x isn't used; it renders 1x.
    fn surface_key(&self, base: u32, regs: &[u32]) -> u32 {
        base | if self.msaa2 && (regs[RB_SURFACE_INFO] >> 16) & 3 == 1 { MSAA2 } else { 0 }
    }

    /// The colour surface for (base, pitch), created from the software EDRAM on first use.
    fn surface(&mut self, base: u32, pitch: u32, format: Fmt) -> &Surface {
        let key = (base, pitch);
        if self.surfaces.get(&key).is_some_and(|s| s.format != format) {
            self.flush();
            self.surfaces.remove(&key);
        }
        if !self.surfaces.contains_key(&key) {
            let samples = key_samples(base);
            let color = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("edram colour"),
                size: wgpu::Extent3d { width: SURFACE * res_scale(), height: SURFACE * res_scale(), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: samples,
                dimension: wgpu::TextureDimension::D2,
                format: format.wgpu(),
                usage: if samples > 1 {
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
                } else {
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING
                },
                view_formats: &[],
            });
            let view = color.create_view(&Default::default());
            self.surfaces.insert(key, Surface { color, view, format });
            let rows = SURFACE.min(4096);
            self.upload_color(base, pitch, 0, 0, pitch.min(SURFACE), rows);
        }
        &self.surfaces[&key]
    }

    fn depth(&mut self, base: u32, pitch: u32) -> &DepthSurface {
        let key = (base, pitch);
        if !self.depths.contains_key(&key) {
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("edram depth"),
                size: wgpu::Extent3d { width: SURFACE * res_scale(), height: SURFACE * res_scale(), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: key_samples(base),
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: if key_samples(base) > 1 {
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
                } else {
                    wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING
                },
                view_formats: &[],
            });
            let view = tex.create_view(&Default::default());
            self.depths.insert(key, DepthSurface { tex, view });
            self.upload_depth(base, pitch);
        }
        &self.depths[&key]
    }

    /// Software EDRAM -> GPU colour surface for a rectangle.
    fn upload_color(&mut self, base: u32, pitch: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.join_encoder();
        if key_samples(base) > 1 {
            return; // multisampled surfaces can't be written by copies; they start cleared
        }
        self.flush();
        let Some(s) = self.surfaces.get(&(base, pitch)) else { return };
        let (w, h) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
        if w == 0 || h == 0 {
            return;
        }
        // each texel s x s times at resolution scale s
        let k = res_scale();
        let mut bytes = Vec::with_capacity((w * h * k * k * s.format.bytes()) as usize);
        for y in (y0 * k..y1 * k).map(|y| y / k) {
            for x in (x0 * k..x1 * k).map(|x| x / k) {
                let c = self.soft.edram[self.soft.edram_index(base, pitch, x, y)];
                match s.format {
                    Fmt::Rgba8 => bytes.extend(c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)),
                    Fmt::Rgba16F => {
                        for v in c {
                            bytes.extend(f32_to_f16(v).to_le_bytes());
                        }
                    }
                }
            }
        }
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &s.color, mip_level: 0, origin: wgpu::Origin3d { x: x0 * k, y: y0 * k, z: 0 }, aspect: wgpu::TextureAspect::All },
            &bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * k * s.format.bytes()), rows_per_image: Some(h * k) },
            wgpu::Extent3d { width: w * k, height: h * k, depth_or_array_layers: 1 },
        );
    }

    /// Software depth -> GPU depth surface. Depth textures can only be copied whole.
    fn upload_depth(&mut self, base: u32, pitch: u32) {
        self.join_encoder();
        if !self.depths.contains_key(&(base, pitch)) || key_samples(base) > 1 {
            return;
        }
        self.flush();
        // Depth32Float can't be a copy destination: put the values in an R32Float texture and
        // draw a full-screen pass that writes them as fragment depth.
        let k = res_scale();
        let n = SURFACE * k;
        let mut bytes = Vec::with_capacity((n * n * 4) as usize);
        for y in 0..n {
            for x in 0..n {
                let z = self.soft.depth[self.soft.edram_index(base, pitch, x / k, y / k)];
                bytes.extend(z.to_le_bytes());
            }
        }
        let src = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth upload"),
            size: wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &src, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(n * 4), rows_per_image: Some(n) },
            wgpu::Extent3d { width: n, height: n, depth_or_array_layers: 1 },
        );
        let src_view = src.create_view(&Default::default());
        let layout = self.depth_upload.get_bind_group_layout(0);
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src_view) }],
        });
        let tri: [f32; 12] = [-1.0, -1.0, 0.0, 1.0, 3.0, -1.0, 0.0, 1.0, -1.0, 3.0, 0.0, 1.0];
        let vbuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("depth upload"), size: 48, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        self.queue.write_buffer(&vbuf, 0, &tri.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>());
        let view = &self.depths[&(base, pitch)].view;
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let tw = self.ts(4);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth upload"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::RenderPassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.depth_upload);
            pass.set_bind_group(0, &bg, &[]);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([enc.finish()]);
    }

    /// Fill a rectangle of a depth surface with `z` (a quad at that depth, scissored).
    fn clear_depth(&mut self, base: u32, pitch: u32, rect: [u32; 4], z: f32) {
        self.depth(base, pitch);
        self.flush();
        let view = &self.depths[&(base, pitch)].view;
        let tri: [f32; 12] = [-1.0, -1.0, z, 1.0, 3.0, -1.0, z, 1.0, -1.0, 3.0, z, 1.0];
        let vbuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("depth fill"), size: 48, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        let bytes: Vec<u8> = tri.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.queue.write_buffer(&vbuf, 0, &bytes);
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let tw = self.ts(3);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("depth fill"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::RenderPassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [x, y, w, h] = scaled_rect(rect[0], rect[1], rect[2] - rect[0], rect[3] - rect[1], res_scale());
            pass.set_scissor_rect(x, y, w, h);
            pass.set_pipeline(if key_samples(base) > 1 { &self.depth_fill2 } else { &self.depth_fill });
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..3, 0..1);
        }
        self.submit_ordered(enc.finish(), Vec::new());
    }

    /// GPU surface rectangle -> software EDRAM (before a resolve reads it).
    fn download(&mut self, color: bool, base: u32, pitch: u32, x0: u32, y0: u32, x1: u32, y1: u32) {
        self.join_encoder();
        let (w, h) = (x1.min(SURFACE).saturating_sub(x0), y1.min(SURFACE).saturating_sub(y0));
        if w == 0 || h == 0 || key_samples(base) > 1 {
            return;
        }
        self.flush();
        let (texture, bpp, aspect, fmt) = if color {
            let Some(s) = self.surfaces.get(&(base, pitch)) else { return };
            (&s.color, s.format.bytes(), wgpu::TextureAspect::All, Some(s.format))
        } else {
            let Some(d) = self.depths.get(&(base, pitch)) else { return };
            (&d.tex, 4, wgpu::TextureAspect::DepthOnly, None)
        };
        // depth textures can only be copied whole: copy everything, read the rectangle (at
        // resolution scale k, the top-left texel of each guest pixel)
        let k = res_scale();
        let (cx, cy, cw, ch) = if color { (x0 * k, y0 * k, w * k, h * k) } else { (0, 0, SURFACE * k, SURFACE * k) };
        let row = (cw * bpp).div_ceil(256) * 256;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("readback"), size: (row * ch) as u64, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo { texture, mip_level: 0, origin: wgpu::Origin3d { x: cx, y: cy, z: 0 }, aspect },
            wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(row), rows_per_image: Some(ch) } },
            wgpu::Extent3d { width: cw, height: ch, depth_or_array_layers: 1 },
        );
        self.queue.submit([enc.finish()]);
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let p = crate::prof::span(crate::prof::Cat::GpuWait);
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        drop(p);
        let data = slice.get_mapped_range();
        for yy in 0..h {
            for xx in 0..w {
                let o = (((yy + y0) * k - cy) * row + ((xx + x0) * k - cx) * bpp) as usize;
                let idx = self.soft.edram_index(base, pitch, x0 + xx, y0 + yy);
                match fmt {
                    Some(Fmt::Rgba8) => self.soft.edram[idx] = std::array::from_fn(|k| data[o + k] as f32 / 255.0),
                    Some(Fmt::Rgba16F) => self.soft.edram[idx] = std::array::from_fn(|k| f16_to_f32(u16::from_le_bytes([data[o + 2 * k], data[o + 2 * k + 1]]))),
                    None => self.soft.depth[idx] = f32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]),
                }
            }
        }
    }

    // ---- shaders, pipelines, textures ----------------------------------------------------

    fn ps_module(&mut self, ps: &[u32]) -> u64 {
        // most draws use the previous draw's shader: a compare is cheaper than the serial hash
        let h = if self.last_ps.0.as_slice() == ps {
            self.last_ps.1
        } else {
            let h = raster::hash(ps);
            self.last_ps = (ps.to_vec(), h);
            h
        };
        if !self.ps_modules.contains_key(&h) {
            self.take_prewarmed();
        }
        if !self.ps_modules.contains_key(&h) {
            let _p = crate::prof::span(crate::prof::Cat::RtPipeline);
            let m = make_ps(&self.device, ps);
            self.ps_modules.insert(h, m);
            self.prewarm.note_shader("ps", h, ps);
        }
        h
    }

    /// Insert what the prewarm thread has built so far (each one only if still missing).
    fn take_prewarmed(&mut self) {
        let Some(rx) = &self.prewarm.rx else { return };
        loop {
            match rx.try_recv() {
                Ok(Warm::Ps(h, m)) => {
                    self.ps_modules.entry(h).or_insert(m);
                }
                Ok(Warm::Vs(h, m)) => {
                    self.vs_modules.entry(h).or_insert(m);
                }
                Ok(Warm::Pipe(key, p)) => {
                    self.pipelines.entry(key).or_insert(p);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.prewarm.rx = None;
                    return;
                }
            }
        }
    }

    fn vs_module(&mut self, vs: &[u32]) -> u64 {
        let h = if self.last_vs.0.as_slice() == vs {
            self.last_vs.1
        } else {
            let h = raster::hash(vs);
            self.last_vs = (vs.to_vec(), h);
            h
        };
        if !self.vs_modules.contains_key(&h) {
            self.take_prewarmed();
        }
        if !self.vs_modules.contains_key(&h) {
            let _p = crate::prof::span(crate::prof::Cat::RtPipeline);
            let m = make_vs(&self.device, vs, self.trim.map(|t| t.max_interp));
            self.vs_modules.insert(h, m);
            self.prewarm.note_shader("vs", h, vs);
        }
        h
    }

    /// LMA_GPU_COUNTERS=1 (#746): what the backend holds, every 5 seconds (wgpu's own live object
    /// counts too with the `gpu-counters` feature). From `sync`: headless runs never present.
    fn log_counters(&mut self) {
        static GPU_COUNTERS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*GPU_COUNTERS.get_or_init(|| std::env::var_os("LMA_GPU_COUNTERS").is_some()) {
            return;
        }
        thread_local!(static LAST: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) });
        let now = std::time::Instant::now();
        if LAST.with(|l| l.get().is_some_and(|t| now.duration_since(t).as_secs() < 5)) {
            return;
        }
        LAST.with(|l| l.set(Some(now)));
        let decoded: usize = self.textures.values().map(|(d, _)| d.w as usize * d.h as usize).sum();
        eprintln!(
            "gpu counters (run {}): tex_cache {} | gpu textures {} ({} Mpx) | pipelines {} | setups {} | surfaces {} + depths {} | resolved {} | composed {} | present bufs {} pending {} | readbacks {}",
            self.run, self.soft.tex_cache_len(), self.textures.len(), decoded / 1_000_000, self.pipelines.len(), self.setup_cache.len(),
            self.surfaces.len(), self.depths.len(), self.resolved.len(), self.composed.len(), self.present_bufs.len(), self.present_pending.len(), self.pending.len()
        );
        #[cfg(feature = "gpu-counters")]
        {
            let c = self.device.get_internal_counters().hal;
            eprintln!(
                "gpu counters (run {}): wgpu buffers {} textures {} views {} bind groups {} samplers {} encoders {} allocations {}",
                self.run, c.buffers.read(), c.textures.read(), c.texture_views.read(), c.bind_groups.read(), c.samplers.read(), c.command_encoders.read(), c.memory_allocations.read()
            );
        }
    }

    fn pipeline(&mut self, key: PipelineKey) -> &wgpu::RenderPipeline {
        if !self.pipelines.contains_key(&key) {
            self.take_prewarmed();
        }
        if !self.pipelines.contains_key(&key) {
            let _p = crate::prof::span(crate::prof::Cat::RtPipeline);
            let pipeline = match self.trim {
                None => {
                    let vs = if key.vs == 0 { &self.vs } else { &self.vs_modules[&key.vs].0 };
                    let ps = if key.ps == 0 { &self.depth_only_ps } else { &self.ps_modules[&key.ps].0 };
                    make_pipeline(&self.device, key, vs, ps, None)
                }
                Some(t) => {
                    // the interpolants this pipeline carries: its vertex shader's, or (CPU-transformed
                    // vertices) the draw's
                    let mask = if key.vs == 0 { first_bits(key.imask, t.max_interp) } else { self.vs_modules[&key.vs].3 };
                    let module = |label: &str, src: String| self.device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(src.into()) });
                    if key.vs == 0 && !self.pass_trimmed.contains_key(&mask) {
                        let m = module("passthrough vs (trimmed)", passthrough_vs_trimmed(mask));
                        self.pass_trimmed.insert(mask, m);
                    }
                    if key.ps != 0 && !self.ps_trimmed.contains_key(&(key.ps, mask)) {
                        let shader = self.ps_modules[&key.ps].2.clone();
                        let m = module("xenos ps (trimmed)", crate::shader_wgsl::translate_with(&shader, crate::shader_wgsl::Stage::Pixel, Some(mask)).wgsl);
                        self.ps_trimmed.insert((key.ps, mask), m);
                    }
                    let vs = if key.vs == 0 { &self.pass_trimmed[&mask] } else { &self.vs_modules[&key.vs].0 };
                    let ps = if key.ps == 0 { &self.depth_only_ps } else { &self.ps_trimmed[&(key.ps, mask)] };
                    make_pipeline(&self.device, key, vs, ps, (key.vs == 0).then_some(mask))
                }
            };
            self.pipelines.insert(key, pipeline);
            self.prewarm.note_pipeline(key);
        }
        &self.pipelines[&key]
    }

    /// The GPU copy of a decoded texture. 8-bit unorm sources go up as Rgba8Unorm (a quarter
    /// of the bytes, exact for their values; DXT interpolants round to the nearest 1/255),
    /// everything else (depth, float, signed) as Rgba32Float.
    fn texture_view(&mut self, d: &Arc<Decoded>, unorm8_format: bool) -> wgpu::TextureView {
        let key = Arc::as_ptr(d) as usize;
        if let Some((_, v)) = self.textures.get(&key) {
            return v.clone();
        }
        // RGBA8 when every value is in 0..=1: decoded that way (`Decoded::px8`), or checked while
        // converting (one pass, not two: #304)
        // BC blocks (LMA_TEX_BC, #304): uploaded as they are, the guest's own mip levels with them
        if let Some((fmt, levels)) = &d.bc {
            let (format, bpb) = match fmt {
                18 => (wgpu::TextureFormat::Bc1RgbaUnorm, 8),
                19 => (wgpu::TextureFormat::Bc2RgbaUnorm, 16),
                _ => (wgpu::TextureFormat::Bc3RgbaUnorm, 16),
            };
            let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("guest texture (BC)"),
                size: wgpu::Extent3d { width: d.w, height: d.h, depth_or_array_layers: 1 },
                mip_level_count: levels.len() as u32,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            for (l, data) in levels.iter().enumerate() {
                let (w, h) = ((d.w >> l).max(1), (d.h >> l).max(1));
                self.queue.write_texture(
                    wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: l as u32, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                    data,
                    wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w.div_ceil(4) * bpb), rows_per_image: Some(h.div_ceil(4)) },
                    // a level under a block (2x2, 1x1) copies its whole block: the physical size
                    wgpu::Extent3d { width: w.next_multiple_of(4), height: h.next_multiple_of(4), depth_or_array_layers: 1 },
                );
            }
            let view = tex.create_view(&Default::default());
            self.textures.insert(key, (d.clone(), view.clone()));
            return view;
        }
        let rgba8 = if unorm8_format && d.px8.is_none() { to_rgba8(&d.px) } else { None };
        let unorm8 = d.px8.is_some() || rgba8.is_some();
        // a full mip chain (the console samples the game's own; ours are box-filtered from the
        // base level on the GPU), so minified textures don't alias into sparkle
        let levels = 32 - d.w.max(d.h).max(1).leading_zeros();
        let format = if unorm8 { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba32Float };
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("guest texture"),
            size: wgpu::Extent3d { width: d.w, height: d.h, depth_or_array_layers: 1 },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | if levels > 1 { wgpu::TextureUsages::STORAGE_BINDING } else { wgpu::TextureUsages::empty() },
            view_formats: &[],
        });
        let bytes: Vec<u8> = match (&d.px8, rgba8) {
            (Some(_), _) => Vec::new(),
            (None, Some(b)) => b,
            // a plain loop into a sized buffer: the nested flat_map it replaces took 51 ms for the
            // 1024x1024 depth texture kickoff samples, a quarter of its worst frame (#304)
            (None, None) => {
                let mut b = Vec::with_capacity(d.px.len() * 16);
                for v in d.px.as_flattened() {
                    b.extend_from_slice(&v.to_le_bytes());
                }
                b
            }
        };
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            d.px8.as_deref().unwrap_or(&bytes),
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(d.w * if unorm8 { 4 } else { 16 }), rows_per_image: Some(d.h) },
            wgpu::Extent3d { width: d.w, height: d.h, depth_or_array_layers: 1 },
        );
        if levels > 1 {
            self.generate_mips(&tex, format, levels);
        }
        let view = tex.create_view(&Default::default());
        self.textures.insert(key, (d.clone(), view.clone()));
        view
    }

    /// Fill mip levels 1.. of `tex` from level 0, each from the one above.
    fn generate_mips(&mut self, tex: &wgpu::Texture, format: wgpu::TextureFormat, levels: u32) {
        let pipe = self.mip_pipes.entry(format).or_insert_with(|| {
            let fmt = if format == wgpu::TextureFormat::Rgba8Unorm { "rgba8unorm" } else { "rgba32float" };
            let module = self.device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("mips"), source: wgpu::ShaderSource::Wgsl(MIP_CS.replace("FORMAT", fmt).into()) });
            self.device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor { label: Some("mips"), layout: None, module: &module, entry_point: Some("main"), compilation_options: Default::default(), cache: None })
        });
        // one view per level (each is a destination, then the next level's source), one bind group
        // per step, and one pass for the whole chain: WebGPU orders a pass's dispatches, so each
        // level reads the one written before it (#304: kickoff records ~100 chains in one frame)
        let views: Vec<wgpu::TextureView> = (0..levels).map(|l| tex.create_view(&wgpu::TextureViewDescriptor { base_mip_level: l, mip_level_count: Some(1), ..Default::default() })).collect();
        let layout = pipe.get_bind_group_layout(0);
        let groups: Vec<wgpu::BindGroup> = views
            .windows(2)
            .map(|v| {
                self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: None,
                    layout: &layout,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&v[0]) }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&v[1]) }],
                })
            })
            .collect();
        // recorded into the shared mip encoder, submitted ahead of the open batch (`submit_mips`):
        // only later draws sample it. One submit for a frame's new textures, not one each (#304).
        let enc = self.mip_enc.get_or_insert_with(|| self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("mips") }));
        let mut pass = enc.begin_compute_pass(&Default::default());
        pass.set_pipeline(pipe);
        for (l, bg) in (1..levels).zip(&groups) {
            let (w, h) = ((tex.width() >> l).max(1), (tex.height() >> l).max(1));
            pass.set_bind_group(0, bg, &[]);
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
        drop(pass);
        self.mip_count += 1;
        if self.mip_count >= MIP_BATCH {
            self.submit_mips();
        }
    }

    /// Submit the mip generation recorded since the last submit, in order with the batches.
    fn submit_mips(&mut self) {
        self.mip_count = 0;
        if let Some(enc) = self.mip_enc.take() {
            self.submit_ordered(enc.finish(), Vec::new());
        }
    }

    fn sampler(&mut self, clamp: (u32, u32, bool)) -> wgpu::Sampler {
        let filter = if self.float_filterable && clamp.2 { wgpu::FilterMode::Linear } else { wgpu::FilterMode::Nearest };
        self.samplers
            .entry(clamp)
            .or_insert_with(|| {
                let mode = |m: u32| match m {
                    0 => wgpu::AddressMode::Repeat,
                    1 => wgpu::AddressMode::MirrorRepeat,
                    _ => wgpu::AddressMode::ClampToEdge,
                };
                self.device.create_sampler(&wgpu::SamplerDescriptor {
                    address_mode_u: mode(clamp.0),
                    address_mode_v: mode(clamp.1),
                    mag_filter: filter,
                    min_filter: filter,
                    mipmap_filter: if filter == wgpu::FilterMode::Linear { wgpu::MipmapFilterMode::Linear } else { wgpu::MipmapFilterMode::Nearest },
                    ..Default::default()
                })
            })
            .clone()
    }

    /// Pixel-shader constants as the translated shader's `Consts` struct expects them.
    #[allow(clippy::too_many_arguments)]
    /// The draw's constant block, on the stack (a heap Vec per draw was part of the render
    /// thread's allocation cost, #304).
    fn consts(regs: &[u32], ctx: &RasterCtx, vmem_base: u32, fetch_idx: u32, vmem: (u32, u32), f_off: (u32, u32), scaled: u32) -> [u8; CONSTS_SIZE] {
        let mut out = [0u8; CONSTS_SIZE];
        let mut n = 0;
        let mut put = |v: u32| {
            out[n..n + 4].copy_from_slice(&v.to_le_bytes());
            n += 4;
        };
        regs[BOOL_BASE..BOOL_BASE + 8].iter().for_each(|&r| put(r));
        regs[LOOP_BASE..LOOP_BASE + 64].iter().for_each(|&r| put(r));
        let cc = regs[0x2202];
        for v in [vmem_base, cc & 0xF, regs[0x210E], poly_offset(ctx).1.to_bits()] {
            put(v);
        }
        for v in [ctx.vte, fetch_idx, vmem.0, vmem.1] {
            put(v);
        }
        let half = SURFACE as f32 / 2.0;
        let vp = ctx.vp;
        for f in [vp[0], vp[1], vp[2], vp[3], vp[4], vp[5], ctx.pix_offset, 0.0, ctx.offset[0], ctx.offset[1], 1.0 / half, 1.0 / half] {
            put(f.to_bits());
        }
        for v in [f_off.0, f_off.1, scaled, 0] {
            put(v);
        }
        debug_assert_eq!(n, CONSTS_SIZE);
        out
    }

    // ---- draws ---------------------------------------------------------------------------

    /// Bind the pixel shader: its hash (0 for none) and texture/sampler pairs.
    #[allow(clippy::type_complexity)]
    fn ps_setup(&mut self, regs: &[u32], mem: &mut dyn GuestMem, ps: &[u32], depth_only: bool) -> (u64, Setup) {
        let ps_hash = if depth_only || ps.is_empty() { 0 } else { self.ps_module(ps) };
        if ps_hash == 0 {
            return (0, Setup::default());
        }
        // the same shader with the same fetch constants binds the same textures until guest
        // memory or the resolves change (a new run, a resolve, a readback landing): most draws
        // of a frame repeat an earlier draw's set (players, pitch tiles)
        let override_gen = crate::tex_override::generation();
        if override_gen != self.setup_override_gen {
            self.setup_override_gen = override_gen;
            self.setup_cache.clear();
            self.last_setup = None;
        }
        if self.setup_gen != (self.run, self.tex_gen) {
            // entries outlive the generation and are re-validated on use (#304; LMA_SETUP_KEEP=0: off)
            // (nearly every miss was a setup the clear had just dropped)
            if !setup_keep() || self.setup_cache.len() > 8192 {
                self.setup_cache.clear();
            }
            self.setup_gen = (self.run, self.tex_gen);
        }
        let mut key = std::mem::take(&mut self.setup_key);
        key.clear();
        key.extend_from_slice(&[ps_hash as u32, (ps_hash >> 32) as u32]);
        for &k in &self.ps_modules[&ps_hash].1 {
            let b = FETCH_BASE + k as usize * 6;
            key.extend_from_slice(&regs[b..b + 6]);
        }
        // half of a match's draws repeat the previous draw's setup key: within one generation
        // that is the setup it got, with no hashing, lookup or check (#304)
        if let Some((h, g, s)) = &self.last_setup {
            if *h == ps_hash && *g == self.setup_gen && self.last_key == key {
                let s = s.clone();
                self.setup_key = key;
                return (ps_hash, s);
            }
        }
        let mut miss_kind = "absent";
        if let Some(e) = self.setup_cache.get(key.as_slice()) {
            miss_kind = "found-invalid";
            let (hit, gen, built, watch) = (e.setup.clone(), e.gen, e.built, e.watch.clone());
            let check = if gen == self.setup_gen {
                Check::Valid
            } else if watch.as_ref().is_some_and(|w| self.setup_fast_valid(mem, w)) {
                crate::prof::count(crate::prof::Cat::SetupFast);
                Check::Valid
            } else {
                crate::prof::count(crate::prof::Cat::SetupCheck);
                let _p = crate::prof::span(crate::prof::Cat::SetupCheckTime);
                self.setup_still_valid(regs, mem, ps_hash, &hit, built, watch.as_ref())
            };
            let hit = match check {
                Check::Valid => Some(hit),
                // its render-target textures were drawn again: the same setup with their new views
                Check::Refresh(views) => {
                    crate::prof::count(crate::prof::Cat::SetupRefresh);
                    let mut tex = hit.0.clone();
                    for (k, view) in views {
                        if let Some(e) = tex.iter_mut().find(|e| e.0 == k) {
                            e.1 = view;
                        }
                    }
                    let fresh: Setup = Arc::new((tex, hit.1.clone()));
                    if let Some(e) = self.setup_cache.get_mut(key.as_slice()) {
                        e.setup = fresh.clone();
                        e.built = self.next_gen;
                    }
                    Some(fresh)
                }
                Check::Invalid => None,
            };
            if let Some(hit) = hit {
                if let Some(e) = self.setup_cache.get_mut(key.as_slice()) {
                    e.gen = self.setup_gen;
                }
                self.setup_key = key;
                self.remember_setup(ps_hash, &hit);
                return (ps_hash, hit);
            }
        }
        crate::prof::count(crate::prof::Cat::SetupMiss);
        // LMA_SETUP_LOG=1 (debug, #6): which shaders miss the setup cache and with what first texture,
        // every 2,000 misses the top entries since the last print
        static SETUP_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *SETUP_LOG.get_or_init(|| std::env::var_os("LMA_SETUP_LOG").is_some()) {
            use std::sync::Mutex;
            static MISSES: Mutex<Vec<(u64, u32, u32, u32, u32)>> = Mutex::new(Vec::new());
            let (b0, b1) = self.ps_modules[&ps_hash].1.first().map_or((0, 0), |&k| (regs[FETCH_BASE + k as usize * 6], regs[FETCH_BASE + k as usize * 6 + 1]));
            let t = raster::TexInfo::from_fetch(&regs[FETCH_BASE + self.ps_modules[&ps_hash].1.first().copied().unwrap_or(0) as usize * 6..][..6]);
            let mut m = MISSES.lock().unwrap();
            let fmt = t.format;
            let same_shader = m.iter().filter(|e| e.0 == ps_hash).count();
            m.push((ps_hash, (b0 & !0xFFF) | (miss_kind == "absent") as u32, t.width, t.height, fmt));
            if m.len() >= 2000 {
                let mut by: HashMap<u64, (u32, std::collections::HashSet<u32>, u32, u32, u32)> = HashMap::default();
                for &(h, base, w, hgt, f) in m.iter() {
                    let e = by.entry(h).or_insert((0, std::collections::HashSet::new(), w, hgt, f));
                    e.0 += 1;
                    e.1.insert(base);
                }
                let mut v: Vec<_> = by.into_iter().collect();
                v.sort_by_key(|e| std::cmp::Reverse(e.1 .0));
                let absent = m.iter().filter(|e| e.1 & 1 == 1).count();
                let top: Vec<String> = v.iter().take(6).map(|(h, (n, bases, w, hgt, f))| format!("ps {:08x}: {n} misses, {} distinct bases, first tex {w}x{hgt} fmt {f}", *h as u32, bases.len())).collect();
                eprintln!("setup misses: {absent} absent keys, {} found but invalid", m.len() - absent);
                let r: Vec<u64> = INVALID_REASON.iter().map(|a| a.swap(0, std::sync::atomic::Ordering::Relaxed)).collect();
                eprintln!("invalid reasons [pending-overlap, composed, no-resolve, alias-stale, layout, decoded-now-resolved, textures-changed, -]: {r:?}");
                let fr: Vec<u64> = FAST_REASON.iter().map(|a| a.swap(0, std::sync::atomic::Ordering::Relaxed)).collect();
                let rr: Vec<u64> = RESOLVE_REUSE.iter().map(|a| a.swap(0, std::sync::atomic::Ordering::Relaxed)).collect();
                eprintln!("resolves [new texture, reused in place, same dest other size, same dest same frame (refused)]: {rr:?}");
                eprintln!("fast path [watch: composed, re-prepared, no-stamp; check: resolved, pending, written; watch: no-range]: {fr:?}");
                eprintln!("setup misses ({}): {}", m.len(), top.join(" | "));
                m.clear();
            }
            let _ = (b1, same_shader);
        }
        let out = Arc::new(self.ps_setup_textures(regs, mem, ps_hash));
        if self.setup_gen == (self.run, self.tex_gen) {
            let watch = self.setup_watch(regs, mem, ps_hash, &out);
            self.setup_cache.insert(key.clone(), CachedSetup { setup: out.clone(), gen: self.setup_gen, built: self.next_gen, watch });
        }
        self.setup_key = key;
        self.remember_setup(ps_hash, &out);
        (ps_hash, out)
    }

    /// The setup `ps_setup` just returned, for the next draw with the same key (`setup_key`).
    fn remember_setup(&mut self, ps_hash: u64, setup: &Setup) {
        self.last_key.clear();
        self.last_key.extend_from_slice(&self.setup_key);
        self.last_setup = Some((ps_hash, self.setup_gen, setup.clone()));
    }

    /// What a setup depends on: its decoded textures' guest ranges with the newest page stamp over
    /// them now, and its aliased resolves with their generation. None for a composed texture (the
    /// full check handles it) or without a write watch.
    fn setup_watch(&mut self, regs: &[u32], mem: &mut dyn GuestMem, ps_hash: u64, setup: &Setup) -> Option<SetupWatch> {
        let shader = self.ps_modules[&ps_hash].2.clone();
        let mut ranges = Vec::new();
        let mut aliases = Vec::new();
        let mut stamp = 0;
        for &(k, id, _) in &setup.1 {
            if id >> 63 == 1 {
                if id >> 62 & 1 == 1 {
                    FAST_REASON[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return None; // composed from several resolves: the full check
                }
                let base = ((id & !(3 << 62)) >> 12) as u32;
                // what this slot's view follows (`alias_follows`): the texture for the identity view,
                // the content for a swizzled copy
                let r = self.resolved.get(&base)?;
                self.res_sampled.insert(r.content);
                aliases.push((base, alias_follows(id, r.gen, r.content), id));
                continue;
            }
            let b = FETCH_BASE + k as usize * 6;
            let t = raster::TexInfo::from_fetch(&regs[b..b + 6]);
            let base = crate::gpu::phys(t.base);
            // texture_ranges gives physical ranges, as the landing checks use them
            let Some((lo, hi)) = raster::texture_ranges(regs, &shader, &[]).into_iter().find(|&(lo, hi)| lo <= base && base < hi) else {
                FAST_REASON[6].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            };
            let Some(s) = self.soft.range_stamp(mem, lo, hi.saturating_sub(lo)) else {
                FAST_REASON[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return None;
            };
            stamp = stamp.max(s);
            ranges.push((lo, hi, base));
        }
        Some(SetupWatch { ranges, stamp, aliases })
    }

    /// A cached setup is still current when its aliased resolves are the same ones with their
    /// memory intact, no page of its decoded textures was written since it was built (the write
    /// watch's stamps), no resolve has appeared at a decoded texture's base (those draws must
    /// alias the GPU copy) and no read-back is about to land over them.
    fn setup_fast_valid(&mut self, mem: &mut dyn GuestMem, watch: &SetupWatch) -> bool {
        let SetupWatch { ranges, stamp, aliases } = watch;
        // an aliased resolve: the same one (not redone since: a redo takes the Refresh path) and
        // its memory still what landed
        for &(base, follows, id) in aliases {
            if let Some(r) = self.resolved.get(&base) {
                self.res_sampled.insert(r.content);
            }
            if !self.resolved.get(&base).is_some_and(|r| alias_follows(id, r.gen, r.content) == follows) {
                FAST_REASON[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            if !self.alias_valid(mem, base) {
                FAST_REASON[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
        }
        self.ranges_unchanged(mem, ranges, *stamp)
    }

    /// No page of these decoded textures was written since `stamp` (the write watch), no resolve
    /// has appeared at a texture's base and no read-back is about to land over them: the decodes
    /// the setup holds are still the texture cache's current ones, without `prepare_textures`.
    fn ranges_unchanged(&mut self, mem: &mut dyn GuestMem, ranges: &[(u32, u32, u32)], stamp: u32) -> bool {
        for &(lo, hi, base) in ranges {
            if self.resolved.contains_key(&base) {
                FAST_REASON[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            if self.pending.iter().any(|r| r.dest < hi && r.dest.saturating_add(r.len as u32) > lo) {
                FAST_REASON[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return false;
            }
            match self.soft.range_stamp(mem, lo, hi.saturating_sub(lo)) {
                Some(s) if s <= stamp => {}
                _ => {
                    FAST_REASON[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return false;
                }
            }
        }
        true
    }

    /// Whether a setup built in an earlier generation still binds what `ps_setup_textures` would
    /// bind now: no readback pending over its textures, every aliased resolve still valid, and
    /// every decoded texture still the texture cache's current decode with no resolve over it.
    /// Aliased resolves redone since (a render target drawn again each frame: shadow maps, bloom)
    /// with the layout and size the fetch wants come back as views to patch in, as
    /// `ps_setup_textures` would bind them (#304: ~80% of setup misses in a match). Anything else
    /// (composed views, placeholders) rebuilds.
    fn setup_still_valid(&mut self, regs: &[u32], mem: &mut dyn GuestMem, ps_hash: u64, hit: &Setup, built: u64, watch: Option<&SetupWatch>) -> Check {
        let shader = self.ps_modules[&ps_hash].2.clone();
        // A resolve still landing in guest memory only matters to textures decoded from that memory.
        // A setup that samples the resolve through its GPU view (the alias case below) is unaffected,
        // and that was every player draw of a match: a blanket check here rebuilt ~1,250 setups a
        // frame (3.6 ms of the render thread on the M4 Max, #6, 12 Oct), all 'pending overlap'.
        let landing: Vec<(u32, u32)> = if self.pending.is_empty() { Vec::new() } else { self.pending.iter().map(|r| (r.dest, r.dest.saturating_add(r.len as u32))).collect() };
        let mut skip = Vec::new();
        let mut refresh = Vec::new();
        let mut decoded = false;
        for &(k, id, _) in &hit.1 {
            if id >> 63 == 1 {
                if id >> 62 & 1 == 1 {
                    { INVALID_REASON[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; } // composed from several resolves
                }
                // the same resolve the setup's view came from, its memory still matching; or a newer
                // one at that address with the fetch's layout and size: its view, patched in
                let base = ((id & !(3 << 62)) >> 12) as u32;
                // the identity view follows the texture (kept across in-place resolves); a swizzled
                // copy follows the content
                if let Some(r) = self.resolved.get(&base) {
                    self.res_sampled.insert(r.content);
                }
                let Some(gen) = self.resolved.get(&base).map(|r| alias_follows(id, r.gen, r.content)) else { return Check::Invalid };
                if !self.alias_valid(mem, base) {
                    { INVALID_REASON[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; }
                }
                if gen >= built {
                    let b = FETCH_BASE + k as usize * 6;
                    let t = raster::TexInfo::from_fetch(&regs[b..b + 6]);
                    let r = &self.resolved[&base];
                    let pitch_aligned = (t.pitch.max(t.width) + 31) & !31;
                    if !(t.format == r.format && t.tiled && t.endian == r.endian && pitch_aligned == r.pitch_aligned && (t.width, t.height) == (r.w, r.h)) {
                        { INVALID_REASON[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; }
                    }
                    refresh.push((k, self.swizzled_view(base, (id & 0xFFF) as u32)));
                }
                skip.push(k);
            } else {
                let b = FETCH_BASE + k as usize * 6;
                let base = crate::gpu::phys(raster::TexInfo::from_fetch(&regs[b..b + 6]).base);
                if self.resolved.contains_key(&base) {
                    { INVALID_REASON[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; } // a resolve there now: the full path decides alias or decode
                }
                decoded = true;
            }
        }
        if decoded && !landing.is_empty() {
            // a decoded texture's bytes may be about to change: decode again once they have landed
            for (lo, hi) in raster::texture_ranges(regs, &shader, &skip) {
                if landing.iter().any(|&(a, z)| a < hi && z > lo) {
                    { INVALID_REASON[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; }
                }
            }
        }
        // the write watch says the decoded textures' pages are untouched since the setup was built:
        // the texture cache would hand back the same decodes, so don't ask it (2.1 of the 2.3 ms a
        // frame of re-validations in a night match, 12 Oct)
        let untouched = decoded && watch.is_some_and(|w| !w.ranges.is_empty() && self.ranges_unchanged(mem, &w.ranges, w.stamp));
        if untouched {
            crate::prof::count(crate::prof::Cat::SetupFast);
        }
        if decoded && !untouched {
            FAST_REASON[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed); // re-validations that re-prepare decoded textures
            let _p = crate::prof::span(crate::prof::Cat::SetupCheckDecoded);
            let now = self.soft.prepare_textures(regs, mem, &shader, &skip);
            // a placeholder (the magenta texture for a fetch with nothing decoded) has its own
            // id, never the cache's, and a slot with no decode now is `None`: both rebuild
            for &(k, id, _) in &hit.1 {
                if id >> 63 == 0 && now.get(k as usize).cloned().flatten().is_none_or(|d| d.id != id) {
                    { INVALID_REASON[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed); return Check::Invalid; }
                }
            }
        }
        if refresh.is_empty() { Check::Valid } else { Check::Refresh(refresh) }
    }

    #[allow(clippy::type_complexity)]
    fn ps_setup_textures(&mut self, regs: &[u32], mem: &mut dyn GuestMem, ps_hash: u64) -> (Vec<(u32, wgpu::TextureView, wgpu::Sampler)>, Vec<(u32, u64, u32)>) {
        let mut entries_tex = Vec::new();
        let mut ids = Vec::new();
        {
            // decoded once per shader (was per draw)
            let (consts_used, shader) = { let m = &self.ps_modules[&ps_hash]; (m.1.clone(), m.2.clone()) };
            // textures that are exactly a resolve of this run: sample the GPU copy
            let mut aliased: Vec<(u32, wgpu::TextureView, u64)> = Vec::new();
            for &k in &consts_used {
                let b = FETCH_BASE + k as usize * 6;
                let t = raster::TexInfo::from_fetch(&regs[b..b + 6]);
                let base = crate::gpu::phys(t.base);
                // resolves are replaced per run: tag their ids so they never match a decode
                let id = (1u64 << 63) | ((base as u64) << 12) | (t.swizzle & 0xFFF) as u64;
                if self.alias_valid(mem, base) {
                    let r = &self.resolved[&base];
                    let pitch_aligned = (t.pitch.max(t.width) + 31) & !31;
                    if t.format == r.format && t.tiled && t.endian == r.endian && pitch_aligned == r.pitch_aligned && (t.width, t.height) == (r.w, r.h) {
                        let view = self.swizzled_view(base, t.swizzle & 0xFFF);
                        aliased.push((k, view, id));
                        continue;
                    }
                    if alias_log() {
                        eprintln!("LMA_ALIAS_LOG: guest frame {} c{k} {base:#010x} fetch {}x{} fmt {} tiled {} vs resolve {}x{} fmt {}: decoded from memory", crate::threaded::GUEST_FRAME.load(std::sync::atomic::Ordering::Relaxed), t.width, t.height, t.format, t.tiled, r.w, r.h, r.format);
                    }
                }
                if let Some(view) = self.composed_view(mem, &t) {
                    aliased.push((k, view, id | (1 << 62)));
                }
            }
            let skip: Vec<u32> = aliased.iter().map(|a| a.0).collect();
            for (lo, hi) in raster::texture_ranges(regs, &shader, &skip) {
                let _p = crate::prof::span(crate::prof::Cat::GpuWaitTex);
                self.complete(mem, lo, hi);
            }
            let _p = crate::prof::span(crate::prof::Cat::RtTexture);
            let decoded = self.soft.prepare_textures(regs, mem, &shader, &skip);
            for k in consts_used {
                let d0 = regs[FETCH_BASE + k as usize * 6];
                // point or bilinear, as the fetch asks (a shadow map's taps must not blend depths)
                let linear = shader.texture_fetch(k).is_none_or(|f| raster::linear_filter(&f, regs[FETCH_BASE + k as usize * 6 + 3]));
                let clamp = ((d0 >> 10) & 7, (d0 >> 13) & 7, linear);
                let clamp_key = (clamp.0 << 3) | clamp.1 | ((linear as u32) << 6);
                if let Some((_, view, id)) = aliased.iter().find(|a| a.0 == k) {
                    let sampler = self.sampler(clamp);
                    entries_tex.push((k, view.clone(), sampler));
                    ids.push((k, *id, clamp_key));
                    continue;
                }
                let d = decoded.get(k as usize).cloned().flatten().unwrap_or_else(|| Arc::new(Decoded::new(1, 1, vec![[1.0, 0.0, 1.0, 1.0]])));
                let format = raster::TexInfo::from_fetch(&regs[FETCH_BASE + k as usize * 6..FETCH_BASE + k as usize * 6 + 6]).format;
                let view = self.texture_view(&d, UNORM8_FORMATS.contains(&format));
                let sampler = self.sampler(clamp);
                entries_tex.push((k, view, sampler));
                ids.push((k, d.id, clamp_key));
            }
        }
        (entries_tex, ids)
    }

    /// Make the draw's surfaces current; returns the scissor (x, y, w, h), None when empty.
    fn begin(&mut self, ctx: &RasterCtx, regs: &[u32]) -> Option<[u32; 4]> {
        let [sx0, sy0, sx1, sy1] = ctx.scissor;
        let (sx0, sy0) = (sx0.clamp(0, SURFACE as i32) as u32, sy0.clamp(0, SURFACE as i32) as u32);
        let (sx1, sy1) = (sx1.clamp(0, SURFACE as i32) as u32, sy1.clamp(0, SURFACE as i32) as u32);
        if sx1 <= sx0 || sy1 <= sy0 {
            return None;
        }
        let pitch = ctx.surface_pitch;
        let (cb, db) = (self.surface_key(ctx.color_base, regs), self.surface_key(ctx.depth_base, regs));
        self.surface(cb, pitch, Fmt::from_color_info(regs[RB_COLOR_INFO]));
        self.depth(db, pitch);
        let target = ((cb, pitch), (db, pitch));
        if self.batch.target != Some(target) {
            self.flush();
            self.batch.target = Some(target);
        }
        Some(scaled_rect(sx0, sy0, sx1 - sx0, sy1 - sy0, res_scale()))
    }

    /// The draw's constants in the storage arena (shared with the previous draw when equal).
    /// This draw's constants record (shared with any earlier draw of the batch with equal bytes).
    #[allow(clippy::too_many_arguments)]
    fn put_consts(&mut self, regs: &[u32], ctx: &RasterCtx, vmem_base: u32, fetch_idx: u32, vmem: (u32, u32), vs_hash: u64, ps_hash: u64, setup: &Setup) -> u32 {
        // borrowed, not cloned (two allocations per draw)
        let b = &mut self.batch;
        // float constants written since the last draw that got here (draws that stop earlier keep
        // the flags for the next one); only for these two calls
        b.floats_clean = self.floats_written.take().map_or([false; 2], |w| [!w[0], !w[1]]);
        let vf = self.vs_modules.get(&vs_hash).map_or(0, |m| b.put_floats(regs, 0, m.2.as_deref()));
        let pf = self.ps_modules.get(&ps_hash).map_or(0, |m| b.put_floats(regs, 256, m.3.as_deref()));
        b.floats_clean = [false; 2];
        let bytes = Self::consts(regs, ctx, vmem_base, fetch_idx, vmem, (vf, pf), scaled_mask(setup));
        let hash = fast_hash(bytes.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())));
        let b = &mut self.batch;
        Batch::intern(&mut b.consts, &mut b.consts_by_hash, hash, &bytes)
    }

    fn key(ctx: &RasterCtx, regs: &[u32], vs: u64, cull: u32, ps: u64, depth_only: bool) -> PipelineKey {
        PipelineKey {
            bias: if vs == 0 { 0 } else { poly_offset(ctx).0.to_bits() },
            vs,
            cull,
            ps,
            format: Fmt::from_color_info(regs[RB_COLOR_INFO]),
            blend: ctx.blend,
            mask: if depth_only { 0 } else { ctx.mask },
            depth: (ctx.z_enable as u32) | ((ctx.z_write as u32) << 1) | (ctx.z_func << 2),
            samples: 1,
            imask: 0,
        }
    }

    /// CPU-transformed vertices (rect lists, points, lines, oversized vertex windows).
    fn draw_gpu(&mut self, regs: &[u32], mem: &mut dyn GuestMem, prim: u32, verts: &[Vtx], ps: &[u32], depth_only: bool) {
        let mut ctx = RasterCtx::new(regs);
        ctx.depth_only = depth_only;
        let tris: Vec<TriSetup> = raster::assemble(prim, verts, regs).into_iter().flat_map(|t| raster::clip_near(&ctx, t)).filter_map(|t| TriSetup::new(&ctx, t)).collect();
        if tris.is_empty() {
            return;
        }
        // vertices: pixel coordinates (window offset applied, as TriSetup has it) -> clip space
        let (half_w, half_h) = (SURFACE as f32 / 2.0, SURFACE as f32 / 2.0);
        let mut data: Vec<f32> = Vec::with_capacity(tris.len() * 3 * VERTEX_FLOATS);
        for t in &tris {
            for (k, v) in t.v.iter().enumerate() {
                let w = if v.pos[3].is_finite() && v.pos[3] != 0.0 { 1.0 / v.pos[3] } else { 1.0 };
                let (x, y) = (t.p[k][0] / half_w - 1.0, 1.0 - t.p[k][1] / half_h);
                data.extend([x * w, y * w, (v.pos[2] + t.zbias).clamp(0.0, 1.0) * w, w]);
                for i in &v.interp {
                    data.extend(i);
                }
            }
        }
        let (ps_hash, textures) = self.ps_setup(regs, mem, ps, depth_only);
        let Some(scissor) = self.begin(&ctx, regs) else { return };
        // trimmed interfaces: the interpolants some vertex carries (one zero everywhere arrives as zero)
        let imask = if self.trim.is_some() { tris.iter().flat_map(|t| t.v.iter()).fold(0u16, |m, v| (0..16).filter(|&d| v.interp[d].iter().any(|x| x.to_bits() != 0)).fold(m, |m, d| m | 1 << d)) } else { 0 };
        let key = PipelineKey { samples: key_samples(self.surface_key(0, regs)), imask, ..Self::key(&ctx, regs, 0, 0, ps_hash, depth_only) };
        self.pipeline(key);
        let di = (ps_hash != 0).then(|| self.put_consts(regs, &ctx, 0, 0, (0, 0), 0, ps_hash, &textures));
        let uses_f = self.ps_modules.get(&ps_hash).is_some_and(|m| m.3.as_ref().is_none_or(|l| !l.is_empty()));
        let first = (self.batch.verts.len() / (VERTEX_FLOATS * 4)) as u32;
        self.batch.verts.extend(data.iter().flat_map(|f| f.to_le_bytes()));
        let c = ctx.constant;
        self.batch.draws.push(Rec { key, setup: textures, di, fetches: false, uses_f, indexed: false, first, count: (tris.len() * 3) as u32, scissor, blend: c });
        self.stats.pixels_drawn += tris.len() as u64; // triangles (pixel counts aren't read back)
    }

    /// Vertex shader on the GPU: guest indices, vertex memory and fetch constants are uploaded and
    /// the translated shader fetches and transforms. Returns false when the draw needs the CPU path.
    #[allow(clippy::too_many_arguments)]
    fn draw_gpu_vs(&mut self, regs: &[u32], mem: &mut dyn GuestMem, prim: u32, indices: &[u32], src: (u32, u32), vs: &[u32], ps: &[u32], depth_only: bool) -> bool {
        // strips and fans become a list in one buffer kept across draws, given back afterwards
        let mut tri = std::mem::take(&mut self.tri_scratch);
        let done = match triangle_list(prim, indices, &mut tri) {
            Some(list) => self.draw_gpu_vs_list(regs, mem, prim, list, src, vs, ps, depth_only),
            None => false,
        };
        self.tri_scratch = tri;
        done
    }

    /// `draw_gpu_vs` for a triangle list.
    #[allow(clippy::too_many_arguments)]
    fn draw_gpu_vs_list(&mut self, regs: &[u32], mem: &mut dyn GuestMem, prim: u32, list: &[u32], src: (u32, u32), vs: &[u32], ps: &[u32], depth_only: bool) -> bool {
        let mut ctx = RasterCtx::new(regs);
        ctx.depth_only = depth_only;
        let vs_hash = self.vs_module(vs);
        // vertex memory: one window over every vertex buffer the shader fetches from
        let window = self.vertex_window(regs, vs_hash);
        if window.is_some_and(|(lo, hi)| hi - lo > MAX_VMEM) {
            return false;
        }
        if list.is_empty() || (ctx.cull_front && ctx.cull_back) {
            return true;
        }
        let (ps_hash, textures) = {
            let _p = crate::prof::span(crate::prof::Cat::RtPsSetup);
            self.ps_setup(regs, mem, ps, depth_only)
        };
        let Some(scissor) = self.begin(&ctx, regs) else { return true };
        let cull = (ctx.cull_front as u32) | ((ctx.cull_back as u32) << 1) | ((ctx.cw_front as u32) << 2);
        let key = PipelineKey { samples: key_samples(self.surface_key(0, regs)), ..Self::key(&ctx, regs, vs_hash, cull, ps_hash, depth_only) };
        self.pipeline(key);
        let vmem_base = window.map_or(0, |w| w.0);
        let (mut fetch_idx, mut vmem) = (0, (0, 0));
        let _vw = crate::prof::span(crate::prof::Cat::RtVwin);
        if let Some((lo, hi)) = window {
            // A match draws each player's parts from the same few vertex buffers hundreds of times
            // a frame (~90 MB of copies and uploads a frame): a window already in the batch is
            // reused when a sample of its bytes (64 of every 4 KB) still matches.
            let len = hi - lo;
            // Resolves still on their way back over this window (post-processing fetches vertex
            // data from a bloom level): their bytes are copied on the GPU at flush, instead of
            // waiting for the readback. Unaligned overlaps (copies need 4-byte offsets) wait.
            let (plo, phi) = (crate::gpu::phys(lo), crate::gpu::phys(lo).saturating_add(len));
            let overlaps: Vec<(wgpu::Buffer, u32, u32, u32)> = self
                .pending
                .iter()
                .filter(|r| r.dest < phi && r.dest.saturating_add(r.len as u32) > plo)
                .map(|r| {
                    let (a, z) = (r.dest.max(plo), r.dest.saturating_add(r.len as u32).min(phi));
                    (r.src.clone(), a - r.dest, a - plo, z - a)
                })
                .collect();
            let overlaps = if overlaps.iter().all(|o| o.1 % 4 == 0 && o.2 % 4 == 0 && o.3 % 4 == 0) {
                overlaps
            } else {
                self.complete(mem, plo, phi);
                Vec::new()
            };
            let sample = vmem_sample(mem, crate::gpu::phys(lo), len);
            let words_len = (len as usize).div_ceil(4).max(1) as u32;
            let b = &mut self.batch;
            vmem = match b.vmem_windows.get(&(lo, len)) {
                Some(&(off, h)) if h == sample && overlaps.is_empty() => (off, words_len),
                _ => {
                    let off = b.vmem.len() as u32;
                    b.vmem.resize(b.vmem.len() + words_len as usize, 0);
                    mem.read(crate::gpu::phys(lo), &mut words_mut(&mut b.vmem[off as usize..])[..len as usize]);
                    if overlaps.is_empty() {
                        b.vmem_windows.insert((lo, len), (off, sample));
                    }
                    for (src, so, vo, n) in overlaps {
                        b.vmem_patches.push((src, so as u64, off as u64 * 4 + vo as u64, n as u64));
                    }
                    (off, words_len)
                }
            };
            let fetch = &regs[FETCH_BASE..FETCH_BASE + 192];
            let hash = fast_hash(fetch.iter().map(|&w| w as u64));
            fetch_idx = Batch::intern(&mut b.fetch, &mut b.fetch_by_hash, hash, fetch);
        }
        drop(_vw);
        let di = {
            let _p = crate::prof::span(crate::prof::Cat::RtConsts);
            Some(self.put_consts(regs, &ctx, vmem_base, fetch_idx, vmem, vs_hash, ps_hash, &textures))
        };
        let _rec = crate::prof::span(crate::prof::Cat::RtRecord);
        let uses = |sl: Option<&Option<Vec<u32>>>| sl.is_some_and(|s| s.as_ref().is_none_or(|l| !l.is_empty()));
        let uses_f = uses(self.vs_modules.get(&vs_hash).map(|m| &m.2)) || uses(self.ps_modules.get(&ps_hash).map(|m| &m.3));
        let b = &mut self.batch;
        let src_key = (src.0, src.1, prim);
        let first = match b.idx_by_src.get(&src_key) {
            Some(&(f, n)) if n as usize == list.len() && b.indices[f as usize..(f + n) as usize] == *list => f,
            _ => {
                let f = b.indices.len() as u32;
                b.indices.extend_from_slice(&list);
                b.idx_by_src.insert(src_key, (f, list.len() as u32));
                f
            }
        };
        let c = ctx.constant;
        self.batch.draws.push(Rec { key, setup: textures, di, fetches: window.is_some(), uses_f, indexed: true, first, count: list.len() as u32, scissor, blend: c });
        self.stats.pixels_drawn += list.len() as u64 / 3;
        true
    }

    /// Submit the recorded draws: one vertex, index and storage buffer, one render pass.
    fn flush(&mut self) {
        crate::zone!("wgpu flush");
        let _p = crate::prof::span(crate::prof::Cat::RtFlush);
        self.submit_mips(); // the batch may sample textures created for it
        let batch = std::mem::take(&mut self.batch);
        let (Some((color_key, depth_key)), false) = (batch.target, batch.draws.is_empty()) else {
            // nothing to draw: keep the allocations (every resolve flushes first, mostly with
            // nothing recorded, and dropping them made each batch regrow its vectors)
            self.batch = batch.recycle();
            return;
        };
        let t0 = std::time::Instant::now();
        crate::prof::count(crate::prof::Cat::Flushes);
        let color = self.surfaces[&color_key].view.clone();
        let depth = self.depths[&depth_key].view.clone();
        if let Some(e) = self.encoder.as_mut() {
            // encoded and submitted on the encoder thread, in order with every other submit
            let mut used: HashMap<PipelineKey, wgpu::RenderPipeline> = HashMap::default();
            for r in &batch.draws {
                used.entry(r.key).or_insert_with(|| self.pipelines[&r.key].clone());
            }
            e.send(Job::Flush { batch, pipelines: used, color, depth });
            self.batch = e.back.try_recv().unwrap_or_default();
        } else {
            let tw = self.ts(0);
            self.batch = encode_batch(&self.device, &self.queue, &mut self.uploads, batch, &self.pipelines, &color, &depth, tw);
        }
        self.stats.timing[1] += t0.elapsed();
    }

    /// Submit `cmd` in order with the batches (through the encoder thread when there is one),
    /// then map `maps` for reading (each flag set once its copy can be read).
    fn submit_ordered(&mut self, cmd: wgpu::CommandBuffer, maps: Vec<(wgpu::Buffer, Arc<std::sync::atomic::AtomicBool>)>) {
        if let Some(enc) = self.mip_enc.take() {
            // pending mips first: `cmd` may read a texture created before it
            self.submit_ordered(enc.finish(), Vec::new());
        }
        match self.encoder.as_mut() {
            Some(e) => e.send(Job::Submit { cmd, maps }),
            None => {
                self.queue.submit([cmd]);
                for (buf, flag) in maps {
                    buf.slice(..).map_async(wgpu::MapMode::Read, move |r| flag.store(r.is_ok(), std::sync::atomic::Ordering::Release));
                }
            }
        }
    }

    /// Wait until the encoder thread has submitted everything sent to it (before waiting on the
    /// GPU, or writing or reading a resource an earlier batch may still use).
    fn join_encoder(&self) {
        if let Some(e) = &self.encoder {
            e.join();
        }
    }

    /// Write every pending readback that overlaps guest physical [lo, hi) into memory, in
    /// submission order (so a newer copy of the same bytes lands last). Waits for the GPU once.
    fn complete(&mut self, mem: &mut dyn GuestMem, lo: u32, hi: u32) {
        if !self.pending.iter().any(|r| r.dest < hi && r.dest.saturating_add(r.len as u32) > lo) {
            return;
        }
        self.join_encoder(); // the copies must be submitted (and mapped) before waiting for them
        let t0 = std::time::Instant::now();
        crate::zone!("wgpu complete wait");
        let p = crate::prof::span(crate::prof::Cat::GpuWait);
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });
        drop(p);
        let mut i = 0;
        while i < self.pending.len() {
            let r = &self.pending[i];
            if r.dest < hi && r.dest.saturating_add(r.len as u32) > lo {
                let r = self.pending.remove(i);
                let bytes = r.read.slice(..).get_mapped_range();
                crate::prof::add(crate::prof::Cat::LandBytes, r.len as u64);
                let hash = self.land(mem, r.dest, &bytes[..r.len]);
                self.soft.invalidate_textures(r.dest, r.len as u32);
                self.landed(r.dest, &bytes[..r.len], hash);
                drop(bytes);
                self.recycle_readback(r.read);
            } else {
                i += 1;
            }
        }
        self.stats.timing[2] += t0.elapsed();
    }

    /// Land the readbacks that have finished, oldest first, without waiting.
    /// Timestamp queries for a pass of category `cat` (`GPU_CATS`), when LMA_GPU_TIMING is on.
    fn ts(&self, cat: usize) -> Option<(wgpu::QuerySet, u32)> {
        self.timer.borrow_mut().as_mut().and_then(|t| t.take(cat))
    }

    fn land_ready(&mut self, mem: &mut dyn GuestMem) {
        if self.pending.is_empty() {
            return;
        }
        let _p = crate::prof::span(crate::prof::Cat::RtLand);
        let _ = self.device.poll(wgpu::PollType::Poll);
        while self.pending.first().is_some_and(|r| r.ready.load(std::sync::atomic::Ordering::Acquire)) {
            let r = self.pending.remove(0);
            let bytes = r.read.slice(..).get_mapped_range();
            crate::prof::add(crate::prof::Cat::LandBytes, r.len as u64);
            let hash = self.land(mem, r.dest, &bytes[..r.len]);
            self.soft.invalidate_textures(r.dest, r.len as u32);
            self.landed(r.dest, &bytes[..r.len], hash);
            drop(bytes);
            self.recycle_readback(r.read);
        }
    }

    fn complete_all(&mut self, mem: &mut dyn GuestMem) {
        self.complete(mem, 0, u32::MAX);
    }

    /// Write a resolve's bytes to guest memory. With a write watch, its page stamp afterwards
    /// (`Err`): nothing to hash. Without one, the bytes' hash (`Ok`), as `alias_valid` compares.
    fn land(&mut self, mem: &mut dyn GuestMem, dest: u32, bytes: &[u8]) -> Result<u64, u32> {
        if debug_env(&LAND_LOG, "LMA_LAND_LOG") {
            eprintln!("LMA_LAND_LOG: rt frame {} dest {dest:#010x} {} KB", self.frame, bytes.len() / 1024);
        }
        if self.land_watch {
            for (k, chunk) in bytes.chunks(1 << 16).enumerate() {
                mem.write(dest.wrapping_add((k << 16) as u32), chunk);
            }
            match self.soft.range_stamp(mem, dest, bytes.len() as u32) {
                Some(stamp) => return Err(stamp),
                None => self.land_watch = false, // no watch after all: hash from here on
            }
        }
        Ok(land_bytes(mem, dest, bytes))
    }

    /// A resolve's bytes are now in guest memory: keep sampling its GPU copy for as long as
    /// that memory is unchanged (`alias_valid`), instead of decoding and uploading it again.
    fn landed(&mut self, dest: u32, bytes: &[u8], hash: Result<u64, u32>) {
        self.tex_gen += 1;
        if let Some(r) = self.resolved.get_mut(&dest) {
            match hash {
                Ok(hash) => {
                    let mut sampled = Vec::new();
                    sample(bytes.len(), |off, n| sampled.extend_from_slice(&bytes[off..off + n]));
                    r.landed = Some((bytes.len() as u32, hash, raster::bytes_hash(&sampled)));
                    r.land_stamp = None;
                }
                Err(stamp) => {
                    r.landed = Some((bytes.len() as u32, 0, 0));
                    r.land_stamp = Some(stamp);
                }
            }
            (r.checked_run, r.checked_frame) = (self.run, self.frame);
        }
    }

    /// Whether the resolve at `dest` may still be sampled from its GPU copy: always before its
    /// readback lands; afterwards while guest memory still holds exactly what landed (checked
    /// once per command-processor run).
    fn alias_valid(&mut self, mem: &mut dyn GuestMem, dest: u32) -> bool {
        let Some(r) = self.resolved.get_mut(&dest) else { return false };
        let Some((len, hash, sample_hash)) = r.landed else { return true };
        if r.checked_run == self.run {
            return true;
        }
        if let Some(stamp) = r.land_stamp {
            // the write watch: valid while none of its pages was written after it landed (a
            // write of the same bytes counts too: then it's decoded from memory, the same image)
            let now = self.soft.range_stamp(mem, dest, len);
            let Some(r) = self.resolved.get_mut(&dest) else { return false };
            if now.is_some_and(|n| n <= stamp) {
                (r.checked_run, r.checked_frame) = (self.run, self.frame);
                return true;
            }
            if alias_log() {
                eprintln!("LMA_ALIAS_LOG: guest frame {} resolve {dest:#010x} ({}x{} fmt {}) dropped: guest memory written since it landed", crate::threaded::GUEST_FRAME.load(std::sync::atomic::Ordering::Relaxed), r.w, r.h, r.format);
            }
            self.resolved.remove(&dest);
            return false;
        }
        // Rehashing every resolve in full each run was ~12% of team select's emulation thread on
        // the PC (the command processor stops many times a frame). Runs within a frame check a
        // sample (64 bytes of every KB: anything that rewrites the texture touches it); each
        // frame's first check compares every byte.
        let full = r.checked_frame != self.frame;
        let now = if full {
            // streamed through one reused 64 KB buffer: a fresh render-target-sized Vec per check
            // (allocated, zeroed, filled, hashed) was a large part of landing on the render
            // thread (#304); the hash is the same (`land_bytes` streams it the same way)
            let mut h = raster::Hasher::default();
            let chunk = &mut self.hash_chunk;
            chunk.resize(1 << 16, 0);
            for off in (0..len as usize).step_by(1 << 16) {
                let n = (len as usize - off).min(1 << 16);
                mem.read(dest + off as u32, &mut chunk[..n]);
                h.update(&chunk[..n]);
            }
            h.finish(len as usize)
        } else {
            let mut buf = Vec::with_capacity(len as usize / 16 + 64);
            let mut line = [0u8; 64];
            sample(len as usize, |off, n| {
                mem.read(dest + off as u32, &mut line[..n]);
                buf.extend_from_slice(&line[..n]);
            });
            raster::bytes_hash(&buf)
        };
        let Some(r) = self.resolved.get_mut(&dest) else { return false };
        if now == if full { hash } else { sample_hash } {
            r.checked_run = self.run;
            if full {
                r.checked_frame = self.frame;
            }
            return true;
        }
        if alias_log() {
            eprintln!("LMA_ALIAS_LOG: guest frame {} resolve {dest:#010x} ({}x{} fmt {}) dropped: guest memory no longer matches ({})", crate::threaded::GUEST_FRAME.load(std::sync::atomic::Ordering::Relaxed), r.w, r.h, r.format, if full { "full" } else { "sample" });
        }
        self.resolved.remove(&dest);
        false
    }

    /// Guest window [lo, hi) over every vertex buffer the vertex shader fetches from.
    fn vertex_window(&mut self, regs: &[u32], vs_hash: u64) -> Option<(u32, u32)> {
        let mut window: Option<(u32, u32)> = None;
        for &k in &self.vs_modules[&vs_hash].1 {
            let (d0, d1) = (regs[FETCH_BASE + 2 * k as usize], regs[FETCH_BASE + 2 * k as usize + 1]);
            let (base, len) = (d0 & !3, ((d1 >> 2) & 0xFF_FFFF) * 4);
            let (lo, hi) = window.unwrap_or((base, base));
            window = Some((lo.min(base), hi.max(base.saturating_add(len))));
        }
        window
    }

    /// A texture that no single resolve matches exactly but resolves cover on the GPU: a target
    /// resolved in bands (rows 0..n, n.., each starting on a tile row, since a row of 32x32
    /// tiles is one contiguous run of memory), or a resolve a few rows or columns larger than
    /// the texture (bloom levels). Copies their GPU copies into one texture of the texture's
    /// size, kept while every part stays valid, so draws sample it instead of waiting for the
    /// readbacks and decoding them on the CPU.
    fn composed_view(&mut self, mem: &mut dyn GuestMem, t: &raster::TexInfo) -> Option<wgpu::TextureView> {
        let base = crate::gpu::phys(t.base);
        if !t.tiled || t.width == 0 || t.height == 0 {
            return None;
        }
        let pitch_aligned = (t.pitch.max(t.width) + 31) & !31;
        let row_bytes = pitch_aligned * 4;
        let end = base.saturating_add(row_bytes * t.height);
        let mut cands: Vec<u32> = self
            .resolved
            .iter()
            .filter(|(&d, r)| d >= base && d < end && (d - base) % (row_bytes * 32) == 0 && r.format == t.format && r.endian == t.endian && r.pitch_aligned == pitch_aligned && r.w >= t.width)
            .map(|(&d, _)| d)
            .collect();
        cands.sort_unstable();
        let mut parts = Vec::new();
        let mut covered = 0;
        for d in cands {
            let row = (d - base) / row_bytes;
            if row > covered {
                break;
            }
            if !self.alias_valid(mem, d) {
                continue;
            }
            let r = &self.resolved[&d];
            if row + r.h > covered {
                covered = row + r.h;
                parts.push((d, r.content));
            }
            if covered >= t.height {
                break;
            }
        }
        if covered < t.height {
            if alias_log() {
                eprintln!("LMA_ALIAS_LOG: guest frame {} compose {base:#010x} {}x{} fmt {}: {covered} of {} rows from valid resolves, decoded from memory", crate::threaded::GUEST_FRAME.load(std::sync::atomic::Ordering::Relaxed), t.width, t.height, t.format, t.height);
            }
            return None;
        }
        let desc = (t.width, t.height, t.format, t.endian, pitch_aligned);
        let swz = t.swizzle & 0xFFF;
        if let Some(c) = self.composed.get(&base).filter(|c| c.desc == desc && c.parts == parts) {
            if swz == 0x688 {
                return Some(c.view.clone()); // identity: x, y, z, w
            }
            if let Some(v) = c.swizzled.get(&swz) {
                return Some(v.clone());
            }
            let view = c.view.clone();
            let v = self.swizzle(&view, t.width * res_scale(), t.height * res_scale(), swz);
            self.composed.get_mut(&base).unwrap().swizzled.insert(swz, v.clone());
            return Some(v);
        }
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("resolved texture (composed)"),
            size: wgpu::Extent3d { width: t.width * res_scale(), height: t.height * res_scale(), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Submitted now, ahead of the open batch, like `swizzled_view`: the resolves it reads
        // were submitted by their own flushes, and only draws recorded from here on sample it.
        let mut enc = self.device.create_command_encoder(&Default::default());
        for &(d, _) in &parts {
            let r = &self.resolved[&d];
            let row = (d - base) / row_bytes;
            let rows = r.h.min(t.height - row);
            let k = res_scale();
            enc.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo { texture: &r.tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                wgpu::TexelCopyTextureInfo { texture: &tex, mip_level: 0, origin: wgpu::Origin3d { x: 0, y: row * k, z: 0 }, aspect: wgpu::TextureAspect::All },
                wgpu::Extent3d { width: t.width * k, height: rows * k, depth_or_array_layers: 1 },
            );
        }
        self.submit_ordered(enc.finish(), Vec::new());
        let view = tex.create_view(&Default::default());
        let mut swizzled = HashMap::default();
        let out = if swz == 0x688 {
            view.clone()
        } else {
            let v = self.swizzle(&view, t.width * res_scale(), t.height * res_scale(), swz);
            swizzled.insert(swz, v.clone());
            v
        };
        self.composed.insert(base, Composed { desc, parts, view, swizzled });
        Some(out)
    }

    /// The resolve at `dest` as the texture decoder would return it with fetch swizzle `swz`.
    fn swizzled_view(&mut self, dest: u32, swz: u32) -> wgpu::TextureView {
        let r = &self.resolved[&dest];
        if swz == 0x688 {
            return r.view.clone(); // identity: x, y, z, w
        }
        if let Some(v) = r.swizzled.get(&swz) {
            return v.clone();
        }
        let (src, w, h) = (r.view.clone(), r.w * res_scale(), r.h * res_scale());
        let view = self.swizzle(&src, w, h, swz);
        self.resolved.get_mut(&dest).unwrap().swizzled.insert(swz, view.clone());
        view
    }

    /// A copy of the `w` x `h` texture `src` with fetch swizzle `swz` applied.
    fn swizzle(&mut self, src: &wgpu::TextureView, w: u32, h: u32, swz: u32) -> wgpu::TextureView {
        let tex = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("resolved texture (swizzled)"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = tex.create_view(&Default::default());
        let ubuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("swizzle"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
        self.queue.write_buffer(&ubuf, 0, &[swz, 0, 0, 0].iter().flat_map(|v: &u32| v.to_le_bytes()).collect::<Vec<u8>>());
        let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.swizzle_pipe.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: ubuf.as_entire_binding() },
            ],
        });
        // Submitted now, ahead of the open batch: the resolve it reads was submitted by its own
        // flush, and only draws recorded from here on sample the new texture, so the batch need
        // not be flushed (that split batches mid-frame).
        let mut enc = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.swizzle_pipe);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
        }
        self.submit_ordered(enc.finish(), Vec::new());
        view
    }

    /// The front buffer straight from its GPU resolve, when the CPU present would decode exactly
    /// that (same texture description as raster::Renderer::present_inner's fast path). Only this
    /// image is read back; other resolve copies stay in flight.
    /// The front buffer composed on the GPU: None when it can't be (not one of this frame's
    /// resolves; the software present runs instead), Some(None) when it was presented but no
    /// image is ready yet (real time: the previous frame's read-back is still on its way).
    fn present_gpu(&mut self, regs: &[u32], fb: u32, w: u32, h: u32) -> Option<Option<Image>> {
        self.join_encoder();
        let mut t = raster::TexInfo::from_fetch(&regs[FETCH_BASE..FETCH_BASE + 6]);
        t.base = fb & !0xFFF;
        if t.width < 2 {
            t.width = w;
            t.height = h;
        }
        let (w, h) = (w.min(4096), h.min(4096));
        // the resolves this frame that are bands of this texture: same layout, starting on a
        // tile-row boundary inside it (a row of 32x32 tiles is one contiguous run of memory)
        let base = crate::gpu::phys(t.base);
        let pitch_aligned = (t.pitch.max(t.width) + 31) & !31;
        let row_bytes = pitch_aligned * 4;
        let mut parts: Vec<(u32, u32, wgpu::TextureView, u64)> = self
            .frame_resolves
            .iter()
            .filter(|(&d, r)| d >= base && (d - base) % (row_bytes * 32) == 0 && r.1 == t.endian && r.2 == pitch_aligned && r.3 >= w)
            .map(|(&d, r)| ((d - base) / row_bytes, r.4, r.0.clone(), r.5))
            .collect();
        parts.sort_by_key(|p| p.0);
        // they must cover rows 0..h in order (later resolves of the same band replaced earlier
        // ones), and are drawn in resolve order
        let (covered, draw) = compose_plan(&parts.iter().map(|p| (p.0, p.1, p.3)).collect::<Vec<_>>(), h);
        let fits = w <= t.width && h <= t.height && covered >= h;
        // LMA_COMPOSE_LOG=1: the presents the old row order composed wrongly (an older part over rows
        // a newer one also covers, drawn after it because it starts lower)
        // (=2: also every present whose parts aren't two, with their rows and order, and every 300
        // presents how many were composed here and why the others weren't; =3 below)
        static COMPOSE_LOG: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let level = *COMPOSE_LOG.get_or_init(|| std::env::var("LMA_COMPOSE_LOG").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
        // =3: also the colour resolves into the picture's memory that the compose leaves out
        // (misaligned, another endian or pitch, narrower): memory holds them, the compose doesn't
        if level >= 3 {
            let end = base + h * row_bytes;
            for (&d, r) in self.frame_resolves.iter().filter(|(&d, _)| d >= base && d < end) {
                let used = (d - base) % (row_bytes * 32) == 0 && r.1 == t.endian && r.2 == pitch_aligned && r.3 >= w;
                if !used {
                    eprintln!("LMA_COMPOSE_LOG: frame {} resolve at row {} left out ({}x{}, endian {} vs {}, pitch {} vs {pitch_aligned}, order {})", self.frame, (d - base) / row_bytes, r.3, r.4, r.1, t.endian, r.2, r.5);
                }
            }
        }
        static TALLY: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
        let why = if t.format != 6 || !t.tiled { 1 } else if covered < h { 2 } else if !fits || w == 0 || h == 0 { 3 } else { 0 };
        if level >= 2 {
            TALLY[why].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if self.frame % 300 == 0 {
                let n = |i: usize| TALLY[i].load(std::sync::atomic::Ordering::Relaxed);
                eprintln!("LMA_COMPOSE_LOG: presents composed {}, fell back: format/tiling {}, rows not covered {} (covered {covered} of {h}, {} parts), size {}", n(0), n(1), n(2), parts.len(), n(3));
            }
        }
        if why != 0 {
            return None;
        }
        self.flush(); // the frame's draws first
        if present_join() {
            self.join_encoder(); // ... and submitted, before the compose is (`present_join`)
        }
        let t0 = std::time::Instant::now();
        // the picture at resolution scale k: k x k pixels per guest pixel (`res_scale`)
        let k = res_scale();
        let (w, h) = (w * k, h * k);
        let len = (w * h * 4) as u64;
        let (dst, read) = match self.present_bufs.iter().position(|b| b.0 == len) {
            Some(i) => {
                let (_, dst, read) = self.present_bufs.swap_remove(i);
                (dst, read)
            }
            None => (
                self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("present"), size: len, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false }),
                self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("present readback"), size: len, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
            ),
        };
        // composed in resolve order, so rows two resolves share show the later one, as in memory: the
        // game resolves scratch targets into the front buffer's memory before the frame's own
        // tiles overwrite them, and composing by start row drew such a scratch band over the lower
        // tile (the maintainer's video of 13 Oct: one-frame purple bands over the lower pitch, #540)
        if level >= 2 && parts.len() != 2 {
            let list: Vec<String> = parts.iter().map(|p| format!("{}..{}#{}", p.0, p.0 + p.1, p.3)).collect();
            eprintln!("LMA_COMPOSE_LOG: frame {} {} parts for {w}x{h}: {}", self.frame, parts.len(), list.join(" "));
        }
        if level >= 1 {
            for (i, a) in parts.iter().enumerate() {
                for b in &parts[i + 1..] {
                    let (old, new) = if a.3 < b.3 { (a, b) } else { (b, a) };
                    if old.0 > new.0 && old.0 < new.0 + new.1 {
                        eprintln!("LMA_COMPOSE_LOG: frame {} rows {}..{} (older) over {}..{} (newer)", self.frame, old.0, old.0 + old.1, new.0, new.0 + new.1);
                    }
                }
            }
        }
        let mut enc = self.device.create_command_encoder(&Default::default());
        for (row, rows, view, _) in draw.iter().map(|&i| &parts[i]) {
            let (row, rows) = (row * k, (*rows * k).min(h - row * k));
            let ubuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("present params"), size: 16, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            self.queue.write_buffer(&ubuf, 0, &[w, rows, t.swizzle & 0xFFF, row].iter().flat_map(|v: &u32| v.to_le_bytes()).collect::<Vec<u8>>());
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.present_pipe.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) },
                    wgpu::BindGroupEntry { binding: 1, resource: dst.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 2, resource: ubuf.as_entire_binding() },
                ],
            });
            let tw = self.ts(5);
            let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::ComputePassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }) });
            pass.set_pipeline(&self.present_pipe);
            pass.set_bind_group(0, &bg, &[]);
            pass.dispatch_workgroups(w.div_ceil(8), rows.div_ceil(8), 1);
        }
        // LMA_SHARED_GPU=1: the frame stays on the GPU; the window copies it into its screen texture
        // (rows of w x 4 bytes must be a multiple of 256 for that copy; else the read-back below)
        if SHARED_PRESENT.load(std::sync::atomic::Ordering::Relaxed) && !READBACK.load(std::sync::atomic::Ordering::Relaxed) && (w * 4) % 256 == 0 {
            self.queue.submit([enc.finish()]);
            // the read-back buffer goes back unused, paired with a fresh compose target
            self.present_bufs.push((len, take_gpu_frame_buf(&self.device, len), read));
            if let Some((old, ..)) = GPU_FRAME.lock().unwrap().replace((dst, w, h)) {
                give_gpu_frame_buf(old); // superseded before the window took it
            }
            self.stats.timing[3] += t0.elapsed();
            return Some(Some((w, h, Vec::new())));
        }
        enc.copy_buffer_to_buffer(&dst, 0, &read, 0, len);
        let index = self.queue.submit([enc.finish()]);
        if self.async_fence {
            // real time: this frame lands while the next one is drawn; show the one before
            let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let flag = ready.clone();
            read.slice(..).map_async(wgpu::MapMode::Read, move |r| flag.store(r.is_ok(), std::sync::atomic::Ordering::Release));
            self.present_pending.push_back((read, ready, w, h, dst));
            let landed = |q: &std::collections::VecDeque<(wgpu::Buffer, Arc<std::sync::atomic::AtomicBool>, u32, u32, wgpu::Buffer)>| {
                q.iter().rposition(|p| p.1.load(std::sync::atomic::Ordering::Acquire))
            };
            let _ = self.device.poll(wgpu::PollType::Poll);
            let mut newest = landed(&self.present_pending);
            if newest.is_none() && self.present_pending.len() > present_queue() {
                // too far behind: wait for the oldest
                let _p = (crate::prof::span(crate::prof::Cat::GpuWait), crate::prof::span(crate::prof::Cat::GpuWaitPresent));
                while newest.is_none() {
                    let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_millis(1)) });
                    newest = landed(&self.present_pending);
                }
            }
            let Some(i) = newest else { return Some(None) }; // still drawing: the window keeps its frame
            // the newest landed frame; older ones are superseded
            let (buf, _, pw, ph, dst) = self.present_pending.drain(..=i).last().unwrap();
            let mut rgba = take_frame_buf();
            rgba.extend_from_slice(&buf.slice(..).get_mapped_range());
            self.recycle_present(dst, buf);
            self.stats.timing[3] += t0.elapsed();
            return Some(Some((pw, ph, rgba)));
        }
        read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let p = crate::prof::span(crate::prof::Cat::GpuWait);
        let pp = crate::prof::span(crate::prof::Cat::GpuWaitPresent);
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: Some(index), timeout: None });
        drop((p, pp));
        let mut rgba = take_frame_buf();
        rgba.extend_from_slice(&read.slice(..).get_mapped_range());
        self.recycle_present(dst, read);
        self.stats.timing[3] += t0.elapsed();
        Some(Some((w, h, rgba)))
    }

    /// Keep a present's buffers for a later frame of the same size (the read-back unmapped; the
    /// GPU is done with both once its copy has been read).
    fn recycle_present(&mut self, dst: wgpu::Buffer, read: wgpu::Buffer) {
        read.unmap();
        if self.present_bufs.len() < present_queue() + 2 {
            self.present_bufs.push((read.size(), dst, read));
        }
    }

    /// A landed read-back's buffer, kept for a later resolve of the same size (#746).
    fn recycle_readback(&mut self, read: wgpu::Buffer) {
        read.unmap();
        if self.readback_bufs.len() < 16 {
            self.readback_bufs.push(read);
        }
    }

    /// A read-back buffer of `len` bytes: a recycled one of that size, else a new one.
    fn take_readback_buf(&mut self, len: u64) -> wgpu::Buffer {
        match self.readback_bufs.iter().position(|b| b.size() == len) {
            Some(i) => self.readback_bufs.swap_remove(i),
            None => self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("resolve readback"), size: len, usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false }),
        }
    }

    fn color_fill(&mut self, (format, samples): (Fmt, u32)) -> &wgpu::RenderPipeline {
        if !self.color_fill.contains_key(&(format, samples)) {
            let attrs = [
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 0, shader_location: 0 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 16, shader_location: 1 },
            ];
            let pipeline = self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("colour fill"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &self.color_fill_module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout { array_stride: 32, step_mode: wgpu::VertexStepMode::Vertex, attributes: &attrs }],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState { count: samples, ..Default::default() },
                fragment: Some(wgpu::FragmentState {
                    module: &self.color_fill_module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState { format: format.wgpu(), blend: None, write_mask: wgpu::ColorWrites::ALL })],
                }),
                multiview_mask: None,
                cache: None,
            });
            self.color_fill.insert((format, samples), pipeline);
        }
        &self.color_fill[&(format, samples)]
    }

    /// Resolve with the GPU: the copy is a compute pass straight from the surface into the guest's
    /// tiled layout (one download, one contiguous memory write), clears are GPU fills. The
    /// software EDRAM is not updated (save states keep it as of the last software resolve).
    fn gpu_resolve(&mut self, regs: &[u32], mem: &mut dyn GuestMem, rect: [u32; 4]) {
        crate::zone!("wgpu resolve");
        let _p = crate::prof::span(crate::prof::Cat::RtResolve);
        self.tex_gen += 1;
        let [x0, y0, x1, y1] = rect;
        let pitch = (regs[RB_SURFACE_INFO] & 0x3FFF).max(1);
        let color_base = self.surface_key(regs[RB_COLOR_INFO] & 0xFFF, regs);
        let depth_base = self.surface_key(regs[RB_DEPTH_INFO] & 0xFFF, regs);
        let ctl = regs[RB_COPY_CONTROL];
        let from_depth = ctl & 7 == 4;
        let dest = regs[RB_COPY_DEST_BASE];
        let dpitch = regs[RB_COPY_DEST_PITCH] & 0x3FFF;
        let dheight = (regs[RB_COPY_DEST_PITCH] >> 16) & 0x3FFF;
        let dinfo = regs[RB_COPY_DEST_INFO];
        let fmt = (dinfo >> 7) & 0x3F;
        let copy_cmd = (ctl >> 20) & 3;
        if debug_env(&RESOLVE_LOG, "LMA_RESOLVE_LOG") {
            eprintln!("LMA_RESOLVE_LOG: guest frame {} dest {dest:#010x} rect {x0},{y0}-{x1},{y1} dpitch {dpitch} dheight {dheight} fmt {fmt} depth {from_depth} cmd {copy_cmd} src {color_base:#x}", crate::threaded::GUEST_FRAME.load(std::sync::atomic::Ordering::Relaxed));
        }
        self.flush();
        let mut enc = self.device.create_command_encoder(&Default::default());
        let mut readback = None;
        if copy_cmd != 3 && dest != 0 && x1 > x0 && y1 > y0 && (from_depth || fmt == 6) {
            let pitch_aligned = (dpitch + 31) & !31;
            let rows = if dheight != 0 { dheight } else { y1 - y0 };
            let full = (pitch_aligned * ((rows + 31) & !31) * 4) as usize;
            // A row of 32x32 tiles is one contiguous run of guest memory. When the copy spans the
            // whole width, every tile row it fully covers is overwritten, so only a partly covered
            // last tile row needs the current guest bytes, and only the written tile rows go back.
            // Otherwise the whole texture is pre-loaded so texels outside the rectangle survive.
            let written_rows = (y1 - y0).min(rows) as usize;
            let tile_row = (pitch_aligned * 32 * 4) as usize;
            let (len, prefill) = if dpitch == pitch_aligned && x1 - x0 >= dpitch {
                let n = written_rows.div_ceil(32);
                ((n * tile_row).min(full), (written_rows % 32 != 0).then(|| (n - 1) * tile_row))
            } else {
                (full, Some(0))
            };
            let dst = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("resolve dst"), size: len as u64, usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            if let Some(from) = prefill.filter(|&from| from < len) {
                let lo = crate::gpu::phys(dest) + from as u32;
                let hi = lo + (len - from) as u32;
                // the guest's bytes, then on the GPU whatever resolves still on their way back will
                // write there (copied from their buffers, in submission order: the queue write runs
                // first). This used to wait for the whole GPU (about 3 waits and 9 ms a frame in a
                // match) to land those resolves in guest memory first.
                let mut bytes = vec![0u8; len - from];
                mem.read(lo, &mut bytes);
                self.queue.write_buffer(&dst, from as u64, &bytes);
                for r in self.pending.iter().filter(|r| r.dest < hi && r.dest.saturating_add(r.len as u32) > lo) {
                    let (s, e) = (r.dest.max(lo), r.dest.saturating_add(r.len as u32).min(hi));
                    enc.copy_buffer_to_buffer(&r.src, (s - r.dest) as u64, &dst, (from as u32 + (s - lo)) as u64, (e - s) as u64);
                }
            }
            let params: [u32; 12] = [x0, y0, x1, y1, dpitch, dheight, pitch_aligned, dinfo & 7, (dinfo >> 24) & 1, res_scale(), 0, 0];
            let pbuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("resolve params"), size: 48, usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            self.queue.write_buffer(&pbuf, 0, &params.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>());
            // the texels written, for draws that sample this resolve before it reaches guest memory
            let (aw, ah) = ((x1 - x0).min(dpitch), written_rows as u32);
            // the same dest resolved again at the same size: write into the previous texture (the
            // queue orders it after every draw already submitted that samples it), so its view
            // and the setups and bind groups holding it stay valid; swizzled copies are redone
            let dest_phys = crate::gpu::phys(dest);
            // never within one frame: a band resolved twice in a frame (post-processing reuses its
            // scratch buffers) is composed from both snapshots later in that frame (`composed_view`,
            // `frame_resolves`), and an in-place rewrite would show the second one in both places
            // (the maintainer, 12 Oct 08:30: a full-screen overlay with a blue tint at the bottom on the
            // first build with in-place reuse); LMA_RESOLVE_REUSE=0 turns the reuse off entirely
            let reuse = self.resolved.get(&dest_phys).filter(|r| reuse_allowed(resolve_reuse_on(), r.made_frame, self.frame, (r.w, r.h, r.format), (aw, ah, if from_depth { 22 } else { 6 }))).map(|r| (r.tex.clone(), r.view.clone(), r.gen));
            let kept_gen = reuse.as_ref().map(|r| r.2);
            RESOLVE_REUSE[reuse.is_some() as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if reuse.is_none() {
                if let Some(r) = self.resolved.get(&dest_phys) {
                    // same dest refused: this frame already resolved it (the suspected overlay case), or another size
                    RESOLVE_REUSE[if r.made_frame == self.frame { 3 } else { 2 }].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let alias = Some(if let Some((tex, view, _)) = reuse { (tex, view) } else {
                let tex = self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("resolved texture"),
                    size: wgpu::Extent3d { width: aw * res_scale(), height: ah * res_scale(), depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba32Float,
                    usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                });
                let view = tex.create_view(&Default::default());
                (tex, view)
            });
            let ms = key_samples(color_base) > 1;
            let (pipeline, view) = if from_depth {
                (if ms { &self.resolve_depth2 } else { &self.resolve_depth }, &self.depths[&(depth_base, pitch)].view)
            } else {
                (if ms { &self.resolve_color2 } else { &self.resolve_color }, &self.surfaces[&(color_base, pitch)].view)
            };
            let mut entries = vec![
                wgpu::BindGroupEntry { binding: 0, resource: pbuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: if from_depth { 3 } else { 1 }, resource: wgpu::BindingResource::TextureView(view) },
                wgpu::BindGroupEntry { binding: 2, resource: dst.as_entire_binding() },
            ];
            if let Some((_, a)) = &alias {
                entries.push(wgpu::BindGroupEntry { binding: 4, resource: wgpu::BindingResource::TextureView(a) });
            }
            let bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor { label: None, layout: &pipeline.get_bind_group_layout(0), entries: &entries });
            {
                let tw = self.ts(1);
                let mut pass = enc.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None, timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::ComputePassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }) });
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &bg, &[]);
                pass.dispatch_workgroups((x1 - x0).min(dpitch).div_ceil(8), (y1 - y0).div_ceil(8), 1);
            }
            // LMA_SKIP_LAND=<hex dest,...> (#171 experiment): these destinations keep their GPU copy only,
            // no read-back into guest memory (which resolves the guest never reads with the CPU)
            if !skip_land(dest_phys) {
                let read = self.take_readback_buf(len as u64);
                enc.copy_buffer_to_buffer(&dst, 0, &read, 0, len as u64);
                readback = Some((read, dst.clone(), len));
            }
            // a newer resolve over this memory replaces older aliases there
            self.resolved.retain(|&d, r| d == dest_phys || d.saturating_add(r.pitch_aligned * r.h * 4) <= dest_phys || d >= dest_phys + len as u32);
            match alias {
                Some((tex, view)) => {
                    if !from_depth {
                        self.frame_resolves.insert(dest_phys, (view.clone(), dinfo & 7, pitch_aligned, aw, ah, self.next_gen));
                    }
                    self.resolved.insert(dest_phys, Resolved { format: if from_depth { 22 } else { 6 }, tex, view, gen: kept_gen.unwrap_or(self.next_gen), content: self.next_gen, made_frame: self.frame, swizzled: HashMap::default(), endian: dinfo & 7, pitch_aligned, w: aw, h: ah, landed: None, land_stamp: None, checked_run: 0, checked_frame: 0 });
                    self.res_made.push((dest_phys, self.next_gen));
                    self.next_gen += 1;
                }
                None => {
                    self.resolved.remove(&dest_phys);
                }
            }
        }
        if ctl & (1 << 8) != 0 && x1 > x0 && y1 > y0 {
            let format = Fmt::from_color_info(regs[RB_COLOR_INFO]);
            self.surface(color_base, pitch, format);
            let c = raster::color_clear_value(regs);
            let mut tri = Vec::with_capacity(24);
            for p in [[-1.0f32, -1.0], [3.0, -1.0], [-1.0, 3.0]] {
                tri.extend([p[0], p[1], 0.0, 1.0, c[0], c[1], c[2], c[3]]);
            }
            let vbuf = self.device.create_buffer(&wgpu::BufferDescriptor { label: Some("colour fill"), size: 96, usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST, mapped_at_creation: false });
            self.queue.write_buffer(&vbuf, 0, &tri.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>());
            let fill_key = (format, key_samples(color_base));
            self.color_fill(fill_key);
            let view = &self.surfaces[&(color_base, pitch)].view;
            let tw = self.ts(2);
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("colour clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment { view, depth_slice: None, resolve_target: None, ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store } })],
                depth_stencil_attachment: None,
                timestamp_writes: tw.as_ref().map(|(q, i)| wgpu::RenderPassTimestampWrites { query_set: q, beginning_of_pass_write_index: Some(*i), end_of_pass_write_index: Some(*i + 1) }),
                occlusion_query_set: None,
                multiview_mask: None,
            });
            let [x, y, w, h] = scaled_rect(x0, y0, x1 - x0, y1 - y0, res_scale());
            pass.set_scissor_rect(x, y, w, h);
            pass.set_pipeline(&self.color_fill[&fill_key]);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.draw(0..3, 0..1);
        }
        // the copy lands in guest memory later (see `complete`): the CPU keeps recording draws
        // while the GPU works; its buffer is mapped once the copy has been submitted
        let readback = readback.map(|(read, src, len)| (Arc::new(std::sync::atomic::AtomicBool::new(false)), read, src, len));
        let maps = readback.iter().map(|(ready, read, _, _)| (read.clone(), ready.clone())).collect();
        self.submit_ordered(enc.finish(), maps);
        // the depth fill is submitted after the copy above, so a depth resolve reads the old values
        if ctl & (1 << 9) != 0 && x1 > x0 && y1 > y0 {
            self.clear_depth(depth_base, pitch, [x0, y0, x1, y1], raster::depth_clear_value(regs));
        }
        if let Some((ready, read, src, len)) = readback {
            let id = self.next_readback;
            self.next_readback += 1;
            self.pending.push(Readback { id, ready, read, src, dest: crate::gpu::phys(dest), len });
            // #746: when the GPU falls behind, read-backs pile up (187 at once in a night match,
            // each holding its own buffers: GBs of footprint). Past the cap, land what is ready
            // and, if still over, wait in 1 ms steps until enough of the oldest have come back
            // (not for the whole GPU); landing early is what `complete` does anyway.
            let cap = readback_cap();
            if cap > 0 && self.pending.len() > cap {
                self.land_ready(mem);
                if self.pending.len() > cap {
                    self.join_encoder(); // the oldest copies must be submitted to come back
                    let _p = crate::prof::span(crate::prof::Cat::GpuWait);
                    let t0 = std::time::Instant::now();
                    while self.pending.len() > cap && t0.elapsed() < std::time::Duration::from_secs(2) {
                        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_millis(1)) });
                        self.land_ready(mem);
                    }
                    if self.pending.len() > cap {
                        self.complete_all(mem); // never came back in 2 s: wait for everything
                    }
                }
            }
        }
    }

    fn resolve(&mut self, regs: &[u32], mem: &mut dyn GuestMem, verts: &[Vtx]) {
        // the rectangle exactly as raster::Renderer::resolve computes it
        let [x0, y0, x1, y1] = raster::resolve_rect(regs, verts);
        let pitch = (regs[RB_SURFACE_INFO] & 0x3FFF).max(1);
        let color_base = self.surface_key(regs[RB_COLOR_INFO] & 0xFFF, regs);
        let depth_base = self.surface_key(regs[RB_DEPTH_INFO] & 0xFFF, regs);
        let ctl = regs[RB_COPY_CONTROL];
        let from_depth = ctl & 7 == 4;
        let on_gpu = if from_depth { self.depths.contains_key(&(depth_base, pitch)) } else { self.surfaces.contains_key(&(color_base, pitch)) };
        if self.gpu_resolve && on_gpu {
            self.gpu_resolve(regs, mem, [x0, y0, x1.min(SURFACE), y1.min(SURFACE)]);
            self.stats.resolves += 1;
            return;
        }
        self.complete_all(mem);
        if from_depth {
            self.download(false, depth_base, pitch, x0, y0, x1, y1);
        } else {
            self.download(true, color_base, pitch, x0, y0, x1, y1);
        }
        self.soft.resolve(regs, mem, verts);
        self.stats.resolves += 1;
        // clears happened in the software EDRAM: push the rectangle back
        if ctl & (1 << 8) != 0 {
            self.surface(color_base, pitch, Fmt::from_color_info(regs[RB_COLOR_INFO]));
            self.upload_color(color_base, pitch, x0, y0, x1.min(SURFACE), y1.min(SURFACE));
        }
        if ctl & (1 << 9) != 0 && x1 > x0 && y1 > y0 {
            let z = self.soft.depth[self.soft.edram_index(depth_base, pitch, x0, y0)];
            self.clear_depth(depth_base, pitch, [x0, y0, x1.min(SURFACE), y1.min(SURFACE)], z);
        }
    }
}

impl RenderBackend for WgpuBackend {
    fn regs_dirty(&mut self, blocks: &[u64]) {
        // float constants: registers 0x4000.. (vertex) and 0x4400.. (pixel), 16 blocks of 64 each
        let w = blocks.get(FLOAT_BASE / 64 / 64).copied().unwrap_or(u64::MAX);
        let now = [w & 0xFFFF != 0, w & 0xFFFF_0000 != 0];
        let acc = self.floats_written.get_or_insert([false; 2]);
        acc[0] |= now[0];
        acc[1] |= now[1];
    }

    fn name(&self) -> &'static str {
        "wgpu"
    }

    fn draw(&mut self, regs: &[u32], mem: &mut dyn GuestMem, init: u32, index_base: u32, vs: &[u32], ps: &[u32]) {
        draw_trace(self.frame, regs, init, vs, ps);
        let prim = init & 0x3F;
        if !self.pending.is_empty() {
            // index buffer and vertex buffers may sit under a resolve still on its way back
            if (init >> 6) & 3 == 0 {
                let lo = crate::gpu::phys(index_base);
                let _p = crate::prof::span(crate::prof::Cat::GpuWaitVerts);
                self.complete(mem, lo, lo.saturating_add((init >> 16) * 4));
            }
        }
        // decoded into one buffer kept across draws (a Vec per draw, #304); given back at both exits
        let mut indices = std::mem::take(&mut self.index_scratch);
        raster::draw_indices_into(regs, mem, init, index_base, &mut indices);
        let mode = regs[RB_MODECONTROL] & 7;
        let depth_only = mode == 5 || ps.is_empty();
        if matches!(mode, 4 | 5) && self.gpu_vs && !vs.is_empty() {
            let t0 = std::time::Instant::now();
            let done = self.draw_gpu_vs(regs, mem, prim, &indices, (index_base, init), vs, ps, depth_only);
            self.stats.timing[1] += t0.elapsed();
            if done {
                self.index_scratch = indices;
                return;
            }
        }
        if !self.pending.is_empty() && !vs.is_empty() {
            // the CPU transform reads the vertex buffers, which may sit under a resolve still on
            // its way back; each buffer on its own: the union window can span unrelated memory
            // (e.g. render targets between buffers) the shader never fetches from
            let vs_hash = self.vs_module(vs);
            for &k in &self.vs_modules[&vs_hash].1.clone() {
                let (d0, d1) = (regs[FETCH_BASE + 2 * k as usize], regs[FETCH_BASE + 2 * k as usize + 1]);
                let p = crate::gpu::phys(d0 & !3);
                let _p = crate::prof::span(crate::prof::Cat::GpuWaitVerts);
                self.complete(mem, p, p.saturating_add(((d1 >> 2) & 0xFF_FFFF) * 4));
            }
        }
        let t0 = std::time::Instant::now();
        let verts = {
            let _p = crate::prof::span(crate::prof::Cat::RtTransform);
            self.soft.transform(regs, mem, vs, &indices)
        };
        self.index_scratch = indices;
        self.stats.timing[0] += t0.elapsed();
        let t0 = std::time::Instant::now();
        match mode {
            6 => {
                self.resolve(regs, mem, &verts);
                self.stats.timing[2] += t0.elapsed();
            }
            4 | 5 => {
                self.draw_gpu(regs, mem, prim, &verts, ps, depth_only);
                self.stats.timing[1] += t0.elapsed();
            }
            _ => {}
        }
    }

    fn complete_range(&mut self, mem: &mut dyn GuestMem, lo: u32, hi: u32) {
        self.complete(mem, lo, hi);
    }

    fn fence(&mut self, mem: &mut dyn GuestMem) {
        // every resolve has landed now; their GPU copies stay sampled while guest memory still
        // holds what landed (`alias_valid`), which a fence doesn't change
        self.complete_all(mem);
    }

    fn set_async_fence(&mut self, on: bool) {
        self.async_fence = on;
    }

    fn write_ticket(&self) -> u64 {
        self.pending.last().map_or(0, |r| r.id)
    }

    fn landed(&mut self, mem: &mut dyn GuestMem, ticket: u64) -> bool {
        self.land_ready(mem);
        self.pending.first().is_none_or(|r| r.id > ticket)
    }

    fn sync(&mut self, mem: &mut dyn GuestMem) {
        if self.async_fence {
            // copies land as they finish; fence writes wait for them (Gpu::fence_store)
            self.land_ready(mem);
            self.run += 1;
            self.log_counters();
            return;
        }
        // The recorded batch stays open: its draws carry their own copies of constants, vertex
        // data and textures, so guest code running before the next run can't change them, and
        // the command processor stops ~200 times a frame (flushing here made 3-draw batches).
        // Pending readbacks were submitted by their resolve's own flush.
        self.complete_all(mem);
        // guest code may change the memory from here on: resolves that landed are re-checked
        // against it before their GPU copies are sampled again (`alias_valid`)
        self.run += 1;
        self.log_counters();
    }

    fn present(&mut self, regs: &[u32], mem: &mut dyn GuestMem, fb: u32, w: u32, h: u32) -> Option<Image> {
        let _p = crate::prof::span(crate::prof::Cat::RtPresent);
        crate::zone!("wgpu present");
        if let Some(t) = self.timer.borrow_mut().as_mut() {
            t.end_frame(&self.device, &self.queue);
        }
        // LMA_RT_SLOW=<ms> with LMA_FRAME_PROFILE=1 (#6): when more than <ms> pass between two presents
        // the render thread executes, its own steps in that window and the game side's counters
        static RT_SLOW: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
        if let Some(limit) = *RT_SLOW.get_or_init(|| std::env::var("LMA_RT_SLOW").ok().and_then(|v| v.parse::<f64>().ok())) {
            thread_local!(static LAST: std::cell::RefCell<Option<(std::time::Instant, [u64; crate::prof::N])>> = const { std::cell::RefCell::new(None) });
            let now = (std::time::Instant::now(), crate::prof::snapshot());
            LAST.with(|l| {
                if let Some((t0, s0)) = l.borrow().as_ref() {
                    let ms = now.0.duration_since(*t0).as_secs_f64() * 1e3;
                    if ms > limit {
                        use crate::prof::Cat::*;
                        let d = |c: crate::prof::Cat| (now.1[c as usize] - s0[c as usize]) as f64 / 1e6;
                        eprintln!("LMA_RT_SLOW: rt frame {} {ms:.1} ms | land {:.1} ({:.0} KB) resolve {:.1} present {:.1} flush {:.1} pipeline {:.1} texture {:.1} psetup {:.1} vwin {:.1} consts {:.1} record {:.1} | GPU wait {:.1} (present {:.1} tex {:.1} verts {:.1})",
                            self.frame, d(RtLand), (now.1[LandBytes as usize] - s0[LandBytes as usize]) as f64 / 1024.0, d(RtResolve), d(RtPresent), d(RtFlush), d(RtPipeline), d(RtTexture), d(RtPsSetup), d(RtVwin), d(RtConsts), d(RtRecord), d(GpuWait), d(GpuWaitPresent), d(GpuWaitTex), d(GpuWaitVerts));
                        let n = |c: crate::prof::Cat| now.1[c as usize] - s0[c as usize];
                        eprintln!("LMA_RT_SLOW:   game side: slice {:.1} cp {:.1} sleep {:.1} wait present {:.1} resolve {:.1} drain {:.1} | cp runs {} blocked {} (on gpu {}) | irq defer {:.1} (render {:.1}) queue {:.1} handler {:.1}",
                            d(Slice), d(Cp), d(Sleep), d(WaitPresent), d(WaitResolve), d(WaitDrain), n(CpRuns), n(CpBlocked), n(CpBlockedGpu), d(IrqDefer), d(IrqRender), d(IrqQueue), d(IrqHandler));
                    }
                }
                *l.borrow_mut() = Some(now);
            });
        }
        self.frame += 1;
        // LMA_SETUP_LOG=1: the resolve reuse counters every 300 frames as well (scenes with few setup
        // misses never reach the miss-triggered print)
        static SETUP_LOG_FRAMES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if self.frame % 300 == 0 && *SETUP_LOG_FRAMES.get_or_init(|| std::env::var_os("LMA_SETUP_LOG").is_some()) {
            let rr: Vec<u64> = RESOLVE_REUSE.iter().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).collect();
            eprintln!("frame {}: resolves so far [new texture, reused in place, same dest other size, same dest same frame (refused)]: {rr:?}", self.frame);
        }
        if debug_env(&RESOLVE_USE, "LMA_RESOLVE_USE") {
            let s_prev = self.res_prev.iter().filter(|(_, c)| self.res_sampled.contains(c)).count();
            let s_now = self.res_made.iter().filter(|(_, c)| self.res_sampled.contains(c)).count();
            let unsampled: Vec<String> = self.res_prev.iter().filter(|(_, c)| !self.res_sampled.contains(c)).map(|(d, _)| format!("{d:#x}")).collect();
            eprintln!("LMA_RESOLVE_USE: rt frame {}: last frame {} resolves, {s_prev} sampled by now; this frame {} resolves, {s_now} sampled so far; last frame unsampled: {}", self.frame, self.res_prev.len(), self.res_made.len(), unsampled.join(" "));
            let keep: std::collections::HashSet<u64> = self.res_made.iter().map(|(_, c)| *c).collect();
            self.res_sampled.retain(|c| keep.contains(c));
            self.res_prev = std::mem::take(&mut self.res_made);
        }
        if let Some(img) = self.present_gpu(regs, fb, w, h) {
            self.frame_resolves.clear();
            self.soft.new_frame();
            self.textures.retain(|_, (d, _)| Arc::strong_count(d) > 1);
            return img;
        }
        self.frame_resolves.clear();
        self.complete_all(mem);
        self.flush(); // a frame boundary: let the GPU start on what was drawn
        // drop GPU copies of textures the software cache no longer holds
        self.textures.retain(|_, (d, _)| Arc::strong_count(d) > 1);
        // LMA_PRESENT_LOG=1 (#864): the software fallback reads the front buffer from guest memory,
        // the one consumer of a resolve's landed bytes a skipped destination would starve
        if debug_env(&PRESENT_LOG, "LMA_PRESENT_LOG") {
            eprintln!("LMA_PRESENT_LOG: rt frame {} software present of {fb:#010x} {w}x{h}", self.frame);
        }
        RenderBackend::present(&mut self.soft, regs, mem, fb, w, h)
    }

    fn start_debug(&mut self) {
        self.soft.start_debug();
    }
    fn take_debug(&mut self) -> Option<Vec<String>> {
        self.soft.take_debug()
    }
    fn debugging(&self) -> bool {
        self.soft.debugging()
    }
    fn take_textures(&mut self) -> Vec<(String, u32, u32, Vec<u8>)> {
        self.soft.take_textures()
    }

    fn stats(&self) -> BackendStats {
        let mut s = self.stats.clone();
        let soft = self.soft.stats();
        s.timing[3] = soft.timing[3];
        s.timing[4] = soft.timing[4];
        s
    }

    fn targets(&self) -> (Vec<Vec4>, Vec<f32>) {
        // GPU surfaces aren't read back here: save states taken with this backend keep the
        // EDRAM as of the last resolve (the game redraws every frame)
        RenderBackend::targets(&self.soft)
    }

    fn set_targets(&mut self, edram: Vec<Vec4>, depth: Vec<f32>) {
        self.batch = Batch::default(); // the state being restored replaces whatever was drawn
        RenderBackend::set_targets(&mut self.soft, edram, depth);
        // surfaces are rebuilt from the restored EDRAM on next use
        self.surfaces.clear();
        self.depths.clear();
    }
}

fn blend_state(b: u32) -> wgpu::BlendState {
    let factor = |f: u32| match f {
        0 => wgpu::BlendFactor::Zero,
        1 => wgpu::BlendFactor::One,
        4 => wgpu::BlendFactor::Src,
        5 => wgpu::BlendFactor::OneMinusSrc,
        6 => wgpu::BlendFactor::SrcAlpha,
        7 => wgpu::BlendFactor::OneMinusSrcAlpha,
        8 => wgpu::BlendFactor::Dst,
        9 => wgpu::BlendFactor::OneMinusDst,
        10 => wgpu::BlendFactor::DstAlpha,
        11 => wgpu::BlendFactor::OneMinusDstAlpha,
        12 | 14 => wgpu::BlendFactor::Constant,
        13 | 15 => wgpu::BlendFactor::OneMinusConstant,
        16 => wgpu::BlendFactor::SrcAlphaSaturated,
        _ => wgpu::BlendFactor::One,
    };
    let op = |o: u32| match o {
        1 => wgpu::BlendOperation::Subtract,
        2 => wgpu::BlendOperation::Min,
        3 => wgpu::BlendOperation::Max,
        4 => wgpu::BlendOperation::ReverseSubtract,
        _ => wgpu::BlendOperation::Add,
    };
    let comp = |src: u32, dst: u32, o: u32| {
        let o = op(o);
        // min/max ignore the factors (WebGPU requires One for them)
        if matches!(o, wgpu::BlendOperation::Min | wgpu::BlendOperation::Max) {
            wgpu::BlendComponent { src_factor: wgpu::BlendFactor::One, dst_factor: wgpu::BlendFactor::One, operation: o }
        } else {
            wgpu::BlendComponent { src_factor: factor(src), dst_factor: factor(dst), operation: o }
        }
    };
    wgpu::BlendState { color: comp(b & 31, (b >> 8) & 31, (b >> 5) & 7), alpha: comp((b >> 16) & 31, (b >> 24) & 31, (b >> 21) & 7) }
}

fn f32_to_f16(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xFF) as i32 - 127 + 15;
    let man = x & 0x7F_FFFF;
    if exp <= 0 {
        sign
    } else if exp >= 31 {
        sign | 0x7C00
    } else {
        sign | ((exp as u16) << 10) | ((man >> 13) as u16)
    }
}

fn f16_to_f32(h: u16) -> f32 {
    crate::shader_exec::half(h as u32)
}

/// Polygon offset (slope scale in 1/16 pixel units, constant) of the draw's enabled face, front
/// first: wgpu has one depth bias per pipeline, and the game sets front and back alike.
fn poly_offset(ctx: &RasterCtx) -> (f32, f32) {
    ctx.poly_offset[0].or(ctx.poly_offset[1]).unwrap_or((0.0, 0.0))
}

/// The present's plan for this frame's resolve bands of the front buffer, `(start row, rows,
/// order)` with `order` the resolve's `next_gen`, listed by start row: how far they cover rows
/// 0.. without a gap, and the bands to draw (those starting above row `h`), in resolve order, so
/// that a later resolve over the same rows wins, as memory would hold them (#540: a scratch target
/// resolved into the front buffer's memory early in the frame, drawn by start row, came out as a
/// band over the lower tile).
fn compose_plan(parts: &[(u32, u32, u64)], h: u32) -> (u32, Vec<usize>) {
    let mut covered = 0;
    for &(row, rows, _) in parts {
        if row > covered {
            break;
        }
        covered = covered.max(row + rows);
    }
    let mut draw: Vec<usize> = (0..parts.len()).filter(|&i| parts[i].0 < h).collect();
    draw.sort_by_key(|&i| parts[i].2);
    (covered, draw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validate(name: &str, src: &str) {
        let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    /// Trimmed interfaces (#6, phones): on below 17 vertex attributes or inter-stage variables (or
    /// forced), carrying at most the limit less one; masks keep their lowest interpolants.
    #[test]
    fn trimming_follows_the_limits() {
        let lim = |a, v| wgpu::Limits { max_vertex_attributes: a, max_inter_stage_shader_variables: v, ..wgpu::Limits::default() };
        assert!(Trim::for_limits(&lim(32, 31), false).is_none(), "desktop GPUs keep the full interface");
        assert!(Trim::for_limits(&lim(17, 17), false).is_none());
        assert_eq!(Trim::for_limits(&lim(16, 31), false).map(|t| t.max_interp), Some(15), "SwiftShader / some phones");
        assert_eq!(Trim::for_limits(&lim(32, 16), false).map(|t| t.max_interp), Some(15));
        assert_eq!(Trim::for_limits(&lim(32, 31), true).map(|t| t.max_interp), Some(16), "LMA_INTERP_TRIM=1");
        assert_eq!(first_bits(0b1011_0110, 3), 0b0001_0110);
        assert_eq!(first_bits(0b1011_0110, 16), 0b1011_0110);
        assert_eq!(first_bits(0xFFFF, 15), 0x7FFF);
    }

    /// The band drawn last over `row`, as the present composes them.
    fn drawn_over(parts: &[(u32, u32, u64)], draw: &[usize], row: u32) -> Option<u64> {
        draw.iter().rev().map(|&i| parts[i]).find(|&(r, n, _)| r <= row && row < r + n).map(|p| p.2)
    }

    /// The two 720p tiles (rows 0..384 and 384..720) with an older scratch band resolved into the
    /// lower tile's memory earlier in the frame: by start row the scratch band was drawn last, over
    /// the tile (#540's purple band); in resolve order the tile wins.
    #[test]
    fn compose_draws_in_resolve_order() {
        let parts = [(0, 384, 5), (384, 336, 6), (512, 48, 2)];
        let (covered, draw) = compose_plan(&parts, 720);
        assert_eq!(covered, 720);
        assert_eq!(draw, vec![2, 0, 1]);
        assert_eq!(drawn_over(&parts, &draw, 530), Some(6), "the newer tile over the scratch band");
        assert_eq!(drawn_over(&parts, &draw, 100), Some(5));
        // a newer band over an older tile still shows
        let parts = [(0, 384, 5), (384, 336, 6), (512, 48, 9)];
        let (_, draw) = compose_plan(&parts, 720);
        assert_eq!(drawn_over(&parts, &draw, 530), Some(9));
        assert_eq!(drawn_over(&parts, &draw, 600), Some(6));
    }

    /// Coverage stops at the first gap; bands starting below the picture aren't drawn.
    #[test]
    fn compose_coverage_and_rows_below() {
        assert_eq!(compose_plan(&[(0, 352, 1), (384, 336, 2)], 720).0, 352, "a gap at rows 352..384");
        assert_eq!(compose_plan(&[(0, 384, 1), (352, 368, 2)], 720).0, 720, "overlapping bands");
        assert_eq!(compose_plan(&[], 720), (0, vec![]));
        let (covered, draw) = compose_plan(&[(0, 720, 3), (736, 32, 4)], 720);
        assert_eq!((covered, draw), (720, vec![0]));
    }

    /// Every row shows the newest band over it, whatever the bands' starts and order.
    #[test]
    fn compose_each_row_shows_the_newest_band() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n
        };
        for _ in 0..500 {
            let mut parts: Vec<(u32, u32, u64)> = (0..1 + next(5)).map(|i| ((next(24) * 32) as u32, (32 * (1 + next(12))) as u32, next(1000) * 8 + i)).collect();
            parts.sort_by_key(|p| p.0);
            let (_, draw) = compose_plan(&parts, 720);
            for row in (0..720).step_by(16) {
                let newest = parts.iter().filter(|&&(r, n, _)| r <= row && row < r + n).map(|p| p.2).max();
                assert_eq!(drawn_over(&parts, &draw, row), newest, "row {row} of {parts:?}");
            }
        }
    }

    /// The backend's own shaders compile only when a GPU first runs them; check them here.
    #[test]
    fn builtin_shaders_validate() {
        validate("passthrough vs", PASSTHROUGH_VS);
        validate("passthrough vs (trimmed)", &passthrough_vs_trimmed(0b1000_0000_0000_1001));
        validate("passthrough vs (trimmed, none)", &passthrough_vs_trimmed(0));
        validate("depth-only ps", DEPTH_ONLY_PS);
        validate("resolve", RESOLVE_CS);
        validate("resolve 2x", &resolve_cs_msaa());
        validate("colour fill", COLOR_FILL);
        validate("swizzle", SWIZZLE_CS);
        validate("present", PRESENT_CS);
        validate("mips rgba8", &MIP_CS.replace("FORMAT", "rgba8unorm"));
        validate("mips rgba32", &MIP_CS.replace("FORMAT", "rgba32float"));
    }
}

/// A finished resolve's bytes into guest memory, returning their hash: copied and hashed in one
/// pass, 64 KB at a time while the chunk is in cache (two passes over ~16 MB a frame in a match
/// were a large part of the render thread's landings, #6).
fn land_bytes(mem: &mut dyn GuestMem, dest: u32, bytes: &[u8]) -> u64 {
    let mut h = raster::Hasher::default();
    for (k, chunk) in bytes.chunks(1 << 16).enumerate() {
        mem.write(dest.wrapping_add((k << 16) as u32), chunk);
        h.update(chunk);
    }
    h.finish(bytes.len())
}

/// Decoded texels as RGBA8 (`v * 255 + 0.5`, truncated), or None as soon as a value is outside
/// 0..=1 (NaN included), when the texture needs the float format.
fn to_rgba8(px: &[[f32; 4]]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(px.len() * 4);
    for c in px {
        for &v in c {
            if !(0.0..=1.0).contains(&v) {
                return None;
            }
            out.push((v * 255.0 + 0.5) as u8);
        }
    }
    Some(out)
}

/// Setup-cache entries survive new runs and landings and are re-validated on use (#304, #314);
/// LMA_SETUP_KEEP=0 clears them at every generation as before.
/// In-place reuse of a resolve's GPU texture across frames (#561); `LMA_RESOLVE_REUSE=0` turns it off.
fn resolve_reuse_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LMA_RESOLVE_REUSE").as_deref() != Ok("0"))
}

fn setup_keep() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LMA_SETUP_KEEP").as_deref() != Ok("0"))
}

/// Whether landed resolves use the write watch (`WgpuBackend::land`) by default, given
/// LMA_LAND_HASH. Not on Windows: `MEM_WRITE_WATCH` makes the first write to each page after a
/// query cost ~0.5 us, and every landing rewrites pages the last query re-armed. On the PC that
/// doubled the landing copy (memmove 29% of the render thread vs 13% hashed) and landings took
/// 6.7-7.2 ms per slow frame vs 5.2-5.6 hashed, Barca v Real night, 3 interleaved rounds (#304).
/// Whether a resolve may write into the previous resolve's GPU texture at the same dest: same
/// size and format (the texture's identity and every view of it then stay valid).
fn resolve_reusable(prev: (u32, u32, u32), new: (u32, u32, u32)) -> bool {
    prev == new
}

/// Whether this resolve may write into the texture of the previous resolve at its dest: the reuse is
/// on, that texture was made or last reused in an earlier frame (a band resolved twice in one frame is
/// composed from both snapshots later in it, #568) and the size and format match.
fn reuse_allowed(on: bool, made_frame: u64, frame: u64, prev: (u32, u32, u32), new: (u32, u32, u32)) -> bool {
    on && made_frame != frame && resolve_reusable(prev, new)
}

/// What a cached setup's alias of a resolve follows: the texture's identity for the identity
/// swizzle (its view is the texture's own), the content for a swizzled copy (redone per resolve).
fn alias_follows(id: u64, gen: u64, content: u64) -> u64 {
    if (id & 0xFFF) as u32 == 0x688 { gen } else { content }
}

fn land_watch_default(env: Option<&str>) -> bool {
    match env {
        Some("1") => false,
        Some("0") => true,
        _ => !cfg!(windows),
    }
}

/// LMA_DRAW_TRACE=<file> (#205): one 32-byte record per draw, little endian: the render thread's
/// frame, the command processor's (`threaded::GUEST_FRAME`, to line up with LMA_FRAME_DIR's
/// numbering), the draw's index in its frame, then 32-bit hashes of the vertex shader, the pixel
/// shader, the vertex and pixel float constants, and the texture fetch constants mixed with the draw
/// word. A one-frame glitch is a draw whose hashes differ in that frame from both neighbours
/// (`tools/draw_trace_diff.py`). The last few KB are lost if the process is killed.
fn draw_trace(frame: u64, regs: &[u32], init: u32, vs: &[u32], ps: &[u32]) {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
    static OUT: std::sync::OnceLock<Option<std::sync::Mutex<std::io::BufWriter<std::fs::File>>>> = std::sync::OnceLock::new();
    static LAST: AtomicU64 = AtomicU64::new(u64::MAX);
    static INDEX: AtomicU64 = AtomicU64::new(0);
    let Some(out) = OUT.get_or_init(|| std::env::var_os("LMA_DRAW_TRACE").and_then(|p| std::fs::File::create(p).ok()).map(|f| std::sync::Mutex::new(std::io::BufWriter::new(f)))) else { return };
    let index = if LAST.swap(frame, Relaxed) == frame { INDEX.fetch_add(1, Relaxed) + 1 } else { INDEX.store(0, Relaxed); 0 };
    let h = |w: &[u32]| w.iter().fold(0x811c_9dc5u32, |a, &x| (a ^ x).wrapping_mul(0x0100_0193));
    let rec = [
        frame as u32,
        crate::threaded::GUEST_FRAME.load(Relaxed) as u32,
        index as u32,
        h(vs),
        h(ps),
        h(&regs[FLOAT_BASE..FLOAT_BASE + 1024]),
        h(&regs[FLOAT_BASE + 1024..FLOAT_BASE + 2048]),
        h(&regs[FETCH_BASE..FETCH_BASE + 192]) ^ init.rotate_left(7),
    ];
    let mut bytes = [0u8; 32];
    for (k, v) in rec.iter().enumerate() {
        bytes[4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
    }
    let _ = out.lock().unwrap().write_all(&bytes);
}

/// Debug logs (#6): LMA_LAND_LOG=1, every resolve landing in guest memory by the render thread's
/// frame; LMA_RESOLVE_LOG=1, every resolve as it runs (tagged with the command processor's frame,
/// `threaded::GUEST_FRAME`, so render-thread work shows up a frame late).
static LAND_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static RESOLVE_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static PRESENT_LOG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
static RESOLVE_USE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Whether debug switch `name` is `1` (read once into `flag`), as LMA_ALIAS_LOG.
fn debug_env(flag: &std::sync::OnceLock<bool>, name: &str) -> bool {
    *flag.get_or_init(|| std::env::var(name).as_deref() == Ok("1"))
}

/// LMA_ALIAS_LOG=1 (#205): log resolve copies the texture path stops sampling (memory changed, or
/// a fetch that doesn't match the copy).
fn alias_log() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LMA_ALIAS_LOG").as_deref() == Ok("1"))
}

#[cfg(test)]
mod land_tests {
    use super::*;

    /// Strips, fans and quads become the triangle list `raster::assemble` draws (same order and
    /// winding) in the caller's buffer; lists pass through unchanged; other primitives are None (#304).
    #[test]
    fn triangle_lists_from_strips_fans_and_quads() {
        let idx = [10u32, 11, 12, 13, 14, 15, 16];
        let mut out = vec![99u32; 3]; // stale contents are replaced
        assert_eq!(triangle_list(4, &idx, &mut out), Some(&idx[..6]), "a list: no copy, whole triangles only");
        assert_eq!(triangle_list(5, &idx[..5], &mut out).map(<[u32]>::to_vec), Some(vec![10, 11, 12, 10, 12, 13, 10, 13, 14]), "fan around the first index");
        assert_eq!(triangle_list(6, &idx[..5], &mut out).map(<[u32]>::to_vec), Some(vec![10, 11, 12, 12, 11, 13, 12, 13, 14]), "strip, every other one flipped");
        assert_eq!(triangle_list(13, &idx[..4], &mut out).map(<[u32]>::to_vec), Some(vec![10, 11, 12, 10, 12, 13]), "quad as two triangles");
        assert_eq!(triangle_list(1, &idx, &mut out), None, "points need the CPU path");
        assert_eq!(triangle_list(6, &idx[..2], &mut out).map(<[u32]>::len), Some(0), "too short for a triangle");
    }

    /// A stage's float constants not written since the previous draw (`floats_clean`) are taken
    /// as they were, unread; written ones are gathered and interned again (#304).
    #[test]
    fn clean_float_constants_skip_gathering() {
        let mut regs = vec![0u32; FLOAT_BASE + 512 * 4];
        regs[FLOAT_BASE + 4 * 3] = 1.5f32.to_bits();
        let slots: Vec<u32> = vec![3, 7];
        let mut b = Batch::default();
        let first = b.put_floats(&regs, 0, Some(&slots));
        // a write the backend wasn't told about: a clean stage still reuses the previous offset
        regs[FLOAT_BASE + 4 * 7] = 2.5f32.to_bits();
        b.floats_clean = [true, false];
        assert_eq!(b.put_floats(&regs, 0, Some(&slots)), first, "clean: not re-read");
        b.floats_clean = [false, false];
        let second = b.put_floats(&regs, 0, Some(&slots));
        assert_ne!(second, first, "written: gathered again");
        assert_eq!(b.fvals[second as usize * 4 + 4], 2.5f32.to_bits());
        // the whole-block path (relative addressing) too
        b.floats_clean = [false, false];
        let block = b.put_floats(&regs, 256, None);
        regs[FLOAT_BASE + 4 * 300] = 9.0f32.to_bits();
        b.floats_clean = [false, true];
        assert_eq!(b.put_floats(&regs, 256, None), block);
    }

    /// A whole constant block (relative addressing) is 16 chunks behind a table of their offsets:
    /// the table points at the right words, a draw changing one constant re-interns only its
    /// chunk, and putting the old values back finds the first block again (#304).
    #[test]
    fn float_blocks_share_unchanged_chunks() {
        let mut regs = vec![0u32; FLOAT_BASE + 512 * 4];
        for (i, r) in regs[FLOAT_BASE + 256 * 4..].iter_mut().enumerate() {
            *r = (i as f32 * 0.25).to_bits();
        }
        let mut b = Batch::default();
        let table = |b: &Batch, t: u32| -> Vec<usize> { (0..16).map(|k| f32::from_bits(b.fvals[t as usize * 4 + k]) as usize).collect() };
        let first = b.put_floats(&regs, 256, None);
        let chunks = table(&b, first);
        for (k, &o) in chunks.iter().enumerate() {
            let want = &regs[FLOAT_BASE + (256 + k * 16) * 4..FLOAT_BASE + (256 + k * 16 + 16) * 4];
            assert_eq!(&b.fvals[o * 4..o * 4 + 64], want, "chunk {k}");
        }
        // constant 256 + 5 * 16 + 3 changes: chunk 5 only
        let at = FLOAT_BASE + (256 + 5 * 16 + 3) * 4;
        let old = regs[at];
        regs[at] = 7.0f32.to_bits();
        let second = b.put_floats(&regs, 256, None);
        assert_ne!(second, first);
        let moved = table(&b, second);
        for k in 0..16 {
            assert_eq!(moved[k] == chunks[k], k != 5, "chunk {k}");
        }
        assert_eq!(b.fvals[moved[5] * 4 + 12], 7.0f32.to_bits());
        // the old values again: the first block's chunks and table, nothing new
        regs[at] = old;
        let words = b.fvals.len();
        assert_eq!(b.put_floats(&regs, 256, None), first);
        assert_eq!(b.fvals.len(), words, "no new words");
        // equal constants through another slot list are shared too
        let (s1, s2): (Vec<u32>, Vec<u32>) = (vec![3, 7], vec![3, 7]);
        let v1 = b.put_floats(&regs, 0, Some(&s1));
        assert_eq!(b.put_floats(&regs, 0, Some(&s2)), v1);
        // a recycled batch starts empty
        let b = b.recycle();
        assert!(b.fvals.is_empty() && b.fvals_by_hash.is_empty() && b.fbank_last == [None; 2]);
    }

    /// LMA_LAND_HASH picks the landing check; unset, Windows hashes (its write watch makes the
    /// landing copy fault page by page) and other platforms watch (#304).
    /// A guest rectangle at 1x and 2x: the same pixels, bands meet without a seam.
    #[test]
    fn scaled_rect_covers_the_same_guest_pixels() {
        assert_eq!(scaled_rect(10, 20, 30, 40, 1), [10, 20, 30, 40], "1x: unchanged");
        assert_eq!(scaled_rect(10, 20, 30, 40, 2), [20, 40, 60, 80]);
        // the lower EDRAM tile at 2x ends where the 1x one does, doubled: no seam between bands
        let a = scaled_rect(0, 0, 1280, 384, 2);
        let b = scaled_rect(0, 384, 1280, 336, 2);
        assert_eq!(a[1] + a[3], b[1]);
        assert_eq!(b[1] + b[3], 1440);
    }

    /// A resolve reuses the texture at its dest only at the same size and format; a setup that
    /// samples it through the identity view follows the texture, a swizzled copy the content.
    #[test]
    fn resolve_reuse_and_what_an_alias_follows() {
        assert!(resolve_reusable((1280, 384, 6), (1280, 384, 6)));
        assert!(!resolve_reusable((1280, 384, 6), (1280, 720, 6)), "another size: a new texture");
        assert!(!resolve_reusable((1280, 384, 6), (1280, 384, 22)), "depth over colour: a new texture");
        let id = (1u64 << 63) | (0x1234_5000u64 << 12);
        assert_eq!(alias_follows(id | 0x688, 7, 9), 7, "identity swizzle: the texture");
        assert_eq!(alias_follows(id | 0x6a8, 7, 9), 9, "a swizzled copy: the content");
        let s = (1280, 384, 6);
        assert!(reuse_allowed(true, 10, 11, s, s), "an earlier frame's texture");
        assert!(!reuse_allowed(true, 11, 11, s, s), "the same frame: a second snapshot is needed");
        assert!(!reuse_allowed(false, 10, 11, s, s), "LMA_RESOLVE_REUSE=0");
        assert!(!reuse_allowed(true, 10, 11, s, (1280, 720, 6)), "another size");
    }

    #[test]
    fn land_check_defaults() {
        assert!(!land_watch_default(Some("1")));
        assert!(land_watch_default(Some("0")));
        assert_eq!(land_watch_default(None), !cfg!(windows));
        assert_eq!(land_watch_default(Some("yes")), !cfg!(windows));
    }

    /// A pipeline key survives the prewarm cache file's line format; bad lines are skipped (#304).
    #[test]
    fn pipeline_keys_round_trip_through_the_cache_file() {
        let k = PipelineKey { vs: 0xdead_beef_1234, cull: 6, ps: 0, format: Fmt::Rgba16F, blend: 0x1234_5678, mask: 15, depth: 0x1d, bias: 1.5f32.to_bits(), samples: 2, imask: 0 };
        let line = k.to_line();
        assert!(PipelineKey::from_line(line.strip_prefix("pipe ").unwrap()) == Some(k));
        assert!(PipelineKey::from_line("1 2 3").is_none(), "too short");
        assert!(PipelineKey::from_line("1 2 0 7 0 0 0 0 1").is_none(), "no such format");
    }

    /// The cache file keeps what parses and checks out, and a file from another format version
    /// is ignored (#304).
    #[test]
    fn prewarm_cache_file_skips_what_doesnt_check_out() {
        let code = [0x1234_5678u32, 0x9abc_def0, 7];
        let h = raster::hash(&code);
        let k = PipelineKey { vs: h, cull: 0, ps: 0, format: Fmt::Rgba8, blend: 0, mask: 15, depth: 3, bias: 0, samples: 1, imask: 0 };
        let words = |c: &[u32]| c.iter().map(|w| format!("{w:x}")).collect::<Vec<_>>().join(" ");
        let text = format!(
            "{PREWARM_HEADER}
vs {h:x} {}
ps {h:x} {}
vs {:x} {}
ps zz 1 2
{}
pipe 1 2
junk
",
            words(&code),
            words(&code),
            h ^ 1,
            words(&code), // the wrong hash for its code: skipped
            k.to_line()
        );
        let (current, ps, vs, pipes) = Prewarm::read(&text);
        assert!(current);
        assert_eq!(vs, vec![(h, code.to_vec())]);
        assert_eq!(ps, vec![(h, code.to_vec())]);
        assert!(pipes == vec![k]);
        let (current, ps, vs, pipes) = Prewarm::read(&text.replacen(PREWARM_HEADER, "lma-rs wgpu pipelines 0", 1));
        assert!(!current && ps.is_empty() && vs.is_empty() && pipes.is_empty(), "an older format is ignored");
    }

    /// One pass gives the bytes the check-then-convert pair gave, and None for any value outside
    /// 0..=1 (NaN, negative, above one), so those textures stay float.
    #[test]
    fn rgba8_conversion_matches_two_passes() {
        let px: Vec<[f32; 4]> = (0..1000).map(|i| [i as f32 / 999.0, 0.5, 1.0, 0.0]).collect();
        let two: Vec<u8> = px.iter().flat_map(|c| c.map(|v| (v * 255.0 + 0.5) as u8)).collect();
        assert_eq!(to_rgba8(&px), Some(two));
        for bad in [f32::NAN, -0.001, 1.001] {
            let mut p = px.clone();
            p[500][1] = bad;
            assert_eq!(to_rgba8(&p), None, "{bad}");
        }
    }

    /// One fused pass gives the bytes and the hash two passes gave (odd length: the hash's tail).
    #[test]
    fn land_bytes_copies_and_hashes_like_two_passes() {
        let mut m = guest::mem::Memory::new();
        let b: Vec<u8> = (0..200_003u32).map(|i| (i.wrapping_mul(7) ^ (i >> 9)) as u8).collect();
        let h = land_bytes(&mut m, 0xA010_0000, &b);
        assert_eq!(h, raster::bytes_hash(&b));
        let mut out = vec![0u8; b.len()];
        GuestMem::read(&mut m, 0xA010_0000, &mut out);
        assert_eq!(out, b);
    }
}

#[cfg(test)]
mod skip_land_tests {
    use super::skip_land_list;

    #[test]
    fn the_list_parses_hex_and_all() {
        assert_eq!(skip_land_list(Some(""), false), (false, vec![]));
        assert_eq!(skip_land_list(Some("all"), false), (true, vec![]));
        assert_eq!(skip_land_list(Some("0xac053000, aa40c000"), true), (false, vec![0xac05_3000, 0xaa40_c000]));
        assert_eq!(skip_land_list(Some("junk,0xaafd8000"), false), (false, vec![0xaafd_8000]), "a bad entry is skipped");
    }

    #[test]
    fn unset_takes_the_binary_default_and_0_lands_everything() {
        assert_eq!(skip_land_list(None, true), (true, vec![]), "lma-cli: skip all");
        assert_eq!(skip_land_list(None, false), (false, vec![]), "lma-app: land all");
        assert_eq!(skip_land_list(Some("0"), true), (false, vec![]), "opt out of the headless default");
        assert_eq!(skip_land_list(Some("none"), true), (false, vec![]));
    }
}
