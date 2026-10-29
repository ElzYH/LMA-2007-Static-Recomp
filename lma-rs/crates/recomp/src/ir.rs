//! The recompiler's IR (#145, M1): every guest instruction as an `Op` with the registers it reads
//! and writes, so the emitter can keep registers in host locals and load or store them only where
//! liveness says. The register file stays `guest::cpu::Cpu`; the IR adds no state, it describes
//! what each instruction touches. `defs`/`uses` are checked against the interpreter by a property
//! test (`ops_touch_what_the_interpreter_touches`): a register outside `defs` never changes, and
//! the result never depends on a register outside `uses`.

/// Bits `from..=to` of `w`, numbered from the most significant (PowerPC order).
fn bits(w: u32, from: u32, to: u32) -> u32 {
    (w >> (31 - to)) & ((1 << (to - from + 1)) - 1)
}

/// A set of guest registers (and the memory as one item, for ordering), as a bitset.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Regs(pub u128);

impl Regs {
    pub const NONE: Regs = Regs(0);
    pub const LR: u32 = 72;
    pub const CTR: u32 = 73;
    pub const CA: u32 = 74;
    /// so and ov together (the interpreter sets both from the same overflow).
    pub const SO: u32 = 75;
    pub const FPSCR: u32 = 76;
    pub const RESERVE: u32 = 77;
    /// All 128 vector registers as one item (M1 keeps VMX in helpers).
    pub const VR: u32 = 78;
    /// Guest memory (a load reads it, a store writes it).
    pub const MEM: u32 = 79;
    pub const COUNT: u32 = 80;

    pub const fn gpr(i: u32) -> u32 {
        i
    }
    pub const fn fpr(i: u32) -> u32 {
        32 + i
    }
    pub const fn cr(f: u32) -> u32 {
        64 + f
    }
    pub fn of(items: &[u32]) -> Regs {
        let mut r = Regs(0);
        for &i in items {
            r.0 |= 1 << i;
        }
        r
    }
    pub fn all() -> Regs {
        Regs((1u128 << Self::COUNT) - 1)
    }
    pub fn has(self, i: u32) -> bool {
        self.0 & (1 << i) != 0
    }
    pub fn with(self, i: u32) -> Regs {
        Regs(self.0 | (1 << i))
    }
    pub fn union(self, o: Regs) -> Regs {
        Regs(self.0 | o.0)
    }
    pub fn minus(self, o: Regs) -> Regs {
        Regs(self.0 & !o.0)
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn iter(self) -> impl Iterator<Item = u32> {
        (0..Self::COUNT).filter(move |&i| self.has(i))
    }
    pub fn name(i: u32) -> String {
        match i {
            0..=31 => format!("r{i}"),
            32..=63 => format!("f{}", i - 32),
            64..=71 => format!("cr{}", i - 64),
            Self::LR => "lr".into(),
            Self::CTR => "ctr".into(),
            Self::CA => "ca".into(),
            Self::SO => "so".into(),
            Self::FPSCR => "fpscr".into(),
            Self::RESERVE => "reserve".into(),
            Self::VR => "vr*".into(),
            Self::MEM => "mem".into(),
            _ => format!("?{i}"),
        }
    }
}

impl std::fmt::Debug for Regs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{{}}}", self.iter().map(Regs::name).collect::<Vec<_>>().join(" "))
    }
}

/// How the emitter treats an instruction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Integer arithmetic, logic, compares, moves, loads and stores with a native expression over
    /// locals (M1's set).
    Native,
    /// Runs through one of today's `rec` helpers on `Cpu` (FP, VMX, 64-bit rotates, OE forms):
    /// the emitter stores the locals in `uses` before it and reloads those in `defs` after.
    Helper,
    /// The interpreter (`interp(c,m,pc,w)`): anything may be read or written.
    Interp,
    /// Ends the block: a branch, call, return or trap.
    Branch,
}

#[derive(Clone, Debug)]
pub struct Op {
    pub pc: u32,
    pub word: u32,
    pub kind: Kind,
    pub defs: Regs,
    pub uses: Regs,
}

/// `d`, `a`, `b` fields, and `ra` as a use only when it is not the "zero" form (`ra == 0` reads 0).
fn fields(w: u32) -> (u32, u32, u32) {
    (bits(w, 6, 10), bits(w, 11, 15), bits(w, 16, 20))
}

fn ra0(a: u32) -> Regs {
    if a == 0 { Regs::NONE } else { Regs::of(&[Regs::gpr(a)]) }
}

/// Decode `w` at `pc` into an `Op`. Unknown words are `Interp` with everything in both sets.
pub fn decode(w: u32, pc: u32) -> Op {
    let mut op = decode_raw(w, pc);
    // every compare and record form copies XER[SO] into the field's SO bit (not the cr moves and
    // cr logic of opcode 19, nor mtcrf, which move whole fields)
    let cr_fields = Regs(0xFFu128 << 64);
    if !op.defs.minus(cr_fields).union(op.defs).minus(op.defs.minus(cr_fields)).is_empty() && w >> 26 != 19 && !(w >> 26 == 31 && bits(w, 21, 30) == 144) {
        op.uses = op.uses.with(Regs::SO);
    }
    op
}

fn decode_raw(w: u32, pc: u32) -> Op {
    let (d, a, b) = fields(w);
    let rc = w & 1 == 1;
    let g = Regs::gpr;
    let cr0 = |r: Regs| if rc { r.with(Regs::cr(0)) } else { r };
    let op = |kind: Kind, defs: Regs, uses: Regs| Op { pc, word: w, kind, defs, uses };
    let native = |defs: Regs, uses: Regs| op(Kind::Native, defs, uses);
    let helper = |defs: Regs, uses: Regs| op(Kind::Helper, defs, uses);
    let interp = || op(Kind::Interp, Regs::all(), Regs::all());
    let mem_load = |defs: Regs, uses: Regs| native(defs, uses.with(Regs::MEM));
    let mem_store = |defs: Regs, uses: Regs| native(defs.with(Regs::MEM), uses);
    match w >> 26 {
        // branches: `bc` reads the condition field and may decrement ctr; `b`; link writes lr
        16 => {
            let (bo, bi) = (bits(w, 6, 10), bits(w, 11, 15));
            let mut uses = Regs::NONE;
            let mut defs = Regs::NONE;
            if bo & 0x10 == 0 {
                uses = uses.with(Regs::cr(bi / 4));
            }
            if bo & 4 == 0 {
                uses = uses.with(Regs::CTR);
                defs = defs.with(Regs::CTR);
            }
            if rc {
                defs = defs.with(Regs::LR);
            }
            op(Kind::Branch, defs, uses)
        }
        18 => op(Kind::Branch, if rc { Regs::of(&[Regs::LR]) } else { Regs::NONE }, Regs::NONE),
        19 => match bits(w, 21, 30) {
            16 | 528 => {
                let (bo, bi) = (bits(w, 6, 10), bits(w, 11, 15));
                let mut uses = Regs::of(&[if bits(w, 21, 30) == 16 { Regs::LR } else { Regs::CTR }]);
                let mut defs = Regs::NONE;
                if bo & 0x10 == 0 {
                    uses = uses.with(Regs::cr(bi / 4));
                }
                if bits(w, 21, 30) == 16 && bo & 4 == 0 {
                    uses = uses.with(Regs::CTR);
                    defs = defs.with(Regs::CTR);
                }
                if rc {
                    defs = defs.with(Regs::LR);
                }
                op(Kind::Branch, defs, uses)
            }
            // mcrf and the cr logical ops: whole fields in the IR
            0 => helper(Regs::of(&[Regs::cr(bits(w, 6, 8))]), Regs::of(&[Regs::cr(bits(w, 11, 13))])),
            // one bit of the destination field changes, the other three stay: a read-modify-write
            33 | 129 | 193 | 225 | 257 | 289 | 417 | 449 => {
                helper(Regs::of(&[Regs::cr(d / 4)]), Regs::of(&[Regs::cr(d / 4), Regs::cr(a / 4), Regs::cr(b / 4)]))
            }
            150 => helper(Regs::NONE, Regs::NONE), // isync
            _ => interp(),
        },
        // traps: a conditional trap reads ra (and rb); taken, it leaves the function
        2 | 3 => op(Kind::Branch, Regs::NONE, Regs::of(&[g(a)])),
        17 => op(Kind::Branch, Regs::NONE, Regs::NONE),
        14 | 15 => native(Regs::of(&[g(d)]), ra0(a)),
        24 | 25 | 26 | 27 => native(Regs::of(&[g(a)]), Regs::of(&[g(d)])),
        28 | 29 => native(Regs::of(&[g(a), Regs::cr(0)]), Regs::of(&[g(d)])),
        7 => native(Regs::of(&[g(d)]), Regs::of(&[g(a)])),
        8 => native(Regs::of(&[g(d), Regs::CA]), Regs::of(&[g(a)])),
        12 => native(Regs::of(&[g(d), Regs::CA]), Regs::of(&[g(a)])),
        13 => native(Regs::of(&[g(d), Regs::CA, Regs::cr(0)]), Regs::of(&[g(a)])),
        10 | 11 => native(Regs::of(&[Regs::cr(bits(w, 6, 8))]), Regs::of(&[g(a)])),
        20 => native(cr0(Regs::of(&[g(a)])), Regs::of(&[g(d), g(a)])),
        21 => native(cr0(Regs::of(&[g(a)])), Regs::of(&[g(d)])),
        23 => native(cr0(Regs::of(&[g(a)])), Regs::of(&[g(d), g(b)])),
        // 64-bit rotates (MD-form): today's `md30` helper
        // rotate doubleword: MD forms read rS, MDS (xo >= 4) rS and rB; rldimi (MD xo 3) inserts into rA,
        // so it reads rA too (#145: missing, the day's number formatter 0x821664d0 built its divide-by-
        // power-of-ten constant on a stale r11)
        30 => helper(
            cr0(Regs::of(&[g(a)])),
            match bits(w, 27, 29) {
                3 => Regs::of(&[g(d), g(a)]),
                x if x >= 4 => Regs::of(&[g(d), g(b)]),
                _ => Regs::of(&[g(d)]),
            },
        ),
        32 | 34 | 40 | 42 => mem_load(Regs::of(&[g(d)]), ra0(a)),
        33 | 35 | 41 | 43 => mem_load(Regs::of(&[g(d), g(a)]), Regs::of(&[g(a)])),
        36 | 38 | 44 => mem_store(Regs::NONE, ra0(a).with(g(d))),
        37 | 39 | 45 => mem_store(Regs::of(&[g(a)]), Regs::of(&[g(a), g(d)])),
        // FP loads and stores (M2): native over f64 locals
        48 | 50 => mem_load(Regs::of(&[Regs::fpr(d)]), ra0(a)),
        49 | 51 => mem_load(Regs::of(&[Regs::fpr(d), g(a)]), Regs::of(&[g(a)])),
        52 | 54 => mem_store(Regs::NONE, ra0(a).with(Regs::fpr(d))),
        53 | 55 => mem_store(Regs::of(&[g(a)]), Regs::of(&[g(a), Regs::fpr(d)])),
        58 => match w & 3 {
            0 | 2 => mem_load(Regs::of(&[g(d)]), ra0(a)),
            1 => mem_load(Regs::of(&[g(d), g(a)]), Regs::of(&[g(a)])),
            _ => interp(),
        },
        62 => match w & 3 {
            0 => mem_store(Regs::NONE, ra0(a).with(g(d))),
            1 => mem_store(Regs::of(&[g(a)]), Regs::of(&[g(a), g(d)])),
            _ => interp(),
        },
        // FP arithmetic: fpscr in and out (rounding mode, status), cr1 with rc
        59 | 63 => {
            let xo5 = bits(w, 26, 30);
            let (fd, fa, fb, fc) = (Regs::fpr(d), Regs::fpr(a), Regs::fpr(b), Regs::fpr(bits(w, 21, 25)));
            let defs = if rc { Regs::of(&[fd, Regs::FPSCR, Regs::cr(1)]) } else { Regs::of(&[fd, Regs::FPSCR]) };
            match xo5 {
                18 | 20 | 21 | 22 | 24 | 26 | 28..=31 | 23 | 25 => {
                    let uses = match xo5 {
                        25 => Regs::of(&[fa, fc, Regs::FPSCR]),       // fmul
                        28..=31 | 23 => Regs::of(&[fa, fb, fc, Regs::FPSCR]), // fmadd family, fsel
                        22 | 24 | 26 => Regs::of(&[fb, Regs::FPSCR]), // fsqrt, fres, frsqrte
                        _ => Regs::of(&[fa, fb, Regs::FPSCR]),
                    };
                    // M2: the arithmetic is native without the cr1 record (fsel exists only as
                    // op 63; the interpreter runs op 59's xo 23 as an X-form). The interpreter
                    // leaves cr1 as it was on a record form, so cr1 is a use there too.
                    if rc {
                        helper(defs, uses.with(Regs::cr(1)))
                    } else if xo5 == 23 && w >> 26 == 59 {
                        helper(defs, uses)
                    } else {
                        native(defs, uses)
                    }
                }
                _ => match bits(w, 21, 30) {
                    0 | 32 => helper(Regs::of(&[Regs::cr(bits(w, 6, 8)), Regs::FPSCR]), Regs::of(&[fa, fb, Regs::FPSCR])), // fcmpu/fcmpo
                    // fneg, fmr, fnabs, fabs, frsp: native without the record (M2)
                    40 | 72 | 136 | 264 | 12 if !rc && w >> 26 == 63 => native(defs, Regs::of(&[fb, Regs::FPSCR])),
                    40 | 72 | 136 | 264 | 12 | 14 | 15 | 814 | 815 | 846 => {
                        let uses = Regs::of(&[fb, Regs::FPSCR]);
                        helper(defs, if rc { uses.with(Regs::cr(1)) } else { uses })
                    }
                    // mffs / mtfsf / mtfsfi / mtfsb*: the status register
                    583 => helper(Regs::of(&[fd]), Regs::of(&[Regs::FPSCR])),
                    711 => helper(Regs::of(&[Regs::FPSCR]), Regs::of(&[fb])),
                    38 | 70 | 134 | 64 => helper(Regs::of(&[Regs::FPSCR, Regs::cr(bits(w, 6, 8))]), Regs::of(&[Regs::FPSCR])),
                    _ => interp(),
                },
            }
        }
        // VMX: all vector registers as one item (M1). Opcodes 4, 5 and 6 carry the VMX128 loads
        // and stores as well as the arithmetic, and a load or store addresses memory through
        // ra (0 for the zero form) and rb: every form is given both and memory, conservatively
        // (an integer register is never written by a vector op, so the cost is a store of a
        // dirty local before the helper, nothing else)
        4 | 5 | 6 => helper(Regs::of(&[Regs::VR, Regs::MEM]), ra0(a).union(Regs::of(&[Regs::VR, g(b), Regs::MEM]))),
        31 => decode31(w, pc),
        _ => interp(),
    }
}

fn decode31(w: u32, pc: u32) -> Op {
    let (d, a, b) = fields(w);
    let rc = w & 1 == 1;
    let g = Regs::gpr;
    let cr0 = |r: Regs| if rc { r.with(Regs::cr(0)) } else { r };
    let op = |kind: Kind, defs: Regs, uses: Regs| Op { pc, word: w, kind, defs, uses };
    let native = |defs: Regs, uses: Regs| op(Kind::Native, defs, uses);
    let helper = |defs: Regs, uses: Regs| op(Kind::Helper, defs, uses);
    let interp = || op(Kind::Interp, Regs::all(), Regs::all());
    let ea = ra0(a).with(g(b)); // indexed address
    let xo = bits(w, 21, 30);
    let xo9 = bits(w, 22, 30);
    let oe = bits(w, 21, 21) == 1;
    if guest::rec::XO_ARITH.contains(&xo9) {
        // XO-form arithmetic (the `xo` helper): reads/writes ca and so/ov by form
        let (uses_ca, defs_ca) = match xo9 {
            10 | 8 => (false, true),                                     // addc, subfc
            138 | 136 | 234 | 232 | 202 | 200 => (true, true),           // adde, subfe, addme, subfme, addze, subfze
            _ => (false, false),
        };
        let two = !matches!(xo9, 104 | 234 | 232 | 202 | 200); // neg/addme/subfme/addze/subfze read ra only
        let mut uses = if two { Regs::of(&[g(a), g(b)]) } else { Regs::of(&[g(a)]) };
        let mut defs = cr0(Regs::of(&[g(d)]));
        if uses_ca {
            uses = uses.with(Regs::CA);
        }
        if defs_ca {
            defs = defs.with(Regs::CA);
        }
        if oe {
            defs = defs.with(Regs::SO);
            uses = uses.with(Regs::SO);
        }
        if rc {
            uses = uses.with(Regs::SO); // cr0's SO bit copies so
        }
        // only the plain add/subf/mullw/neg (no OE) are native in M1
        let kind = if !oe && matches!(xo9, 266 | 40 | 235 | 104) { Kind::Native } else { Kind::Helper };
        return op(kind, defs, uses);
    }
    let f = Regs::cr(bits(w, 6, 8));
    match xo {
        0 | 32 => native(Regs::of(&[f]), Regs::of(&[g(a), g(b)])),
        339 => match (b << 5) | a {
            8 => native(Regs::of(&[g(d)]), Regs::of(&[Regs::LR])),
            9 => native(Regs::of(&[g(d)]), Regs::of(&[Regs::CTR])),
            1 => helper(Regs::of(&[g(d)]), Regs::of(&[Regs::CA, Regs::SO])), // mfxer
            _ => interp(),
        },
        467 => match (b << 5) | a {
            8 => native(Regs::of(&[Regs::LR]), Regs::of(&[g(d)])),
            9 => native(Regs::of(&[Regs::CTR]), Regs::of(&[g(d)])),
            1 => helper(Regs::of(&[Regs::CA, Regs::SO]), Regs::of(&[g(d)])), // mtxer
            _ => interp(),
        },
        19 => helper(Regs::of(&[g(d)]), Regs(0xFFu128 << 64)),        // mfcr
        144 => helper(Regs(0xFFu128 << 64), Regs::of(&[g(d)]).union(Regs(0xFFu128 << 64))), // mtcrf (fields by mask; whole set for M1)
        23 | 87 | 279 | 343 | 21 | 341 | 790 | 534 | 532 => native(Regs::of(&[g(d)]), ea.with(Regs::MEM)),
        55 | 119 | 311 | 375 | 53 | 373 => native(Regs::of(&[g(d), g(a)]), Regs::of(&[g(a), g(b), Regs::MEM])), // update forms
        151 | 215 | 407 | 149 | 918 | 662 | 660 => native(Regs::of(&[Regs::MEM]), ea.with(g(d))),
        183 | 247 | 439 | 181 => native(Regs::of(&[Regs::MEM, g(a)]), Regs::of(&[g(a), g(b), g(d)])),
        // lwarx / stwcx.: the reservation
        20 | 84 => helper(Regs::of(&[g(d), Regs::RESERVE]), ea.with(Regs::MEM)),
        150 | 214 => helper(Regs::of(&[Regs::MEM, Regs::RESERVE, Regs::cr(0)]), ea.with(g(d)).with(Regs::RESERVE).with(Regs::SO)),
        // logic and shifts: rS is the `d` field
        444 | 28 | 60 | 316 | 124 | 476 | 284 | 412 | 24 | 536 | 27 | 539 => native(cr0(Regs::of(&[g(a)])), Regs::of(&[g(d), g(b)])),
        922 | 954 | 986 | 26 | 58 => native(cr0(Regs::of(&[g(a)])), Regs::of(&[g(d)])),
        792 | 794 => native(cr0(Regs::of(&[g(a), Regs::CA])), Regs::of(&[g(d), g(b)])), // sraw, srad
        824 | 826 | 827 => native(cr0(Regs::of(&[g(a), Regs::CA])), Regs::of(&[g(d)])), // srawi, sradi
        // cache hints, barriers, eieio, dcbz/dcbz128 (writes memory)
        54 | 86 | 246 | 278 | 598 | 854 | 982 => native(Regs::NONE, Regs::NONE),
        1014 => helper(Regs::of(&[Regs::MEM]), ea),
        // FP indexed loads/stores (fpx_mem)
        535 | 567 | 599 | 631 => helper(Regs::of(&[Regs::fpr(d)]).union(if xo & 32 != 0 { Regs::of(&[g(a)]) } else { Regs::NONE }), ea.with(Regs::MEM)),
        663 | 695 | 727 | 759 | 983 => helper(Regs::of(&[Regs::MEM]).union(if xo & 32 != 0 && xo != 983 { Regs::of(&[g(a)]) } else { Regs::NONE }), ea.with(Regs::fpr(d))),
        // VMX indexed loads/stores and lvsl/lvsr
        6 | 38 | 7 | 39 | 103 | 359 | 519 | 775 | 1031 | 1287 | 1543 | 1799 => helper(Regs::of(&[Regs::VR]), ea.with(Regs::MEM)),
        135 | 391 | 647 | 903 | 1159 | 1415 | 1671 | 1927 => helper(Regs::of(&[Regs::MEM]), ea.with(Regs::VR)),
        // mftb, mfmsr, tw, dcbt variants: via the interpreter
        _ => interp(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use guest::cpu::Cpu;
    use guest::mem::Memory;

    /// rldimi inserts into rA, so the helper reads rA as well as rS (#145: 0x794b000e,
    /// `rldimi r11,r10,...` in the number formatter 0x821664d0, ran on a stale r11).
    #[test]
    fn rldimi_reads_ra() {
        let op = decode(0x794b_000e, 0x8216_6630);
        assert!(op.uses.has(Regs::gpr(10)) && op.uses.has(Regs::gpr(11)), "rldimi r11,r10: uses r10 and r11");
        assert!(op.defs.has(Regs::gpr(11)));
        // rldicl (MD xo 0) reads rS only
        let op = decode(0x796a_0fe0, 0x8216_6644);
        assert!(op.uses.has(Regs::gpr(11)) && !op.uses.has(Regs::gpr(10)), "rldicl r10,r11: uses r11 only");
    }

    /// A scratch area every random register points into, so loads and stores land.
    pub const BASE: u32 = 0x4000_0000;
    pub const SPAN: u32 = 0x2_0000;

    pub struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    pub fn random_cpu(rng: &mut Rng) -> Cpu {
        let mut c = Cpu::default();
        for i in 0..32 {
            // valid guest addresses inside the scratch span, 8-aligned, with room for ±32 KB offsets
            c.gpr[i] = (BASE + 0x1_0000 + (rng.next() as u32 % 0x8000) * 8) as u64;
            // some registers carry arbitrary 64-bit values instead
            if rng.next() % 3 == 0 {
                c.gpr[i] = rng.next();
            }
            c.fpr[i] = f64::from_bits(rng.next());
        }
        for f in 0..8 {
            c.cr[f] = (rng.next() & 15) as u8;
        }
        c.lr = rng.next() & 0xFFFF_FFFC;
        c.ctr = rng.next();
        c.ca = rng.next() & 1 == 1;
        c.so = rng.next() & 1 == 1;
        c.ov = rng.next() & 1 == 1;
        c.fpscr = (rng.next() & 3) as u32;
        c
    }

    /// (value of every register item, including the scratch memory's bytes as "MEM")
    fn snapshot(c: &Cpu, m: &mut Memory) -> Vec<(u32, Vec<u8>)> {
        let mut v = Vec::new();
        for i in 0..32 {
            v.push((Regs::gpr(i), c.gpr[i as usize].to_le_bytes().to_vec()));
            v.push((Regs::fpr(i), c.fpr[i as usize].to_bits().to_le_bytes().to_vec()));
        }
        for f in 0..8 {
            v.push((Regs::cr(f), vec![c.cr[f as usize]]));
        }
        v.push((Regs::LR, c.lr.to_le_bytes().to_vec()));
        v.push((Regs::CTR, c.ctr.to_le_bytes().to_vec()));
        v.push((Regs::CA, vec![c.ca as u8]));
        v.push((Regs::SO, vec![c.so as u8, c.ov as u8]));
        v.push((Regs::FPSCR, c.fpscr.to_le_bytes().to_vec()));
        v.push((Regs::RESERVE, c.reserve.map_or(vec![0], |r| r.to_le_bytes().to_vec())));
        let mut mem = vec![0u8; SPAN as usize];
        m.read(BASE, &mut mem);
        v.push((Regs::MEM, mem));
        v
    }

    fn run(c: &mut Cpu, m: &mut Memory, w: u32) -> Result<(), guest::cpu::Stop> {
        c.pc = 0x8200_1000;
        c.exec(m, w)
    }

    /// Words to test: every form `decode` classifies, with random fields.
    pub fn words(rng: &mut Rng) -> Vec<u32> {
        let mut out = Vec::new();
        let r = |rng: &mut Rng, n: u32| rng.next() as u32 % n;
        for _ in 0..40 {
            let (d, a, b) = (r(rng, 32), r(rng, 32), r(rng, 32));
            let rc = r(rng, 2);
            let imm = rng.next() as u32 & 0xFFFF;
            let regs = (d << 21) | (a << 16) | (b << 11);
            for opcd in [7u32, 8, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 32, 34, 36, 38, 40, 42, 44, 48, 50, 52, 54] {
                out.push((opcd << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8));
            }
            // update forms need ra != 0 and ra != rd
            if a != 0 && a != d {
                for opcd in [33u32, 35, 37, 39, 41, 43, 45, 49, 51, 53, 55] {
                    out.push((opcd << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8));
                }
                out.push((58 << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8) | 1);
                out.push((62 << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8) | 1);
            }
            out.push((58 << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8));
            out.push((58 << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8) | 2);
            out.push((62 << 26) | (d << 21) | (a << 16) | (imm & 0xFFF8));
            // FP A-forms (ops 59 / 63) and op 63's register moves, with and without the record (M2)
            let fc = r(rng, 32);
            for opcd in [59u32, 63] {
                for xo5 in [18u32, 20, 21, 22, 23, 24, 25, 26, 28, 29, 30, 31] {
                    out.push((opcd << 26) | regs | (fc << 6) | (xo5 << 1) | rc);
                }
            }
            for xo in [12u32, 40, 72, 136, 264] {
                out.push((63 << 26) | (d << 21) | (b << 11) | (xo << 1) | rc);
            }
            // rlwinm / rlwimi / rlwnm
            let (sh, mb, me) = (r(rng, 32), r(rng, 32), r(rng, 32));
            for opcd in [20u32, 21] {
                out.push((opcd << 26) | regs | (sh << 11) | (mb << 6) | (me << 1) | rc);
            }
            out.push((23 << 26) | regs | (mb << 6) | (me << 1) | rc);
            // opcode 31 forms
            for xo in [0u32, 32, 444, 28, 60, 316, 124, 476, 284, 412, 24, 536, 27, 539, 922, 954, 986, 26, 58, 792, 794, 824, 23, 87, 279, 343, 21, 341, 151, 215, 407, 149, 790, 918] {
                out.push((31 << 26) | regs | (xo << 1) | rc);
            }
            for xo in [826u32, 827] {
                out.push((31 << 26) | regs | (xo << 1) | rc);
            }
            if a != 0 && a != d {
                for xo in [55u32, 119, 311, 375, 53, 373, 183, 247, 439, 181] {
                    out.push((31 << 26) | regs | (xo << 1) | rc);
                }
            }
            for xo9 in guest::rec::XO_ARITH {
                for oe in [0u32, 1] {
                    out.push((31 << 26) | regs | (oe << 10) | (xo9 << 1) | rc);
                }
            }
            for spr in [8u32, 9] {
                out.push((31 << 26) | (d << 21) | (spr << 16) | (339 << 1));
                out.push((31 << 26) | (d << 21) | (spr << 16) | (467 << 1));
            }
            // branches (not taken into a call here: exec only sets lr / ctr / next)
            let (bo, bi) = (r(rng, 32), r(rng, 32));
            out.push((16 << 26) | (bo << 21) | (bi << 16) | 0x100 | rc);
            out.push((18 << 26) | 0x100 | rc);
            out.push((19 << 26) | (bo << 21) | (bi << 16) | (16 << 1) | rc);
            out.push((19 << 26) | ((bo | 4) << 21) | (bi << 16) | (528 << 1) | rc);
            out.push((19 << 26) | (d << 21) | (a << 16) | (b << 11) | (257 << 1));
            out.push((19 << 26) | (d << 21) | (a << 16) | (b << 11) | (449 << 1));
            // VMX128 stores and loads in the opcode 4 space (stvx128 0x1c3, lvx128 0x0c3) and the
            // standard stvx / lvx (31/231, 31/103): they address memory through ra and rb
            out.push((4 << 26) | (d << 21) | (a << 16) | (b << 11) | 0x1c3);
            out.push((4 << 26) | (d << 21) | (a << 16) | (b << 11) | 0x0c3);
            out.push((31 << 26) | (d << 21) | (a << 16) | (b << 11) | (231 << 1));
            out.push((31 << 26) | (d << 21) | (a << 16) | (b << 11) | (103 << 1));
            // 64-bit rotates
            out.push((30 << 26) | regs | (sh << 11) | (mb << 6) | rc);
            out.push((30 << 26) | regs | (sh << 11) | (mb << 6) | (1 << 2) | rc);
        }
        out
    }

    /// For every op kind with exact sets: nothing outside `defs` changes, and a different value in
    /// anything outside `uses` gives the same `defs`.
    #[test]
    fn ops_touch_what_the_interpreter_touches() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut tested = 0;
        for w in words(&mut rng) {
            let op = decode(w, 0x8200_1000);
            if op.kind == Kind::Interp {
                continue;
            }
            let mut m = Memory::new();
            let mut fill = vec![0u8; SPAN as usize];
            for x in &mut fill {
                *x = rng.next() as u8;
            }
            m.write(BASE, &fill);
            let c0 = random_cpu(&mut rng);
            let before = snapshot(&c0, &mut m);
            let mut c1 = c0.clone();
            if run(&mut c1, &mut m, w).is_err() {
                continue; // a trap or an unimplemented form: not this test's business
            }
            let after = snapshot(&c1, &mut m);
            for ((i, b), (_, a)) in before.iter().zip(&after) {
                assert!(op.defs.has(*i) || b == a, "{w:#010x} ({}) changed {} outside defs {:?}", ppc::mnemonic(w).unwrap_or("?"), Regs::name(*i), op.defs);
            }
            // the same op from a state that differs only outside `uses` must leave the same defs
            let mut m2 = Memory::new();
            m2.write(BASE, &fill);
            let mut c2 = random_cpu(&mut rng);
            for i in 0..32 {
                if op.uses.has(Regs::gpr(i)) {
                    c2.gpr[i as usize] = c0.gpr[i as usize];
                }
                if op.uses.has(Regs::fpr(i)) {
                    c2.fpr[i as usize] = c0.fpr[i as usize];
                }
            }
            for f in 0..8 {
                if op.uses.has(Regs::cr(f)) {
                    c2.cr[f as usize] = c0.cr[f as usize];
                }
            }
            if op.uses.has(Regs::LR) { c2.lr = c0.lr; }
            if op.uses.has(Regs::CTR) { c2.ctr = c0.ctr; }
            if op.uses.has(Regs::CA) { c2.ca = c0.ca; }
            if op.uses.has(Regs::SO) { (c2.so, c2.ov) = (c0.so, c0.ov); }
            if op.uses.has(Regs::FPSCR) { c2.fpscr = c0.fpscr; }
            // memory is the same in both (the op's own reads must match too)
            if run(&mut c2, &mut m2, w).is_err() {
                continue;
            }
            let after2 = snapshot(&c2, &mut m2);
            for ((i, a1), (_, a2)) in after.iter().zip(&after2) {
                if op.defs.has(*i) && *i != Regs::MEM {
                    assert!(a1 == a2, "{w:#010x} ({}) {} depends on something outside uses {:?}", ppc::mnemonic(w).unwrap_or("?"), Regs::name(*i), op.uses);
                }
            }
            if op.defs.has(Regs::MEM) {
                // a store writes the same bytes at the same place: the memory deltas agree
                let (m1, m2v) = (&after.last().unwrap().1, &after2.last().unwrap().1);
                let b0 = &before.last().unwrap().1;
                let d1: Vec<(usize, u8)> = m1.iter().enumerate().zip(b0).filter(|((_, x), y)| x != y).map(|((i, &x), _)| (i, x)).collect();
                let d2: Vec<(usize, u8)> = m2v.iter().enumerate().zip(b0).filter(|((_, x), y)| x != y).map(|((i, &x), _)| (i, x)).collect();
                assert!(d1 == d2, "{w:#010x} ({}) stored differently from a state equal on uses {:?}", ppc::mnemonic(w).unwrap_or("?"), op.uses);
            }
            tested += 1;
        }
        assert!(tested > 2000, "{tested} ops tested");
    }

    #[test]
    fn regs_names_and_sets() {
        let r = Regs::of(&[Regs::gpr(3), Regs::cr(0), Regs::CA]);
        assert_eq!(format!("{r:?}"), "{r3 cr0 ca}");
        assert!(r.with(Regs::LR).has(Regs::LR) && !r.has(Regs::LR));
        assert_eq!(r.minus(Regs::of(&[Regs::CA])), Regs::of(&[Regs::gpr(3), Regs::cr(0)]));
        assert_eq!(Regs::all().iter().count(), Regs::COUNT as usize);
    }
}

/// What the emitter generates for a `Native` op, run directly on a `Cpu` (the semantic model the
/// emitted text follows one to one; the property test `native_eval_matches_the_interpreter` pins
/// it to `Cpu::exec`). Returns false for an op that is not native.
pub fn eval_native(op: &Op, c: &mut guest::cpu::Cpu, m: &mut guest::mem::Memory) -> bool {
    use guest::native as n;
    let w = op.word;
    let (d, a, b) = fields(w);
    let (d, a, b) = (d as usize, a as usize, b as usize);
    let rc = w & 1 == 1;
    let simm = (w & 0xFFFF) as i16 as i64;
    let uimm = (w & 0xFFFF) as u64;
    let ra0 = |c: &guest::cpu::Cpu| if a == 0 { 0 } else { c.gpr[a] };
    macro_rules! set {
        ($c:expr, $r:expr, $v:expr) => {{
            let v = $v;
            $c.gpr[$r] = v;
            if rc {
                $c.cr[0] = n::cr0(v, $c.so);
            }
        }};
    }
    match w >> 26 {
        14 => c.gpr[d] = ra0(c).wrapping_add(simm as u64),
        15 => c.gpr[d] = ra0(c).wrapping_add((simm << 16) as u64),
        24 => c.gpr[a] = c.gpr[d] | uimm,
        25 => c.gpr[a] = c.gpr[d] | (uimm << 16),
        26 => c.gpr[a] = c.gpr[d] ^ uimm,
        27 => c.gpr[a] = c.gpr[d] ^ (uimm << 16),
        28 => {
            c.gpr[a] = c.gpr[d] & uimm;
            c.cr[0] = n::cr0(c.gpr[a], c.so);
        }
        29 => {
            c.gpr[a] = c.gpr[d] & (uimm << 16);
            c.cr[0] = n::cr0(c.gpr[a], c.so);
        }
        7 => c.gpr[d] = n::mulli(c.gpr[a], simm),
        8 => (c.gpr[d], c.ca) = n::subfic(c.gpr[a], simm as u64),
        12 => (c.gpr[d], c.ca) = n::addic(c.gpr[a], simm as u64),
        13 => {
            (c.gpr[d], c.ca) = n::addic(c.gpr[a], simm as u64);
            c.cr[0] = n::cr0(c.gpr[d], c.so);
        }
        10 => c.cr[bits(w, 6, 8) as usize] = n::cmp_unsigned(bits(w, 10, 10) == 1, c.gpr[a], uimm, c.so),
        11 => c.cr[bits(w, 6, 8) as usize] = n::cmp_signed(bits(w, 10, 10) == 1, c.gpr[a], simm as u64, c.so),
        20 | 21 | 23 => {
            let (sh, mb, me) = (bits(w, 16, 20), bits(w, 21, 25), bits(w, 26, 30));
            let mask = guest::cpu::mask32(mb, me);
            let v = match w >> 26 {
                20 => n::rlwimi(c.gpr[d], c.gpr[a], sh, mask),
                21 => n::rlwinm(c.gpr[d], sh, mask),
                _ => n::rlwnm(c.gpr[d], c.gpr[b], mask),
            };
            set!(c, a, v);
        }
        32 => c.gpr[d] = m.read32(n::ea(ra0(c), simm as i32)) as u64,
        34 => c.gpr[d] = m.read8(n::ea(ra0(c), simm as i32)) as u64,
        40 => c.gpr[d] = m.read16(n::ea(ra0(c), simm as i32)) as u64,
        42 => c.gpr[d] = n::lha(m.read16(n::ea(ra0(c), simm as i32))),
        33 | 35 | 41 | 43 => {
            let e = n::ea(c.gpr[a], simm as i32);
            c.gpr[d] = match w >> 26 {
                33 => m.read32(e) as u64,
                35 => m.read8(e) as u64,
                41 => m.read16(e) as u64,
                _ => n::lha(m.read16(e)),
            };
            c.gpr[a] = e as u64;
        }
        48 => c.fpr[d] = f32::from_bits(m.read32(n::ea(ra0(c), simm as i32))) as f64,
        50 => c.fpr[d] = f64::from_bits(m.read64(n::ea(ra0(c), simm as i32))),
        52 => m.write32(n::ea(ra0(c), simm as i32), (c.fpr[d] as f32).to_bits()),
        54 => m.write64(n::ea(ra0(c), simm as i32), c.fpr[d].to_bits()),
        49 | 51 | 53 | 55 => {
            let e = n::ea(c.gpr[a], simm as i32);
            match w >> 26 {
                49 => c.fpr[d] = f32::from_bits(m.read32(e)) as f64,
                51 => c.fpr[d] = f64::from_bits(m.read64(e)),
                53 => m.write32(e, (c.fpr[d] as f32).to_bits()),
                _ => m.write64(e, c.fpr[d].to_bits()),
            }
            c.gpr[a] = e as u64;
        }
        59 | 63 => {
            let (fa, fb, fc) = (c.fpr[a], c.fpr[b], c.fpr[bits(w, 21, 25) as usize]);
            let v = match bits(w, 26, 30) {
                18 => fa / fb,
                20 => fa - fb,
                21 => fa + fb,
                22 => fb.sqrt(),
                23 => if fa >= 0.0 { fc } else { fb },
                24 => 1.0 / fb,
                25 => fa * fc,
                26 => 1.0 / fb.sqrt(),
                28 => fa.mul_add(fc, -fb),
                29 => fa.mul_add(fc, fb),
                30 => -fa.mul_add(fc, -fb),
                31 => -fa.mul_add(fc, fb),
                _ => match bits(w, 21, 30) {
                    12 => fb as f32 as f64,
                    40 => -fb,
                    72 => fb,
                    136 => -fb.abs(),
                    264 => fb.abs(),
                    _ => return false,
                },
            };
            c.fpr[d] = if w >> 26 == 59 { v as f32 as f64 } else { v };
        }
        36 => m.write32(n::ea(ra0(c), simm as i32), c.gpr[d] as u32),
        38 => m.write8(n::ea(ra0(c), simm as i32), c.gpr[d] as u8),
        44 => m.write16(n::ea(ra0(c), simm as i32), c.gpr[d] as u16),
        37 | 39 | 45 => {
            let e = n::ea(c.gpr[a], simm as i32);
            match w >> 26 {
                37 => m.write32(e, c.gpr[d] as u32),
                39 => m.write8(e, c.gpr[d] as u8),
                _ => m.write16(e, c.gpr[d] as u16),
            }
            c.gpr[a] = e as u64;
        }
        58 => {
            let ds = (w & 0xFFFC) as i16 as i32;
            match w & 3 {
                0 => c.gpr[d] = m.read64(n::ea(ra0(c), ds)),
                2 => c.gpr[d] = n::lwa(m.read32(n::ea(ra0(c), ds))),
                _ => {
                    let e = n::ea(c.gpr[a], ds);
                    c.gpr[d] = m.read64(e);
                    c.gpr[a] = e as u64;
                }
            }
        }
        62 => {
            let ds = (w & 0xFFFC) as i16 as i32;
            match w & 3 {
                0 => m.write64(n::ea(ra0(c), ds), c.gpr[d]),
                _ => {
                    let e = n::ea(c.gpr[a], ds);
                    m.write64(e, c.gpr[d]);
                    c.gpr[a] = e as u64;
                }
            }
        }
        31 => {
            let xo = bits(w, 21, 30);
            let xo9 = bits(w, 22, 30);
            if guest::rec::XO_ARITH.contains(&xo9) {
                let (x, y) = (c.gpr[a], c.gpr[b]);
                let v = match xo9 {
                    266 => x.wrapping_add(y),
                    40 => y.wrapping_sub(x),
                    235 => n::mullw(x, y),
                    104 => n::neg(x),
                    _ => return false,
                };
                set!(c, d, v);
                return true;
            }
            let ea = n::ea_x(ra0(c), c.gpr[b]);
            let s = c.gpr[d];
            match xo {
                0 => c.cr[bits(w, 6, 8) as usize] = n::cmp_signed(bits(w, 10, 10) == 1, c.gpr[a], c.gpr[b], c.so),
                32 => c.cr[bits(w, 6, 8) as usize] = n::cmp_unsigned(bits(w, 10, 10) == 1, c.gpr[a], c.gpr[b], c.so),
                339 => match (b << 5) | a {
                    8 => c.gpr[d] = c.lr,
                    9 => c.gpr[d] = c.ctr,
                    _ => return false,
                },
                467 => match (b << 5) | a {
                    8 => c.lr = s,
                    9 => c.ctr = s,
                    _ => return false,
                },
                444 => set!(c, a, s | c.gpr[b]),
                28 => set!(c, a, s & c.gpr[b]),
                60 => set!(c, a, s & !c.gpr[b]),
                316 => set!(c, a, s ^ c.gpr[b]),
                124 => set!(c, a, !(s | c.gpr[b])),
                476 => set!(c, a, !(s & c.gpr[b])),
                284 => set!(c, a, !(s ^ c.gpr[b])),
                412 => set!(c, a, s | !c.gpr[b]),
                24 => set!(c, a, n::slw(s, c.gpr[b])),
                536 => set!(c, a, n::srw(s, c.gpr[b])),
                27 => set!(c, a, n::sld(s, c.gpr[b])),
                539 => set!(c, a, n::srd(s, c.gpr[b])),
                922 => set!(c, a, n::extsh(s)),
                954 => set!(c, a, n::extsb(s)),
                986 => set!(c, a, n::extsw(s)),
                26 => set!(c, a, n::cntlzw(s)),
                58 => set!(c, a, n::cntlzd(s)),
                792 => {
                    let (v, ca) = n::sraw(s, c.gpr[b] as u32);
                    c.ca = ca;
                    set!(c, a, v);
                }
                824 => {
                    let (v, ca) = n::sraw(s, bits(w, 16, 20));
                    c.ca = ca;
                    set!(c, a, v);
                }
                794 => {
                    let (v, ca) = n::srad(s, c.gpr[b] as u32);
                    c.ca = ca;
                    set!(c, a, v);
                }
                826 | 827 => {
                    let (v, ca) = n::srad(s, bits(w, 16, 20) | (bits(w, 30, 30) << 5));
                    c.ca = ca;
                    set!(c, a, v);
                }
                23 => c.gpr[d] = m.read32(ea) as u64,
                87 => c.gpr[d] = m.read8(ea) as u64,
                279 => c.gpr[d] = m.read16(ea) as u64,
                343 => c.gpr[d] = n::lha(m.read16(ea)),
                21 => c.gpr[d] = m.read64(ea),
                341 => c.gpr[d] = n::lwa(m.read32(ea)),
                790 => c.gpr[d] = m.read16(ea).swap_bytes() as u64,
                534 => c.gpr[d] = m.read32(ea).swap_bytes() as u64,
                532 => c.gpr[d] = m.read64(ea).swap_bytes(),
                55 | 119 | 311 | 375 | 53 | 373 => {
                    let e = n::ea_x(c.gpr[a], c.gpr[b]);
                    c.gpr[d] = match xo {
                        55 => m.read32(e) as u64,
                        119 => m.read8(e) as u64,
                        311 => m.read16(e) as u64,
                        375 => n::lha(m.read16(e)),
                        53 => m.read64(e),
                        _ => n::lwa(m.read32(e)),
                    };
                    c.gpr[a] = e as u64;
                }
                151 => m.write32(ea, s as u32),
                215 => m.write8(ea, s as u8),
                407 => m.write16(ea, s as u16),
                149 => m.write64(ea, s),
                918 => m.write16(ea, (s as u16).swap_bytes()),
                662 => m.write32(ea, (s as u32).swap_bytes()),
                660 => m.write64(ea, s.swap_bytes()),
                183 | 247 | 439 | 181 => {
                    let e = n::ea_x(c.gpr[a], c.gpr[b]);
                    match xo {
                        183 => m.write32(e, s as u32),
                        247 => m.write8(e, s as u8),
                        439 => m.write16(e, s as u16),
                        _ => m.write64(e, s),
                    }
                    c.gpr[a] = e as u64;
                }
                54 | 86 | 246 | 278 | 598 | 854 | 982 => {}
                _ => return false,
            }
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod native_tests {
    use super::tests::{random_cpu, words, Rng, BASE, SPAN};
    use super::*;
    use guest::mem::Memory;

    /// Every `Native` op, run by `eval_native` on random states, leaves exactly the interpreter's
    /// registers and memory.
    #[test]
    fn native_eval_matches_the_interpreter() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let (mut tested, mut skipped) = (0, 0);
        for w in words(&mut rng) {
            let op = decode(w, 0x8200_1000);
            if op.kind != Kind::Native {
                continue;
            }
            let mut fill = vec![0u8; SPAN as usize];
            for x in &mut fill {
                *x = rng.next() as u8;
            }
            let c0 = random_cpu(&mut rng);
            let (mut m1, mut m2) = (Memory::new(), Memory::new());
            m1.write(BASE, &fill);
            m2.write(BASE, &fill);
            let mut c1 = c0.clone();
            c1.pc = 0x8200_1000;
            if c1.exec(&mut m1, w).is_err() {
                continue;
            }
            let mut c2 = c0.clone();
            if !eval_native(&op, &mut c2, &mut m2) {
                skipped += 1;
                continue;
            }
            let name = ppc::mnemonic(w).unwrap_or("?");
            assert_eq!(c1.gpr, c2.gpr, "{w:#010x} ({name}) gpr");
            assert_eq!(c1.cr, c2.cr, "{w:#010x} ({name}) cr");
            assert_eq!((c1.ca, c1.so, c1.ov, c1.lr, c1.ctr), (c2.ca, c2.so, c2.ov, c2.lr, c2.ctr), "{w:#010x} ({name}) flags/lr/ctr");
            let bits_of = |c: &guest::cpu::Cpu| c.fpr.map(f64::to_bits);
            assert_eq!(bits_of(&c1), bits_of(&c2), "{w:#010x} ({name}) fpr");
            let (mut b1, mut b2) = (vec![0u8; SPAN as usize], vec![0u8; SPAN as usize]);
            m1.read(BASE, &mut b1);
            m2.read(BASE, &mut b2);
            assert!(b1 == b2, "{w:#010x} ({name}) memory");
            tested += 1;
        }
        assert!(tested > 1500 && skipped == 0, "{tested} native ops tested, {skipped} without an eval");
    }
}
