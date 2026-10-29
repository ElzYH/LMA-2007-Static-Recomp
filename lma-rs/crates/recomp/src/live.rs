//! Control flow and liveness over the IR (#145): a function's blocks, each block's upward-exposed
//! uses (`gen`) and definitions (`kill`), and the per-block `live_in`/`live_out` sets by backward
//! dataflow. Edges that leave the recompiled function's own control flow (calls, returns, indirect
//! branches, traps, targets outside the function, the interpreter) are conservative: everything is
//! live there, because the callee, the dispatcher, a save state or the interpreter may read any
//! register. The analysis decides what a block or region must *load*; stores stay complete at every
//! exit to the dispatcher (save states and fingerprints observe the whole register file).

use crate::ir::{decode, Kind, Op, Regs};
use std::collections::BTreeSet;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Succ {
    /// Falls through or jumps to one block of this function.
    One(u32),
    /// A conditional branch: (taken, not taken).
    Two(u32, u32),
    /// Leaves the function's own control flow: call, return, indirect branch, trap, a target
    /// outside the function, or an interpreter fallback at the end (everything live).
    Exit,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub start: u32,
    pub ops: Vec<Op>,
    pub succ: Succ,
    /// Registers read before any write in this block.
    pub gen: Regs,
    /// Registers written in this block.
    pub kill: Regs,
}

/// Direct branch target of `w` at `pc` (b, bc), None for other words (including blr/bctr/sc,
/// which have no static target).
fn direct_target(w: u32, pc: u32) -> Option<u32> {
    match w >> 26 {
        18 => {
            let li = ((w & 0x03FF_FFFC) << 6) as i32 >> 6;
            Some(if w & 2 != 0 { li as u32 } else { pc.wrapping_add(li as u32) })
        }
        16 => {
            let bd = (w & 0xFFFC) as i16 as i32;
            Some(if w & 2 != 0 { bd as u32 } else { pc.wrapping_add(bd as u32) })
        }
        _ => None,
    }
}

/// The function `start..end` (words as `read(pc)`), cut at `leaders` (the function's own, as the
/// recompiler finds them: every branch target and the word after every branch).
pub fn blocks(start: u32, end: u32, leaders: &BTreeSet<u32>, read: impl Fn(u32) -> u32) -> Vec<Block> {
    let leaders: Vec<u32> = leaders.iter().copied().filter(|&l| l >= start && l < end).collect();
    let inside = |t: u32| t >= start && t < end;
    let mut out = Vec::new();
    for (i, &bs) in leaders.iter().enumerate() {
        let be = leaders.get(i + 1).copied().unwrap_or(end);
        let mut ops = Vec::new();
        let mut succ = Succ::One(be);
        let mut pc = bs;
        while pc < be {
            let w = read(pc);
            let op = decode(w, pc);
            let branch = op.kind == Kind::Branch;
            ops.push(op);
            if branch {
                let next = pc.wrapping_add(4);
                succ = match w >> 26 {
                    // b / bl: a call leaves; a jump inside stays
                    18 => match (w & 1 == 1, direct_target(w, pc)) {
                        (false, Some(t)) if inside(t) => Succ::One(t),
                        _ => Succ::Exit,
                    },
                    16 => {
                        let bo = (w >> 21) & 0x1F;
                        match (w & 1 == 1, direct_target(w, pc)) {
                            (true, _) => Succ::Exit,
                            (false, Some(t)) if inside(t) => {
                                if bo & 0x14 == 0x14 { Succ::One(t) } else { Succ::Two(t, next) }
                            }
                            _ => Succ::Exit,
                        }
                    }
                    _ => Succ::Exit,
                };
                break;
            }
            pc = pc.wrapping_add(4);
        }
        if be >= end && matches!(succ, Succ::One(t) if t >= end) {
            succ = Succ::Exit;
        }
        // an interpreter fallback anywhere makes the block conservative at that point: gen all
        let (mut gen, mut kill) = (Regs::NONE, Regs::NONE);
        for op in &ops {
            gen = gen.union(op.uses.minus(kill));
            kill = kill.union(op.defs);
        }
        out.push(Block { start: bs, ops, succ, gen, kill });
    }
    out
}

/// `(live_in, live_out)` per block, by backward dataflow to a fixed point.
pub fn liveness(blocks: &[Block]) -> Vec<(Regs, Regs)> {
    let index = |pc: u32| blocks.iter().position(|b| b.start == pc);
    let mut live_in = vec![Regs::NONE; blocks.len()];
    let mut live_out = vec![Regs::NONE; blocks.len()];
    loop {
        let mut changed = false;
        for (i, b) in blocks.iter().enumerate().rev() {
            let out = match b.succ {
                Succ::Exit => Regs::all(),
                Succ::One(t) => index(t).map_or(Regs::all(), |j| live_in[j]),
                Succ::Two(t, n) => {
                    let a = index(t).map_or(Regs::all(), |j| live_in[j]);
                    let c = index(n).map_or(Regs::all(), |j| live_in[j]);
                    a.union(c)
                }
            };
            let inn = b.gen.union(out.minus(b.kill));
            if out != live_out[i] || inn != live_in[i] {
                live_out[i] = out;
                live_in[i] = inn;
                changed = true;
            }
        }
        if !changed {
            return live_in.iter().copied().zip(live_out.iter().copied()).collect();
        }
    }
}

/// The recompiler's leaders for `start..end`: the function entry, every direct branch target
/// inside it and the word after every branch.
pub fn leaders(start: u32, end: u32, read: impl Fn(u32) -> u32) -> BTreeSet<u32> {
    let mut l = BTreeSet::from([start]);
    let mut pc = start;
    while pc < end {
        let w = read(pc);
        if decode(w, pc).kind == Kind::Branch {
            l.insert(pc.wrapping_add(4));
            if let Some(t) = direct_target(w, pc).filter(|&t| t >= start && t < end) {
                l.insert(t);
            }
        }
        pc = pc.wrapping_add(4);
    }
    l.retain(|&x| x < end);
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    // assembler for the handful of forms the tests need
    fn addi(d: u32, a: u32, imm: u32) -> u32 {
        (14 << 26) | (d << 21) | (a << 16) | (imm & 0xFFFF)
    }
    fn add(d: u32, a: u32, b: u32) -> u32 {
        (31 << 26) | (d << 21) | (a << 16) | (b << 11) | (266 << 1)
    }
    fn cmpwi(f: u32, a: u32, imm: u32) -> u32 {
        (11 << 26) | (f << 23) | (a << 16) | (imm & 0xFFFF)
    }
    fn bc(bo: u32, bi: u32, off: i32) -> u32 {
        (16 << 26) | (bo << 21) | (bi << 16) | (off as u32 & 0xFFFC)
    }
    fn b(off: i32) -> u32 {
        (18 << 26) | (off as u32 & 0x03FF_FFFC)
    }
    const BLR: u32 = (19 << 26) | (20 << 21) | (16 << 1);

    fn run(words: &[u32]) -> (Vec<Block>, Vec<(Regs, Regs)>) {
        let start = 0x8200_0000u32;
        let end = start + 4 * words.len() as u32;
        let read = |pc: u32| words[((pc - start) / 4) as usize];
        let l = leaders(start, end, read);
        let bl = blocks(start, end, &l, read);
        let lv = liveness(&bl);
        (bl, lv)
    }

    /// r5 = r3 + r4; r3 = r5 + 1; blr: the block reads r3, r4 (and lr), writes r3 and r5. The
    /// return sees everything, so live_in is the whole file except r5, which is written first.
    #[test]
    fn straight_line_gen_kill() {
        let (bl, lv) = run(&[add(5, 3, 4), addi(3, 5, 1), BLR]);
        assert_eq!(bl.len(), 1, "the word after the blr is the end, not a leader: {bl:?}");
        let b0 = &bl[0];
        assert_eq!(b0.gen, Regs::of(&[Regs::gpr(3), Regs::gpr(4), Regs::LR]), "{:?}", b0.gen);
        assert_eq!(b0.kill, Regs::of(&[Regs::gpr(3), Regs::gpr(5)]), "{:?}", b0.kill);
        assert_eq!(b0.succ, Succ::Exit);
        // live_in = gen ∪ (all − kill)
        assert_eq!(lv[0].0, Regs::all().minus(Regs::of(&[Regs::gpr(5)])).with(Regs::gpr(3)), "{:?}", lv[0].0);
        assert_eq!(lv[0].1, Regs::all());
    }

    /// A loop: `r3 = r3 + r4; cmpwi cr0, r3, 10; blt -8` then `blr`. r4 and r3 are live around the
    /// loop; cr0 is written and read inside one block, so it is not live at the loop head.
    #[test]
    fn loop_liveness() {
        let (bl, lv) = run(&[add(3, 3, 4), cmpwi(0, 3, 10), bc(12, 0, -8), BLR]);
        assert_eq!(bl.len(), 2);
        assert_eq!(bl[0].succ, Succ::Two(0x8200_0000, 0x8200_000C));
        assert_eq!(bl[0].gen, Regs::of(&[Regs::gpr(3), Regs::gpr(4), Regs::SO]), "{:?}", bl[0].gen);
        assert!(!lv[0].0.has(Regs::cr(0)), "cr0 is set before it is read: {:?}", lv[0].0);
        assert!(lv[0].0.has(Regs::gpr(3)) && lv[0].0.has(Regs::gpr(4)));
        assert_eq!(bl[1].succ, Succ::Exit);
        assert_eq!(lv[0].1, Regs::all(), "the loop exit goes to a return: everything live");
    }

    /// Two paths that join: `cmpwi; beq L; addi r3,r3,1; L: add r5,r3,r4; blr`. r3 is live into
    /// both paths; the diamond's join block reads r3 and r4.
    #[test]
    fn diamond_join() {
        let (bl, lv) = run(&[cmpwi(0, 3, 0), bc(12, 2, 8), addi(3, 3, 1), add(5, 3, 4), BLR]);
        assert_eq!(bl.len(), 3);
        assert_eq!(bl[0].succ, Succ::Two(0x8200_000C, 0x8200_0008));
        assert_eq!(bl[1].succ, Succ::One(0x8200_000C));
        assert!(lv[1].0.has(Regs::gpr(3)) && lv[1].0.has(Regs::gpr(4)), "{:?}", lv[1].0);
        assert!(lv[0].0.has(Regs::gpr(3)) && !lv[0].0.has(Regs::cr(0)));
    }

    /// A direct jump out of the function and a call are exits.
    #[test]
    fn exits() {
        let (bl, _) = run(&[b(0x1000), addi(3, 0, 1), b(-4 + 0x1000 - 0x1000) | 1]);
        assert_eq!(bl[0].succ, Succ::Exit, "jump outside");
        assert_eq!(bl[1].succ, Succ::Exit, "a call (bl)");
    }
}
