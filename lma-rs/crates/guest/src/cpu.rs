//! Reference interpreter for Xenon scalar code. The recompiler's output is checked against it.
//!
//! Conventions follow XenonRecomp/Xenia: 64-bit GPRs, 32-bit effective addresses,
//! record forms and carries computed on the low 32 bits (the 360 runs 32-bit code).

use crate::mem::Memory;
use ppc::bits;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Cpu {
    pub pc: u32,
    pub gpr: [u64; 32],
    pub fpr: [f64; 32],
    pub vr: Vec<crate::vmx::V>, // 128 VMX128 registers
    pub cr: [u8; 8], // each field: LT=8, GT=4, EQ=2, SO=1
    pub lr: u64,
    pub ctr: u64,
    pub ca: bool,
    pub so: bool,
    pub ov: bool,
    pub fpscr: u32,
    pub reserve: Option<u32>,
    /// The value `lwarx`/`ldarx` loaded: `stwcx.`/`stdcx.` store only if the word still holds it
    /// (a compare-exchange, so it holds across host threads too; #39). Not part of save states.
    #[serde(skip)]
    pub reserve_val: u64,
    pub insns: u64,
    /// Time base (mftb), in CLOCK_HZ ticks: the machine copies its global virtual clock here
    /// before running guest code, so it advances the same for interpreted and recompiled code.
    pub timebase: u64,
    /// Direct calls between recompiled functions (rec::call): clock at the end of the current
    /// slice and the native call depth. Not part of save states.
    #[serde(skip)]
    pub slice_end: u64,
    #[serde(skip)]
    pub call_depth: u32,
    /// Real-time pacing (#56): when set, the timebase inside a slice is `rt_base` plus the
    /// instructions consumed (`slice_len - budget left`) >> `rec::RT_SHIFT`, instead of `slice_end - left`.
    #[serde(skip)]
    pub rt: bool,
    #[serde(skip)]
    pub rt_base: u64,
    #[serde(skip)]
    pub slice_len: i64,
}

#[derive(Debug)]
pub enum Stop {
    /// Instruction the interpreter does not implement yet.
    Unimplemented { pc: u32, word: u32, mnemonic: Option<&'static str> },
    /// `sc`, `tw`/`td` trap and similar: handed to the caller.
    Trap { pc: u32, word: u32 },
}

impl Default for Cpu {
    fn default() -> Self {
        Self {
            pc: 0,
            gpr: [0; 32],
            fpr: [0.0; 32],
            vr: vec![[0; 16]; 128],
            cr: [0; 8],
            lr: 0,
            ctr: 0,
            ca: false,
            so: false,
            ov: false,
            fpscr: 0,
            reserve: None,
            reserve_val: 0,
            insns: 0,
            timebase: 0,
            slice_end: 0,
            call_depth: 0,
            rt: false,
            rt_base: 0,
            slice_len: 0,
        }
    }
}

const LT: u8 = 8;
const GT: u8 = 4;
const EQ: u8 = 2;

/// tw/td condition: TO bits are lt, gt, eq, <u, >u (MSB first).
pub(crate) fn trap_cond(to: u32, a: i64, b: i64) -> bool {
    (to & 16 != 0 && a < b)
        || (to & 8 != 0 && a > b)
        || (to & 4 != 0 && a == b)
        || (to & 2 != 0 && (a as u64) < (b as u64))
        || (to & 1 != 0 && (a as u64) > (b as u64))
}

fn rot32(v: u32, n: u32) -> u32 {
    v.rotate_left(n & 31)
}

/// MASK(mb, me) for 32-bit rotates, in the low word.
pub fn mask32(mb: u32, me: u32) -> u32 {
    let a = u32::MAX >> mb;
    let b = u32::MAX << (31 - me);
    if mb <= me { a & b } else { a | b }
}

pub fn mask64(mb: u32, me: u32) -> u64 {
    let a = u64::MAX >> mb;
    let b = u64::MAX << (63 - me);
    if mb <= me { a & b } else { a | b }
}

impl Cpu {
    pub fn set_cr_signed(&mut self, field: usize, a: i64, b: i64) {
        let v = if a < b { LT } else if a > b { GT } else { EQ };
        self.cr[field] = v | self.so as u8;
    }
    pub fn set_cr_unsigned(&mut self, field: usize, a: u64, b: u64) {
        let v = if a < b { LT } else if a > b { GT } else { EQ };
        self.cr[field] = v | self.so as u8;
    }
    pub fn rc(&mut self, w: u32, result: u64) {
        if w & 1 == 1 {
            self.set_cr_signed(0, result as u32 as i32 as i64, 0);
        }
    }
    pub fn cr_bit(&self, bi: u32) -> bool {
        self.cr[(bi / 4) as usize] & (8 >> (bi % 4)) != 0
    }
    pub fn set_cr_bit(&mut self, bi: u32, v: bool) {
        let f = &mut self.cr[(bi / 4) as usize];
        let m = 8 >> (bi % 4);
        if v { *f |= m } else { *f &= !m }
    }
    pub fn cr_word(&self) -> u32 {
        self.cr.iter().fold(0, |acc, &f| (acc << 4) | f as u32)
    }
    pub fn set_cr_word(&mut self, v: u32, fxm: u32) {
        for i in 0..8 {
            if fxm & (0x80 >> i) != 0 {
                self.cr[i] = ((v >> (28 - 4 * i)) & 0xF) as u8;
            }
        }
    }
    pub(crate) fn xer(&self) -> u64 {
        ((self.so as u64) << 31) | ((self.ov as u64) << 30) | ((self.ca as u64) << 29)
    }

    fn ea_d(&self, w: u32) -> u32 {
        let ra = bits(w, 11, 15) as usize;
        let d = (w & 0xFFFF) as i16 as i32 as u32;
        if ra == 0 { d } else { (self.gpr[ra] as u32).wrapping_add(d) }
    }
    fn ea_ds(&self, w: u32) -> u32 {
        let ra = bits(w, 11, 15) as usize;
        let d = (w & 0xFFFC) as i16 as i32 as u32;
        if ra == 0 { d } else { (self.gpr[ra] as u32).wrapping_add(d) }
    }
    fn ea_x(&self, w: u32) -> u32 {
        let ra = bits(w, 11, 15) as usize;
        let rb = bits(w, 16, 20) as usize;
        let base = if ra == 0 { 0 } else { self.gpr[ra] as u32 };
        base.wrapping_add(self.gpr[rb] as u32)
    }

    /// `v` rounded to an integer as FPSCR[RN] says: nearest (ties to even), toward zero,
    /// toward +infinity, toward -infinity.
    pub fn fp_round_int(&self, v: f64) -> f64 {
        match self.fpscr & 3 {
            0 => v.round_ties_even(),
            1 => v.trunc(),
            2 => v.ceil(),
            _ => v.floor(),
        }
    }

    /// fctiw (14), fctiwz (15), fctid (814), fctidz (815) of `b`, as the FPR bits they leave:
    /// fctiw and fctid round as FPSCR[RN] says (nearest even by default), the z forms toward
    /// zero; out of range saturates and NaN gives the most negative integer. Shared by the
    /// interpreter and the recompiled code (rec::fpx).
    pub fn fp_to_int(&self, xo10: u32, b: f64) -> f64 {
        let v = if matches!(xo10, 14 | 814) { self.fp_round_int(b) } else { b.trunc() };
        let bits = if matches!(xo10, 14 | 15) {
            (if v.is_nan() { i32::MIN } else { v.clamp(i32::MIN as f64, i32::MAX as f64) as i32 }) as u32 as u64
        } else {
            (if v.is_nan() { i64::MIN } else { v as i64 }) as u64
        };
        f64::from_bits(bits)
    }

    pub fn branch_cond(&mut self, bo: u32, bi: u32) -> bool {
        if bo & 4 == 0 {
            self.ctr = self.ctr.wrapping_sub(1);
        }
        let ctr_ok = bo & 4 != 0 || (((self.ctr as u32) != 0) ^ (bo & 2 != 0));
        let cond_ok = bo & 16 != 0 || (self.cr_bit(bi) == (bo & 8 != 0));
        ctr_ok && cond_ok
    }

    /// Execute one instruction at `pc`.
    pub fn step(&mut self, mem: &mut Memory) -> Result<(), Stop> {
        let w = mem.read32(self.pc);
        self.exec(mem, w)
    }

    /// Execute instruction word `w` as if it were at `self.pc` (used by recompiled code for
    /// instructions it does not translate natively). Advances `pc` like `step`.
    pub fn exec(&mut self, mem: &mut Memory, w: u32) -> Result<(), Stop> {
        let pc = self.pc;
        let mut next = pc.wrapping_add(4);
        let rd = bits(w, 6, 10) as usize;
        let ra = bits(w, 11, 15) as usize;
        let rb = bits(w, 16, 20) as usize;
        let simm = (w & 0xFFFF) as i16 as i64 as u64;
        let uimm = (w & 0xFFFF) as u64;
        let unimpl = || Stop::Unimplemented { pc, word: w, mnemonic: ppc::mnemonic(w) };
        let ra0 = |c: &Cpu| if ra == 0 { 0 } else { c.gpr[ra] };

        match ppc::opcd(w) {
            2 | 3 => {
                let to = bits(w, 6, 10);
                let (x, y) = if ppc::opcd(w) == 3 {
                    (self.gpr[ra] as u32 as i32 as i64, simm as i64)
                } else {
                    (self.gpr[ra] as i64, simm as i64)
                };
                if trap_cond(to, x, y) {
                    return Err(Stop::Trap { pc, word: w });
                }
            }
            7 => self.gpr[rd] = (self.gpr[ra] as i64).wrapping_mul(simm as i64) as u64,
            8 => {
                let a = self.gpr[ra] as u32;
                self.gpr[rd] = simm.wrapping_sub(self.gpr[ra]);
                self.ca = (simm as u32) >= a; // !a + imm + 1 carries
            }
            10 => {
                let f = bits(w, 6, 8) as usize;
                if bits(w, 10, 10) == 1 {
                    self.set_cr_unsigned(f, self.gpr[ra], uimm)
                } else {
                    self.set_cr_unsigned(f, self.gpr[ra] as u32 as u64, uimm)
                }
            }
            11 => {
                let f = bits(w, 6, 8) as usize;
                if bits(w, 10, 10) == 1 {
                    self.set_cr_signed(f, self.gpr[ra] as i64, simm as i64)
                } else {
                    self.set_cr_signed(f, self.gpr[ra] as u32 as i32 as i64, simm as i64)
                }
            }
            12 | 13 => {
                let a = self.gpr[ra] as u32;
                self.gpr[rd] = self.gpr[ra].wrapping_add(simm);
                self.ca = a.checked_add(simm as u32).is_none();
                if ppc::opcd(w) == 13 {
                    self.set_cr_signed(0, self.gpr[rd] as u32 as i32 as i64, 0);
                }
            }
            14 => self.gpr[rd] = ra0(self).wrapping_add(simm),
            15 => self.gpr[rd] = ra0(self).wrapping_add(simm << 16),
            16 => {
                if self.branch_cond(bits(w, 6, 10), bits(w, 11, 15)) {
                    let bd = (w & 0xFFFC) as i16 as i32 as u32;
                    next = if w & 2 != 0 { bd } else { pc.wrapping_add(bd) };
                }
                if w & 1 != 0 {
                    self.lr = pc.wrapping_add(4) as u64;
                }
            }
            17 => return Err(Stop::Trap { pc, word: w }),
            18 => {
                let li = ((w & 0x03FF_FFFC) << 6) as i32 >> 6;
                next = if w & 2 != 0 { li as u32 } else { pc.wrapping_add(li as u32) };
                if w & 1 != 0 {
                    self.lr = pc.wrapping_add(4) as u64;
                }
            }
            19 => match ppc::xo10(w) {
                0 => self.cr[bits(w, 6, 8) as usize] = self.cr[bits(w, 11, 13) as usize],
                16 | 528 => {
                    let target = if ppc::xo10(w) == 16 { self.lr as u32 & !3 } else { self.ctr as u32 & !3 };
                    let bo = bits(w, 6, 10);
                    let take = if ppc::xo10(w) == 528 {
                        bo & 16 != 0 || (self.cr_bit(bits(w, 11, 15)) == (bo & 8 != 0))
                    } else {
                        self.branch_cond(bo, bits(w, 11, 15))
                    };
                    if take {
                        next = target;
                    }
                    if w & 1 != 0 {
                        self.lr = pc.wrapping_add(4) as u64;
                    }
                }
                150 => {}
                x => {
                    let (a, b) = (self.cr_bit(bits(w, 11, 15)), self.cr_bit(bits(w, 16, 20)));
                    let v = match x {
                        33 => !(a | b),
                        129 => a & !b,
                        193 => a ^ b,
                        225 => !(a & b),
                        257 => a & b,
                        289 => !(a ^ b),
                        417 => a | !b,
                        449 => a | b,
                        _ => return Err(unimpl()),
                    };
                    self.set_cr_bit(bits(w, 6, 10), v);
                }
            },
            20 | 21 | 23 => {
                let sh = if ppc::opcd(w) == 23 { self.gpr[rb] as u32 & 31 } else { bits(w, 16, 20) };
                let m = mask32(bits(w, 21, 25), bits(w, 26, 30));
                let r = rot32(self.gpr[rd] as u32, sh) & m;
                let v = if ppc::opcd(w) == 20 { r | (self.gpr[ra] as u32 & !m) } else { r };
                self.gpr[ra] = v as u64;
                self.rc(w, v as u64);
            }
            24 => self.gpr[ra] = self.gpr[rd] | uimm,
            25 => self.gpr[ra] = self.gpr[rd] | (uimm << 16),
            26 => self.gpr[ra] = self.gpr[rd] ^ uimm,
            27 => self.gpr[ra] = self.gpr[rd] ^ (uimm << 16),
            28 | 29 => {
                let imm = if ppc::opcd(w) == 28 { uimm } else { uimm << 16 };
                self.gpr[ra] = self.gpr[rd] & imm;
                self.set_cr_signed(0, self.gpr[ra] as u32 as i32 as i64, 0);
            }
            30 => {
                let sh = bits(w, 16, 20) | (bits(w, 30, 30) << 5);
                let mb6 = bits(w, 21, 25) | (bits(w, 26, 26) << 5);
                let s = self.gpr[rd];
                let v = match bits(w, 27, 29) {
                    0 => s.rotate_left(sh) & mask64(mb6, 63),
                    1 => s.rotate_left(sh) & mask64(0, mb6),
                    2 => s.rotate_left(sh) & mask64(mb6, 63 - sh),
                    3 => {
                        let m = mask64(mb6, 63 - sh);
                        (s.rotate_left(sh) & m) | (self.gpr[ra] & !m)
                    }
                    4 => {
                        let n = self.gpr[rb] as u32 & 63;
                        if bits(w, 30, 30) == 0 { s.rotate_left(n) & mask64(mb6, 63) } else { s.rotate_left(n) & mask64(0, mb6) }
                    }
                    _ => return Err(unimpl()),
                };
                self.gpr[ra] = v;
                self.rc(w, v);
            }
            31 => return self.op31(mem, w, pc, next),
            32 => self.gpr[rd] = mem.read32(self.ea_d(w)) as u64,
            33 => {
                let ea = self.ea_d(w);
                self.gpr[rd] = mem.read32(ea) as u64;
                self.gpr[ra] = ea as u64;
            }
            34 => self.gpr[rd] = mem.read8(self.ea_d(w)) as u64,
            35 => {
                let ea = self.ea_d(w);
                self.gpr[rd] = mem.read8(ea) as u64;
                self.gpr[ra] = ea as u64;
            }
            36 => mem.write32(self.ea_d(w), self.gpr[rd] as u32),
            37 => {
                let ea = self.ea_d(w);
                mem.write32(ea, self.gpr[rd] as u32);
                self.gpr[ra] = ea as u64;
            }
            38 => mem.write8(self.ea_d(w), self.gpr[rd] as u8),
            39 => {
                let ea = self.ea_d(w);
                mem.write8(ea, self.gpr[rd] as u8);
                self.gpr[ra] = ea as u64;
            }
            40 => self.gpr[rd] = mem.read16(self.ea_d(w)) as u64,
            41 => {
                let ea = self.ea_d(w);
                self.gpr[rd] = mem.read16(ea) as u64;
                self.gpr[ra] = ea as u64;
            }
            42 => self.gpr[rd] = mem.read16(self.ea_d(w)) as i16 as i64 as u64,
            43 => {
                let ea = self.ea_d(w);
                self.gpr[rd] = mem.read16(ea) as i16 as i64 as u64;
                self.gpr[ra] = ea as u64;
            }
            44 => mem.write16(self.ea_d(w), self.gpr[rd] as u16),
            45 => {
                let ea = self.ea_d(w);
                mem.write16(ea, self.gpr[rd] as u16);
                self.gpr[ra] = ea as u64;
            }
            46 => {
                let mut ea = self.ea_d(w);
                for r in rd..32 {
                    self.gpr[r] = mem.read32(ea) as u64;
                    ea = ea.wrapping_add(4);
                }
            }
            47 => {
                let mut ea = self.ea_d(w);
                for r in rd..32 {
                    mem.write32(ea, self.gpr[r] as u32);
                    ea = ea.wrapping_add(4);
                }
            }
            48 | 49 => {
                let ea = self.ea_d(w);
                self.fpr[rd] = f32::from_bits(mem.read32(ea)) as f64;
                if ppc::opcd(w) == 49 {
                    self.gpr[ra] = ea as u64;
                }
            }
            50 | 51 => {
                let ea = self.ea_d(w);
                self.fpr[rd] = f64::from_bits(mem.read64(ea));
                if ppc::opcd(w) == 51 {
                    self.gpr[ra] = ea as u64;
                }
            }
            52 | 53 => {
                let ea = self.ea_d(w);
                mem.write32(ea, (self.fpr[rd] as f32).to_bits());
                if ppc::opcd(w) == 53 {
                    self.gpr[ra] = ea as u64;
                }
            }
            54 | 55 => {
                let ea = self.ea_d(w);
                mem.write64(ea, self.fpr[rd].to_bits());
                if ppc::opcd(w) == 55 {
                    self.gpr[ra] = ea as u64;
                }
            }
            58 => {
                let ea = self.ea_ds(w);
                match w & 3 {
                    0 => self.gpr[rd] = mem.read64(ea),
                    1 => {
                        self.gpr[rd] = mem.read64(ea);
                        self.gpr[ra] = ea as u64;
                    }
                    2 => self.gpr[rd] = mem.read32(ea) as i32 as i64 as u64,
                    _ => return Err(unimpl()),
                }
            }
            62 => {
                let ea = self.ea_ds(w);
                mem.write64(ea, self.gpr[rd]);
                if w & 3 == 1 {
                    self.gpr[ra] = ea as u64;
                }
            }
            59 | 63 => self.fp(w)?,
            4..=6 => {
                let name = ppc::lookup(w).ok_or_else(unimpl)?;
                self.vmx(mem, w, name)?;
            }
            _ => return Err(unimpl()),
        }
        self.pc = next;
        self.insns += 1;
        Ok(())
    }

    fn op31(&mut self, mem: &mut Memory, w: u32, pc: u32, next: u32) -> Result<(), Stop> {
        let rd = bits(w, 6, 10) as usize;
        let ra = bits(w, 11, 15) as usize;
        let rb = bits(w, 16, 20) as usize;
        let unimpl = || Stop::Unimplemented { pc, word: w, mnemonic: ppc::mnemonic(w) };
        let (a, b) = (self.gpr[ra], self.gpr[rb]);
        let (a32, b32) = (a as u32, b as u32);

        // XO-form arithmetic: rD = f(rA, rB)
        let xo9 = ppc::xo9(w);
        let arith = match xo9 {
            266 => Some(a.wrapping_add(b)),
            40 => Some(b.wrapping_sub(a)),
            104 => Some((a as i64).wrapping_neg() as u64),
            235 => Some((a32 as i32 as i64).wrapping_mul(b32 as i32 as i64) as u64),
            233 => Some((a as i64).wrapping_mul(b as i64) as u64),
            75 => Some((((a32 as i32 as i64) * (b32 as i32 as i64)) >> 32) as u64),
            11 => Some(((a32 as u64 * b32 as u64) >> 32) as u64),
            73 => Some((((a as i64 as i128) * (b as i64 as i128)) >> 64) as u64),
            9 => Some((((a as u128) * (b as u128)) >> 64) as u64),
            491 => Some(if b32 == 0 { 0 } else { (a32 as i32).wrapping_div(b32 as i32) as u32 as u64 }),
            459 => Some(if b32 == 0 { 0 } else { (a32 / b32) as u64 }),
            489 => Some(if b == 0 { 0 } else { (a as i64).wrapping_div(b as i64) as u64 }),
            457 => Some(if b == 0 { 0 } else { a / b }),
            10 => {
                self.ca = a32.checked_add(b32).is_none();
                Some(a.wrapping_add(b))
            }
            8 => {
                self.ca = b32 >= a32;
                Some(b.wrapping_sub(a))
            }
            138 => {
                let (s1, c1) = a32.overflowing_add(b32);
                let (_, c2) = s1.overflowing_add(self.ca as u32);
                let r = a.wrapping_add(b).wrapping_add(self.ca as u64);
                self.ca = c1 | c2;
                Some(r)
            }
            136 => {
                let (s1, c1) = (!a32).overflowing_add(b32);
                let (_, c2) = s1.overflowing_add(self.ca as u32);
                let r = (!a).wrapping_add(b).wrapping_add(self.ca as u64);
                self.ca = c1 | c2;
                Some(r)
            }
            202 => {
                let r = a.wrapping_add(self.ca as u64);
                self.ca = a32.checked_add(self.ca as u32).is_none();
                Some(r)
            }
            200 => {
                let r = (!a).wrapping_add(self.ca as u64);
                self.ca = (!a32).checked_add(self.ca as u32).is_none();
                Some(r)
            }
            234 => {
                let r = a.wrapping_add(self.ca as u64).wrapping_sub(1);
                self.ca = a32 != 0 || self.ca;
                Some(r)
            }
            232 => {
                let r = (!a).wrapping_add(self.ca as u64).wrapping_sub(1);
                self.ca = !a32 != 0 || self.ca;
                Some(r)
            }
            _ => None,
        };
        if let Some(r) = arith {
            self.gpr[rd] = r;
            self.rc(w, r);
            self.pc = next;
            self.insns += 1;
            return Ok(());
        }

        let s = self.gpr[rd]; // rS for logical/store forms
        match ppc::xo10(w) {
            0 | 32 => {
                let f = bits(w, 6, 8) as usize;
                let l64 = bits(w, 10, 10) == 1;
                if ppc::xo10(w) == 0 {
                    if l64 { self.set_cr_signed(f, a as i64, b as i64) } else { self.set_cr_signed(f, a32 as i32 as i64, b32 as i32 as i64) }
                } else if l64 {
                    self.set_cr_unsigned(f, a, b)
                } else {
                    self.set_cr_unsigned(f, a32 as u64, b32 as u64)
                }
            }
            4 | 68 => {
                let to = bits(w, 6, 10);
                let (x, y) = if ppc::xo10(w) == 4 {
                    (a32 as i32 as i64, b32 as i32 as i64)
                } else {
                    (a as i64, b as i64)
                };
                if trap_cond(to, x, y) {
                    return Err(Stop::Trap { pc, word: w });
                }
            }
            19 => self.gpr[rd] = self.cr_word() as u64,
            144 => self.set_cr_word(s as u32, bits(w, 12, 19)),
            339 | 371 => {
                let spr = bits(w, 16, 20) << 5 | bits(w, 11, 15);
                self.gpr[rd] = match spr {
                    1 => self.xer(),
                    8 => self.lr,
                    9 => self.ctr,
                    268 | 269 => self.timebase >> if spr == 269 { 32 } else { 0 },
                    _ => 0,
                };
            }
            467 => {
                let spr = bits(w, 16, 20) << 5 | bits(w, 11, 15);
                match spr {
                    1 => {
                        self.so = s >> 31 & 1 == 1;
                        self.ov = s >> 30 & 1 == 1;
                        self.ca = s >> 29 & 1 == 1;
                    }
                    8 => self.lr = s,
                    9 => self.ctr = s,
                    _ => {}
                }
            }
            83 => self.gpr[rd] = 0x8000, // MSR: EE set
            146 | 178 => {}
            28 => { self.gpr[ra] = s & b; self.rc(w, self.gpr[ra]) }
            60 => { self.gpr[ra] = s & !b; self.rc(w, self.gpr[ra]) }
            124 => { self.gpr[ra] = !(s | b); self.rc(w, self.gpr[ra]) }
            284 => { self.gpr[ra] = !(s ^ b); self.rc(w, self.gpr[ra]) }
            316 => { self.gpr[ra] = s ^ b; self.rc(w, self.gpr[ra]) }
            412 => { self.gpr[ra] = s | !b; self.rc(w, self.gpr[ra]) }
            444 => { self.gpr[ra] = s | b; self.rc(w, self.gpr[ra]) }
            476 => { self.gpr[ra] = !(s & b); self.rc(w, self.gpr[ra]) }
            26 => { self.gpr[ra] = (s as u32).leading_zeros() as u64; self.rc(w, self.gpr[ra]) }
            58 => { self.gpr[ra] = s.leading_zeros() as u64; self.rc(w, self.gpr[ra]) }
            922 => { self.gpr[ra] = s as u16 as i16 as i64 as u64; self.rc(w, self.gpr[ra]) }
            954 => { self.gpr[ra] = s as u8 as i8 as i64 as u64; self.rc(w, self.gpr[ra]) }
            986 => { self.gpr[ra] = s as u32 as i32 as i64 as u64; self.rc(w, self.gpr[ra]) }
            24 => {
                let n = b32 & 63;
                self.gpr[ra] = if n > 31 { 0 } else { ((s as u32) << n) as u64 };
                self.rc(w, self.gpr[ra]);
            }
            536 => {
                let n = b32 & 63;
                self.gpr[ra] = if n > 31 { 0 } else { ((s as u32) >> n) as u64 };
                self.rc(w, self.gpr[ra]);
            }
            27 => { let n = b32 & 127; self.gpr[ra] = if n > 63 { 0 } else { s << n }; self.rc(w, self.gpr[ra]) }
            539 => { let n = b32 & 127; self.gpr[ra] = if n > 63 { 0 } else { s >> n }; self.rc(w, self.gpr[ra]) }
            792 | 824 => {
                let n = if ppc::xo10(w) == 824 { bits(w, 16, 20) } else { b32 & 63 };
                let v = s as u32 as i32;
                let (r, lost) = if n > 31 {
                    ((v >> 31) as i64, v < 0)
                } else {
                    let lost = n > 0 && (v as u32) << (32 - n) != 0;
                    ((v >> n) as i64, lost)
                };
                self.ca = v < 0 && lost;
                self.gpr[ra] = r as u64;
                self.rc(w, self.gpr[ra]);
            }
            794 | 826 | 827 => {
                let n = if ppc::xo10(w) == 794 { b32 & 127 } else { bits(w, 16, 20) | (bits(w, 30, 30) << 5) };
                let v = s as i64;
                let (r, lost) = if n > 63 {
                    (v >> 63, v < 0)
                } else {
                    (v >> n, n > 0 && (v as u64) << (64 - n) != 0)
                };
                self.ca = v < 0 && lost;
                self.gpr[ra] = r as u64;
                self.rc(w, self.gpr[ra]);
            }
            // indexed loads/stores
            23 => self.gpr[rd] = mem.read32(self.ea_x(w)) as u64,
            55 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read32(ea) as u64; self.gpr[ra] = ea as u64 }
            21 => self.gpr[rd] = mem.read64(self.ea_x(w)),
            53 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read64(ea); self.gpr[ra] = ea as u64 }
            87 => self.gpr[rd] = mem.read8(self.ea_x(w)) as u64,
            119 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read8(ea) as u64; self.gpr[ra] = ea as u64 }
            279 => self.gpr[rd] = mem.read16(self.ea_x(w)) as u64,
            311 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read16(ea) as u64; self.gpr[ra] = ea as u64 }
            343 => self.gpr[rd] = mem.read16(self.ea_x(w)) as i16 as i64 as u64,
            375 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read16(ea) as i16 as i64 as u64; self.gpr[ra] = ea as u64 }
            341 => self.gpr[rd] = mem.read32(self.ea_x(w)) as i32 as i64 as u64,
            373 => { let ea = self.ea_x(w); self.gpr[rd] = mem.read32(ea) as i32 as i64 as u64; self.gpr[ra] = ea as u64 }
            534 => self.gpr[rd] = mem.read32(self.ea_x(w)).swap_bytes() as u64,
            790 => self.gpr[rd] = mem.read16(self.ea_x(w)).swap_bytes() as u64,
            532 => self.gpr[rd] = mem.read64(self.ea_x(w)).swap_bytes(),
            151 => mem.write32(self.ea_x(w), s as u32),
            183 => { let ea = self.ea_x(w); mem.write32(ea, s as u32); self.gpr[ra] = ea as u64 }
            149 => mem.write64(self.ea_x(w), s),
            181 => { let ea = self.ea_x(w); mem.write64(ea, s); self.gpr[ra] = ea as u64 }
            215 => mem.write8(self.ea_x(w), s as u8),
            247 => { let ea = self.ea_x(w); mem.write8(ea, s as u8); self.gpr[ra] = ea as u64 }
            407 => mem.write16(self.ea_x(w), s as u16),
            439 => { let ea = self.ea_x(w); mem.write16(ea, s as u16); self.gpr[ra] = ea as u64 }
            662 => {
                let ea = self.ea_x(w);
                crate::mmio::store_le(ea, s as u32); // command registers (XMA kicks)
                mem.write32(ea, (s as u32).swap_bytes())
            }
            918 => mem.write16(self.ea_x(w), (s as u16).swap_bytes()),
            660 => mem.write64(self.ea_x(w), s.swap_bytes()),
            535 | 567 => {
                let ea = self.ea_x(w);
                self.fpr[rd] = f32::from_bits(mem.read32(ea)) as f64;
                if ppc::xo10(w) == 567 { self.gpr[ra] = ea as u64 }
            }
            599 | 631 => {
                let ea = self.ea_x(w);
                self.fpr[rd] = f64::from_bits(mem.read64(ea));
                if ppc::xo10(w) == 631 { self.gpr[ra] = ea as u64 }
            }
            663 | 695 => {
                let ea = self.ea_x(w);
                mem.write32(ea, (self.fpr[rd] as f32).to_bits());
                if ppc::xo10(w) == 695 { self.gpr[ra] = ea as u64 }
            }
            727 | 759 => {
                let ea = self.ea_x(w);
                mem.write64(ea, self.fpr[rd].to_bits());
                if ppc::xo10(w) == 759 { self.gpr[ra] = ea as u64 }
            }
            983 => mem.write32(self.ea_x(w), self.fpr[rd].to_bits() as u32),
            // atomics: the reservation is the address and the value loaded; the conditional store
            // is a compare-exchange against that value (#39), as rec::larx / rec::stcx
            20 => { let ea = self.ea_x(w); self.reserve = Some(ea); self.gpr[rd] = mem.read32(ea) as u64; self.reserve_val = self.gpr[rd] }
            84 => { let ea = self.ea_x(w); self.reserve = Some(ea); self.gpr[rd] = mem.read64(ea); self.reserve_val = self.gpr[rd] }
            150 | 214 => {
                let ea = self.ea_x(w);
                let ok = self.reserve.take() == Some(ea)
                    && if ppc::xo10(w) == 150 { mem.cas32(ea, self.reserve_val as u32, s as u32) } else { mem.cas64(ea, self.reserve_val, s) };
                self.cr[0] = if ok { EQ } else { 0 } | self.so as u8;
            }
            1014 => {
                // dcbz128 (RT = 1) clears the Xenon's whole 128-byte cache line, dcbz 32 bytes (#204)
                let n = if bits(w, 6, 10) == 1 { 128 } else { 32 };
                let ea = self.ea_x(w) & !(n - 1);
                mem.write(ea, &[0; 128][..n as usize]);
            }
            54 | 86 | 246 | 278 | 982 => {} // cache hints
            598 => crate::rec::barrier(rd & 3 == 0), // sync (L = 0) / lwsync (#39)
            854 => crate::rec::barrier(false), // eieio
            _ => match ppc::lookup(w) {
                Some(name) if name.starts_with("lv") || name.starts_with("stv") => self.vmx(mem, w, name)?,
                _ => return Err(unimpl()),
            },
        }
        self.pc = next;
        self.insns += 1;
        Ok(())
    }

    fn fp(&mut self, w: u32) -> Result<(), Stop> {
        let d = bits(w, 6, 10) as usize;
        let (a, b, c) = (
            self.fpr[bits(w, 11, 15) as usize],
            self.fpr[bits(w, 16, 20) as usize],
            self.fpr[bits(w, 21, 25) as usize],
        );
        let single = ppc::opcd(w) == 59;
        let round = |v: f64| if single { v as f32 as f64 } else { v };
        let r = match bits(w, 26, 30) {
            18 => Some(a / b),
            20 => Some(a - b),
            21 => Some(a + b),
            22 => Some(b.sqrt()),
            23 if !single => Some(if a >= 0.0 { c } else { b }),
            24 => Some(1.0 / b),
            25 => Some(a * c),
            26 => Some(1.0 / b.sqrt()),
            28 => Some(a.mul_add(c, -b)),
            29 => Some(a.mul_add(c, b)),
            30 => Some(-a.mul_add(c, -b)),
            31 => Some(-a.mul_add(c, b)),
            _ => None,
        };
        if let Some(v) = r {
            self.fpr[d] = round(v);
            return Ok(());
        }
        let pc = self.pc;
        match ppc::xo10(w) {
            0 | 32 => {
                let f = bits(w, 6, 8) as usize;
                self.cr[f] = if a.is_nan() || b.is_nan() { 1 } else if a < b { LT } else if a > b { GT } else { EQ };
            }
            12 => self.fpr[d] = b as f32 as f64,
            x @ (14 | 15 | 814 | 815) => self.fpr[d] = self.fp_to_int(x, b),
            846 => self.fpr[d] = b.to_bits() as i64 as f64,
            40 => self.fpr[d] = -b,
            72 => self.fpr[d] = b,
            136 => self.fpr[d] = -b.abs(),
            264 => self.fpr[d] = b.abs(),
            583 => self.fpr[d] = f64::from_bits(self.fpscr as u64),
            711 => self.fpscr = b.to_bits() as u32,
            38 | 70 | 134 | 64 => {}
            _ => return Err(Stop::Unimplemented { pc, word: w, mnemonic: ppc::mnemonic(w) }),
        }
        Ok(())
    }
}
