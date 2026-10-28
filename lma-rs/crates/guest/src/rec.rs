//! Tiny always-inlined helpers the recompiled code calls (`lma-recomp` output). Register numbers
//! and immediates are literals at every call site, so after inlining each helper folds into the
//! same machine code a hand-written translation would produce, while the generated source stays
//! small enough to compile the whole game.

#![allow(clippy::too_many_arguments)]

use crate::cpu::{Cpu, Stop};
use crate::mem::Memory;

/// A recompiled guest function: enter at `pc`, run until control leaves the function or the
/// instruction `budget` runs out, return the next guest pc.
pub type FuncFn = fn(&mut Cpu, &mut Memory, u32, &mut i64) -> Result<u32, Stop>;

#[inline(always)]
fn ra0(c: &Cpu, a: usize) -> u64 {
    if a == 0 { 0 } else { c.gpr[a] }
}
#[inline(always)]
fn ea(c: &Cpu, a: usize, off: i32) -> u32 {
    (ra0(c, a) as u32).wrapping_add(off as u32)
}
#[inline(always)]
fn eax(c: &Cpu, a: usize, b: usize) -> u32 {
    (ra0(c, a) as u32).wrapping_add(c.gpr[b] as u32)
}
#[inline(always)]
fn cr0(c: &mut Cpu, rc: bool, v: u64) {
    if rc {
        c.set_cr_signed(0, v as u32 as i32 as i64, 0);
    }
}

// ---- immediate arithmetic / logic --------------------------------------------------------
#[inline(always)]
pub fn addi(c: &mut Cpu, d: usize, a: usize, i: i64) {
    c.gpr[d] = ra0(c, a).wrapping_add(i as u64);
}
#[inline(always)]
pub fn ori(c: &mut Cpu, a: usize, s: usize, i: u64) {
    c.gpr[a] = c.gpr[s] | i;
}
#[inline(always)]
pub fn xori(c: &mut Cpu, a: usize, s: usize, i: u64) {
    c.gpr[a] = c.gpr[s] ^ i;
}
#[inline(always)]
pub fn andi(c: &mut Cpu, a: usize, s: usize, i: u64) {
    c.gpr[a] = c.gpr[s] & i;
    cr0(c, true, c.gpr[a]);
}
#[inline(always)]
pub fn mulli(c: &mut Cpu, d: usize, a: usize, i: i64) {
    c.gpr[d] = (c.gpr[a] as i64).wrapping_mul(i) as u64;
}
#[inline(always)]
pub fn cmpi(c: &mut Cpu, f: usize, l: bool, a: usize, i: i64) {
    let v = if l { c.gpr[a] as i64 } else { c.gpr[a] as u32 as i32 as i64 };
    c.set_cr_signed(f, v, i);
}
#[inline(always)]
pub fn cmpli(c: &mut Cpu, f: usize, l: bool, a: usize, i: u64) {
    let v = if l { c.gpr[a] } else { c.gpr[a] as u32 as u64 };
    c.set_cr_unsigned(f, v, i);
}
#[inline(always)]
pub fn rlwinm(c: &mut Cpu, a: usize, s: usize, sh: u32, mask: u32, rc: bool) {
    let v = (c.gpr[s] as u32).rotate_left(sh) & mask;
    c.gpr[a] = v as u64;
    cr0(c, rc, v as u64);
}
#[inline(always)]
pub fn rlwimi(c: &mut Cpu, a: usize, s: usize, sh: u32, mask: u32, rc: bool) {
    let v = ((c.gpr[s] as u32).rotate_left(sh) & mask) | (c.gpr[a] as u32 & !mask);
    c.gpr[a] = v as u64;
    cr0(c, rc, v as u64);
}

// ---- register arithmetic / logic ---------------------------------------------------------
macro_rules! rrr {
    ($($name:ident: |$c:ident, $x:ident, $y:ident| $e:expr;)*) => {$(
        #[inline(always)]
        pub fn $name($c: &mut Cpu, d: usize, a: usize, b: usize, rc: bool) {
            let ($x, $y) = ($c.gpr[a], $c.gpr[b]);
            let v = $e;
            $c.gpr[d] = v;
            cr0($c, rc, v);
        }
    )*};
}
rrr! {
    add: |c, x, y| x.wrapping_add(y);
    subf: |c, x, y| y.wrapping_sub(x);
    mullw: |c, x, y| (x as u32 as i32 as i64).wrapping_mul(y as u32 as i32 as i64) as u64;
    or: |c, x, y| x | y;
    and: |c, x, y| x & y;
    andc: |c, x, y| x & !y;
    xor: |c, x, y| x ^ y;
    nor: |c, x, y| !(x | y);
    slw: |c, x, y| { let n = y as u32 & 63; if n > 31 { 0 } else { ((x as u32) << n) as u64 } };
    srw: |c, x, y| { let n = y as u32 & 63; if n > 31 { 0 } else { ((x as u32) >> n) as u64 } };
}
macro_rules! rr {
    ($($name:ident: |$x:ident| $e:expr;)*) => {$(
        #[inline(always)]
        pub fn $name(c: &mut Cpu, d: usize, a: usize, rc: bool) {
            let $x = c.gpr[a];
            let v = $e;
            c.gpr[d] = v;
            cr0(c, rc, v);
        }
    )*};
}
rr! {
    neg: |x| (x as i64).wrapping_neg() as u64;
    extsh: |x| x as u16 as i16 as i64 as u64;
    extsb: |x| x as u8 as i8 as i64 as u64;
    extsw: |x| x as u32 as i32 as i64 as u64;
    cntlzw: |x| (x as u32).leading_zeros() as u64;
}
#[inline(always)]
pub fn cmp(c: &mut Cpu, f: usize, l: bool, a: usize, b: usize) {
    if l {
        c.set_cr_signed(f, c.gpr[a] as i64, c.gpr[b] as i64)
    } else {
        c.set_cr_signed(f, c.gpr[a] as u32 as i32 as i64, c.gpr[b] as u32 as i32 as i64)
    }
}
#[inline(always)]
pub fn cmpl(c: &mut Cpu, f: usize, l: bool, a: usize, b: usize) {
    if l {
        c.set_cr_unsigned(f, c.gpr[a], c.gpr[b])
    } else {
        c.set_cr_unsigned(f, c.gpr[a] as u32 as u64, c.gpr[b] as u32 as u64)
    }
}

// ---- loads / stores ------------------------------------------------------------------------
macro_rules! load {
    ($($name:ident, $namex:ident: |$m:ident, $e:ident| $v:expr;)*) => {$(
        #[inline(always)]
        pub fn $name(c: &mut Cpu, $m: &mut Memory, d: usize, a: usize, off: i32) {
            let $e = ea(c, a, off);
            c.gpr[d] = $v;
        }
        #[inline(always)]
        pub fn $namex(c: &mut Cpu, $m: &mut Memory, d: usize, a: usize, b: usize) {
            let $e = eax(c, a, b);
            c.gpr[d] = $v;
        }
    )*};
}
load! {
    lwz, lwzx: |m, e| m.read32(e) as u64;
    lbz, lbzx: |m, e| m.read8(e) as u64;
    lhz, lhzx: |m, e| m.read16(e) as u64;
    lha, lhax: |m, e| m.read16(e) as i16 as i64 as u64;
    ld, ldx: |m, e| m.read64(e);
    lwa, lwax: |m, e| m.read32(e) as i32 as i64 as u64;
}
macro_rules! store {
    ($($name:ident, $namex:ident: |$m:ident, $e:ident, $s:ident| $st:expr;)*) => {$(
        #[inline(always)]
        pub fn $name(c: &mut Cpu, $m: &mut Memory, s: usize, a: usize, off: i32) {
            let ($e, $s) = (ea(c, a, off), c.gpr[s]);
            $st
        }
        #[inline(always)]
        pub fn $namex(c: &mut Cpu, $m: &mut Memory, s: usize, a: usize, b: usize) {
            let ($e, $s) = (eax(c, a, b), c.gpr[s]);
            $st
        }
    )*};
}
store! {
    stw, stwx: |m, e, s| m.write32(e, s as u32);
    stb, stbx: |m, e, s| m.write8(e, s as u8);
    sth, sthx: |m, e, s| m.write16(e, s as u16);
    std_, stdx: |m, e, s| m.write64(e, s);
}
/// Update forms (lwzu, stwu, ...): access at a+off, then a = ea.
#[inline(always)]
pub fn lwzu(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    c.gpr[d] = m.read32(e) as u64;
    c.gpr[a] = e as u64;
}
#[inline(always)]
pub fn ldu(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    c.gpr[d] = m.read64(e);
    c.gpr[a] = e as u64;
}
#[inline(always)]
pub fn stwu(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    m.write32(e, c.gpr[s] as u32);
    c.gpr[a] = e as u64;
}
#[inline(always)]
pub fn stdu(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    m.write64(e, c.gpr[s]);
    c.gpr[a] = e as u64;
}
#[inline(always)]
pub fn lfs(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, off: i32) {
    c.fpr[d] = f32::from_bits(m.read32(ea(c, a, off))) as f64;
}
#[inline(always)]
pub fn lfd(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, off: i32) {
    c.fpr[d] = f64::from_bits(m.read64(ea(c, a, off)));
}
#[inline(always)]
pub fn stfs(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, off: i32) {
    m.write32(ea(c, a, off), (c.fpr[s] as f32).to_bits());
}
#[inline(always)]
pub fn stfd(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, off: i32) {
    m.write64(ea(c, a, off), c.fpr[s].to_bits());
}

// ---- the game's register save / restore helpers (__savegprlr / __restgprlr) ------------------
// Entered at r`first` (14..31): `std rK,-152+8*(K-14)(r1)` for K = first..31, `stw r12,-8(r1)`,
// `blr`; the restore helper loads them back, then `lwz r12,-8(r1)`, `mtlr r12`, `blr`. The
// recompiled code calls these natively only when the budget covers the whole helper, so the
// bytes, registers and instruction count are exactly the guest's, with one address translation
// instead of one per register (#6: 8.4% of a match's guest instructions).

/// `__savegprlr` from r`first`: the same stores as the guest code.
#[inline(always)]
pub fn save_gprlr(c: &mut Cpu, m: &mut Memory, first: usize) {
    let sp = c.gpr[1] as u32;
    // the whole save area (r14's slot at sp-152 up to the LR word at sp-8) in the fast path: the
    // stores directly (unrolled: `first` is a literal at every call site), not a buffer and a
    // bulk copy, which ended in a memmove call per function entry (#6)
    if let Some(area) = m.fast::<148>(sp.wrapping_sub(152)) {
        for k in first..32 {
            area[8 * (k - 14)..8 * (k - 13)].copy_from_slice(&c.gpr[k].to_be_bytes());
        }
        area[144..148].copy_from_slice(&(c.gpr[12] as u32).to_be_bytes());
        return;
    }
    let mut buf = [0u8; 8 * 18];
    let n = 32 - first;
    for (i, k) in (first..32).enumerate() {
        buf[8 * i..8 * i + 8].copy_from_slice(&c.gpr[k].to_be_bytes());
    }
    m.write(sp.wrapping_sub(152).wrapping_add(8 * (first as u32 - 14)), &buf[..8 * n]);
    m.write32(sp.wrapping_sub(8), c.gpr[12] as u32);
}
/// `__restgprlr` from r`first`: the same loads, then LR from the saved word (`mtlr r12`).
#[inline(always)]
pub fn rest_gprlr(c: &mut Cpu, m: &mut Memory, first: usize) {
    let sp = c.gpr[1] as u32;
    // as `save_gprlr`: the loads directly when the whole area is in the fast path
    if let Some(area) = m.fast::<148>(sp.wrapping_sub(152)) {
        for k in first..32 {
            c.gpr[k] = u64::from_be_bytes(area[8 * (k - 14)..8 * (k - 13)].try_into().unwrap());
        }
        c.gpr[12] = u32::from_be_bytes(area[144..148].try_into().unwrap()) as u64;
        c.lr = c.gpr[12];
        return;
    }
    let mut buf = [0u8; 8 * 18];
    let n = 32 - first;
    m.read(sp.wrapping_sub(152).wrapping_add(8 * (first as u32 - 14)), &mut buf[..8 * n]);
    for (i, k) in (first..32).enumerate() {
        c.gpr[k] = u64::from_be_bytes(buf[8 * i..8 * i + 8].try_into().unwrap());
    }
    c.gpr[12] = m.read32(sp.wrapping_sub(8)) as u64;
    c.lr = c.gpr[12];
}

// ---- special registers / fallback ----------------------------------------------------------
#[inline(always)]
pub fn mflr(c: &mut Cpu, d: usize) {
    c.gpr[d] = c.lr;
}
#[inline(always)]
pub fn mfctr(c: &mut Cpu, d: usize) {
    c.gpr[d] = c.ctr;
}
#[inline(always)]
pub fn mtlr(c: &mut Cpu, s: usize) {
    c.lr = c.gpr[s];
}
#[inline(always)]
pub fn mtctr(c: &mut Cpu, s: usize) {
    c.ctr = c.gpr[s];
}
/// LMA_INTERP_STATS: how often each instruction word goes through `interp` (which ones to
/// translate natively next). Off by default: one relaxed load per call.
/// The year the game's code counts from (#31 mod spaces): the recompiler emits `base_year()`
/// where the game loads or compares the immediate 2006, so a mod can start in another season.
pub static BASE_YEAR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(2006);
/// Sites whose immediate 2006 is not a year and must stay: 0x82221090 loads the resource id 2006
/// (group 2, item 6: `0x822f12f8` splits it by 1000) for the match's pitch; moved to 2026 it
/// fetched another resource and the pitch rendered without its texture (sky through the grass).
/// 0x820d79d4 is one arm of a switch whose eight arms load 2004..2012 for 0x823fa358 (an object
/// by handle, then 0x823f9d48 with the value): ids in scrambled order, not years.
/// 0x8257efec loads 2026 as an error code of the D3DX shader assembler ("Matrices cannot be
/// specified in temp registers...", its neighbours 2025 and 2027 the codes beside it; #706).
pub const NOT_YEAR_SITES: &[u32] = &[0x8222_1090, 0x820d_79d4, 0x8257_efec];
#[inline(always)]
pub fn base_year() -> i64 {
    BASE_YEAR.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// The unit of a player's stored value (#31, the maintainer 11 Oct: Transfermarkt-level values): the u16 at
/// record+0x14 counts GBP 1,000s in the vanilla game (cap GBP 65.5m). Exactly two sites turn it
/// into GBP, `mulli rD,r3,1000` right after a call to the getter `f8211dc58` (the new-game
/// valuation loop in `f8220df78` and the later revaluation loop), and the setter `f8211dcb0`
/// zeroes values of 1000 or less (`cmpwi` at 0x8211dcd8). With `value_unit = 5000` in mod.toml the
/// recompiler emits `value_unit()` at the two multiplies and `value_floor()` (1000 x 1000 / unit)
/// at the compare; the interpreter sees the same words rewritten in memory (`runtime::mods`).
pub static VALUE_UNIT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1000);
pub const VALUE_UNIT_SITES: &[u32] = &[0x8220_e200, 0x8215_da98];
pub const VALUE_FLOOR_SITE: u32 = 0x8211_dcd8;
#[inline(always)]
pub fn value_unit() -> i64 {
    VALUE_UNIT.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// The setter's "too small to store" bound in units: 1000 units in the vanilla game (GBP 1m).
#[inline(always)]
pub fn value_floor() -> i64 {
    1_000_000 / value_unit()
}
/// A cup draw that can't fill (#735). 0x8213c8c8 draws a 28-club cup (the Portuguese League Cup):
/// it seeds a set with the top division's clubs that aren't exempt, then loops `while count < 28`
/// adding random clubs of the second division (16 in its block), with no other exit. When the seed
/// leaves out more clubs than the game expects (fc27final27: five, so 11 + 16 = 27), no pick is new
/// and the career spins on the LOADING screen forever. The loop's two compares with 28 (after the
/// seed, and at the back edge) read these: 28 as before, but after `DRAW_FILL_TRIES` picks without
/// reaching it, the count itself, so the loop ends with the clubs it has and the draw gets a bye (as
/// the vanilla English League Cup already does with 35 of 36). A filling draw takes ~50 picks, so
/// nothing changes until it can't fill.
pub const DRAW_FILL_FIRST: u32 = 0x8213_c958;
pub const DRAW_FILL_LOOP: u32 = 0x8213_c9e8;
pub const DRAW_FILL_TRIES: u32 = 4096;
static DRAW_TRIES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
/// The compare after the seed: a new draw, so the tries start over.
#[inline(always)]
pub fn draw_fill_first() -> i64 {
    DRAW_TRIES.store(0, std::sync::atomic::Ordering::Relaxed);
    28
}
/// The compare at the loop's back edge, with the set's count: 28 until `DRAW_FILL_TRIES` picks.
#[inline(always)]
pub fn draw_fill_target(count: i64) -> i64 {
    if DRAW_TRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= DRAW_FILL_TRIES {
        count
    } else {
        28
    }
}
/// The game's own valuation (`f821208e0`) caps a player at GBP 40m: `lis r11,0x262` at
/// 0x82120b7c, `ori r11,r11,0x5a00` here (PC check, 11 Oct: every star at exactly GBP 40,000,000
/// with value_unit 5000). The cap scales with the unit (GBP 200m at 5000); the recompiler loads the
/// whole cap at the `ori`, the interpreter sees both words rewritten (`runtime::mods`).
pub const VALUE_CAP_SITE: u32 = 0x8212_0b80;
pub const VALUE_CAP_LIS: u32 = 0x8212_0b7c;
#[inline(always)]
pub fn value_cap() -> i64 {
    40_000_000 * value_unit() / 1000
}
/// Transfer offers are capped at GBP 80m (2006's world record scale): `lis rX,0x4c4` + `ori rY,rX,0xb400`
/// at seven sites of the transfer code (the offer screen's max = min(2 x value, cap) at 0x821f39a8,
/// its validation, counter-offers, the AI's bids and `FUN_820e5a50`). With value_unit 5000 real
/// values pass 80m (the maintainer, 11 Oct: every offer stopped at 80m with 306m funds), so the cap scales
/// with the unit like the valuation's: the recompiler loads the whole cap at each `ori`, the
/// interpreter sees both words rewritten (`runtime::mods`). `(lis, ori)` addresses.
pub const OFFER_CAP_SITES: [(u32, u32); 7] = [
    (0x820e_5a58, 0x820e_5a5c),
    (0x821e_c284, 0x821e_c28c),
    (0x821f_3158, 0x821f_3160),
    (0x821f_39a8, 0x821f_39ac),
    (0x821f_3db8, 0x821f_3dbc),
    (0x821f_4770, 0x821f_4778),
    (0x821f_48d8, 0x821f_48e0),
];
#[inline(always)]
pub fn offer_cap() -> i64 {
    // the game compares it with cmpw: keep it a positive i32 (value_unit above 26,843 would wrap)
    (80_000_000 * value_unit() / 1000).min(0x7FFF_FFFF)
}
/// A contract offer's weekly wage is capped at GBP 250,000 (2006): `lis rX,3` + `ori rY,rX,0xd090` at
/// twelve sites of the contract code (the offer helper 0x821f6f00, the wage rounding 0x82118e88 with
/// its 250k ceiling, the offer screens 0x821f7260 / 0x821f7358 / 0x821f7450, the contract screens
/// 0x82187738 / 0x82187cb8 and the demand clamps around 0x821d5968). Real wages pass it (the maintainer, 11 Oct:
/// Yamal on 280k, offers stop at 250k), so mod.toml's `wage_cap` sets it: the recompiler loads it at
/// each `ori`, the interpreter sees both words rewritten (`runtime::mods`). `(lis, ori)` addresses.
/// (0x82118dcc's 250,000 is a rounding step for sums above 10m, not this cap: left alone.)
/// An AI club's signing is completed only if the agreed weekly wage is at most GBP 100,000 (2006):
/// the completion check 0x821889c0 answers 6 ('the wage') above it, for AI buyers only (a manager's
/// club checks its wage budget, 0x8210b900), and the deal is dropped after the player agreed terms
/// (the maintainer, 22 Oct, #704: >100k players accept, then back out). `lis r3,1` + `ori r3,r3,0x86a0`;
/// mod.toml's `ai_signing_wage_cap` sets it (the recompiler loads it at the `ori`). `(lis, ori)`.
pub static AI_SIGNING_WAGE_CAP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100_000);
pub const AI_SIGNING_WAGE_CAP_SITE: (u32, u32) = (0x8218_8d20, 0x8218_8d24);
#[inline(always)]
pub fn ai_signing_wage_cap() -> i64 {
    AI_SIGNING_WAGE_CAP.load(std::sync::atomic::Ordering::Relaxed) as i64
}
pub static WAGE_OFFER_CAP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(250_000);
pub const WAGE_OFFER_CAP_SITES: [(u32, u32); 12] = [
    (0x8211_8ef8, 0x8211_8f00),
    (0x8218_774c, 0x8218_7750),
    (0x8218_7cfc, 0x8218_7d04),
    (0x8218_7e48, 0x8218_7e50),
    (0x821d_5ae0, 0x821d_5ae8),
    (0x821d_5b68, 0x821d_5b70),
    (0x821d_5c1c, 0x821d_5c24),
    (0x821d_5c74, 0x821d_5c7c),
    (0x821f_6f90, 0x821f_6f98),
    (0x821f_72a8, 0x821f_72ac),
    (0x821f_73a0, 0x821f_73a4),
    (0x821f_77f0, 0x821f_77f4),
];
#[inline(always)]
pub fn wage_offer_cap() -> i64 {
    WAGE_OFFER_CAP.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// The clubs' weekly wage budget is recalculated (0x8210b8xx) and capped at GBP 3.2m: `lis r11,0x30`
/// at 0x8210b89c, `ori r30,r11,0xd400` here. Real wage bills pass it (PC, 11 Oct: Arsenal GBP 3.3m,
/// Liverpool 3.55m a week), so mod.toml's `wage_budget_cap` sets it; the recompiler loads it at the
/// `ori`, the interpreter sees both words rewritten (`runtime::mods`).
pub static WAGE_BUDGET_CAP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(3_200_000);
pub const WAGE_CAP_SITE: u32 = 0x8210_b8a4;
pub const WAGE_CAP_LIS: u32 = 0x8210_b89c;
#[inline(always)]
pub fn wage_budget_cap() -> i64 {
    WAGE_BUDGET_CAP.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// A competitive match allows 3 substitutions: match setup stores r16 into the match's limit word
/// (+20572, which the sub screen's `0x82190c88` checks), and `addi r16,0,3` here sets it (other
/// match types already get 5). mod.toml's `subs` sets it (#530); the recompiler loads it here, the
/// interpreter sees the word rewritten (`runtime::mods`).
pub static SUBS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(3);
pub const SUBS_SITE: u32 = 0x8221_3fd4;
#[inline(always)]
pub fn subs_limit() -> i64 {
    SUBS.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// Formations (win3, 21 Oct): the game's 14 shapes are two fixed tables with live data after
/// both: the slot roles at 0x82710f58 (14 x 11 u32, stride 44; 0 GK 1 FB 2 SW 3 CB 4 WB 5 CM
/// 6 WM 7 AM 8 CF 9 WF) and the names at 0x827023f0 (14 C-string pointers, with a second copy of
/// the pointers at 0x827135bc behind the name getter 0x82326280 the tactics screens call); the
/// count is a literal 14 at nine code sites (`FORMATION_COUNT_LI_SITES`, `FORMATION_COUNT_CMP_SITES`)
/// and one caption indexes a null-terminated list of the names' string ids (`FORMATION_IDS_VANILLA`).
/// A team's choice is a 4-bit field (the top nibble of word +0x24 of its tactics record, club
/// object + 0x1da0 + slot x 1472, written by the setter 0x8210de88's key 0 and saved as such), so
/// 16 shapes is the most the game can hold. A mod's formations.csv (`runtime::mods`) copies the
/// tables and the id list into the section gap `FORMATION_REGION` with its rows added (ids 14,
/// 15) or changed, and the code reads them there: the recompiler emits the halves of these
/// addresses at the lis/addi pairs (`formation_roles_hi/lo`, `formation_names_hi/lo`,
/// `formation_ids_hi/lo`, `formation_spots_hi/lo`) and `formation_count()` at the count sites; the interpreter sees the
/// words rewritten. `FORMATION_COUNT` is the rows in the copied tables.
pub static FORMATION_ROLES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_ROLES_VANILLA);
pub static FORMATION_NAMES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_NAMES_VANILLA);
pub static FORMATION_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_COUNT_VANILLA);
pub static FORMATION_IDS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_IDS_VANILLA);
pub static FORMATION_SPOTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_SPOTS_VANILLA);
pub static FORMATION_ORDER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(FORMATION_ORDER_VANILLA);
pub const FORMATION_ROLES_VANILLA: u32 = 0x8271_0f58;
pub const FORMATION_NAMES_VANILLA: u32 = 0x8270_23f0;
pub const FORMATION_COUNT_VANILLA: u32 = 14;
/// The caption's list of the 14 names' string ids (3322-3335, then 0), indexed by the record's
/// nibble at 0x822dc7b4; the names the tactics screens show come from the pointer tables instead.
pub const FORMATION_IDS_VANILLA: u32 = 0x8202_7dcc;
/// The Team Tactics pitch diagram's table (14 x 11 u32): each slot's spot on the pitch grid, an
/// index into the 34 (x, y) pairs at 0x82705160 (x 16.7-83.3 across, y 90 GK, 74 sweeper, 64
/// defence, 51, 39 midfield, 26, 14 attack; `runtime::mods::FORMATION_SPOT_ROWS`). The diagram
/// (0x82191b40) is its only reader; the match engine places players by role.
pub const FORMATION_SPOTS_VANILLA: u32 = 0x8200_b370;
/// The team-sheet order (14 x 11 i32): for each formation, the slot at each team-sheet place
/// (GK, the full backs, ...). 0x82326220 (formation, slot) searches its row for the slot and returns
/// the place, -1 when absent; the pitch diagram and the match engine (0x82223bf0, 0x82245090,
/// 0x8224c3d0, 0x82250708, ...) call it. No bound on the formation: past 14 it read other data
/// and returned -1 (the empty diagram of #749).
pub const FORMATION_ORDER_VANILLA: u32 = 0x8202_cd38;
/// Spots in the pitch grid.
pub const FORMATION_SPOT_COUNT: u32 = 34;
/// Slots per formation (the eleven on the pitch) and the table's row stride in bytes.
pub const FORMATION_SLOTS: usize = 11;
/// The most formations the 4-bit field can name.
pub const FORMATION_MAX: usize = 16;
/// Free guest memory for the copied tables: the padding between the last `.embsec_` section
/// (ends 0x826ff3b8) and `.data`'s first object (0x826fffa4). Nothing in the image addresses it
/// and it is still zero in a late-career save state.
pub const FORMATION_REGION: (u32, u32) = (0x826f_f400, 0x826f_ffa4);
/// (lis, addi) pairs that build the role table's address (`lis rA,0x8271; addi rD,rA,3928`), the
/// register serving nothing else between them: 7 in the squad, tactics and AI code, the rest in
/// the match engine. The one pair whose lis also serves another table is `FORMATION_ROLES_SHARED`.
pub const FORMATION_ROLES_SITES: &[(u32, u32)] = &[
    (0x8210_fb60, 0x8210_fb70), (0x8216_a410, 0x8216_a420), (0x8219_1614, 0x8219_161c), (0x8219_6a70, 0x8219_6a7c),
    (0x8220_c6c8, 0x8220_c6d0), (0x8225_c0e4, 0x8225_c0ec), (0x8226_01e8, 0x8226_01f4), (0x822a_2294, 0x822a_22b8),
    (0x822a_2890, 0x822a_2898), (0x822a_292c, 0x822a_2934), (0x822a_29dc, 0x822a_29e4), (0x822a_2a64, 0x822a_2a6c),
    (0x822a_2c68, 0x822a_2c74), (0x822a_5e54, 0x822a_5e5c), (0x822a_6ea4, 0x822a_6eac), (0x822a_ba0c, 0x822a_ba18),
    (0x822a_ba8c, 0x822a_ba98), (0x822a_bb0c, 0x822a_bb18), (0x822a_bb8c, 0x822a_bb98), (0x822a_bd58, 0x822a_bd60),
    (0x822a_bde0, 0x822a_bde8), (0x822a_be98, 0x822a_bea0), (0x822a_c0dc, 0x822a_c0e8), (0x822a_c484, 0x822a_c490),
    (0x822a_c674, 0x822a_c680), (0x822a_c864, 0x822a_c870), (0x822a_ca48, 0x822a_ca50), (0x822a_cae8, 0x822a_caf0),
    (0x822a_cba0, 0x822a_cba8), (0x822a_cc4c, 0x822a_cc58), (0x822a_d444, 0x822a_d44c), (0x822a_e15c, 0x822a_e164),
    (0x822a_ea10, 0x822a_ea14), (0x822a_f558, 0x822a_f55c), (0x822b_0180, 0x822b_0184),
];
/// 0x822adfb8's `lis r11,0x8271` also serves the 0x827100e4 table (r28, r11+228), so its
/// `addi r29,r11,3928` (.1) takes the absolute address in the recompiled code; for the
/// interpreter the dead `addi r28,r11,-228` before the lis (.0; r28 is written again at
/// 0x822adfc8 before any read) becomes `lis r29,hi` and .1 `addi r29,r29,lo`.
pub const FORMATION_ROLES_SHARED: (u32, u32) = (0x822a_dfb4, 0x822a_dfc0);
/// The one (lis, addi) pair reading the name table (`lis r11,0x8270; addi r11,r11,9200`: the
/// tactics screen's formation row).
pub const FORMATION_NAMES_SITE: (u32, u32) = (0x820d_2e84, 0x820d_2e8c);
/// The second name table's pair (`lis r11,0x8271; addi r11,r11,13756` in the getter 0x82326280:
/// the Team Tactics screen's Formation row and the match-day screens show its string).
pub const FORMATION_NAMES2_SITE: (u32, u32) = (0x8232_6290, 0x8232_6298);
/// The id list's pair (`lis r11,0x8202; addi r11,r11,32204` before the caption's `lwzx` at 0x822dc7b4).
pub const FORMATION_IDS_SITE: (u32, u32) = (0x822d_c7a8, 0x822d_c7b0);
/// The spot table's pair (`lis r11,0x8201; addi r11,r11,-19600` in the diagram's slot placement 0x82191b40).
pub const FORMATION_SPOTS_SITE: (u32, u32) = (0x8219_1ba8, 0x8219_1bb4);
/// The team-sheet order's two pairs (`lis r11,0x8203; addi r11,r11,-13000`: 0x82326200, 0x82326220).
pub const FORMATION_ORDER_SITES: [(u32, u32); 2] = [(0x8232_6200, 0x8232_6208), (0x8232_6240, 0x8232_6248)];
/// `li rD,14` sites that are the formation count: the count getter 0x823262a8 (a vtable entry,
/// 0x823243b8, the AI reaches), the Team Tactics screen's option count for its Formation row
/// (0x821d42e8, widget +0x144 of the screen object 0x82752ab0) and for its four situation
/// pickers (0x821d3d98, widgets +0x12c..). Found by the diff of two states a RIGHT press apart.
pub const FORMATION_COUNT_LI_SITES: &[u32] = &[0x8232_62a8, 0x821d_42e8, 0x821d_3d98];
/// `cmpwi crN,rA,14` sites that are the formation count: the name getter's bound (0x82326280:
/// null past it), the record setter's clamps for its two formation nibbles (0x8210dfa4,
/// 0x8210dfd8: 0 past it), the situation pickers' wrap (0x82193a54), the match engine's
/// "GP: Error - Formation being set to F_442" guard (0x822ad408) and its "use the team's own
/// formation" fallback (0x822ae0b8). The many other 14s near them are the tactic-instruction code
/// for "none" (13 instructions; 0x82261b48, 0x82261cc8 and the match-day screen) and 11 + 3 players.
pub const FORMATION_COUNT_CMP_SITES: &[u32] = &[0x8232_6280, 0x8210_dfa4, 0x8210_dfd8, 0x8219_3a54, 0x822a_d408, 0x822a_e0b8];
/// (hi, lo) of a guest address as a `lis` / signed `addi` pair would build it: `hi` is the lis
/// result (sign-extended, a multiple of 0x10000), `lo` the addi's immediate.
pub fn hi_lo(a: u32) -> (i64, i64) {
    let lo = a as u16 as i16 as i64;
    ((a as i32 as i64) - lo, lo)
}
#[inline(always)]
pub fn formation_roles() -> i64 {
    FORMATION_ROLES.load(std::sync::atomic::Ordering::Relaxed) as i32 as i64
}
#[inline(always)]
pub fn formation_roles_hi() -> i64 {
    hi_lo(FORMATION_ROLES.load(std::sync::atomic::Ordering::Relaxed)).0
}
#[inline(always)]
pub fn formation_roles_lo() -> i64 {
    hi_lo(FORMATION_ROLES.load(std::sync::atomic::Ordering::Relaxed)).1
}
#[inline(always)]
pub fn formation_names_hi() -> i64 {
    hi_lo(FORMATION_NAMES.load(std::sync::atomic::Ordering::Relaxed)).0
}
#[inline(always)]
pub fn formation_names_lo() -> i64 {
    hi_lo(FORMATION_NAMES.load(std::sync::atomic::Ordering::Relaxed)).1
}
#[inline(always)]
pub fn formation_ids_hi() -> i64 {
    hi_lo(FORMATION_IDS.load(std::sync::atomic::Ordering::Relaxed)).0
}
#[inline(always)]
pub fn formation_ids_lo() -> i64 {
    hi_lo(FORMATION_IDS.load(std::sync::atomic::Ordering::Relaxed)).1
}
#[inline(always)]
pub fn formation_spots_hi() -> i64 {
    hi_lo(FORMATION_SPOTS.load(std::sync::atomic::Ordering::Relaxed)).0
}
#[inline(always)]
pub fn formation_spots_lo() -> i64 {
    hi_lo(FORMATION_SPOTS.load(std::sync::atomic::Ordering::Relaxed)).1
}
#[inline(always)]
pub fn formation_order_hi() -> i64 {
    hi_lo(FORMATION_ORDER.load(std::sync::atomic::Ordering::Relaxed)).0
}
#[inline(always)]
pub fn formation_order_lo() -> i64 {
    hi_lo(FORMATION_ORDER.load(std::sync::atomic::Ordering::Relaxed)).1
}
#[inline(always)]
pub fn formation_count() -> i64 {
    FORMATION_COUNT.load(std::sync::atomic::Ordering::Relaxed) as i64
}
/// The quick-sim planner (0x822a6270, once per simulated match) gives a team with 3-5 tired players
/// 2 or 3 subs at random: `addi r31,0,2` / `addi r31,0,3` here. With `subs` they become subs - 1
/// and subs (#530), so a 5-sub match plans 4 or 5.
pub const SUBS_SIM_LOW: u32 = 0x822a_64a8;
pub const SUBS_SIM_HIGH: u32 = 0x822a_64b0;
pub static INTERP_STATS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static INTERP_STATS: std::sync::Mutex<Option<std::collections::HashMap<u32, u64>>> = std::sync::Mutex::new(None);

/// mfmsr: the MSR reads with EE set (as the interpreter); mtmsr/mtmsrd change nothing here.
#[inline(always)]
pub fn mfmsr(c: &mut Cpu, d: usize) {
    c.gpr[d] = 0x8000;
}

/// lwarx / ldarx: load and reserve: the address and the value loaded (a context switch clears
/// the reservation).
#[inline(always)]
pub fn larx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize, dbl: bool) {
    let ea = eax(c, a, b);
    c.reserve = Some(ea);
    c.gpr[d] = if dbl { m.read64(ea) } else { m.read32(ea) as u64 };
    c.reserve_val = c.gpr[d];
}

/// stwcx. / stdcx.: store if the reservation is still ours and the word still holds the value
/// `larx` loaded, as one compare-exchange (value-based LL/SC, so it holds when guest threads run
/// on several host threads, #39; ABA is accepted, as in Xenia); CR0 = EQ on success, plus SO.
#[inline(always)]
pub fn stcx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize, dbl: bool) {
    let ea = eax(c, a, b);
    let ok = c.reserve.take() == Some(ea)
        && if dbl { m.cas64(ea, c.reserve_val, c.gpr[s]) } else { m.cas32(ea, c.reserve_val as u32, c.gpr[s] as u32) };
    c.cr[0] = if ok { 2 } else { 0 } | c.so as u8;
}

/// stwux: store word with update.
#[inline(always)]
pub fn stwux(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    let ea = eax(c, a, b);
    m.write32(ea, c.gpr[s] as u32);
    c.gpr[a] = ea as u64;
}

/// mfcr / mfocrf (the interpreter returns the whole CR for both), mtcrf, orc, rlwnm, and
/// mfspr / mtspr of the registers without their own helper (XER; the rest read 0, ignore
/// writes): each as `Cpu::exec`.
#[inline(always)]
pub fn mfcr(c: &mut Cpu, d: usize) {
    c.gpr[d] = c.cr_word() as u64;
}
#[inline(always)]
pub fn mtcrf(c: &mut Cpu, s: usize, fxm: u32) {
    c.set_cr_word(c.gpr[s] as u32, fxm);
}
#[inline(always)]
pub fn orc(c: &mut Cpu, a: usize, s: usize, b: usize, rc: bool) {
    c.gpr[a] = c.gpr[s] | !c.gpr[b];
    cr0(c, rc, c.gpr[a]);
}
#[inline(always)]
pub fn rlwnm(c: &mut Cpu, a: usize, s: usize, b: usize, mask: u32, rc: bool) {
    let v = (c.gpr[s] as u32).rotate_left(c.gpr[b] as u32 & 31) & mask;
    c.gpr[a] = v as u64;
    cr0(c, rc, v as u64);
}
#[inline(always)]
pub fn mfspr(c: &mut Cpu, d: usize, spr: u32) {
    c.gpr[d] = if spr == 1 { c.xer() } else { 0 };
}
#[inline(always)]
pub fn mtspr(c: &mut Cpu, s: usize, spr: u32) {
    if spr == 1 {
        let v = c.gpr[s];
        c.so = v >> 31 & 1 == 1;
        c.ov = v >> 30 & 1 == 1;
        c.ca = v >> 29 & 1 == 1;
    }
}
/// lwbrx / lhbrx / ldbrx / stwbrx / sthbrx / stdbrx: byte-reversed loads and stores, as `Cpu::exec`
/// (stwbrx also tells the MMIO layer: the XMA command registers are written this way).
#[inline(always)]
pub fn lwbrx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    c.gpr[d] = m.read32(eax(c, a, b)).swap_bytes() as u64;
}
#[inline(always)]
pub fn lhbrx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    c.gpr[d] = m.read16(eax(c, a, b)).swap_bytes() as u64;
}
#[inline(always)]
pub fn ldbrx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    c.gpr[d] = m.read64(eax(c, a, b)).swap_bytes();
}
#[inline(always)]
pub fn stwbrx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    let ea = eax(c, a, b);
    crate::mmio::store_le(ea, c.gpr[s] as u32);
    m.write32(ea, (c.gpr[s] as u32).swap_bytes());
}
#[inline(always)]
pub fn sthbrx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    m.write16(eax(c, a, b), (c.gpr[s] as u16).swap_bytes());
}
#[inline(always)]
pub fn stdbrx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    m.write64(eax(c, a, b), c.gpr[s].swap_bytes());
}

/// twi / tdi: whether the trap fires (the recompiled code then hands the word to the
/// interpreter, which stops with the trap; usually it doesn't).
#[inline(always)]
pub fn trap_hit(c: &Cpu, to: u32, a: usize, simm: i64, word: bool) -> bool {
    let x = if word { c.gpr[a] as u32 as i32 as i64 } else { c.gpr[a] as i64 };
    crate::cpu::trap_cond(to, x, simm)
}

/// Any other instruction: run it through the reference interpreter.
#[inline(never)]
pub fn interp(c: &mut Cpu, m: &mut Memory, pc: u32, w: u32) -> Result<(), Stop> {
    if INTERP_STATS_ON.load(std::sync::atomic::Ordering::Relaxed) {
        *INTERP_STATS.lock().unwrap().get_or_insert_with(Default::default).entry(w).or_default() += 1;
    }
    c.pc = pc;
    c.exec(m, w)
}
/// mftb / mfspr 268, 269: the time base (upper word for TBU), as `Cpu::exec`.
#[inline(always)]
pub fn mftb(c: &mut Cpu, d: usize, upper: bool) {
    c.gpr[d] = c.timebase >> if upper { 32 } else { 0 };
}
/// Any other vector instruction: the interpreter's own VMX routine, with the mnemonic decoded at
/// recompile time (no lookup, same results as `interp` by construction). An unimplemented one
/// goes through `exec` so the stop reports its pc.
#[inline(never)]
pub fn vx(c: &mut Cpu, m: &mut Memory, pc: u32, w: u32, name: &'static str) -> Result<(), Stop> {
    if c.vmx(m, w, name).is_err() {
        c.pc = pc;
        return c.exec(m, w);
    }
    Ok(())
}
/// lvx / lvxl / lvx128 / lvxl128, native: 16 bytes from (RA|0) + RB, aligned down to 16 (as the
/// interpreter's `Cpu::vmx`). The vector register `d` is decoded at recompile time for both forms.
#[inline(always)]
pub fn vlvx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    let ea = (if a == 0 { 0 } else { c.gpr[a] as u32 }).wrapping_add(c.gpr[b] as u32) & !15;
    c.vr[d] = m.read_n::<16>(ea);
}
/// stvx / stvxl / stvx128 / stvxl128, native.
#[inline(always)]
pub fn vstvx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    let ea = (if a == 0 { 0 } else { c.gpr[a] as u32 }).wrapping_add(c.gpr[b] as u32) & !15;
    let v = c.vr[s];
    m.write_n(ea, &v);
}
/// lvlx / lvlx128 (and the l forms), native: the bytes from (RA|0) + RB to the end of its
/// 16-byte line, into the left of the register, zeros after (as `Cpu::vmx`).
#[inline(always)]
pub fn vlvlx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    let ea = (if a == 0 { 0 } else { c.gpr[a] as u32 }).wrapping_add(c.gpr[b] as u32);
    // the whole aligned line in the fast path, shifted: no variable-length copy (a memmove call)
    if let Some(line) = m.fast::<16>(ea & !15) {
        c.vr[d] = (u128::from_be_bytes(*line) << (8 * (ea & 15))).to_be_bytes();
        return;
    }
    let n = 16 - (ea & 15) as usize;
    let mut v = [0u8; 16];
    m.read(ea, &mut v[..n]);
    c.vr[d] = v;
}
/// lvrx / lvrx128, native: the bytes of the line before (RA|0) + RB, into the right of the register.
#[inline(always)]
pub fn vlvrx(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, b: usize) {
    let ea = (if a == 0 { 0 } else { c.gpr[a] as u32 }).wrapping_add(c.gpr[b] as u32);
    let n = (ea & 15) as usize;
    // as `vlvlx`: the line shifted (nothing when ea is aligned)
    if let Some(line) = m.fast::<16>(ea & !15) {
        c.vr[d] = if n == 0 { [0; 16] } else { (u128::from_be_bytes(*line) >> (8 * (16 - n))).to_be_bytes() };
        return;
    }
    let mut v = [0u8; 16];
    m.read(ea & !15, &mut v[16 - n..]);
    c.vr[d] = v;
}
#[inline(always)]
pub fn vor(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (va, vb) = (c.vr[a], c.vr[b]);
    c.vr[d] = std::array::from_fn(|i| va[i] | vb[i]);
}
/// vsel: bits from b where the mask is set, else from a; the mask is VC (vsel) or VD (vsel128).
#[inline(always)]
pub fn vsel(c: &mut Cpu, d: usize, a: usize, b: usize, mask: usize) {
    let (va, vb, mk) = (c.vr[a], c.vr[b], c.vr[mask]);
    c.vr[d] = std::array::from_fn(|i| (va[i] & !mk[i]) | (vb[i] & mk[i]));
}
#[inline(always)]
fn vwords(v: &[u8; 16]) -> [u32; 4] {
    std::array::from_fn(|i| u32::from_be_bytes([v[4 * i], v[4 * i + 1], v[4 * i + 2], v[4 * i + 3]]))
}
#[inline(always)]
fn vfrom_words(x: [u32; 4]) -> [u8; 16] {
    let mut v = [0u8; 16];
    for i in 0..4 {
        v[4 * i..4 * i + 4].copy_from_slice(&x[i].to_be_bytes());
    }
    v
}
/// vmrghw (high = true) / vmrglw, native.
#[inline(always)]
pub fn vmrgw(c: &mut Cpu, d: usize, a: usize, b: usize, high: bool) {
    let (x, y) = (vwords(&c.vr[a]), vwords(&c.vr[b]));
    c.vr[d] = vfrom_words(if high { [x[0], y[0], x[1], y[1]] } else { [x[2], y[2], x[3], y[3]] });
}
/// vslo (left = true) / vsro: shift the whole register by octets (vb's low byte, bits 1-4).
#[inline(always)]
pub fn vso(c: &mut Cpu, d: usize, a: usize, b: usize, left: bool) {
    let n = ((c.vr[b][15] >> 3) & 15) as u32 * 8;
    let x = u128::from_be_bytes(c.vr[a]);
    c.vr[d] = (if left { x.checked_shl(n).unwrap_or(0) } else { x.checked_shr(n).unwrap_or(0) }).to_be_bytes();
}

/// stvewx / stvewx128: store the word of VS selected by the (word-aligned) address.
#[inline(always)]
pub fn vstvewx(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, b: usize) {
    let ea = (if a == 0 { 0 } else { c.gpr[a] as u32 }).wrapping_add(c.gpr[b] as u32) & !3;
    let i = ((ea >> 2) & 3) as usize;
    let v = c.vr[s];
    m.write_n(ea, &[v[4 * i], v[4 * i + 1], v[4 * i + 2], v[4 * i + 3]]);
}
/// vspltisw and friends: every word set to a constant.
#[inline(always)]
pub fn vsplatw(c: &mut Cpu, d: usize, x: u32) {
    c.vr[d] = vfrom_words([x; 4]);
}
/// vcfsx / vcfux (and the 128 forms): words to floats, scaled by 2^-uimm.
#[inline(always)]
pub fn vcfx(c: &mut Cpu, d: usize, b: usize, uimm: u32, signed: bool) {
    let s = (2.0f32).powi(-(uimm as i32));
    let x = vwords(&c.vr[b]);
    c.vr[d] = vfrom_words(x.map(|x| { let f = if signed { x as i32 as f32 } else { x as f32 }; (f * s).to_bits() }));
}
/// vctsxs / vcfpsxws128: floats scaled by 2^uimm to saturated signed words (NaN -> 0).
#[inline(always)]
pub fn vctsxs(c: &mut Cpu, d: usize, b: usize, uimm: u32) {
    let s = (2.0f32).powi(uimm as i32);
    let x = vwords(&c.vr[b]).map(f32::from_bits);
    c.vr[d] = vfrom_words(x.map(|x| if x.is_nan() { 0 } else { (x * s).clamp(i32::MIN as f32, i32::MAX as f32) as i32 as u32 }));
}
/// vrlimi128: rotate VB left by `rot` words and insert the words selected by `mask` into VD.
#[inline(always)]
pub fn vrlimi(c: &mut Cpu, d: usize, b: usize, mask: u32, rot: usize) {
    let x = vwords(&c.vr[b]);
    let old = vwords(&c.vr[d]);
    c.vr[d] = vfrom_words(std::array::from_fn(|i| if mask & (8 >> i) != 0 { x[(i + rot) & 3] } else { old[i] }));
}
fn vhalves(v: &[u8; 16]) -> [u16; 8] {
    std::array::from_fn(|i| u16::from_be_bytes([v[2 * i], v[2 * i + 1]]))
}
fn vfrom_halves(x: [u16; 8]) -> [u8; 16] {
    std::array::from_fn(|i| x[i / 2].to_be_bytes()[i % 2])
}
fn sat16(x: i32) -> u16 {
    x.clamp(i16::MIN as i32, i16::MAX as i32) as i16 as u16
}

// Integer vector ops the game's video decoder and others run in loops (#204: half the intro's
// time was these in the interpreter). Each is the interpreter's arm in `vmx.rs`, lane for lane
// (no VSCR saturation bit, as there); `vmx_native_matches_interpreter` checks them against it.
macro_rules! vlanes {
    ($($name:ident: $get:ident $put:ident |$x:ident, $y:ident| $e:expr;)*) => {$(
        #[inline(always)]
        pub fn $name(c: &mut Cpu, d: usize, a: usize, b: usize) {
            let (xs, ys) = ($get(&c.vr[a]), $get(&c.vr[b]));
            c.vr[d] = $put(std::array::from_fn(|i| { let ($x, $y) = (xs[i], ys[i]); $e }));
        }
    )*};
}
vlanes! {
    vadduhm: vhalves vfrom_halves |x, y| x.wrapping_add(y);
    vsubuhm: vhalves vfrom_halves |x, y| x.wrapping_sub(y);
    vaddshs: vhalves vfrom_halves |x, y| sat16(x as i16 as i32 + y as i16 as i32);
    vsubshs: vhalves vfrom_halves |x, y| sat16(x as i16 as i32 - y as i16 as i32);
    vslh: vhalves vfrom_halves |x, y| x << (y & 15);
    vsrh: vhalves vfrom_halves |x, y| x >> (y & 15);
    vsrah: vhalves vfrom_halves |x, y| ((x as i16) >> (y & 15)) as u16;
    vrlh: vhalves vfrom_halves |x, y| x.rotate_left((y & 15) as u32);
    vaddsws: vwords vfrom_words |x, y| (x as i32).saturating_add(y as i32) as u32;
    vsubsws: vwords vfrom_words |x, y| (x as i32).saturating_sub(y as i32) as u32;
    vsraw: vwords vfrom_words |x, y| ((x as i32) >> (y & 31)) as u32;
}
/// vmrghb / vmrglb: bytes of the high (low) halves of VA and VB, interleaved.
#[inline(always)]
pub fn vmrgb(c: &mut Cpu, d: usize, a: usize, b: usize, high: bool) {
    let (x, y, o) = (c.vr[a], c.vr[b], if high { 0 } else { 8 });
    c.vr[d] = std::array::from_fn(|i| if i % 2 == 0 { x[o + i / 2] } else { y[o + i / 2] });
}
/// vmrghh / vmrglh: halfwords of the high (low) halves of VA and VB, interleaved.
#[inline(always)]
pub fn vmrgh(c: &mut Cpu, d: usize, a: usize, b: usize, high: bool) {
    let (x, y, o) = (vhalves(&c.vr[a]), vhalves(&c.vr[b]), if high { 0 } else { 4 });
    c.vr[d] = vfrom_halves(std::array::from_fn(|i| if i % 2 == 0 { x[o + i / 2] } else { y[o + i / 2] }));
}
/// vpkshus: VA then VB's signed halfwords saturated to unsigned bytes.
#[inline(always)]
pub fn vpkshus(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (x, y) = (vhalves(&c.vr[a]), vhalves(&c.vr[b]));
    c.vr[d] = std::array::from_fn(|i| (if i < 8 { x[i] } else { y[i - 8] } as i16).clamp(0, 255) as u8);
}
/// vpkswss: VA then VB's signed words saturated to signed halfwords.
#[inline(always)]
pub fn vpkswss(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (x, y) = (vwords(&c.vr[a]), vwords(&c.vr[b]));
    c.vr[d] = vfrom_halves(std::array::from_fn(|i| sat16(if i < 4 { x[i] } else { y[i - 4] } as i32)));
}
/// vupkhsh / vupklsh: the high (low) four signed halfwords of VB, sign-extended to words.
#[inline(always)]
pub fn vupksh(c: &mut Cpu, d: usize, b: usize, high: bool) {
    let (x, o) = (vhalves(&c.vr[b]), if high { 0 } else { 4 });
    c.vr[d] = vfrom_words(std::array::from_fn(|i| x[o + i] as i16 as i32 as u32));
}
/// vspltish / vspltisb: every halfword (byte) set to a constant.
#[inline(always)]
pub fn vsplath(c: &mut Cpu, d: usize, x: u16) {
    c.vr[d] = vfrom_halves([x; 8]);
}
#[inline(always)]
pub fn vsplatb(c: &mut Cpu, d: usize, x: u8) {
    c.vr[d] = [x; 16];
}
/// vspltb / vsplth / vspltw: one element of VB (`size` bytes, index `i`) in every lane.
#[inline(always)]
pub fn vsplte(c: &mut Cpu, d: usize, b: usize, size: usize, i: usize) {
    let v = c.vr[b];
    let n = 16 / size;
    let o = (i % n) * size;
    c.vr[d] = std::array::from_fn(|k| v[o + k % size]);
}
/// Vector float and logic ops natively (#6): the interpreter's own operations (crate::vmx), with the
/// registers decoded at recompile time (standard and VMX128 forms alike).
#[inline(always)]
pub fn vaddfp(c: &mut Cpu, d: usize, a: usize, b: usize) {
    c.vr[d] = crate::vmx::add_f(&c.vr[a], &c.vr[b]);
}
#[inline(always)]
pub fn vsubfp(c: &mut Cpu, d: usize, a: usize, b: usize) {
    c.vr[d] = crate::vmx::sub_f(&c.vr[a], &c.vr[b]);
}
#[inline(always)]
pub fn vmulfp(c: &mut Cpu, d: usize, a: usize, b: usize) {
    c.vr[d] = crate::vmx::mul_f(&c.vr[a], &c.vr[b]);
}
/// x * y + z per lane (vmaddfp: a, c, b; vmaddfp128: a, b, d)
#[inline(always)]
pub fn vmaddfp(c: &mut Cpu, d: usize, x: usize, y: usize, z: usize) {
    c.vr[d] = crate::vmx::madd_f(&c.vr[x], &c.vr[y], &c.vr[z]);
}
#[inline(always)]
pub fn vmsum3fp(c: &mut Cpu, d: usize, a: usize, b: usize) {
    c.vr[d] = crate::vmx::msum3_f(&c.vr[a], &c.vr[b]);
}
#[inline(always)]
pub fn vsldoi(c: &mut Cpu, d: usize, a: usize, b: usize, sh: usize) {
    c.vr[d] = crate::vmx::sldoi(&c.vr[a], &c.vr[b], sh);
}
#[inline(always)]
pub fn vand(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (x, y) = (c.vr[a], c.vr[b]);
    c.vr[d] = std::array::from_fn(|i| x[i] & y[i]);
}
#[inline(always)]
pub fn vandc(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (x, y) = (c.vr[a], c.vr[b]);
    c.vr[d] = std::array::from_fn(|i| x[i] & !y[i]);
}
#[inline(always)]
pub fn vxor(c: &mut Cpu, d: usize, a: usize, b: usize) {
    let (x, y) = (c.vr[a], c.vr[b]);
    c.vr[d] = std::array::from_fn(|i| x[i] ^ y[i]);
}
/// vperm / vperm128: bytes of VA:VB picked by the control register.
#[inline(always)]
pub fn vperm(c: &mut Cpu, d: usize, a: usize, b: usize, ctl: usize) {
    let mut cat = [0u8; 32];
    cat[..16].copy_from_slice(&c.vr[a]);
    cat[16..].copy_from_slice(&c.vr[b]);
    let k = c.vr[ctl];
    c.vr[d] = std::array::from_fn(|i| cat[(k[i] & 31) as usize]);
}
/// dcbz: zero the 32-byte block.
#[inline(always)]
pub fn dcbz(c: &mut Cpu, m: &mut Memory, a: usize, b: usize) {
    m.write_n(eax(c, a, b) & !31, &[0; 32]);
}
/// dcbz128: zero the Xenon's whole 128-byte cache line (#204: the game's WMV3 decoder clears its
/// 64-coefficient blocks with it; clearing 32 bytes left stale coefficients that built up from
/// frame to frame, the noise on the Codemasters logo).
#[inline(always)]
pub fn dcbz128(c: &mut Cpu, m: &mut Memory, a: usize, b: usize) {
    m.write_n(eax(c, a, b) & !127, &[0; 128]);
}
/// lbzu / stbu (a != 0, and a != d for the load: the recompiler leaves other forms to the interpreter).
#[inline(always)]
pub fn lbzu(c: &mut Cpu, m: &mut Memory, d: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    c.gpr[d] = m.read8(e) as u64;
    c.gpr[a] = e as u64;
}
#[inline(always)]
pub fn stbu(c: &mut Cpu, m: &mut Memory, s: usize, a: usize, off: i32) {
    let e = ea(c, a, off);
    m.write8(e, c.gpr[s] as u8);
    c.gpr[a] = e as u64;
}

/// Opcode 30 (rldicl, rldicr, rldic, rldimi, rldcl/rldcr), native: the interpreter's arm with
/// the word as a literal at the call site, so the field decoding folds away. Sub-ops 0-4 only
/// (the recompiler emits nothing else here).
#[inline(always)]
pub fn md30(c: &mut Cpu, w: u32) {
    use ppc::bits;
    let (rd, ra, rb) = (bits(w, 6, 10) as usize, bits(w, 11, 15) as usize, bits(w, 16, 20) as usize);
    let sh = bits(w, 16, 20) | (bits(w, 30, 30) << 5);
    let mb6 = bits(w, 21, 25) | (bits(w, 26, 26) << 5);
    let s = c.gpr[rd];
    let v = match bits(w, 27, 29) {
        0 => s.rotate_left(sh) & crate::cpu::mask64(mb6, 63),
        1 => s.rotate_left(sh) & crate::cpu::mask64(0, mb6),
        2 => s.rotate_left(sh) & crate::cpu::mask64(mb6, 63 - sh),
        3 => {
            let m = crate::cpu::mask64(mb6, 63 - sh);
            (s.rotate_left(sh) & m) | (c.gpr[ra] & !m)
        }
        _ => {
            let n = c.gpr[rb] as u32 & 63;
            if bits(w, 30, 30) == 0 { s.rotate_left(n) & crate::cpu::mask64(mb6, 63) } else { s.rotate_left(n) & crate::cpu::mask64(0, mb6) }
        }
    };
    c.gpr[ra] = v;
    c.rc(w, v);
}

/// 64-bit shifts and count (opcode 31: sld, srd, srad, sradi, cntlzd), native: the
/// interpreter's arms (`Cpu::op31`), the word a literal at the call site.
#[inline(always)]
pub fn x31s(c: &mut Cpu, w: u32) {
    use ppc::bits;
    let (rd, ra, rb) = (bits(w, 6, 10) as usize, bits(w, 11, 15) as usize, bits(w, 16, 20) as usize);
    let s = c.gpr[rd];
    let b32 = c.gpr[rb] as u32;
    match ppc::xo10(w) {
        27 => { let n = b32 & 127; c.gpr[ra] = if n > 63 { 0 } else { s << n }; }
        539 => { let n = b32 & 127; c.gpr[ra] = if n > 63 { 0 } else { s >> n }; }
        58 => c.gpr[ra] = s.leading_zeros() as u64,
        _ => {
            // 794 srad, 826/827 sradi
            let n = if ppc::xo10(w) == 794 { b32 & 127 } else { bits(w, 16, 20) | (bits(w, 30, 30) << 5) };
            let v = s as i64;
            let (r, lost) = if n > 63 { (v >> 63, v < 0) } else { (v >> n, n > 0 && (v as u64) << (64 - n) != 0) };
            c.ca = v < 0 && lost;
            c.gpr[ra] = r as u64;
        }
    }
    c.rc(w, c.gpr[ra]);
}

/// A branch the generator does not translate: interpret it and report where it went.
#[inline(never)]
pub fn interp_branch(c: &mut Cpu, m: &mut Memory, pc: u32, w: u32) -> Result<u32, Stop> {
    c.pc = pc;
    c.exec(m, w)?;
    Ok(c.pc)
}
/// Conditional branch to `lr` / `ctr` (bclr / bcctr): target or `None` when not taken.
#[inline(always)]
pub fn bclr(c: &mut Cpu, bo: u32, bi: u32) -> Option<u32> {
    let t = c.lr as u32 & !3;
    c.branch_cond(bo, bi).then_some(t)
}
#[inline(always)]
pub fn bcctr(c: &mut Cpu, bo: u32, bi: u32) -> Option<u32> {
    let take = bo & 16 != 0 || c.cr_bit(bi) == (bo & 8 != 0);
    take.then_some(c.ctr as u32 & !3)
}

/// Recompiled code lives at and above this guest address (the start of the image's code).
pub const BLOCK_BASE: u32 = 0x8200_0000;

// ---- direct calls between recompiled functions -------------------------------------------
// Without them every guest call and return goes back to `Machine::run_slice` to find the next
// function. `call` enters the callee natively instead. It mirrors the dispatcher exactly (same
// timebase on every function entry, same budget), so results are bit-identical; anything the
// dispatcher would handle differently (budget spent, no recompiled block, deep recursion)
// returns the target pc, which leaves the caller's range and so goes back to the dispatcher.

use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering::Relaxed};

/// Recompiled functions by `(pc - BLOCK_BASE) / 4`, published by `Machine::install_blocks`.
/// (Plain atomics: thread-locals cost a `_tlv_get_addr` call per access on macOS.)
static TABLE: AtomicPtr<Option<FuncFn>> = AtomicPtr::new(std::ptr::null_mut());
static TABLE_LEN: AtomicUsize = AtomicUsize::new(0);

/// Native call depth before calls go through the dispatcher again (bounds the host stack).
const MAX_DEPTH: u32 = 48;

/// Set while guest threads run on several host threads (#39): guest barriers become host fences.
pub static PARALLEL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// sync (`full`) / lwsync, eieio: nothing to order with one host thread; host fences while guest
/// threads run on several (`PARALLEL`), so stores a guest thread orders before a flag reach the
/// others in that order (the Mac's ARM cores reorder stores like the Xenon's). lwsync and eieio
/// need acquire/release only (free on x86, `dmb` on ARM); sync is a full fence (#346 review).
#[inline(always)]
pub fn barrier(full: bool) {
    if PARALLEL.load(Relaxed) {
        std::sync::atomic::fence(if full { std::sync::atomic::Ordering::SeqCst } else { std::sync::atomic::Ordering::AcqRel });
    }
}

/// The recompiled function starting the block at `pc`, if any (the table `call` uses: no import
/// thunks or hooked functions). For host workers running guest threads (#39).
pub fn block_at(pc: u32) -> Option<FuncFn> {
    let i = (pc.wrapping_sub(BLOCK_BASE) >> 2) as usize;
    if i >= TABLE_LEN.load(Relaxed) {
        return None;
    }
    // SAFETY: the table is leaked (never freed) and published before its length
    unsafe { *TABLE.load(Relaxed).add(i) }
}

pub fn set_table(t: &'static mut [Option<FuncFn>]) {
    TABLE_LEN.store(0, Relaxed);
    TABLE.store(t.as_mut_ptr(), Relaxed);
    TABLE_LEN.store(t.len(), Relaxed);
}

/// Real-time pacing: guest time advances one tick per 2^RT_SHIFT instructions within a slice.
/// That only keeps timebase reads increasing; it is far slower than the host clock (a
/// 20k-instruction slice adds < 0.4 us while taking >= ~2 us), so guest time cannot run ahead
/// of real time however fast the CPU is.
pub const RT_SHIFT: u32 = 10;

/// Guest timebase when `b` budget is left in the current slice, exactly as the dispatcher
/// computes it: instruction clock (`slice_end - b`) normally, or under real-time pacing (#56)
/// the slice's real-time base plus instructions consumed >> RT_SHIFT.
#[inline(always)]
pub fn timebase_at(c: &Cpu, b: i64) -> u64 {
    if c.rt {
        c.rt_base + ((c.slice_len - b).max(0) as u64 >> RT_SHIFT)
    } else {
        c.slice_end.wrapping_sub(b as u64)
    }
}

/// LMA_CALL_TARGETS=1 (analysis, #171 / #145): `call` counts each indirect call (a `bctrl` at
/// the return address - 4) by (call site, target), to size inline caching at the hot sites.
pub static CALL_TARGETS_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
pub static CALL_TARGETS: std::sync::Mutex<Option<std::collections::HashMap<(u32, u32), u64>>> = std::sync::Mutex::new(None);

#[cold]
#[inline(never)]
fn count_call(c: &Cpu, m: &mut Memory, target: u32) {
    let site = (c.lr as u32).wrapping_sub(4);
    if m.read32(site) == 0x4e80_0421 {
        *CALL_TARGETS.lock().unwrap().get_or_insert_with(Default::default).entry((site, target)).or_default() += 1;
    }
}

/// `bl target` / `bctrl` from recompiled code: run the callee natively and return the pc it
/// stopped at (normally the return address, so the caller carries on in its own `match`).
#[inline(never)]
pub fn call(c: &mut Cpu, m: &mut Memory, target: u32, b: &mut i64) -> Result<u32, Stop> {
    if CALL_TARGETS_ON.load(Relaxed) {
        count_call(c, m, target);
    }
    let i = (target.wrapping_sub(BLOCK_BASE) >> 2) as usize;
    if *b <= 0 || i >= TABLE_LEN.load(Relaxed) || c.call_depth >= MAX_DEPTH {
        return Ok(target);
    }
    // SAFETY: the table is leaked (never freed) and published before its length
    let Some(f) = (unsafe { *TABLE.load(Relaxed).add(i) }) else { return Ok(target) };
    c.timebase = timebase_at(c, *b); // the dispatcher sets this on every function entry
    c.call_depth += 1;
    let r = f(c, m, target, b);
    c.call_depth -= 1;
    let next = r?;
    c.timebase = timebase_at(c, *b); // ...including re-entering the caller
    Ok(next)
}

/// `call` for a target known at recompile time to start a function in the same part crate:
/// the same checks and timebase updates, without the table lookup (and the callee can inline).
#[inline(always)]
pub fn call_local(c: &mut Cpu, m: &mut Memory, target: u32, b: &mut i64, f: FuncFn) -> Result<u32, Stop> {
    if *b <= 0 || c.call_depth >= MAX_DEPTH {
        return Ok(target);
    }
    c.timebase = timebase_at(c, *b);
    c.call_depth += 1;
    let r = f(c, m, target, b);
    c.call_depth -= 1;
    let next = r?;
    c.timebase = timebase_at(c, *b);
    Ok(next)
}

// ---- instructions that used to go through the interpreter -------------------------------
// Each body is copied from the matching case of `Cpu::exec` / `Cpu::fp` (`self.` -> `c.`), so
// results are bit-identical; the opcode is a literal at every call site, so the match folds.

/// XO-form integer arithmetic (op 31, 9-bit XO; the interpreter ignores OE, so do we).
pub const XO_ARITH: [u32; 21] = [8, 9, 10, 11, 40, 73, 75, 104, 136, 138, 200, 202, 232, 233, 234, 235, 266, 457, 459, 489, 491];
#[inline(always)]
pub fn xo(c: &mut Cpu, xo9: u32, rd: usize, ra: usize, rb: usize, rc: bool) {
    let (a, b) = (c.gpr[ra], c.gpr[rb]);
    let (a32, b32) = (a as u32, b as u32);
    let r = match xo9 {
        266 => a.wrapping_add(b),
        40 => b.wrapping_sub(a),
        104 => (a as i64).wrapping_neg() as u64,
        235 => (a32 as i32 as i64).wrapping_mul(b32 as i32 as i64) as u64,
        233 => (a as i64).wrapping_mul(b as i64) as u64,
        75 => (((a32 as i32 as i64) * (b32 as i32 as i64)) >> 32) as u64,
        11 => ((a32 as u64 * b32 as u64) >> 32) as u64,
        73 => (((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64,
        9 => (((a as u128) * (b as u128)) >> 64) as u64,
        491 => if b32 == 0 { 0 } else { (a32 as i32).wrapping_div(b32 as i32) as u32 as u64 },
        459 => if b32 == 0 { 0 } else { (a32 / b32) as u64 },
        489 => if b == 0 { 0 } else { (a as i64).wrapping_div(b as i64) as u64 },
        457 => if b == 0 { 0 } else { a / b },
        10 => {
            c.ca = a32.checked_add(b32).is_none();
            a.wrapping_add(b)
        }
        8 => {
            c.ca = b32 >= a32;
            b.wrapping_sub(a)
        }
        138 => {
            let (s1, c1) = a32.overflowing_add(b32);
            let (_, c2) = s1.overflowing_add(c.ca as u32);
            let r = a.wrapping_add(b).wrapping_add(c.ca as u64);
            c.ca = c1 | c2;
            r
        }
        136 => {
            let (s1, c1) = (!a32).overflowing_add(b32);
            let (_, c2) = s1.overflowing_add(c.ca as u32);
            let r = (!a).wrapping_add(b).wrapping_add(c.ca as u64);
            c.ca = c1 | c2;
            r
        }
        202 => {
            let r = a.wrapping_add(c.ca as u64);
            c.ca = a32.checked_add(c.ca as u32).is_none();
            r
        }
        200 => {
            let r = (!a).wrapping_add(c.ca as u64);
            c.ca = (!a32).checked_add(c.ca as u32).is_none();
            r
        }
        234 => {
            let r = a.wrapping_add(c.ca as u64).wrapping_sub(1);
            c.ca = a32 != 0 || c.ca;
            r
        }
        232 => {
            let r = (!a).wrapping_add(c.ca as u64).wrapping_sub(1);
            c.ca = !a32 != 0 || c.ca;
            r
        }
        _ => unreachable!(),
    };
    c.gpr[rd] = r;
    if rc {
        c.set_cr_signed(0, r as u32 as i32 as i64, 0);
    }
}
/// sraw / srawi
#[inline(always)]
pub fn sraw(c: &mut Cpu, srawi: bool, ra: usize, rs: usize, rb: usize, sh: u32, rc: bool) {
    let n = if srawi { sh } else { c.gpr[rb] as u32 & 63 };
    let v = c.gpr[rs] as u32 as i32;
    let (r, lost) = if n > 31 {
        ((v >> 31) as i64, v < 0)
    } else {
        let lost = n > 0 && (v as u32) << (32 - n) != 0;
        ((v >> n) as i64, lost)
    };
    c.ca = v < 0 && lost;
    c.gpr[ra] = r as u64;
    if rc {
        c.set_cr_signed(0, c.gpr[ra] as u32 as i32 as i64, 0);
    }
}
/// subfic (op 8)
#[inline(always)]
pub fn subfic(c: &mut Cpu, rd: usize, ra: usize, simm: u64) {
    let a = c.gpr[ra] as u32;
    c.gpr[rd] = simm.wrapping_sub(c.gpr[ra]);
    c.ca = (simm as u32) >= a;
}
/// addic / addic. (ops 12 / 13)
#[inline(always)]
pub fn addic(c: &mut Cpu, rd: usize, ra: usize, simm: u64, dot: bool) {
    let a = c.gpr[ra] as u32;
    c.gpr[rd] = c.gpr[ra].wrapping_add(simm);
    c.ca = a.checked_add(simm as u32).is_none();
    if dot {
        c.set_cr_signed(0, c.gpr[rd] as u32 as i32 as i64, 0);
    }
}
/// Floating point A-form (ops 59 / 63; `single` = op 59): arithmetic and rounding as Cpu::fp.
#[inline(always)]
pub fn fpa(c: &mut Cpu, xo5: u32, single: bool, d: usize, fa: usize, fb: usize, fc: usize) {
    let (a, b, c_) = (c.fpr[fa], c.fpr[fb], c.fpr[fc]);
    let v = match xo5 {
        18 => a / b,
        20 => a - b,
        21 => a + b,
        22 => b.sqrt(),
        23 => if a >= 0.0 { c_ } else { b },
        24 => 1.0 / b,
        25 => a * c_,
        26 => 1.0 / b.sqrt(),
        28 => a.mul_add(c_, -b),
        29 => a.mul_add(c_, b),
        30 => -a.mul_add(c_, -b),
        31 => -a.mul_add(c_, b),
        _ => unreachable!(),
    };
    c.fpr[d] = if single { v as f32 as f64 } else { v };
}
/// Floating point X-form (compare, moves, conversions, fpscr) as in Cpu::fp.
#[inline(always)]
pub fn fpx(c: &mut Cpu, xo10: u32, d: usize, f: usize, fa: usize, fb: usize) {
    let (a, b) = (c.fpr[fa], c.fpr[fb]);
    match xo10 {
        0 | 32 => {
            c.cr[f] = if a.is_nan() || b.is_nan() { 1 } else if a < b { 8 } else if a > b { 4 } else { 2 };
        }
        12 => c.fpr[d] = b as f32 as f64,
        x @ (14 | 15 | 814 | 815) => c.fpr[d] = c.fp_to_int(x, b),
        846 => c.fpr[d] = b.to_bits() as i64 as f64,
        40 => c.fpr[d] = -b,
        72 => c.fpr[d] = b,
        136 => c.fpr[d] = -b.abs(),
        264 => c.fpr[d] = b.abs(),
        583 => c.fpr[d] = f64::from_bits(c.fpscr as u64),
        711 => c.fpscr = b.to_bits() as u32,
        38 | 70 | 134 | 64 => {}
        _ => unreachable!(),
    }
}
/// Floating point indexed loads/stores (op 31: lfsx 535, lfsux 567, lfdx 599, lfdux 631,
/// stfsx 663, stfsux 695, stfdx 727, stfdux 759, stfiwx 983), as in Cpu::exec.
#[inline(always)]
pub fn fpx_mem(c: &mut Cpu, m: &mut Memory, xo10: u32, d: usize, a: usize, b: usize) {
    let ea = eax(c, a, b);
    match xo10 {
        535 | 567 => c.fpr[d] = f32::from_bits(m.read32(ea)) as f64,
        599 | 631 => c.fpr[d] = f64::from_bits(m.read64(ea)),
        663 | 695 => m.write32(ea, (c.fpr[d] as f32).to_bits()),
        727 | 759 => m.write64(ea, c.fpr[d].to_bits()),
        983 => m.write32(ea, c.fpr[d].to_bits() as u32),
        _ => unreachable!(),
    }
    if matches!(xo10, 567 | 631 | 695 | 759) {
        c.gpr[a] = ea as u64;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dcbz clears its 32-byte block, dcbz128 (RT = 1) the whole 128-byte line (#204), in the
    /// interpreter and in the recompiler's helpers alike.
    #[test]
    fn dcbz_and_dcbz128() {
        let base = 0x4000_0000u32;
        for (rt, n) in [(0u32, 32usize), (1, 128)] {
            let w = (31 << 26) | (rt << 21) | (3 << 11) | (4 << 16) | (1014 << 1); // dcbz r3, r4
            let mut results = Vec::new();
            for native in [false, true] {
                let mut m = Memory::new();
                m.write(base, &[0xAA; 256]);
                let mut c = Cpu::default();
                (c.gpr[3], c.gpr[4]) = (base as u64, 0x85); // inside the second 128-byte line
                if native {
                    if rt == 1 { dcbz128(&mut c, &mut m, 3, 4) } else { dcbz(&mut c, &mut m, 3, 4) }
                } else {
                    c.exec(&mut m, w).unwrap();
                }
                let mut b = [0u8; 256];
                m.read(base, &mut b);
                results.push(b);
            }
            assert_eq!(results[0], results[1], "interpreter and helper agree");
            let start = 0x85 & !(n - 1);
            let b = results[0];
            assert!(b[start..start + n].iter().all(|&x| x == 0));
            assert!(b[..start].iter().chain(&b[start + n..]).all(|&x| x == 0xAA));
        }
    }
}

#[cfg(test)]
mod vmx_native_tests {
    use super::*;

    /// The first opcode-4 word `ppc::lookup` names `name`, with VD = 1, VA = 2, VB = 3 and the
    /// immediate/element field (VA's bits) set to `imm` when given.
    fn word(name: &str, imm: Option<u32>) -> u32 {
        let regs = |a: u32| (4 << 26) | (1 << 21) | (a << 16) | (3 << 11);
        (0..0x800u32)
            .map(|xo| regs(imm.unwrap_or(2)) | xo)
            .find(|&w| ppc::lookup(w) == Some(name))
            .unwrap_or_else(|| panic!("no encoding for {name}"))
    }

    /// Every native integer helper gives exactly what the interpreter does, on random lanes
    /// (edge values mixed in: 0x8000, 0x7fff, 0xffff, shift counts over 15).
    #[test]
    fn vmx_native_matches_interpreter() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        type F = fn(&mut Cpu);
        let ops: &[(&str, Option<u32>, F)] = &[
            ("vadduhm", None, |c| vadduhm(c, 1, 2, 3)),
            ("vsubuhm", None, |c| vsubuhm(c, 1, 2, 3)),
            ("vaddshs", None, |c| vaddshs(c, 1, 2, 3)),
            ("vsubshs", None, |c| vsubshs(c, 1, 2, 3)),
            ("vslh", None, |c| vslh(c, 1, 2, 3)),
            ("vsrh", None, |c| vsrh(c, 1, 2, 3)),
            ("vsrah", None, |c| vsrah(c, 1, 2, 3)),
            ("vrlh", None, |c| vrlh(c, 1, 2, 3)),
            ("vaddsws", None, |c| vaddsws(c, 1, 2, 3)),
            ("vsubsws", None, |c| vsubsws(c, 1, 2, 3)),
            ("vsraw", None, |c| vsraw(c, 1, 2, 3)),
            ("vpkshus", None, |c| vpkshus(c, 1, 2, 3)),
            ("vpkswss", None, |c| vpkswss(c, 1, 2, 3)),
            ("vmrghb", None, |c| vmrgb(c, 1, 2, 3, true)),
            ("vmrglb", None, |c| vmrgb(c, 1, 2, 3, false)),
            ("vmrghh", None, |c| vmrgh(c, 1, 2, 3, true)),
            ("vmrglh", None, |c| vmrgh(c, 1, 2, 3, false)),
            ("vupkhsh", None, |c| vupksh(c, 1, 3, true)),
            ("vupklsh", None, |c| vupksh(c, 1, 3, false)),
            ("vspltish", Some(0x1b), |c| vsplath(c, 1, ((0x1bi32 << 27) >> 27) as i16 as u16)),
            ("vspltisb", Some(0x05), |c| vsplatb(c, 1, 5)),
            ("vspltb", Some(13), |c| vsplte(c, 1, 3, 1, 13)),
            ("vsplth", Some(6), |c| vsplte(c, 1, 3, 2, 6)),
            ("vspltw", Some(3), |c| vsplte(c, 1, 3, 4, 3)),
        ];
        let edges = [0x0000u16, 0x7fff, 0x8000, 0xffff, 0x0010, 0x001f];
        for &(name, imm, native) in ops {
            let w = word(name, imm);
            for round in 0..200 {
                let mut c = Cpu::default();
                for r in 1..4 {
                    for k in 0..8 {
                        let v = if round % 4 == 0 { edges[(rnd() % 6) as usize] } else { rnd() as u16 };
                        c.vr[r][2 * k..2 * k + 2].copy_from_slice(&v.to_be_bytes());
                    }
                }
                let mut want = c.clone();
                let mut m = Memory::new();
                want.exec(&mut m, w).unwrap();
                native(&mut c);
                assert_eq!(c.vr[1], want.vr[1], "{name} (round {round})");
            }
        }
    }
}
