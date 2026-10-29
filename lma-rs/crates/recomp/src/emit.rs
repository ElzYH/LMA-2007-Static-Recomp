//! The IR emitter (#145, M1): a block's instructions as Rust over host locals. Registers a block
//! reads are loaded once at its entry, native ops compute on the locals, and every exit (the
//! budget checks between fall-through leaders, the tail) stores back what the block wrote, so
//! `Cpu` is exact wherever anything else can look at it (the dispatcher, a call, a helper, the
//! interpreter, a save state). Helpers keep running on `Cpu` as today: the locals they read are
//! stored before and the ones they write reloaded after. Text conventions are the current
//! emitter's (`*b-=n;`, `return Ok(pc)`, helper calls, the tail expression), so the superblock
//! budget rewrite, the leaf inlining and `budget_in_local` apply unchanged.

use crate::ir::{Kind, Op, Regs};
use std::fmt::Write as _;

/// One straight-line piece of a superblock: its ops (with a helper's call text), their
/// instruction count and what follows: a budget check returning to the next leader, nothing (the
/// next piece starts an entry-only leader), or the block's tail.
pub struct Seg<'a> {
    pub ops: &'a [(Op, String)],
    pub count: u32,
    pub next: Next,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Next {
    Check(u32),
    Plain,
    Tail,
}

/// Registers that live in locals: integer and FP registers (M2), the cr fields, lr, ctr, ca and so.
fn local(i: u32) -> bool {
    i < 64 || (64..72).contains(&i) || matches!(i, Regs::LR | Regs::CTR | Regs::CA | Regs::SO)
}

fn name(i: u32) -> String {
    match i {
        0..=31 => format!("r{i}"),
        32..=63 => format!("f{}", i - 32),
        64..=71 => format!("cr{}", i - 64),
        Regs::LR => "lr".into(),
        Regs::CTR => "ctr".into(),
        Regs::CA => "ca".into(),
        Regs::SO => "so".into(),
        _ => unreachable!("not a local"),
    }
}

fn load(i: u32) -> String {
    let n = name(i);
    match i {
        0..=31 => format!("{n}=c.gpr[{i}];"),
        32..=63 => format!("{n}=c.fpr[{}];", i - 32),
        64..=71 => format!("{n}=c.cr[{}];", i - 64),
        Regs::LR => format!("{n}=c.lr;"),
        Regs::CTR => format!("{n}=c.ctr;"),
        Regs::CA => format!("{n}=c.ca;"),
        Regs::SO => format!("{n}=c.so;"),
        _ => unreachable!(),
    }
}

fn store(i: u32) -> String {
    let n = name(i);
    match i {
        0..=31 => format!("c.gpr[{i}]={n};"),
        32..=63 => format!("c.fpr[{}]={n};", i - 32),
        64..=71 => format!("c.cr[{}]={n};", i - 64),
        Regs::LR => format!("c.lr={n};"),
        Regs::CTR => format!("c.ctr={n};"),
        Regs::CA => format!("c.ca={n};"),
        Regs::SO => format!("c.so={n};"),
        _ => unreachable!(),
    }
}

fn ty(i: u32) -> &'static str {
    match i {
        0..=31 | Regs::LR | Regs::CTR => "u64",
        32..=63 => "f64",
        64..=71 => "u8",
        _ => "bool",
    }
}

struct State {
    /// Locals declared for this block.
    locals: Regs,
    /// Locals assigned since they were last stored to `Cpu`.
    dirty: Regs,
    /// Locals that hold `Cpu`'s current value (loaded or stored, not written natively since).
    out: String,
}

impl State {
    fn sync(&mut self, regs: Regs) {
        for i in regs.iter().filter(|&i| self.dirty.has(i)) {
            self.out.push_str(&store(i));
        }
        self.dirty = self.dirty.minus(regs);
    }
    fn sync_all(&mut self) {
        let d = self.dirty;
        self.sync(d);
    }
    fn reload(&mut self, regs: Regs) {
        for i in regs.iter().filter(|&i| self.locals.has(i)) {
            self.out.push_str(&load(i));
        }
        self.dirty = self.dirty.minus(regs);
    }
}

/// The block's body: loads, the segments with their budget exits, the tail (a next-pc expression
/// as `translate_branch` makes it, or `0xNEXT`), with the locals stored before every exit.
pub fn block(segs: &[Seg], tail: &str) -> String {
    // locals: everything a native op reads or writes (helpers and the interpreter use Cpu)
    let mut locals = Regs::NONE;
    let (mut gen, mut kill) = (Regs::NONE, Regs::NONE);
    for (op, _) in segs.iter().flat_map(|s| s.ops) {
        if op.kind == Kind::Native {
            let (u, d) = (Regs(op.uses.0 & local_mask()), Regs(op.defs.0 & local_mask()));
            locals = locals.union(u).union(d);
            gen = gen.union(u.minus(kill));
            kill = kill.union(d);
        }
    }
    let mut st = State { locals, dirty: Regs::NONE, out: String::new() };
    for i in locals.iter() {
        if gen.has(i) {
            let _ = write!(st.out, "let mut {}", load(i));
        } else {
            let _ = write!(st.out, "let mut {}:{};", name(i), ty(i));
        }
    }
    for seg in segs {
        for (op, text) in seg.ops {
            match op.kind {
                Kind::Native => {
                    st.out.push_str(&native(op));
                    st.dirty = st.dirty.union(Regs(op.defs.0 & locals.0));
                }
                Kind::Helper => {
                    st.sync(op.uses);
                    st.out.push_str(text);
                    st.reload(op.defs);
                }
                Kind::Interp | Kind::Branch => {
                    // a trap or anything the interpreter runs: everything may change
                    st.sync_all();
                    if text.is_empty() {
                        let _ = write!(st.out, "interp(c,m,{:#x},{:#x})?;", op.pc, op.word);
                    } else {
                        st.out.push_str(text);
                    }
                    let all = st.locals;
                    st.reload(all);
                }
            }
        }
        match seg.next {
            Next::Check(exit) => {
                let _ = write!(st.out, "*b-={};if *b<=0{{", seg.count);
                for i in st.dirty.iter() {
                    st.out.push_str(&store(i));
                }
                let _ = write!(st.out, "return Ok({exit:#x})}}");
            }
            Next::Plain => {
                let _ = write!(st.out, "*b-={};", seg.count);
            }
            Next::Tail => {
                let _ = write!(st.out, "*b-={};", seg.count);
                st.sync_all();
                st.out.push_str(tail);
            }
        }
    }
    // guest memory through the base read once per block (#145): see `Memory::base_ptr`
    let mut out = st.out;
    for f in ["read8", "read16", "read32", "read64", "write8", "write16", "write32", "write64"] {
        out = out.replace(&format!("m.{f}("), &format!("m.{f}_b(mb,"));
    }
    if out.contains("_b(mb,") {
        out.insert_str(0, "let mb=m.base_ptr();");
    }
    out
}

fn local_mask() -> u128 {
    (0..Regs::COUNT).filter(|&i| local(i)).fold(0u128, |m, i| m | (1 << i))
}

/// The statements of one native op over the locals, exactly `ir::eval_native`'s semantics.
fn native(op: &Op) -> String {
    let w = op.word;
    let d = (w >> 21) & 31;
    let a = (w >> 16) & 31;
    let b = (w >> 11) & 31;
    let rc = w & 1 == 1;
    let simm = (w & 0xFFFF) as i16 as i64;
    let uimm = (w & 0xFFFF) as u64;
    let bits = |from: u32, to: u32| (w >> (31 - to)) & ((1 << (to - from + 1)) - 1);
    let r = |i: u32| format!("r{i}");
    let ra0 = if a == 0 { "0u64".to_string() } else { r(a) };
    let crf = |f: u32| format!("cr{f}");
    // a result into r{d} with the optional cr0 record
    let set = |reg: u32, v: String| if rc { format!("{}={v};cr0=n::cr0({},so);", r(reg), r(reg)) } else { format!("{}={v};", r(reg)) };
    let mut s = String::new();
    match w >> 26 {
        14 => s = format!("{}={ra0}.wrapping_add({}u64);", r(d), simm as u64),
        15 => s = format!("{}={ra0}.wrapping_add({}u64);", r(d), (simm << 16) as u64),
        24 => s = format!("{}={}|{uimm:#x};", r(a), r(d)),
        25 => s = format!("{}={}|{:#x};", r(a), r(d), uimm << 16),
        26 => s = format!("{}={}^{uimm:#x};", r(a), r(d)),
        27 => s = format!("{}={}^{:#x};", r(a), r(d), uimm << 16),
        28 => s = format!("{}={}&{uimm:#x};cr0=n::cr0({},so);", r(a), r(d), r(a)),
        29 => s = format!("{}={}&{:#x};cr0=n::cr0({},so);", r(a), r(d), uimm << 16, r(a)),
        7 => s = format!("{}=n::mulli({},{simm});", r(d), r(a)),
        8 => s = format!("({},ca)=n::subfic({},{}u64);", r(d), r(a), simm as u64),
        12 => s = format!("({},ca)=n::addic({},{}u64);", r(d), r(a), simm as u64),
        13 => s = format!("({},ca)=n::addic({},{}u64);cr0=n::cr0({},so);", r(d), r(a), simm as u64, r(d)),
        10 => s = format!("{}=n::cmp_unsigned({},{},{uimm}u64,so);", crf(bits(6, 8)), bits(10, 10) == 1, r(a)),
        11 => s = format!("{}=n::cmp_signed({},{},{}u64,so);", crf(bits(6, 8)), bits(10, 10) == 1, r(a), simm as u64),
        20 | 21 | 23 => {
            let (sh, mb, me) = (bits(16, 20), bits(21, 25), bits(26, 30));
            let mask = guest::cpu::mask32(mb, me);
            let v = match w >> 26 {
                20 => format!("n::rlwimi({},{},{sh},{mask:#x})", r(d), r(a)),
                21 => format!("n::rlwinm({},{sh},{mask:#x})", r(d)),
                _ => format!("n::rlwnm({},{},{mask:#x})", r(d), r(b)),
            };
            s = set(a, v);
        }
        32 => s = format!("{}=m.read32(n::ea({ra0},{simm})) as u64;", r(d)),
        34 => s = format!("{}=m.read8(n::ea({ra0},{simm})) as u64;", r(d)),
        40 => s = format!("{}=m.read16(n::ea({ra0},{simm})) as u64;", r(d)),
        42 => s = format!("{}=n::lha(m.read16(n::ea({ra0},{simm})));", r(d)),
        33 | 35 | 41 | 43 => {
            let rd = match w >> 26 {
                33 => "m.read32(e) as u64",
                35 => "m.read8(e) as u64",
                41 => "m.read16(e) as u64",
                _ => "n::lha(m.read16(e))",
            };
            s = format!("{{let e=n::ea({},{simm});{}={rd};{}=e as u64;}}", r(a), r(d), r(a));
        }
        48 => s = format!("f{d}=f32::from_bits(m.read32(n::ea({ra0},{simm}))) as f64;"),
        50 => s = format!("f{d}=f64::from_bits(m.read64(n::ea({ra0},{simm})));"),
        52 => s = format!("m.write32(n::ea({ra0},{simm}),(f{d} as f32).to_bits());"),
        54 => s = format!("m.write64(n::ea({ra0},{simm}),f{d}.to_bits());"),
        49 | 51 | 53 | 55 => {
            let x = match w >> 26 {
                49 => format!("f{d}=f32::from_bits(m.read32(e)) as f64"),
                51 => format!("f{d}=f64::from_bits(m.read64(e))"),
                53 => format!("m.write32(e,(f{d} as f32).to_bits())"),
                _ => format!("m.write64(e,f{d}.to_bits())"),
            };
            s = format!("{{let e=n::ea({},{simm});{x};{}=e as u64;}}", r(a), r(a));
        }
        59 | 63 => {
            let (fa, fb, fc) = (format!("f{a}"), format!("f{b}"), format!("f{}", bits(21, 25)));
            let v = match bits(26, 30) {
                18 => format!("{fa}/{fb}"),
                20 => format!("{fa}-{fb}"),
                21 => format!("{fa}+{fb}"),
                22 => format!("{fb}.sqrt()"),
                23 => format!("if {fa}>=0.0{{{fc}}}else{{{fb}}}"),
                24 => format!("1.0/{fb}"),
                25 => format!("{fa}*{fc}"),
                26 => format!("1.0/{fb}.sqrt()"),
                28 => format!("{fa}.mul_add({fc},-{fb})"),
                29 => format!("{fa}.mul_add({fc},{fb})"),
                30 => format!("-{fa}.mul_add({fc},-{fb})"),
                31 => format!("-{fa}.mul_add({fc},{fb})"),
                _ => match bits(21, 30) {
                    12 => format!("{fb} as f32 as f64"),
                    40 => format!("-{fb}"),
                    72 => fb.clone(),
                    136 => format!("-{fb}.abs()"),
                    264 => format!("{fb}.abs()"),
                    x => unreachable!("fp xo {x}"),
                },
            };
            s = if w >> 26 == 59 { format!("f{d}=({v}) as f32 as f64;") } else { format!("f{d}={v};") };
        }
        36 => s = format!("m.write32(n::ea({ra0},{simm}),{} as u32);", r(d)),
        38 => s = format!("m.write8(n::ea({ra0},{simm}),{} as u8);", r(d)),
        44 => s = format!("m.write16(n::ea({ra0},{simm}),{} as u16);", r(d)),
        37 | 39 | 45 => {
            let wr = match w >> 26 {
                37 => format!("m.write32(e,{} as u32)", r(d)),
                39 => format!("m.write8(e,{} as u8)", r(d)),
                _ => format!("m.write16(e,{} as u16)", r(d)),
            };
            s = format!("{{let e=n::ea({},{simm});{wr};{}=e as u64;}}", r(a), r(a));
        }
        58 => {
            let ds = (w & 0xFFFC) as i16 as i32;
            s = match w & 3 {
                0 => format!("{}=m.read64(n::ea({ra0},{ds}));", r(d)),
                2 => format!("{}=n::lwa(m.read32(n::ea({ra0},{ds})));", r(d)),
                _ => format!("{{let e=n::ea({},{ds});{}=m.read64(e);{}=e as u64;}}", r(a), r(d), r(a)),
            };
        }
        62 => {
            let ds = (w & 0xFFFC) as i16 as i32;
            s = match w & 3 {
                0 => format!("m.write64(n::ea({ra0},{ds}),{});", r(d)),
                _ => format!("{{let e=n::ea({},{ds});m.write64(e,{});{}=e as u64;}}", r(a), r(d), r(a)),
            };
        }
        31 => {
            let xo = bits(21, 30);
            let xo9 = bits(22, 30);
            if guest::rec::XO_ARITH.contains(&xo9) {
                let v = match xo9 {
                    266 => format!("{}.wrapping_add({})", r(a), r(b)),
                    40 => format!("{}.wrapping_sub({})", r(b), r(a)),
                    235 => format!("n::mullw({},{})", r(a), r(b)),
                    104 => format!("n::neg({})", r(a)),
                    _ => unreachable!("not native"),
                };
                return set(d, v);
            }
            let ea = format!("n::ea_x({ra0},{})", r(b));
            let eau = format!("n::ea_x({},{})", r(a), r(b));
            let sv = r(d); // rS
            s = match xo {
                0 => format!("{}=n::cmp_signed({},{},{},so);", crf(bits(6, 8)), bits(10, 10) == 1, r(a), r(b)),
                32 => format!("{}=n::cmp_unsigned({},{},{},so);", crf(bits(6, 8)), bits(10, 10) == 1, r(a), r(b)),
                339 => match (b << 5) | a {
                    8 => format!("{}=lr;", r(d)),
                    _ => format!("{}=ctr;", r(d)),
                },
                467 => match (b << 5) | a {
                    8 => format!("lr={sv};"),
                    _ => format!("ctr={sv};"),
                },
                444 => set(a, format!("{sv}|{}", r(b))),
                28 => set(a, format!("{sv}&{}", r(b))),
                60 => set(a, format!("{sv}&!{}", r(b))),
                316 => set(a, format!("{sv}^{}", r(b))),
                124 => set(a, format!("!({sv}|{})", r(b))),
                476 => set(a, format!("!({sv}&{})", r(b))),
                284 => set(a, format!("!({sv}^{})", r(b))),
                412 => set(a, format!("{sv}|!{}", r(b))),
                24 => set(a, format!("n::slw({sv},{})", r(b))),
                536 => set(a, format!("n::srw({sv},{})", r(b))),
                27 => set(a, format!("n::sld({sv},{})", r(b))),
                539 => set(a, format!("n::srd({sv},{})", r(b))),
                922 => set(a, format!("n::extsh({sv})")),
                954 => set(a, format!("n::extsb({sv})")),
                986 => set(a, format!("n::extsw({sv})")),
                26 => set(a, format!("n::cntlzw({sv})")),
                58 => set(a, format!("n::cntlzd({sv})")),
                792 => format!("{{let (v,k)=n::sraw({sv},{} as u32);ca=k;{}}}", r(b), set(a, "v".into())),
                824 => format!("{{let (v,k)=n::sraw({sv},{});ca=k;{}}}", bits(16, 20), set(a, "v".into())),
                794 => format!("{{let (v,k)=n::srad({sv},{} as u32);ca=k;{}}}", r(b), set(a, "v".into())),
                826 | 827 => format!("{{let (v,k)=n::srad({sv},{});ca=k;{}}}", bits(16, 20) | (bits(30, 30) << 5), set(a, "v".into())),
                23 => format!("{}=m.read32({ea}) as u64;", r(d)),
                87 => format!("{}=m.read8({ea}) as u64;", r(d)),
                279 => format!("{}=m.read16({ea}) as u64;", r(d)),
                343 => format!("{}=n::lha(m.read16({ea}));", r(d)),
                21 => format!("{}=m.read64({ea});", r(d)),
                341 => format!("{}=n::lwa(m.read32({ea}));", r(d)),
                790 => format!("{}=m.read16({ea}).swap_bytes() as u64;", r(d)),
                534 => format!("{}=m.read32({ea}).swap_bytes() as u64;", r(d)),
                532 => format!("{}=m.read64({ea}).swap_bytes();", r(d)),
                55 | 119 | 311 | 375 | 53 | 373 => {
                    let rd = match xo {
                        55 => "m.read32(e) as u64",
                        119 => "m.read8(e) as u64",
                        311 => "m.read16(e) as u64",
                        375 => "n::lha(m.read16(e))",
                        53 => "m.read64(e)",
                        _ => "n::lwa(m.read32(e))",
                    };
                    format!("{{let e={eau};{}={rd};{}=e as u64;}}", r(d), r(a))
                }
                151 => format!("m.write32({ea},{sv} as u32);"),
                215 => format!("m.write8({ea},{sv} as u8);"),
                407 => format!("m.write16({ea},{sv} as u16);"),
                149 => format!("m.write64({ea},{sv});"),
                918 => format!("m.write16({ea},({sv} as u16).swap_bytes());"),
                662 => format!("m.write32({ea},({sv} as u32).swap_bytes());"),
                660 => format!("m.write64({ea},{sv}.swap_bytes());"),
                183 | 247 | 439 | 181 => {
                    let wr = match xo {
                        183 => format!("m.write32(e,{sv} as u32)"),
                        247 => format!("m.write8(e,{sv} as u8)"),
                        439 => format!("m.write16(e,{sv} as u16)"),
                        _ => format!("m.write64(e,{sv})"),
                    };
                    format!("{{let e={eau};{wr};{}=e as u64;}}", r(a))
                }
                54 | 86 | 246 | 278 | 598 | 854 | 982 => String::new(),
                _ => unreachable!("not native: {w:#x}"),
            };
        }
        _ => unreachable!("not native: {w:#x}"),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::decode;

    fn ops(words: &[u32]) -> Vec<(Op, String)> {
        words.iter().enumerate().map(|(i, &w)| (decode(w, 0x8200_0000 + 4 * i as u32), String::new())).collect()
    }

    /// `add r5,r3,r4; addi r3,r5,1; stw r3,0(r6)`: r3, r4 and r6 are loaded once, r5 is declared,
    /// the store uses the local, and the tail stores r3 and r5 back (r4 and r6 were only read).
    #[test]
    fn a_block_loads_once_and_stores_what_it_wrote() {
        let o = ops(&[(31 << 26) | (5 << 21) | (3 << 16) | (4 << 11) | (266 << 1), (14 << 26) | (3 << 21) | (5 << 16) | 1, (36 << 26) | (3 << 21) | (6 << 16)]);
        let body = block(&[Seg { ops: &o, count: 3, next: Next::Tail }], "0x8200000c");
        assert_eq!(
            body,
            "let mb=m.base_ptr();let mut r3=c.gpr[3];let mut r4=c.gpr[4];let mut r5:u64;let mut r6=c.gpr[6];\
             r5=r3.wrapping_add(r4);r3=r5.wrapping_add(1u64);m.write32_b(mb,n::ea(r6,0),r3 as u32);\
             *b-=3;c.gpr[3]=r3;c.gpr[5]=r5;0x8200000c"
        );
    }

    /// A helper in the middle (lfsx: FP indexed, with r1 as its address) sees the stored r1 and the locals
    /// it writes are reloaded; a mid-superblock budget exit stores only what is dirty so far.
    #[test]
    fn helpers_and_budget_exits_sync_the_locals() {
        let o = ops(&[(14 << 26) | (1 << 21) | (1 << 16) | 0x10, (31 << 26) | (2 << 21) | (1 << 16) | (4 << 11) | (535 << 1), (31 << 26) | (3 << 21) | (3 << 16) | (3 << 11) | (266 << 1)]);
        let mut o = o;
        o[1].1 = "fpx_mem(c,m,535,2,1,4);".into();
        let segs = [Seg { ops: &o[..2], count: 2, next: Next::Check(0x8200_0008) }, Seg { ops: &o[2..], count: 1, next: Next::Tail }];
        let body = block(&segs, "0x8200000c");
        assert_eq!(
            body,
            "let mut r1=c.gpr[1];let mut r3=c.gpr[3];r1=r1.wrapping_add(16u64);c.gpr[1]=r1;fpx_mem(c,m,535,2,1,4);\
             *b-=2;if *b<=0{return Ok(0x82000008)}r3=r3.wrapping_add(r3);*b-=1;c.gpr[3]=r3;0x8200000c"
        );
    }

    /// cr0 from a record form stays local until the tail, where the branch reads it from Cpu.
    #[test]
    fn record_forms_keep_cr0_local_until_the_tail() {
        let o = ops(&[(31 << 26) | (3 << 21) | (3 << 16) | (4 << 11) | (266 << 1) | 1]);
        let body = block(&[Seg { ops: &o, count: 2, next: Next::Tail }], "let t=c.branch_cond(12,2);if t{0x82000010}else{0x82000008}");
        assert!(body.starts_with("let mut r3=c.gpr[3];let mut r4=c.gpr[4];let mut cr0:u8;let mut so=c.so;r3=r3.wrapping_add(r4);cr0=n::cr0(r3,so);*b-=2;"), "{body}");
        assert!(body.contains("c.gpr[3]=r3;c.cr[0]=cr0;let t=c.branch_cond"), "{body}");
    }

    /// Opt-in (`LMA_EMIT_TEST=1`, ~1 min): random blocks emitted as Rust, compiled into a scratch
    /// crate against `guest`, and run against the interpreter on random states, both to the end
    /// (the tail stores) and through a mid-block budget exit (the exit's stores). The text is what
    /// the parts get; this is the check that it means what `eval_native` means.
    #[test]
    fn emitted_blocks_match_the_interpreter() {
        if std::env::var("LMA_EMIT_TEST").is_err() {
            return;
        }
        use crate::ir::tests::{words, Rng};
        use std::fmt::Write as _;
        let mut rng = Rng(0x5851_F42D_4C95_7F2D);
        let mut pool: Vec<u32> = words(&mut rng)
            .into_iter()
            .filter(|&w| {
                let op = decode(w, 0x8200_1000);
                matches!(op.kind, Kind::Native | Kind::Helper) && crate::translate(0x8200_1000, w).is_some() && !matches!(w >> 26, 2 | 3)
            })
            .collect();
        // shuffle
        for i in (1..pool.len()).rev() {
            let j = (rng.next() % (i as u64 + 1)) as usize;
            pool.swap(i, j);
        }
        let mut lib = String::from(
            "#![allow(unused_variables, unused_mut, unused_assignments, unused_parens, clippy::all)]\n\
             use guest::cpu::{Cpu, Stop};\nuse guest::mem::Memory;\nuse guest::rec::*;\nuse guest::native as n;\n\
             pub type Blk = fn(&mut Cpu, &mut Memory, &mut i64) -> Result<u32, Stop>;\n",
        );
        let mut table = String::from("pub static BLOCKS: &[(&[u32], u32, Blk)] = &[\n");
        let mut i = 0;
        let mut k = 0;
        while k < pool.len() && i < 400 {
            let n = 1 + (rng.next() % 8) as usize;
            let ws: Vec<u32> = pool[k..(k + n).min(pool.len())].to_vec();
            k += n;
            let ops: Vec<(Op, String)> = ws.iter().enumerate().map(|(j, &w)| {
                let op = decode(w, 0x8200_1000 + 4 * j as u32);
                let text = if op.kind == Kind::Helper { crate::translate(0x8200_1000 + 4 * j as u32, w).unwrap() } else { String::new() };
                (op, text)
            }).collect();
            // a budget exit after the first `split` ops (0 = none)
            let split = if ws.len() > 1 && rng.next() % 2 == 0 { 1 + (rng.next() % (ws.len() as u64 - 1)) as usize } else { 0 };
            let body = if split > 0 {
                block(&[Seg { ops: &ops[..split], count: split as u32, next: Next::Check(0x8200_2000) }, Seg { ops: &ops[split..], count: (ws.len() - split) as u32, next: Next::Tail }], "0x82003000")
            } else {
                block(&[Seg { ops: &ops, count: ws.len() as u32, next: Next::Tail }], "0x82003000")
            };
            let _ = writeln!(lib, "pub fn blk{i}(c:&mut Cpu,m:&mut Memory,b:&mut i64)->Result<u32,Stop>{{Ok({{{body}}})}}");
            let _ = writeln!(table, "(&[{}], {split}, blk{i}),", ws.iter().map(|w| format!("{w:#x}")).collect::<Vec<_>>().join(","));
            i += 1;
        }
        table.push_str("];\n");
        lib.push_str(&table);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/emit-test");
        let _ = std::fs::create_dir_all(dir.join("src"));
        let _ = std::fs::create_dir_all(dir.join("tests"));
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"emit_test\"\nedition = \"2021\"\nversion = \"0.1.0\"\npublish = false\n[workspace]\n[dependencies]\nguest = { path = \"../../crates/guest\" }\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib).unwrap();
        std::fs::write(dir.join("tests/diff.rs"), EMIT_TEST_DRIVER).unwrap();
        let out = std::process::Command::new("cargo").args(["test", "--release", "-q"]).current_dir(&dir).env("CARGO_TARGET_DIR", dir.join("target")).output().expect("cargo");
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "emitted blocks differ from the interpreter (crate in {}):\n{}", dir.display(), text.chars().take(6000).collect::<String>());
    }

    const EMIT_TEST_DRIVER: &str = r#"
use guest::cpu::Cpu;
use guest::mem::Memory;
const BASE: u32 = 0x4000_0000;
const SPAN: u32 = 0x2_0000;
struct Rng(u64);
impl Rng { fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 } }
fn random_cpu(rng: &mut Rng) -> Cpu {
    let mut c = Cpu::default();
    for i in 0..32 {
        c.gpr[i] = (BASE + 0x1_0000 + (rng.next() as u32 % 0x8000) * 8) as u64;
        if rng.next() % 3 == 0 { c.gpr[i] = rng.next(); }
        c.fpr[i] = f64::from_bits(rng.next());
    }
    for f in 0..8 { c.cr[f] = (rng.next() & 15) as u8; }
    c.lr = rng.next() & 0xFFFF_FFFC; c.ctr = rng.next(); c.ca = rng.next() & 1 == 1; c.so = rng.next() & 1 == 1; c.ov = rng.next() & 1 == 1; c.fpscr = (rng.next() & 3) as u32;
    c
}
fn state(c: &Cpu, m: &mut Memory) -> (Vec<u64>, [u8; 8], (bool, bool, bool, u64, u64, u32), Vec<u64>, Vec<u8>) {
    let mut mem = vec![0u8; SPAN as usize];
    m.read(BASE, &mut mem);
    (c.gpr.to_vec(), c.cr, (c.ca, c.so, c.ov, c.lr, c.ctr, c.fpscr), c.fpr.iter().map(|f| f.to_bits()).collect(), mem)
}
#[test]
fn blocks_match() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut failures = Vec::new();
    for (bi, (words, split, blk)) in emit_test::BLOCKS.iter().enumerate() {
        for exit in [false, true] {
            if exit && *split == 0 { continue; }
            for _ in 0..8 {
                let mut fill = vec![0u8; SPAN as usize];
                for x in &mut fill { *x = rng.next() as u8; }
                let c0 = random_cpu(&mut rng);
                let (mut m1, mut m2) = (Memory::new(), Memory::new());
                m1.write(BASE, &fill); m2.write(BASE, &fill);
                let mut c1 = c0.clone();
                let n = if exit { *split as usize } else { words.len() };
                let mut ok = true;
                for (j, &w) in words[..n].iter().enumerate() {
                    c1.pc = 0x8200_1000 + 4 * j as u32;
                    if c1.exec(&mut m1, w).is_err() { ok = false; break; }
                }
                if !ok { continue; }
                let mut c2 = c0.clone();
                let mut b: i64 = if exit { *split as i64 } else { 1 << 40 };
                let r = blk(&mut c2, &mut m2, &mut b);
                let want = if exit { 0x8200_2000 } else { 0x8200_3000 };
                let s1 = state(&c1, &mut m1);
                let s2 = state(&c2, &mut m2);
                if !matches!(r, Ok(pc) if pc == want) || s1 != s2 {
                    let mut why = String::new();
                    for i in 0..32 { if s1.0[i] != s2.0[i] { why += &format!(" r{i}:{:x}/{:x}", s1.0[i], s2.0[i]); } }
                    for f in 0..8 { if s1.1[f] != s2.1[f] { why += &format!(" cr{f}:{:x}/{:x}", s1.1[f], s2.1[f]); } }
                    if s1.2 != s2.2 { why += &format!(" flags/lr/ctr {:?}/{:?}", s1.2, s2.2); }
                    if s1.3 != s2.3 { why += " fpr"; }
                    if s1.4 != s2.4 { why += " memory"; }
                    if !matches!(r, Ok(pc) if pc == want) { why += &format!(" exit {:?}", r.as_ref().ok()); }
                    failures.push(format!("block {bi} exit={exit} words {:x?}:{why}", words));
                    break;
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}
{} blocks failed", failures.join("
"), failures.len());
}
"#;
}
