//! AltiVec + VMX128 for the reference interpreter.
//!
//! Registers are 16 big-endian bytes (element 0 is the most significant). Field layouts follow
//! Xenia's ppc_decode_data.h; semantics follow its ppc_emit_altivec.cc. Instructions are
//! dispatched by mnemonic from the generated decoder (`ppc::lookup`).

use crate::cpu::{Cpu, Stop};
use crate::mem::Memory;
use ppc::bits;

pub type V = [u8; 16];

#[inline(always)]
fn w(v: &V) -> [u32; 4] {
    std::array::from_fn(|i| u32::from_be_bytes(v[4 * i..4 * i + 4].try_into().unwrap()))
}
#[inline(always)]
fn from_w(x: [u32; 4]) -> V {
    let mut v = [0; 16];
    for i in 0..4 {
        v[4 * i..4 * i + 4].copy_from_slice(&x[i].to_be_bytes());
    }
    v
}
fn h(v: &V) -> [u16; 8] {
    std::array::from_fn(|i| u16::from_be_bytes([v[2 * i], v[2 * i + 1]]))
}
fn from_h(x: [u16; 8]) -> V {
    let mut v = [0; 16];
    for i in 0..8 {
        v[2 * i..2 * i + 2].copy_from_slice(&x[i].to_be_bytes());
    }
    v
}
#[inline(always)]
fn f(v: &V) -> [f32; 4] {
    w(v).map(f32::from_bits)
}
#[inline(always)]
fn from_f(x: [f32; 4]) -> V {
    from_w(x.map(f32::to_bits))
}
/// vmaxfp / vminfp as AltiVec defines them, the same on every host: a NaN operand gives that
/// operand made quiet (vA's when both are NaN), max(-0, +0) = +0 and min(-0, +0) = -0.
fn max_min(x: f32, y: f32, max: bool) -> f32 {
    let quiet = |v: f32| f32::from_bits(v.to_bits() | 0x0040_0000);
    if x.is_nan() {
        quiet(x)
    } else if y.is_nan() {
        quiet(y)
    } else if x != y {
        if (x > y) == max { x } else { y }
    } else if max {
        // equal: the same bits, or -0 and +0, where max takes +0 and min -0
        f32::from_bits(x.to_bits() & y.to_bits())
    } else {
        f32::from_bits(x.to_bits() | y.to_bits())
    }
}
// Operations the recompiled code also runs natively (rec::vaddfp ...): one implementation for both,
// so the results are the interpreter's (#6).
#[inline(always)]
pub fn add_f(a: &V, b: &V) -> V {
    zip_f(a, b, |x, y| x + y)
}
#[inline(always)]
pub fn sub_f(a: &V, b: &V) -> V {
    zip_f(a, b, |x, y| x - y)
}
#[inline(always)]
pub fn mul_f(a: &V, b: &V) -> V {
    zip_f(a, b, |x, y| x * y)
}
/// x * y + z, fused, per lane
#[inline(always)]
pub fn madd_f(x: &V, y: &V, z: &V) -> V {
    let (x, y, z) = (f(x), f(y), f(z));
    from_f(std::array::from_fn(|i| x[i].mul_add(y[i], z[i])))
}
#[inline(always)]
pub fn msum3_f(a: &V, b: &V) -> V {
    let (x, y) = (f(a), f(b));
    let d = x[0] * y[0] + x[1] * y[1] + x[2] * y[2];
    from_f([d; 4])
}
#[inline(always)]
pub fn sldoi(a: &V, b: &V, sh: usize) -> V {
    let mut cat = [0u8; 32];
    cat[..16].copy_from_slice(a);
    cat[16..].copy_from_slice(b);
    std::array::from_fn(|i| cat[i + sh])
}
fn sat_i16(x: i32) -> u16 {
    x.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16
}
fn sat_i8(x: i32) -> u8 {
    x.clamp(i8::MIN as i32, i8::MAX as i32) as i8 as u8
}

/// Register numbers for the instruction formats.
struct Regs {
    d: usize,
    a: usize,
    b: usize,
    c: usize,
}

fn std_regs(wd: u32) -> Regs {
    Regs {
        d: bits(wd, 6, 10) as usize,
        a: bits(wd, 11, 15) as usize,
        b: bits(wd, 16, 20) as usize,
        c: bits(wd, 21, 25) as usize,
    }
}

fn vx128_regs(wd: u32) -> Regs {
    Regs {
        d: (bits(wd, 6, 10) | bits(wd, 28, 29) << 5) as usize,
        a: (bits(wd, 11, 15) | bits(wd, 26, 26) << 5 | bits(wd, 21, 21) << 6) as usize,
        b: (bits(wd, 16, 20) | bits(wd, 30, 31) << 5) as usize,
        c: bits(wd, 23, 25) as usize,
    }
}

impl Cpu {
    fn ea_vx(&self, ra: usize, rb: usize) -> u32 {
        let base = if ra == 0 { 0 } else { self.gpr[ra] as u32 };
        base.wrapping_add(self.gpr[rb] as u32)
    }

    fn set_cr6(&mut self, all: bool, none: bool) {
        self.cr[6] = if all { 8 } else { 0 } | if none { 2 } else { 0 };
    }

    /// Execute a vector instruction. Returns Err for forms not implemented yet.
    pub(crate) fn vmx(&mut self, mem: &mut Memory, wd: u32, name: &'static str) -> Result<(), Stop> {
        let is128 = name.ends_with("128");
        let r = if is128 { vx128_regs(wd) } else { std_regs(wd) };
        let (va, vb, vc) = (self.vr[r.a], self.vr[r.b], self.vr[r.c]);
        let vd_old = self.vr[r.d];
        let unimpl = || Stop::Unimplemented { pc: self.pc, word: wd, mnemonic: Some(name) };
        // loads/stores use the integer RA/RB fields
        let (ra, rb) = (bits(wd, 11, 15) as usize, bits(wd, 16, 20) as usize);
        let uimm5 = bits(wd, 11, 15);
        let simm5 = ((uimm5 as i32) << 27) >> 27;
        // strip_suffix, not trim_end_matches: that builds a substring searcher per call, and this
        // runs for every vector instruction the interpreter executes (match loading: 25x slower)
        let base = name.strip_suffix("128").unwrap_or(name);

        let out: V = match base {
            // ---- loads / stores ------------------------------------------
            "lvx" | "lvxl" => {
                let ea = self.ea_vx(ra, rb) & !15;
                let mut v = [0; 16];
                mem.read(ea, &mut v);
                v
            }
            "stvx" | "stvxl" => {
                let ea = self.ea_vx(ra, rb) & !15;
                mem.write(ea, &vd_old);
                return Ok(());
            }
            "lvlx" | "lvlxl" => {
                let ea = self.ea_vx(ra, rb);
                let n = 16 - (ea & 15) as usize;
                let mut v = [0; 16];
                mem.read(ea, &mut v[..n]);
                v
            }
            "lvrx" | "lvrxl" => {
                let ea = self.ea_vx(ra, rb);
                let n = (ea & 15) as usize;
                let mut v = [0; 16];
                mem.read(ea & !15, &mut v[16 - n..]);
                v
            }
            "stvlx" | "stvlxl" => {
                let ea = self.ea_vx(ra, rb);
                let n = 16 - (ea & 15) as usize;
                mem.write(ea, &vd_old[..n]);
                return Ok(());
            }
            "stvrx" | "stvrxl" => {
                let ea = self.ea_vx(ra, rb);
                let n = (ea & 15) as usize;
                mem.write(ea & !15, &vd_old[16 - n..]);
                return Ok(());
            }
            "lvewx" => {
                let ea = self.ea_vx(ra, rb) & !3;
                let mut v = vd_old;
                let i = ((ea >> 2) & 3) as usize;
                let x = mem.read32(ea);
                v[4 * i..4 * i + 4].copy_from_slice(&x.to_be_bytes());
                v
            }
            "stvewx" => {
                let ea = self.ea_vx(ra, rb) & !3;
                let i = ((ea >> 2) & 3) as usize;
                mem.write(ea, &vd_old[4 * i..4 * i + 4]);
                return Ok(());
            }
            "lvsl" | "lvsr" => {
                let sh = (self.ea_vx(ra, rb) & 15) as u8;
                let start = if base == "lvsl" { sh } else { 16 - sh };
                std::array::from_fn(|i| start + i as u8)
            }
            // ---- logical ---------------------------------------------------
            "vor" => std::array::from_fn(|i| va[i] | vb[i]),
            "vand" => std::array::from_fn(|i| va[i] & vb[i]),
            "vandc" => std::array::from_fn(|i| va[i] & !vb[i]),
            "vxor" => std::array::from_fn(|i| va[i] ^ vb[i]),
            "vnor" => std::array::from_fn(|i| !(va[i] | vb[i])),
            "vsel" => {
                let m = if is128 { vd_old } else { vc };
                std::array::from_fn(|i| (va[i] & !m[i]) | (vb[i] & m[i]))
            }
            // ---- float -----------------------------------------------------
            "vaddfp" => add_f(&va, &vb),
            "vsubfp" => sub_f(&va, &vb),
            "vmulfp" => mul_f(&va, &vb),
            // AltiVec: a NaN operand gives a NaN (f32::max/min would return the other operand)
            "vmaxfp" => zip_f(&va, &vb, |x, y| max_min(x, y, true)),
            "vminfp" => zip_f(&va, &vb, |x, y| max_min(x, y, false)),
            "vmaddfp" => {
                // standard: a*c + b; 128: a*b + d
                if is128 { madd_f(&va, &vb, &vd_old) } else { madd_f(&va, &vc, &vb) }
            }
            "vmaddcfp" => {
                let (x, y, z) = (f(&va), f(&vd_old), f(&vb));
                from_f(std::array::from_fn(|i| x[i].mul_add(y[i], z[i])))
            }
            "vnmsubfp" => {
                let (x, y, z) = (f(&va), f(&vc), f(&vb));
                from_f(std::array::from_fn(|i| -(x[i].mul_add(y[i], -z[i]))))
            }
            "vmsum3fp" => msum3_f(&va, &vb),
            "vmsum4fp" => {
                let (x, y) = (f(&va), f(&vb));
                let d = x[0] * y[0] + x[1] * y[1] + x[2] * y[2] + x[3] * y[3];
                from_f([d; 4])
            }
            "vrefp" => from_f(f(&vb).map(|x| 1.0 / x)),
            "vrsqrtefp" => from_f(f(&vb).map(|x| 1.0 / x.sqrt())),
            "vexptefp" => from_f(f(&vb).map(f32::exp2)),
            "vlogefp" => from_f(f(&vb).map(f32::log2)),
            "vrfin" => from_f(f(&vb).map(f32::round_ties_even)),
            "vrfiz" => from_f(f(&vb).map(f32::trunc)),
            "vrfip" => from_f(f(&vb).map(f32::ceil)),
            "vrfim" => from_f(f(&vb).map(f32::floor)),
            "vcfsx" | "vcsxwfp" => {
                let s = (2.0f32).powi(-(uimm5 as i32));
                from_f(w(&vb).map(|x| x as i32 as f32 * s))
            }
            "vcfux" | "vcuxwfp" => {
                let s = (2.0f32).powi(-(uimm5 as i32));
                from_f(w(&vb).map(|x| x as f32 * s))
            }
            "vctsxs" | "vcfpsxws" => {
                let s = (2.0f32).powi(uimm5 as i32);
                from_w(f(&vb).map(|x| if x.is_nan() { 0 } else { (x * s).clamp(i32::MIN as f32, i32::MAX as f32) as i32 as u32 }))
            }
            "vctuxs" | "vcfpuxws" => {
                let s = (2.0f32).powi(uimm5 as i32);
                from_w(f(&vb).map(|x| if x.is_nan() { 0 } else { (x * s).clamp(0.0, u32::MAX as f32) as u32 }))
            }
            // ---- integer ---------------------------------------------------
            "vaddubm" => std::array::from_fn(|i| va[i].wrapping_add(vb[i])),
            "vsububm" => std::array::from_fn(|i| va[i].wrapping_sub(vb[i])),
            "vaddubs" => std::array::from_fn(|i| va[i].saturating_add(vb[i])),
            "vaddsbs" => std::array::from_fn(|i| sat_i8(va[i] as i8 as i32 + vb[i] as i8 as i32)),
            "vadduhm" => zip_h(&va, &vb, |x, y| x.wrapping_add(y)),
            "vsubuhm" => zip_h(&va, &vb, |x, y| x.wrapping_sub(y)),
            "vaddshs" => zip_h(&va, &vb, |x, y| sat_i16(x as i16 as i32 + y as i16 as i32)),
            "vsubshs" => zip_h(&va, &vb, |x, y| sat_i16(x as i16 as i32 - y as i16 as i32)),
            "vmaxsh" => zip_h(&va, &vb, |x, y| (x as i16).max(y as i16) as u16),
            "vminsh" => zip_h(&va, &vb, |x, y| (x as i16).min(y as i16) as u16),
            "vavgsh" => zip_h(&va, &vb, |x, y| ((x as i16 as i32 + y as i16 as i32 + 1) >> 1) as u16),
            "vavgsb" => std::array::from_fn(|i| ((va[i] as i8 as i32 + vb[i] as i8 as i32 + 1) >> 1) as u8),
            "vadduwm" => zip_w(&va, &vb, |x, y| x.wrapping_add(y)),
            "vadduws" => zip_w(&va, &vb, |x, y| x.saturating_add(y)),
            "vaddsws" => zip_w(&va, &vb, |x, y| (x as i32).saturating_add(y as i32) as u32),
            "vsubsws" => zip_w(&va, &vb, |x, y| (x as i32).saturating_sub(y as i32) as u32),
            "vmaxsw" => zip_w(&va, &vb, |x, y| (x as i32).max(y as i32) as u32),
            "vslb" => std::array::from_fn(|i| va[i] << (vb[i] & 7)),
            "vslh" => zip_h(&va, &vb, |x, y| x << (y & 15)),
            "vsrh" => zip_h(&va, &vb, |x, y| x >> (y & 15)),
            "vsrah" => zip_h(&va, &vb, |x, y| ((x as i16) >> (y & 15)) as u16),
            "vrlh" => zip_h(&va, &vb, |x, y| x.rotate_left((y & 15) as u32)),
            "vslw" => zip_w(&va, &vb, |x, y| x << (y & 31)),
            "vsrw" => zip_w(&va, &vb, |x, y| x >> (y & 31)),
            "vsraw" => zip_w(&va, &vb, |x, y| ((x as i32) >> (y & 31)) as u32),
            "vsl" | "vsr" => {
                let n = (vb[15] & 7) as u32;
                let x = u128::from_be_bytes(va);
                (if base == "vsl" { x << n } else { x >> n }).to_be_bytes()
            }
            "vslo" | "vsro" => {
                let n = ((vb[15] >> 3) & 15) as u32 * 8;
                let x = u128::from_be_bytes(va);
                (if base == "vslo" { x.checked_shl(n).unwrap_or(0) } else { x.checked_shr(n).unwrap_or(0) }).to_be_bytes()
            }
            "vsldoi" => {
                sldoi(&va, &vb, bits(wd, 22, 25) as usize)
            }
            "vperm" => {
                let ctl = if is128 { self.vr[r.c] } else { vc };
                let mut cat = [0u8; 32];
                cat[..16].copy_from_slice(&va);
                cat[16..].copy_from_slice(&vb);
                std::array::from_fn(|i| cat[(ctl[i] & 31) as usize])
            }
            "vpermwi" => {
                let perm = bits(wd, 11, 15) | bits(wd, 23, 25) << 5;
                let x = w(&vb);
                from_w(std::array::from_fn(|i| x[((perm >> (6 - 2 * i)) & 3) as usize]))
            }
            "vrlimi" => {
                let mask = bits(wd, 11, 15);
                let rot = bits(wd, 24, 25) as usize;
                let x = w(&vb);
                let rotated: [u32; 4] = std::array::from_fn(|i| x[(i + rot) & 3]);
                let d = w(&vd_old);
                from_w(std::array::from_fn(|i| if mask & (8 >> i) != 0 { rotated[i] } else { d[i] }))
            }
            "vmrghb" => std::array::from_fn(|i| if i % 2 == 0 { va[i / 2] } else { vb[i / 2] }),
            "vmrglb" => std::array::from_fn(|i| if i % 2 == 0 { va[8 + i / 2] } else { vb[8 + i / 2] }),
            "vmrghh" => {
                let (x, y) = (h(&va), h(&vb));
                from_h(std::array::from_fn(|i| if i % 2 == 0 { x[i / 2] } else { y[i / 2] }))
            }
            "vmrglh" => {
                let (x, y) = (h(&va), h(&vb));
                from_h(std::array::from_fn(|i| if i % 2 == 0 { x[4 + i / 2] } else { y[4 + i / 2] }))
            }
            "vmrghw" => {
                let (x, y) = (w(&va), w(&vb));
                from_w([x[0], y[0], x[1], y[1]])
            }
            "vmrglw" => {
                let (x, y) = (w(&va), w(&vb));
                from_w([x[2], y[2], x[3], y[3]])
            }
            "vspltb" => [vb[(uimm5 & 15) as usize]; 16],
            "vsplth" => from_h([h(&vb)[(uimm5 & 7) as usize]; 8]),
            "vspltw" => from_w([w(&vb)[(uimm5 & 3) as usize]; 4]),
            "vspltisb" => [simm5 as i8 as u8; 16],
            "vspltish" => from_h([simm5 as i16 as u16; 8]),
            "vspltisw" => from_w([simm5 as u32; 4]),
            "vupkhsb" => from_h(std::array::from_fn(|i| vb[i] as i8 as i16 as u16)),
            "vupklsb" => from_h(std::array::from_fn(|i| vb[8 + i] as i8 as i16 as u16)),
            "vupkhsh" => {
                let x = h(&vb);
                from_w(std::array::from_fn(|i| x[i] as i16 as i32 as u32))
            }
            "vupklsh" => {
                let x = h(&vb);
                from_w(std::array::from_fn(|i| x[4 + i] as i16 as i32 as u32))
            }
            "vpkshus" | "vpkshss" | "vpkuhus" => {
                let (x, y) = (h(&va), h(&vb));
                std::array::from_fn(|i| {
                    let v = if i < 8 { x[i] } else { y[i - 8] };
                    match base {
                        "vpkshus" => (v as i16).clamp(0, 255) as u8,
                        "vpkshss" => sat_i8(v as i16 as i32),
                        _ => v.min(255) as u8,
                    }
                })
            }
            "vpkswss" | "vpkswus" => {
                let (x, y) = (w(&va), w(&vb));
                from_h(std::array::from_fn(|i| {
                    let v = if i < 4 { x[i] } else { y[i - 4] } as i32;
                    if base == "vpkswss" { sat_i16(v) } else { v.clamp(0, 65535) as u16 }
                }))
            }
            "vupkd3d" => {
                // VX128_3: type = UIMM >> 2. Packed data sits in the last word(s) of VB.
                let x = w(&vb);
                let one = 1.0f32.to_bits();
                let three = 0x4040_0000u32; // 3.0: low mantissa bits carry 2^-22 steps
                let s16 = |v: u32| three.wrapping_add(v as u16 as i16 as i32 as u32);
                let out: [u32; 4] = match uimm5 >> 2 {
                    0 => {
                        // D3DCOLOR ARGB -> R,G,B,A as 1.0 + byte * 2^-23
                        let c = x[3].to_be_bytes();
                        [one | c[1] as u32, one | c[2] as u32, one | c[3] as u32, one | c[0] as u32]
                    }
                    1 => [s16(x[3] >> 16), s16(x[3]), 0, one],
                    2 => {
                        let v = x[3];
                        let s10 = |b: u32| three.wrapping_add((((b & 0x3FF) << 22) as i32 >> 22) as u32);
                        [s10(v), s10(v >> 10), s10(v >> 20), one.wrapping_add(v >> 30)]
                    }
                    3 => [f16(x[3] >> 16), f16(x[3]), 0, one],
                    4 => [s16(x[2] >> 16), s16(x[2]), s16(x[3] >> 16), s16(x[3])],
                    5 => [f16(x[2] >> 16), f16(x[2]), f16(x[3] >> 16), f16(x[3])],
                    _ => return Err(unimpl()),
                };
                from_w(out)
            }
            // ---- compares (Rc bit 21 for standard, bit 25 for 128) ---------
            c if c.starts_with("vcmp") => {
                let res: V = match c {
                    "vcmpgtub" => std::array::from_fn(|i| if va[i] > vb[i] { 0xFF } else { 0 }),
                    "vcmpequb" => std::array::from_fn(|i| if va[i] == vb[i] { 0xFF } else { 0 }),
                    "vcmpgtuh" => zip_h(&va, &vb, |x, y| if x > y { 0xFFFF } else { 0 }),
                    "vcmpgtsh" => zip_h(&va, &vb, |x, y| if (x as i16) > (y as i16) { 0xFFFF } else { 0 }),
                    "vcmpequh" => zip_h(&va, &vb, |x, y| if x == y { 0xFFFF } else { 0 }),
                    "vcmpgtsw" => zip_w(&va, &vb, |x, y| if (x as i32) > (y as i32) { !0 } else { 0 }),
                    "vcmpequw" => zip_w(&va, &vb, |x, y| if x == y { !0 } else { 0 }),
                    "vcmpgtfp" => cmp_f(&va, &vb, |x, y| x > y),
                    "vcmpgefp" => cmp_f(&va, &vb, |x, y| x >= y),
                    "vcmpeqfp" => cmp_f(&va, &vb, |x, y| x == y),
                    _ => return Err(unimpl()),
                };
                let rc = if is128 { bits(wd, 25, 25) } else { bits(wd, 21, 21) };
                if rc == 1 {
                    self.set_cr6(res.iter().all(|&b| b == 0xFF), res.iter().all(|&b| b == 0));
                }
                res
            }
            _ => return Err(unimpl()),
        };
        self.vr[r.d] = out;
        Ok(())
    }
}

/// IEEE half (low 16 bits of `h`) -> f32 bits.
fn f16(h: u32) -> u32 {
    let h = h & 0xFFFF;
    let sign = (h & 0x8000) << 16;
    let exp = (h >> 10) & 0x1F;
    let man = h & 0x3FF;
    match exp {
        0 if man == 0 => sign,
        0 => {
            // subnormal: normalise
            let mut e = -1i32;
            let mut m = man;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (((e + 127 - 14) as u32) << 23) | ((m & 0x3FF) << 13)
        }
        31 => sign | 0x7F80_0000 | (man << 13),
        _ => sign | ((exp + 127 - 15) << 23) | (man << 13),
    }
}

#[inline(always)]
fn zip_f(a: &V, b: &V, op: impl Fn(f32, f32) -> f32) -> V {
    let (x, y) = (f(a), f(b));
    from_f(std::array::from_fn(|i| op(x[i], y[i])))
}
fn zip_h(a: &V, b: &V, op: impl Fn(u16, u16) -> u16) -> V {
    let (x, y) = (h(a), h(b));
    from_h(std::array::from_fn(|i| op(x[i], y[i])))
}
fn zip_w(a: &V, b: &V, op: impl Fn(u32, u32) -> u32) -> V {
    let (x, y) = (w(a), w(b));
    from_w(std::array::from_fn(|i| op(x[i], y[i])))
}
fn cmp_f(a: &V, b: &V, op: impl Fn(f32, f32) -> bool) -> V {
    let (x, y) = (f(a), f(b));
    from_w(std::array::from_fn(|i| if op(x[i], y[i]) { !0 } else { 0 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vx128_fields() {
        // vor128 v127, v127, v127 style: all extension bits set
        let wd = 0x1400_02d0 | (31 << 21) | (31 << 16) | (31 << 11) | 0b1111 | (1 << 5) | (1 << 10);
        let r = vx128_regs(wd);
        assert_eq!((r.d, r.b), (127, 127));
        assert_eq!(r.a, 127);
    }
    #[test]
    fn half_floats() {
        assert_eq!(f32::from_bits(f16(0x3C00)), 1.0);
        assert_eq!(f32::from_bits(f16(0xC000)), -2.0);
        assert_eq!(f32::from_bits(f16(0x3555)), 0.333_251_95);
    }
    #[test]
    fn lanes_roundtrip() {
        let v: V = std::array::from_fn(|i| i as u8);
        assert_eq!(from_w(w(&v)), v);
        assert_eq!(from_h(h(&v)), v);
    }
}
