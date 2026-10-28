//! Xenos shader microcode -> WGSL, for the wgpu backend (#6).
//!
//! The generated code mirrors `shader_exec` (the reference interpreter) statement by statement:
//! the control-flow program becomes a `loop` over a `switch` on the CF index, every ALU / fetch
//! instruction becomes straight-line WGSL with its operands, swizzles and masks resolved at
//! translation time. Anything the interpreter does, the WGSL does in the same order (vector
//! operands are read before the scalar unit runs, MAXA sets a0 before the scalar operands are
//! read, kills are deferred to the end), so the software renderer stays the reference.
//!
//! Resource layout (bind group 0), shared by all draws of a batch: each draw's index `di` comes
//! from `instance_index` (the backend draws instance `di`), and the VS hands it to the PS.
//! - binding 0: `consts_arr` storage buffer — one `Consts` per draw: 512 float constants (VS
//!   0..255, PS 256..511), 8 words of bool constants, 64 loop constants, render state;
//! - binding 1: `fetch_arr` storage buffer — per draw (`fetch_idx`), the 96 vertex fetch
//!   constants as raw dwords (2 per constant, the same words the PM4 stream sets);
//! - binding 2: `vmem` storage buffer — guest memory windows for vertex fetches, as raw
//!   little-endian dwords; a draw's window starts at word `vmem_off`, is `vmem_len` words long and
//!   maps guest physical `vmem_base`;
//! - bindings 16 + 2k / 17 + 2k: texture and sampler for texture fetch constant k (pixel shaders).
//!
//! A vertex shader ends with `xenos_position`, which does what the software path does after the
//! shader (W format, perspective divide, viewport, pixel-centre shift, window offset) and maps
//! the screen position onto the backend's EDRAM surface, so its output is a host clip position.

use crate::shader::*;
use std::fmt::Write as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Pixel,
}

/// Translation result: WGSL source plus the texture and vertex fetch constants it reads.
pub struct Translated {
    pub wgsl: String,
    pub textures: Vec<u32>,
    pub vertex_consts: Vec<u32>,
    /// Float constants (absolute register numbers, 0..511) the shader reads, in the order the
    /// backend must store them from the draw's `vf_off`/`pf_off`; None: the stage's whole block
    /// (256 constants from 0 or 256), because it addresses constants relatively, read through a
    /// chunk table at `vf_off`/`pf_off` (`fb`).
    pub const_slots: Option<Vec<u32>>,
    /// Vertex shader: the interpolants it exports (bit d = `interp[d]`); pixel shader: the
    /// interpolants its trimmed interface reads (`translate_with`), else 0.
    pub interp: u16,
}

/// Rewrite `consts.f[...]` reads to the draw's compact constants in `fvals` (see `const_slots`).
fn compact_consts(s: &str, base: u32, off: &str) -> (String, Option<Vec<u32>>) {
    const PAT: &str = "consts.f[";
    // pass 1: absolute indices, and whether any read is relative
    let mut used = std::collections::BTreeSet::new();
    let mut relative = false;
    for (i, _) in s.match_indices(PAT) {
        let rest = &s[i + PAT.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let after = &rest[digits.len()..];
        if after.starts_with("u]") {
            used.insert(digits.parse::<u32>().unwrap());
        } else {
            relative = true;
        }
    }
    let slots: Option<Vec<u32>> = (!relative).then(|| used.into_iter().collect());
    // pass 2: rewrite
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (i, _) in s.match_indices(PAT) {
        out.push_str(&s[last..i]);
        let rest = &s[i + PAT.len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let n: u32 = digits.parse().unwrap();
        let after = &rest[digits.len()..];
        if let Some(stripped) = after.strip_prefix("u]") {
            match &slots {
                Some(list) => out.push_str(&format!("fvals[{off} + {}u]", list.iter().position(|&x| x == n).unwrap())),
                None => out.push_str(&format!("fb({off}, {}u)", n - base)),
            }
            last = s.len() - stripped.len();
        } else {
            // relative: "consts.f[{base}u + (index & 255u)]" -> "fb(off, (index & 255u))"
            let tail = after.strip_prefix("u + ").expect("relative constant read");
            let mut depth = 0;
            let end = tail
                .char_indices()
                .find(|&(_, c)| {
                    depth += match c { '[' | '(' => 1, ']' | ')' => -1, _ => 0 };
                    depth < 0
                })
                .expect("relative constant read")
                .0;
            out.push_str(&format!("fb({off}, {})", &tail[..end]));
            last = s.len() - tail.len() + end + 1;
        }
    }
    out.push_str(&s[last..]);
    (out, slots)
}

const PRELUDE: &str = r#"
// texture fetches take their mip level from screen-space derivatives (`tfetch`); the translated
// shader's control flow is a loop over a switch, uniform in practice but not provably
diagnostic(off, derivative_uniformity);
struct Consts {
    b: array<u32, 8>,
    l: array<u32, 64>,
    vmem_base: u32,
    // RB_COLORCONTROL alpha test (bit 3 enable, bits 0-2 function) and RB_ALPHA_REF bits
    alpha_test: u32,
    alpha_ref: u32,
    z_offset: u32, // polygon offset, constant part (f32 bits)
    // vertex shader epilogue: PA_CL_VTE_CNTL, PA_CL_VPORT_* and the screen -> clip mapping
    vte: u32,
    fetch_idx: u32,
    vmem_off: u32,
    vmem_len: u32,
    vport: vec4<f32>,   // x scale, x offset, y scale, y offset
    vport_z: vec4<f32>, // z scale, z offset, pixel-centre shift, unused
    screen: vec4<f32>,  // window offset x, y; 2 / surface width, 2 / surface height
    // where this draw's float constants start in `fvals` (vertex and pixel shader)
    vf_off: u32,
    pf_off: u32,
    _p1: u32,
    _p2: u32,
};
@group(0) @binding(0) var<storage, read> consts_arr: array<Consts>;
@group(0) @binding(1) var<storage, read> fetch_arr: array<array<u32, 192>>;
@group(0) @binding(2) var<storage, read> vmem: array<u32>;
// float constants: per draw only those the shader reads (see `const_slots`), or its stage's
// whole 256-entry block when it addresses them relatively, in 16 chunks of 16 shared between
// draws: at the draw's offset 4 vec4s list each chunk's offset (as floats)
@group(0) @binding(3) var<storage, read> fvals: array<vec4<f32>>;
fn fb(table: u32, i: u32) -> vec4<f32> {
    return fvals[u32(fvals[table + (i >> 6u)][(i >> 4u) & 3u]) + (i & 15u)];
}
// this draw's index into consts_arr (set at the start of each entry point)
var<private> di: u32;

// Direct3D 9 rule used by Xenos: 0 * anything = 0
fn mul0(a: f32, b: f32) -> f32 { return select(a * b, 0.0, a == 0.0 || b == 0.0); }
fn mul0v(a: vec4<f32>, b: vec4<f32>) -> vec4<f32> { return vec4<f32>(mul0(a.x, b.x), mul0(a.y, b.y), mul0(a.z, b.z), mul0(a.w, b.w)); }
fn b2f(x: bool) -> f32 { return select(0.0, 1.0, x); }
fn bool_const(i: u32) -> bool { return ((consts.b[i / 32u] >> (i % 32u)) & 1u) == 1u; }
fn gpu_swap(v: u32, endian: u32) -> u32 {
    switch endian & 3u {
        case 1u: { return ((v >> 8u) & 0x00FF00FFu) | ((v << 8u) & 0xFF00FF00u); }
        case 2u: { return ((v >> 24u) & 0xFFu) | ((v >> 8u) & 0xFF00u) | ((v << 8u) & 0xFF0000u) | (v << 24u); }
        case 3u: { return (v >> 16u) | (v << 16u); }
        default: { return v; }
    }
}
fn vread(addr: u32, endian: u32) -> u32 {
    let i = (addr - consts.vmem_base) / 4u;
    if (i >= consts.vmem_len) { return 0u; }
    return gpu_swap(vmem[consts.vmem_off + i], endian);
}
// Shader position -> host clip position, as raster::Renderer::transform + TriSetup + the
// passthrough path compute it on the CPU.
fn xenos_position(p_in: vec4<f32>) -> vec4<f32> {
    var p = p_in;
    let vte = consts.vte;
    // VTX_W0_FMT set: the shader wrote W itself; clear: it wrote 1/W
    var w = 1.0;
    if ((vte & 0x400u) != 0u) { w = p.w; } else if (p.w != 0.0) { w = 1.0 / p.w; }
    let inv_w = select(1.0, 1.0 / w, w != 0.0);
    if ((vte & 0x100u) == 0u) { p.x = p.x * inv_w; p.y = p.y * inv_w; }
    if ((vte & 0x200u) == 0u) { p.z = p.z * inv_w; }
    if ((vte & 1u) != 0u) { p.x = p.x * consts.vport.x; }
    if ((vte & 2u) != 0u) { p.x = p.x + consts.vport.y; }
    if ((vte & 4u) != 0u) { p.y = p.y * consts.vport.z; }
    if ((vte & 8u) != 0u) { p.y = p.y + consts.vport.w; }
    if ((vte & 16u) != 0u) { p.z = p.z * consts.vport_z.x; }
    if ((vte & 32u) != 0u) { p.z = p.z + consts.vport_z.y; }
    let sx = p.x + consts.vport_z.z + consts.screen.x;
    let sy = p.y + consts.vport_z.z + consts.screen.y;
    let hw = select(1.0, w, w == w && abs(w) < 3.0e38); // a non-finite W draws unprojected, like the CPU path
    return vec4<f32>((sx * consts.screen.z - 1.0) * hw, (1.0 - sy * consts.screen.w) * hw, clamp(p.z + bitcast<f32>(consts.z_offset), 0.0, 1.0) * hw, hw);
}
fn x_half(h: u32) -> f32 { return unpack2x16float(h & 0xFFFFu).x; }
fn x_snorm(raw: u32, bits: u32, normalized: bool) -> f32 {
    let s = f32(bitcast<i32>(raw << (32u - bits)) >> (32u - bits));
    if (normalized) { return max(s / f32((1u << (bits - 1u)) - 1u), -1.0); }
    return s;
}
fn x_unorm(raw: u32, bits: u32, normalized: bool) -> f32 {
    if (normalized) { return f32(raw) / f32(select((1u << bits) - 1u, 0xFFFFFFFFu, bits == 32u)); }
    return f32(raw);
}
"#;

/// Translate a decoded shader. `stage` decides the constant bank, exports and kill handling.
pub fn translate(shader: &Shader, stage: Stage) -> Translated {
    translate_with(shader, stage, None)
}

/// The draw index and interpolant `d` of `mask` in a trimmed interface: the draw index at
/// location 0, the interpolants packed after it in order (`translate_with`).
pub fn interp_location(mask: u16, d: u32) -> u32 {
    1 + (mask & ((1u32 << d) - 1) as u16).count_ones()
}

/// `translate`, with the stages' interface trimmed when `trim` is Some (#6, phones: GPUs with 16
/// vertex attributes / inter-stage variables): a vertex shader outputs only the interpolants it
/// exports that `trim` allows (`Translated::interp`; 0xFFFF allows all), a pixel shader reads only
/// those in the mask given (its vertex shader's), at `interp_location`; the registers of the others
/// keep their zero, as the full interface's unwritten interpolants arrive as zero. None: the full
/// interface, all 16 at their own locations and the draw index at 16.
pub fn translate_with(shader: &Shader, stage: Stage, trim: Option<u16>) -> Translated {
    let pixel = stage == Stage::Pixel;
    let mut t = Tr { out: String::new(), pixel, textures: Vec::new(), vertex_consts: Vec::new(), interp: 0 };
    let mut body = String::new();
    for (i, cf) in shader.cf.iter().enumerate() {
        let next = i + 1;
        let mut c = String::new();
        match *cf {
            Cf::Exec { addr, count, seq, end, cond } => {
                let _ = writeln!(c, "if ({}) {{", cond_expr(&cond));
                for k in 0..count {
                    let is_fetch = (seq >> (2 * k)) & 1 == 1;
                    match shader.instr_ref(addr + k, is_fetch) {
                        Some(Instr::Alu(a)) => t.alu(a, &mut c),
                        Some(Instr::Fetch(Fetch::Vertex(v))) => t.vfetch(v, &mut c),
                        Some(Instr::Fetch(Fetch::Texture(x))) => t.tfetch(x, &mut c),
                        None => {}
                    }
                }
                c.push_str("}\n");
                if end {
                    c.push_str("done = true;\n");
                } else {
                    let _ = writeln!(c, "pc = {next};");
                }
            }
            Cf::LoopStart { addr, loop_id, .. } => {
                let lid = loop_id; // shared loop constants, as in the interpreter
                let _ = writeln!(
                    c,
                    "{{ let lc = consts.l[{lid}u]; let count = lc & 0xFFu; \
                     if (count == 0u) {{ pc = {addr}; }} else {{ \
                     lbody[ld] = {next}; lrem[ld] = count; lal[ld] = al; lstep[ld] = i32(bitcast<i32>(lc << 8u) >> 24u); ld = ld + 1u; \
                     al = i32((lc >> 8u) & 0xFFu); pc = {next}; }} }}"
                );
            }
            Cf::LoopEnd { .. } => {
                let _ = writeln!(
                    c,
                    "if (ld > 0u) {{ ld = ld - 1u; if (lrem[ld] > 1u) {{ al = al + lstep[ld]; lrem[ld] = lrem[ld] - 1u; ld = ld + 1u; pc = lbody[ld - 1u]; }} \
                     else {{ al = lal[ld]; pc = {next}; }} }} else {{ pc = {next}; }}"
                );
            }
            Cf::CondCall { addr, cond } => {
                let _ = writeln!(c, "if ({}) {{ cstack[cd] = {next}; cd = cd + 1u; pc = {addr}; }} else {{ pc = {next}; }}", cond_expr(&cond));
            }
            Cf::Return => {
                c.push_str("if (cd > 0u) { cd = cd - 1u; pc = cstack[cd]; } else { done = true; }\n");
            }
            Cf::CondJmp { addr, cond } => {
                let _ = writeln!(c, "if ({}) {{ pc = {addr}; }} else {{ pc = {next}; }}", cond_expr(&cond));
            }
            _ => {
                let _ = writeln!(c, "pc = {next};");
            }
        }
        let _ = writeln!(body, "case {i}: {{\n{c}}}");
    }

    let mut s = String::from(PRELUDE);
    for &k in &t.textures {
        let _ = writeln!(s, "@group(0) @binding({}) var tex{k}: texture_2d<f32>;", 16 + 2 * k);
        let _ = writeln!(s, "@group(0) @binding({}) var smp{k}: sampler;", 17 + 2 * k);
    }
    s.push_str(&t.out);
    if let (true, Some(mask)) = (pixel, trim) {
        s.push_str(&trimmed_ps_head(mask));
    } else if pixel {
        s.push_str(
            "struct PsIn { @builtin(position) frag: vec4<f32>, @location(0) i0: vec4<f32>, @location(1) i1: vec4<f32>, @location(2) i2: vec4<f32>, @location(3) i3: vec4<f32>,\n\
             @location(4) i4: vec4<f32>, @location(5) i5: vec4<f32>, @location(6) i6: vec4<f32>, @location(7) i7: vec4<f32>, @location(8) i8: vec4<f32>, @location(9) i9: vec4<f32>,\n\
             @location(10) i10: vec4<f32>, @location(11) i11: vec4<f32>, @location(12) i12: vec4<f32>, @location(13) i13: vec4<f32>, @location(14) i14: vec4<f32>, @location(15) i15: vec4<f32>,\n\
             @location(16) @interpolate(flat) di: u32 };\n\
             struct PsOut { @location(0) c0: vec4<f32> };\n\
             @fragment fn main(input: PsIn) -> PsOut {\n\
             di = input.di;\n\
             var r: array<vec4<f32>, 64>;\n\
             r[0] = input.i0; r[1] = input.i1; r[2] = input.i2; r[3] = input.i3; r[4] = input.i4; r[5] = input.i5; r[6] = input.i6; r[7] = input.i7;\n\
             r[8] = input.i8; r[9] = input.i9; r[10] = input.i10; r[11] = input.i11; r[12] = input.i12; r[13] = input.i13; r[14] = input.i14; r[15] = input.i15;\n\
             var colors: array<vec4<f32>, 4>;\nvar killed = false;\n",
        );
    } else if let Some(allow) = trim {
        s.push_str(&trimmed_vs_head(t.interp & allow));
    } else {
        s.push_str(
            "struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) i0: vec4<f32>, @location(1) i1: vec4<f32>, @location(2) i2: vec4<f32>, @location(3) i3: vec4<f32>,\n\
             @location(4) i4: vec4<f32>, @location(5) i5: vec4<f32>, @location(6) i6: vec4<f32>, @location(7) i7: vec4<f32>, @location(8) i8: vec4<f32>, @location(9) i9: vec4<f32>,\n\
             @location(10) i10: vec4<f32>, @location(11) i11: vec4<f32>, @location(12) i12: vec4<f32>, @location(13) i13: vec4<f32>, @location(14) i14: vec4<f32>, @location(15) i15: vec4<f32>,\n\
             @location(16) @interpolate(flat) di: u32 };\n\
             @vertex fn main(@builtin(vertex_index) vid: u32, @builtin(instance_index) ii: u32) -> VsOut {\n\
             di = ii;\n\
             var r: array<vec4<f32>, 64>;\n\
             r[0] = vec4<f32>(f32(vid), 0.0, 0.0, 0.0);\n\
             var position = vec4<f32>(0.0, 0.0, 0.0, 0.0);\nvar interp: array<vec4<f32>, 16>;\nvar psize = 0.0;\n",
        );
    }
    s.push_str(
        "var pred = false;\nvar ps = 0.0;\nvar a0 = 0;\nvar al = 0;\n\
         var lbody: array<i32, 4>; var lrem: array<u32, 4>; var lal: array<i32, 4>; var lstep: array<i32, 4>; var ld = 0u;\n\
         var cstack: array<i32, 4>; var cd = 0u;\n\
         var pc = 0;\nvar done = false;\nvar guard = 0;\n\
         loop {\nif (done || guard > 100000) { break; }\nguard = guard + 1;\nswitch pc {\n",
    );
    s.push_str(&body);
    s.push_str("default: { done = true; }\n}\n}\n");
    if pixel {
        // The alpha test compares the 8-bit value the render target stores, as the hardware does: a
        // constant alpha of 1.0 interpolates to 0.99999994 here and there, and the game's alpha >= 1.0
        // tests dropped those pixels (speckled player kits).
        s.push_str(
            "if (killed) { discard; }\n\
             if ((consts.alpha_test & 8u) != 0u) {\n\
             let a = round(clamp(colors[0].w, 0.0, 1.0) * 255.0); let r = round(clamp(bitcast<f32>(consts.alpha_ref), 0.0, 1.0) * 255.0);\n\
             var alpha_ok = true;\n\
             switch consts.alpha_test & 7u {\n\
             case 0u: { alpha_ok = false; } case 1u: { alpha_ok = a < r; } case 2u: { alpha_ok = a == r; } case 3u: { alpha_ok = a <= r; }\n\
             case 4u: { alpha_ok = a > r; } case 5u: { alpha_ok = a != r; } case 6u: { alpha_ok = a >= r; } default: { alpha_ok = true; }\n\
             }\n\
             if (!alpha_ok) { discard; }\n\
             }\n\
             var o: PsOut;\no.c0 = colors[0];\nreturn o;\n}\n",
        );
    } else if let Some(allow) = trim {
        s.push_str(&trimmed_vs_tail(t.interp & allow));
    } else {
        s.push_str(
            "var o: VsOut;\no.pos = xenos_position(position);\no.di = di;\n\
             o.i0 = interp[0]; o.i1 = interp[1]; o.i2 = interp[2]; o.i3 = interp[3]; o.i4 = interp[4]; o.i5 = interp[5]; o.i6 = interp[6]; o.i7 = interp[7];\n\
             o.i8 = interp[8]; o.i9 = interp[9]; o.i10 = interp[10]; o.i11 = interp[11]; o.i12 = interp[12]; o.i13 = interp[13]; o.i14 = interp[14]; o.i15 = interp[15];\n\
             return o;\n}\n",
        );
    }
    let (s, const_slots) = compact_consts(&s, if pixel { 256 } else { 0 }, if pixel { "consts.pf_off" } else { "consts.vf_off" });
    // every constant read goes through this draw's record
    let s = s.replace("consts.", "consts_arr[di].");
    let interp = if pixel { trim.unwrap_or(0) } else { t.interp & trim.unwrap_or(0xFFFF) };
    Translated { wgsl: s, textures: t.textures, vertex_consts: t.vertex_consts, const_slots, interp }
}

/// A trimmed pixel shader's input struct and entry: the draw index, then `mask`'s interpolants.
fn trimmed_ps_head(mask: u16) -> String {
    let mut s = String::from("struct PsIn { @builtin(position) frag: vec4<f32>, @location(0) @interpolate(flat) di: u32");
    let mut copy = String::new();
    for d in 0..16u32 {
        if mask & (1 << d) != 0 {
            let _ = write!(s, ", @location({}) i{d}: vec4<f32>", interp_location(mask, d));
            let _ = write!(copy, "r[{d}] = input.i{d}; ");
        }
    }
    s.push_str(" };\nstruct PsOut { @location(0) c0: vec4<f32> };\n@fragment fn main(input: PsIn) -> PsOut {\ndi = input.di;\nvar r: array<vec4<f32>, 64>;\n");
    s.push_str(&copy);
    s.push_str("\nvar colors: array<vec4<f32>, 4>;\nvar killed = false;\n");
    s
}

/// A trimmed vertex shader's output struct and entry: the draw index, then the interpolants it
/// exports (`mask`).
fn trimmed_vs_head(mask: u16) -> String {
    let mut s = String::from("struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) @interpolate(flat) di: u32");
    for d in 0..16u32 {
        if mask & (1 << d) != 0 {
            let _ = write!(s, ", @location({}) i{d}: vec4<f32>", interp_location(mask, d));
        }
    }
    s.push_str(" };\n@vertex fn main(@builtin(vertex_index) vid: u32, @builtin(instance_index) ii: u32) -> VsOut {\ndi = ii;\nvar r: array<vec4<f32>, 64>;\nr[0] = vec4<f32>(f32(vid), 0.0, 0.0, 0.0);\nvar position = vec4<f32>(0.0, 0.0, 0.0, 0.0);\nvar interp: array<vec4<f32>, 16>;\nvar psize = 0.0;\n");
    s
}

/// A trimmed vertex shader's return: position, draw index and the exported interpolants.
fn trimmed_vs_tail(mask: u16) -> String {
    let mut s = String::from("var o: VsOut;\no.pos = xenos_position(position);\no.di = di;\n");
    for d in 0..16u32 {
        if mask & (1 << d) != 0 {
            let _ = write!(s, "o.i{d} = interp[{d}]; ");
        }
    }
    s.push_str("\nreturn o;\n}\n");
    s
}

fn cond_expr(c: &ExecCond) -> String {
    match *c {
        ExecCond::Always => "true".into(),
        ExecCond::Bool { index, value } => format!("bool_const({index}u) == {value}"),
        ExecCond::Pred { value } => format!("pred == {value}"),
    }
}

const COMP: [char; 4] = ['x', 'y', 'z', 'w'];

struct Tr {
    /// Extra module-level items (none yet; kept for helper functions).
    out: String,
    pixel: bool,
    textures: Vec<u32>,
    vertex_consts: Vec<u32>,
    /// Vertex shader: the interpolants an export writes (bit d = `interp[d]`).
    interp: u16,
}

impl Tr {
    fn const_base(&self) -> u32 {
        if self.pixel { 256 } else { 0 }
    }

    /// WGSL expression for ALU source operand `i` (same rules as `shader_exec::operand`).
    fn operand(&self, a: &Alu, i: usize) -> String {
        let reg = a.reg[i];
        let raw = if a.is_temp[i] {
            let idx = if reg & 0x40 != 0 { format!("r[({} + al) & 63]", reg & 0x3F) } else { format!("r[{}]", reg & 0x3F) };
            if reg & 0x80 != 0 { format!("abs({idx})") } else { idx }
        } else {
            let addressed = match i {
                0 => a.const_rel[0],
                1 => if a.is_temp[0] { a.const_rel[0] } else { a.const_rel[1] },
                _ => if a.is_temp[0] && a.is_temp[1] { a.const_rel[0] } else { a.const_rel[1] },
            };
            let idx = if addressed {
                format!("consts.f[{}u + (u32({reg} + {}) & 255u)]", self.const_base(), if a.const_addr_reg_rel { "a0" } else { "al" })
            } else {
                format!("consts.f[{}u]", self.const_base() + (reg & 255))
            };
            if a.abs_constants { format!("abs({idx})") } else { idx }
        };
        let sw: String = (0..4).map(|c| COMP[alu_component(a.swiz[i], c)]).collect();
        let v = if sw == "xyzw" { raw } else { format!("{raw}.{sw}") };
        if a.negate[i] { format!("(-{v})") } else { v }
    }

    fn alu(&mut self, a: &Alu, c: &mut String) {
        let nvec = VECTOR_OPERANDS[a.vector_op as usize] as usize;
        let _ = writeln!(c, "{{ // {} | {}", VECTOR_OPS[a.vector_op as usize], SCALAR_OPS.get(a.scalar_op as usize).copied().unwrap_or("?"));
        if a.predicated {
            let _ = writeln!(c, "if (pred == {}) {{", a.pred_cond);
        }
        for i in 0..nvec.max(1) {
            let _ = writeln!(c, "let s{i} = {};", self.operand(a, i));
        }
        if nvec < 2 {
            c.push_str("let s1 = vec4<f32>(0.0);\n");
        }
        if nvec < 3 {
            c.push_str("let s2 = vec4<f32>(0.0);\n");
        }
        c.push_str("var pv: i32 = -1;\n"); // predicate set by the vector unit: -1 none, 0/1
        let v = match a.vector_op {
            0 => "s0 + s1".to_string(),
            1 => "mul0v(s0, s1)".into(),
            2 => "select(s1, s0, s0 >= s1)".into(),
            3 => "select(s1, s0, s0 < s1)".into(),
            4 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 == s1)".into(),
            5 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 > s1)".into(),
            6 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 >= s1)".into(),
            7 => "select(vec4<f32>(0.0), vec4<f32>(1.0), s0 != s1)".into(),
            8 => "s0 - floor(s0)".into(),
            9 => "trunc(s0)".into(),
            10 => "floor(s0)".into(),
            11 => "mul0v(s0, s1) + s2".into(),
            12 => "select(s2, s1, s0 == vec4<f32>(0.0))".into(),
            13 => "select(s2, s1, s0 >= vec4<f32>(0.0))".into(),
            14 => "select(s2, s1, s0 > vec4<f32>(0.0))".into(),
            15 => "vec4<f32>(mul0(s0.x, s1.x) + mul0(s0.y, s1.y) + mul0(s0.z, s1.z) + mul0(s0.w, s1.w))".into(),
            16 => "vec4<f32>(mul0(s0.x, s1.x) + mul0(s0.y, s1.y) + mul0(s0.z, s1.z))".into(),
            17 => "vec4<f32>(mul0(s0.x, s1.x) + mul0(s0.y, s1.y) + s2.x)".into(),
            19 => "vec4<f32>(max(max(s0.x, s0.y), max(s0.z, s0.w)))".into(),
            20..=23 => {
                let hit = match a.vector_op {
                    20 => "s0.w == 0.0 && s1.w == 0.0",
                    21 => "s0.w != 0.0 || s1.w != 0.0",
                    22 => "s0.w > 0.0 && s1.w > 0.0",
                    _ => "s0.w >= 0.0 && s1.w >= 0.0",
                };
                let _ = writeln!(c, "let hit = {hit}; pv = select(0, 1, hit);");
                "vec4<f32>(select(s0.x + 1.0, 0.0, hit))".into()
            }
            24..=27 => {
                let op = ["==", ">", ">=", "!="][(a.vector_op - 24) as usize];
                let _ = writeln!(c, "let k = any(s0 {op} s1);");
                if self.pixel {
                    c.push_str("if (k) { killed = true; }\n");
                }
                "vec4<f32>(b2f(k))".into()
            }
            28 => "vec4<f32>(1.0, mul0(s0.y, s1.y), s0.z, s1.w)".into(),
            29 => {
                c.push_str("a0 = i32(clamp(floor(s0.w + 0.5), -256.0, 255.0));\n");
                "select(s1, s0, s0 >= s1)".into()
            }
            _ => "vec4<f32>(0.0)".into(),
        };
        let _ = writeln!(c, "var vres = {v};");

        // scalar unit
        let sop = a.scalar_op as usize;
        let mut changes_ps = true;
        let mut sres = "ps".to_string();
        if sop < SCALAR_OPS.len() {
            match SCALAR_OPERANDS[sop] {
                2 => {
                    // constant (relative addressing like operand 2) and a temp chosen by the op bits
                    let addressed = if a.is_temp[0] && a.is_temp[1] { a.const_rel[0] } else { a.const_rel[1] };
                    let reg = a.reg[2];
                    let cidx = if addressed {
                        format!("consts.f[{}u + (u32({reg} + {}) & 255u)]", self.const_base(), if a.const_addr_reg_rel { "a0" } else { "al" })
                    } else {
                        format!("consts.f[{}u]", self.const_base() + (reg & 255))
                    };
                    let cc = COMP[alu_component(a.swiz[2], 3)];
                    let cexpr = if a.abs_constants { format!("abs({cidx}.{cc})") } else { format!("{cidx}.{cc}") };
                    let ti = (((a.scalar_op & 1) | ((a.is_temp[2] as u32) << 1) | (a.swiz[2] & 0x3C)) & 63) as usize;
                    let tc = COMP[alu_component(a.swiz[2], 0)];
                    let neg = if a.negate[2] { "-" } else { "" };
                    let _ = writeln!(c, "let x = {neg}{cexpr}; let y = {neg}r[{ti}].{tc};");
                }
                1 => {
                    let _ = writeln!(c, "let sv = {};", self.operand(a, 2));
                    let _ = writeln!(c, "let x = sv.w; let y = sv.{};", if nvec == 3 { 'z' } else { 'x' });
                }
                _ => c.push_str("let x = 0.0; let y = 0.0;\n"),
            }
            sres = match sop {
                0 => "x + y".into(),
                1 => "x + ps".into(),
                2 => "mul0(x, y)".into(),
                3 => "mul0(x, ps)".into(),
                4 => "select(mul0(x, ps), -3.4028235e38, ps == -3.4028235e38 || abs(ps) > 3.4028235e38 || y <= 0.0)".into(),
                5 => "select(y, x, x >= y)".into(),
                6 => "select(y, x, x < y)".into(),
                7 => "b2f(x == 0.0)".into(),
                8 => "b2f(x > 0.0)".into(),
                9 => "b2f(x >= 0.0)".into(),
                10 => "b2f(x != 0.0)".into(),
                11 => "x - floor(x)".into(),
                12 => "trunc(x)".into(),
                13 => "floor(x)".into(),
                14 => "exp2(x)".into(),
                15 => "select(log2(x), -3.4028235e38, x == 0.0)".into(),
                16 => "log2(x)".into(),
                17 => "clamp(1.0 / x, -3.4028235e38, 3.4028235e38)".into(),
                18 => "select(1.0 / x, 0.0, x == 0.0)".into(),
                19 => "1.0 / x".into(),
                20 => "min(inverseSqrt(x), 3.4028235e38)".into(),
                21 => "select(inverseSqrt(x), 0.0, x == 0.0)".into(),
                22 => "inverseSqrt(x)".into(),
                23 | 24 => {
                    c.push_str("a0 = i32(clamp(floor(x + 0.5), -256.0, 255.0));\n");
                    "select(y, x, x >= y)".into()
                }
                25 => "x - y".into(),
                26 => "x - ps".into(),
                27 => {
                    c.push_str("pred = x == 0.0;\n");
                    "b2f(!pred)".into()
                }
                28 => {
                    c.push_str("pred = x != 0.0;\n");
                    "b2f(!pred)".into()
                }
                29 => {
                    c.push_str("pred = x > 0.0;\n");
                    "b2f(!pred)".into()
                }
                30 => {
                    c.push_str("pred = x >= 0.0;\n");
                    "b2f(!pred)".into()
                }
                31 => {
                    c.push_str("pred = x == 0.0;\n");
                    "select(select(x, 0.0, x == 1.0), 1.0, x == 0.0)".into()
                }
                32 => {
                    c.push_str("pred = x - 1.0 <= 0.0;\n");
                    "select(x - 1.0, 0.0, x - 1.0 <= 0.0)".into()
                }
                33 => {
                    c.push_str("pred = false;\n");
                    "3.4028235e38".into()
                }
                34 => {
                    c.push_str("pred = x == 0.0;\n");
                    "x".into()
                }
                35..=39 => {
                    let k = ["x == 0.0", "x > 0.0", "x >= 0.0", "x != 0.0", "x == 1.0"][sop - 35];
                    let _ = writeln!(c, "let sk = {k};");
                    if self.pixel {
                        c.push_str("if (sk) { killed = true; }\n");
                    }
                    "b2f(sk)".into()
                }
                40 => "sqrt(x)".into(),
                42 | 43 => "mul0(x, y)".into(),
                44 | 45 => "x + y".into(),
                46 | 47 => "x - y".into(),
                48 => "sin(x)".into(),
                49 => "cos(x)".into(),
                50 => {
                    changes_ps = false;
                    "ps".into()
                }
                _ => "ps".into(),
            };
        }
        let _ = writeln!(c, "var sres = {sres};");
        c.push_str("if (pv >= 0) { pred = pv == 1; }\n");
        if a.vector_clamp {
            c.push_str("vres = clamp(vres, vec4<f32>(0.0), vec4<f32>(1.0));\n");
        }
        if a.scalar_clamp {
            c.push_str("sres = clamp(sres, 0.0, 1.0);\n");
        }
        if changes_ps {
            c.push_str("ps = sres;\n");
        }

        // writes
        if a.export {
            let vmask = a.vector_mask & !a.scalar_mask;
            let smask = a.scalar_mask & !a.vector_mask;
            let one_mask = a.vector_mask & a.scalar_mask;
            let zero_mask = if a.scalar_dest_rel { 0xF & !(a.vector_mask | a.scalar_mask) } else { 0 };
            let target = match (self.pixel, a.vector_dest) {
                (false, 62) => Some("position".to_string()),
                (false, 63) => Some("pst".to_string()),
                (false, d @ 0..=15) => Some(format!("interp[{d}]")),
                (true, d @ 0..=3) => Some(format!("colors[{d}]")),
                _ => None,
            };
            if !self.pixel && a.vector_dest <= 15 && (vmask | smask | one_mask | zero_mask) != 0 {
                self.interp |= 1 << a.vector_dest;
            }
            if let Some(tgt) = target {
                if tgt == "pst" {
                    c.push_str("var pst = vec4<f32>(psize, 0.0, 0.0, 0.0);\n");
                }
                for i in 0..4 {
                    let comp = COMP[i];
                    if vmask & (1 << i) != 0 {
                        let _ = writeln!(c, "{tgt}.{comp} = vres.{comp};");
                    }
                    if smask & (1 << i) != 0 {
                        let _ = writeln!(c, "{tgt}.{comp} = sres;");
                    }
                    if one_mask & (1 << i) != 0 {
                        let _ = writeln!(c, "{tgt}.{comp} = 1.0;");
                    }
                    if zero_mask & (1 << i) != 0 {
                        let _ = writeln!(c, "{tgt}.{comp} = 0.0;");
                    }
                }
                if tgt == "pst" {
                    c.push_str("psize = pst.x;\n");
                }
            }
        } else {
            let vd = if a.vector_dest_rel { format!("({} + al) & 63", a.vector_dest) } else { format!("{}", a.vector_dest & 63) };
            for i in 0..4 {
                if a.vector_mask & (1 << i) != 0 {
                    let _ = writeln!(c, "r[{vd}].{} = vres.{};", COMP[i], COMP[i]);
                }
            }
            let sd = if a.scalar_dest_rel { format!("({} + al) & 63", a.scalar_dest) } else { format!("{}", a.scalar_dest & 63) };
            for i in 0..4 {
                if a.scalar_mask & (1 << i) != 0 {
                    let _ = writeln!(c, "r[{sd}].{} = sres;", COMP[i]);
                }
            }
        }
        if a.predicated {
            c.push_str("}\n");
        }
        c.push_str("}\n");
    }

    fn vfetch(&mut self, v: &VertexFetch, c: &mut String) {
        let _ = writeln!(c, "{{ // vfetch fmt {} stride {} offset {}", v.format, v.stride, v.offset);
        if v.predicated {
            let _ = writeln!(c, "if (pred == {}) {{", v.pred_cond);
        }
        let k = v.const_index as usize;
        if !self.vertex_consts.contains(&(k as u32)) {
            self.vertex_consts.push(k as u32);
        }
        let _ = writeln!(c, "let index = u32(i32(round(r[{}].{})));", v.src, COMP[v.src_swiz as usize]);
        let _ = writeln!(c, "let d0 = fetch_arr[consts.fetch_idx][{}]; let d1 = fetch_arr[consts.fetch_idx][{}];", 2 * k, 2 * k + 1);
        c.push_str("let base = d0 & 0xFFFFFFFCu; let endian = d1 & 3u;\n");
        let _ = writeln!(c, "let addr = base + u32(i32(index * {}u) + ({})) * 4u;", v.stride, v.offset);
        let norm = if v.normalized { "true" } else { "false" };
        let conv = |raw: &str, bits: u32| {
            if v.signed { format!("x_snorm({raw}, {bits}u, {norm})") } else { format!("x_unorm({raw}, {bits}u, {norm})") }
        };
        c.push_str("var o = vec4<f32>(0.0, 0.0, 0.0, 1.0);\n");
        let ncomp = match v.format {
            33 | 36 => 1,
            25 | 31 | 34 | 37 => 2,
            16 | 17 | 57 => 3,
            _ => 4,
        };
        match v.format {
            36 | 37 | 38 | 57 => {
                for k in 0..ncomp {
                    let _ = writeln!(c, "o.{} = bitcast<f32>(vread(addr + {}u, endian));", COMP[k], 4 * k);
                }
            }
            33..=35 => {
                for k in 0..ncomp {
                    let _ = writeln!(c, "o.{} = {};", COMP[k], conv(&format!("vread(addr + {}u, endian)", 4 * k), 32));
                }
            }
            6 => {
                c.push_str("let d = vread(addr, endian);\n");
                for k in 0..4 {
                    let _ = writeln!(c, "o.{} = {};", COMP[k], conv(&format!("((d >> {}u) & 0xFFu)", 8 * k), 8));
                }
            }
            7 => {
                c.push_str("let d = vread(addr, endian);\n");
                let _ = writeln!(
                    c,
                    "o = vec4<f32>({}, {}, {}, {});",
                    conv("(d & 0x3FFu)", 10),
                    conv("((d >> 10u) & 0x3FFu)", 10),
                    conv("((d >> 20u) & 0x3FFu)", 10),
                    conv("(d >> 30u)", 2)
                );
            }
            25 | 26 => {
                for k in 0..ncomp {
                    let _ = writeln!(c, "o.{} = {};", COMP[k], conv(&format!("((vread(addr + {}u, endian) >> {}u) & 0xFFFFu)", 4 * (k / 2), 16 * (k % 2)), 16));
                }
            }
            31 | 32 => {
                for k in 0..ncomp {
                    let _ = writeln!(c, "o.{} = x_half(vread(addr + {}u, endian) >> {}u);", COMP[k], 4 * (k / 2), 16 * (k % 2));
                }
            }
            16 | 17 => {
                let (bx, by) = if v.format == 16 { (10u32, 11u32) } else { (11, 11) };
                c.push_str("let d = vread(addr, endian);\n");
                let _ = writeln!(c, "o.x = {};", conv(&format!("(d & {}u)", (1u32 << bx) - 1), bx));
                let _ = writeln!(c, "o.y = {};", conv(&format!("((d >> {bx}u) & {}u)", (1u32 << by) - 1), by));
                let _ = writeln!(c, "o.z = {};", conv(&format!("(d >> {}u)", bx + by), 32 - bx - by));
            }
            _ => {}
        }
        if v.exp_adjust != 0 {
            let s = 2f32.powi(v.exp_adjust);
            for k in 0..ncomp {
                let _ = writeln!(c, "o.{} = o.{} * {s:?};", COMP[k], COMP[k]);
            }
        }
        write_fetch_swizzle(c, v.dst, v.dst_swiz, "o");
        if v.predicated {
            c.push_str("}\n");
        }
        c.push_str("}\n");
    }

    fn tfetch(&mut self, t: &TextureFetch, c: &mut String) {
        if t.opcode != 1 {
            return; // LOD/gradient queries: the interpreter ignores them too
        }
        if !self.pixel {
            return; // vertex texture fetch: not supported by the backend yet
        }
        let k = t.const_index;
        if !self.textures.contains(&k) {
            self.textures.push(k);
        }
        let _ = writeln!(c, "{{ // tfetch tf{k}");
        if t.predicated {
            let _ = writeln!(c, "if (pred == {}) {{", t.pred_cond);
        }
        let comps: String = (0..4).map(|i| COMP[((t.src_swiz >> (2 * i)) & 3) as usize]).collect();
        let _ = writeln!(c, "let coord = r[{}].{comps};", t.src);
        if t.unnormalized {
            let _ = writeln!(c, "let uv = coord.xy / vec2<f32>(textureDimensions(tex{k}));");
        } else {
            c.push_str("let uv = coord.xy;\n");
        }
        // the mip level from the coordinates' screen-space derivatives, as the hardware picks
        // it (level 0 only made minified textures sparkle: noisy kits on distant players)
        let _ = writeln!(c, "let o = textureSampleGrad(tex{k}, smp{k}, uv, dpdx(uv), dpdy(uv));");
        write_fetch_swizzle(c, t.dst, t.dst_swiz, "o");
        if t.predicated {
            c.push_str("}\n");
        }
        c.push_str("}\n");
    }
}

/// `apply_fetch_swizzle`: 0-3 component, 4 zero, 5 one, 6-7 keep.
fn write_fetch_swizzle(c: &mut String, dst: u32, swiz: u32, src: &str) {
    for i in 0..4 {
        match (swiz >> (3 * i)) & 7 {
            s @ 0..=3 => {
                let _ = writeln!(c, "r[{dst}].{} = {src}.{};", COMP[i], COMP[s as usize]);
            }
            4 => {
                let _ = writeln!(c, "r[{dst}].{} = 0.0;", COMP[i]);
            }
            5 => {
                let _ = writeln!(c, "r[{dst}].{} = 1.0;", COMP[i]);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compact_consts;
    use super::{interp_location, translate_with, Stage};
    use crate::shader::Shader;

    /// One ALU instruction: `max` of r`src` with itself into export `dest` (all four components).
    fn export(dest: u32, src: u32) -> [u32; 3] {
        [dest | (1 << 15) | (0xF << 16) | (50 << 26), 0, (src << 16) | (src << 8) | src | (2 << 24) | (7 << 29)]
    }

    /// A program of ALU instructions in one exec that ends it.
    fn program(instrs: &[[u32; 3]]) -> Shader {
        let n = instrs.len() as u32;
        let cf = [1 | (n << 12), 2 << 12, 0];
        Shader::new([cf].iter().chain(instrs).flatten().copied().collect())
    }

    fn validate(name: &str, src: &str) {
        let module = naga::front::wgsl::parse_str(src).unwrap_or_else(|e| panic!("{name}: {}", e.emit_to_string(src)));
        naga::valid::Validator::new(naga::valid::ValidationFlags::all(), naga::valid::Capabilities::all())
            .validate(&module)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    }

    /// A vertex shader exporting interpolants 0, 3 and 7: its trimmed interface carries the draw
    /// index and those three, packed (16-attribute GPUs, #6), and its pixel shader reads them at
    /// the same locations; untrimmed, both keep all 16 and the draw index at 16.
    #[test]
    fn trimmed_interfaces_carry_only_the_exported_interpolants() {
        let vs = program(&[export(62, 1), export(0, 1), export(3, 2), export(7, 3)]);
        let full = translate_with(&vs, Stage::Vertex, None);
        assert_eq!(full.interp, 0b1000_1001);
        assert!(full.wgsl.contains("@location(15) i15: vec4<f32>") && full.wgsl.contains("@location(16) @interpolate(flat) di: u32"));
        let t = translate_with(&vs, Stage::Vertex, Some(0xFFFF));
        assert_eq!(t.interp, 0b1000_1001);
        validate("trimmed vs", &t.wgsl);
        assert!(t.wgsl.contains("@location(0) @interpolate(flat) di: u32, @location(1) i0: vec4<f32>, @location(2) i3: vec4<f32>, @location(3) i7: vec4<f32> };"), "{}", t.wgsl);
        assert!(!t.wgsl.contains("i1:") && t.wgsl.contains("o.i7 = interp[7];"));
        assert_eq!((0..16).filter(|&d| t.interp & (1 << d) != 0).map(|d| interp_location(t.interp, d)).collect::<Vec<_>>(), vec![1, 2, 3]);

        let ps = program(&[export(0, 3)]);
        let p = translate_with(&ps, Stage::Pixel, Some(t.interp));
        validate("trimmed ps", &p.wgsl);
        assert!(p.wgsl.contains("@location(0) @interpolate(flat) di: u32, @location(1) i0: vec4<f32>, @location(2) i3: vec4<f32>, @location(3) i7: vec4<f32> };"), "{}", p.wgsl);
        assert!(p.wgsl.contains("r[3] = input.i3;") && !p.wgsl.contains("input.i1"), "the others keep their zero");
        let p_full = translate_with(&ps, Stage::Pixel, None);
        assert!(p_full.wgsl.contains("r[15] = input.i15;"));
        // a device allowing fewer: the vertex shader outputs only the allowed ones
        let capped = translate_with(&vs, Stage::Vertex, Some(0b1001));
        assert_eq!(capped.interp, 0b1001);
        validate("capped vs", &capped.wgsl);
        assert!(!capped.wgsl.contains("o.i7") && capped.wgsl.contains("o.i3 = interp[3];"));
    }

    #[test]
    fn constant_reads_become_compact_slots() {
        // absolute reads only: dense slots in register order
        let (s, slots) = compact_consts("a = consts.f[300u]; b = consts.f[257u]; c = consts.f[300u];", 256, "consts.pf_off");
        assert_eq!(slots, Some(vec![257, 300]));
        assert_eq!(s, "a = fvals[consts.pf_off + 1u]; b = fvals[consts.pf_off + 0u]; c = fvals[consts.pf_off + 1u];");
        // any relative read: the stage's whole block, indices relative to its base
        let (s, slots) = compact_consts("a = consts.f[5u]; b = consts.f[0u + (u32(3 + a0) & 255u)];", 0, "consts.vf_off");
        assert_eq!(slots, None);
        assert_eq!(s, "a = fb(consts.vf_off, 5u); b = fb(consts.vf_off, (u32(3 + a0) & 255u));");
        let (s, _) = compact_consts("b = consts.f[256u + (u32(3 + a0) & 255u)].x * consts.f[256u + (u32(r[2] + al) & 255u)].y;", 256, "o");
        assert_eq!(s, "b = fb(o, (u32(3 + a0) & 255u)).x * fb(o, (u32(r[2] + al) & 255u)).y;");
        // no constants at all
        assert_eq!(compact_consts("x", 0, "o"), ("x".to_string(), Some(vec![])));
    }
}
