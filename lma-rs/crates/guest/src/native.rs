//! Value-level forms of the integer instructions the recompiler's IR emits natively (#145, M1):
//! each takes and returns plain values (registers in host locals), with exactly the interpreter's
//! semantics (`Cpu::exec`, `rec::*`). Flags come back as values too: `cr` is a 4-bit field (LT=8,
//! GT=4, EQ=2, SO=1) built from `so`, `ca` a bool. The recompiler checks every one of these against
//! the interpreter on random inputs (`recomp::ir::tests`).

const LT: u8 = 8;
const GT: u8 = 4;
const EQ: u8 = 2;

/// cr field for a signed compare, with the SO bit from `so`.
#[inline(always)]
pub fn cr_signed(a: i64, b: i64, so: bool) -> u8 {
    (if a < b { LT } else if a > b { GT } else { EQ }) | so as u8
}
/// cr field for an unsigned compare, with the SO bit from `so`.
#[inline(always)]
pub fn cr_unsigned(a: u64, b: u64, so: bool) -> u8 {
    (if a < b { LT } else if a > b { GT } else { EQ }) | so as u8
}
/// cr0 of a record form: the low 32 bits of the result compared with 0, signed.
#[inline(always)]
pub fn cr0(v: u64, so: bool) -> u8 {
    cr_signed(v as u32 as i32 as i64, 0, so)
}
/// cmpi / cmp with the L bit: 64-bit or sign-extended 32-bit operands.
#[inline(always)]
pub fn cmp_signed(l: bool, a: u64, b: u64, so: bool) -> u8 {
    if l { cr_signed(a as i64, b as i64, so) } else { cr_signed(a as u32 as i32 as i64, b as u32 as i32 as i64, so) }
}
#[inline(always)]
pub fn cmp_unsigned(l: bool, a: u64, b: u64, so: bool) -> u8 {
    if l { cr_unsigned(a, b, so) } else { cr_unsigned(a as u32 as u64, b as u32 as u64, so) }
}

#[inline(always)]
pub fn rlwinm(s: u64, sh: u32, mask: u32) -> u64 {
    ((s as u32).rotate_left(sh) & mask) as u64
}
#[inline(always)]
pub fn rlwimi(s: u64, a: u64, sh: u32, mask: u32) -> u64 {
    (((s as u32).rotate_left(sh) & mask) | (a as u32 & !mask)) as u64
}
#[inline(always)]
pub fn rlwnm(s: u64, b: u64, mask: u32) -> u64 {
    ((s as u32).rotate_left(b as u32 & 31) & mask) as u64
}
#[inline(always)]
pub fn mullw(a: u64, b: u64) -> u64 {
    (a as u32 as i32 as i64).wrapping_mul(b as u32 as i32 as i64) as u64
}
#[inline(always)]
pub fn mulli(a: u64, i: i64) -> u64 {
    (a as i64).wrapping_mul(i) as u64
}
#[inline(always)]
pub fn neg(a: u64) -> u64 {
    (a as i64).wrapping_neg() as u64
}
#[inline(always)]
pub fn slw(x: u64, y: u64) -> u64 {
    let n = y as u32 & 63;
    if n > 31 { 0 } else { ((x as u32) << n) as u64 }
}
#[inline(always)]
pub fn srw(x: u64, y: u64) -> u64 {
    let n = y as u32 & 63;
    if n > 31 { 0 } else { ((x as u32) >> n) as u64 }
}
#[inline(always)]
pub fn sld(x: u64, y: u64) -> u64 {
    let n = y as u32 & 127;
    if n > 63 { 0 } else { x << n }
}
#[inline(always)]
pub fn srd(x: u64, y: u64) -> u64 {
    let n = y as u32 & 127;
    if n > 63 { 0 } else { x >> n }
}
#[inline(always)]
pub fn extsh(x: u64) -> u64 {
    x as u16 as i16 as i64 as u64
}
#[inline(always)]
pub fn extsb(x: u64) -> u64 {
    x as u8 as i8 as i64 as u64
}
#[inline(always)]
pub fn extsw(x: u64) -> u64 {
    x as u32 as i32 as i64 as u64
}
#[inline(always)]
pub fn cntlzw(x: u64) -> u64 {
    (x as u32).leading_zeros() as u64
}
#[inline(always)]
pub fn cntlzd(x: u64) -> u64 {
    x.leading_zeros() as u64
}
/// sraw (n from rb) / srawi (n immediate): (result, ca).
#[inline(always)]
pub fn sraw(s: u64, n: u32) -> (u64, bool) {
    let n = n & 63;
    let v = s as u32 as i32;
    let (r, lost) = if n > 31 {
        ((v >> 31) as i64, v < 0)
    } else {
        let lost = n > 0 && (v as u32) << (32 - n) != 0;
        ((v >> n) as i64, lost)
    };
    (r as u64, v < 0 && lost)
}
/// srad / sradi: (result, ca).
#[inline(always)]
pub fn srad(s: u64, n: u32) -> (u64, bool) {
    let n = n & 127;
    let v = s as i64;
    let (r, lost) = if n > 63 { (v >> 63, v < 0) } else { let lost = n > 0 && (v as u64) << (64 - n) != 0; (v >> n, lost) };
    (r as u64, v < 0 && lost)
}
/// subfic: (result, ca).
#[inline(always)]
pub fn subfic(a: u64, simm: u64) -> (u64, bool) {
    (simm.wrapping_sub(a), (simm as u32) >= a as u32)
}
/// addic: (result, ca).
#[inline(always)]
pub fn addic(a: u64, simm: u64) -> (u64, bool) {
    (a.wrapping_add(simm), (a as u32).checked_add(simm as u32).is_none())
}
/// Effective address of a D-form access (`ra` already resolved to 0 for the zero form).
#[inline(always)]
pub fn ea(base: u64, off: i32) -> u32 {
    (base as u32).wrapping_add(off as u32)
}
#[inline(always)]
pub fn ea_x(base: u64, index: u64) -> u32 {
    (base as u32).wrapping_add(index as u32)
}
#[inline(always)]
pub fn lha(v: u16) -> u64 {
    v as i16 as i64 as u64
}
#[inline(always)]
pub fn lwa(v: u32) -> u64 {
    v as i32 as i64 as u64
}
