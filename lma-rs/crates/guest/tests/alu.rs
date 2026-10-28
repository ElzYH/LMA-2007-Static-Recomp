//! Integer instruction semantics in the interpreter (which the recompiled code must match),
//! checked against the PowerPC architecture: carries, shifts with the carry rule, rotates with
//! wrapping masks, counts, sign extension and compares. Expected values are from the ISA, not
//! from running the code.

use guest::cpu::Cpu;
use guest::mem::Memory;

fn x(op: u32, rt: u32, ra: u32, rb: u32, xo: u32, rc: u32) -> u32 {
    (op << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1) | rc
}

/// Runs `w` with r3 = a and r4 = b (and CA), returns (r5, CA).
fn run(w: u32, a: u64, b: u64, ca: bool) -> (u64, bool) {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    c.gpr[3] = a;
    c.gpr[4] = b;
    c.ca = ca;
    c.exec(&mut m, w).unwrap();
    (c.gpr[5], c.ca)
}

#[test]
fn carries() {
    // addc r5,r3,r4 (XO 10): CA is the carry out of the 64-bit add
    assert_eq!(run(x(31, 5, 3, 4, 10, 0), u64::MAX, 1, false), (0, true));
    assert_eq!(run(x(31, 5, 3, 4, 10, 0), 5, 6, true), (11, false));
    // adde (XO 138) adds CA in
    assert_eq!(run(x(31, 5, 3, 4, 138, 0), 5, 6, true), (12, false));
    assert_eq!(run(x(31, 5, 3, 4, 138, 0), u64::MAX, 0, true), (0, true));
    // subfc r5,r3,r4 (XO 8): r4 - r3, CA = no borrow
    assert_eq!(run(x(31, 5, 3, 4, 8, 0), 3, 10, false), (7, true));
    assert_eq!(run(x(31, 5, 3, 4, 8, 0), 10, 3, false), (3u64.wrapping_sub(10), false));
    // subfe (XO 136): ~r3 + r4 + CA
    assert_eq!(run(x(31, 5, 3, 4, 136, 0), 3, 10, true), (7, true));
    assert_eq!(run(x(31, 5, 3, 4, 136, 0), 3, 10, false), (6, true));
    // addze r5,r3 (XO 202)
    assert_eq!(run(x(31, 5, 3, 0, 202, 0), 41, 0, true), (42, false));
}

#[test]
fn algebraic_shifts_and_carry() {
    // srawi r5,r3,4 (XO 824, rs=3 ra=5): CA set only for a negative value losing 1-bits
    let srawi = |sh: u32| x(31, 3, 5, sh, 824, 0);
    assert_eq!(run(srawi(4), (-17i64) as u64, 0, false), ((-2i64) as u64, true));
    assert_eq!(run(srawi(4), (-16i64) as u64, 0, false), ((-1i64) as u64, false));
    assert_eq!(run(srawi(4), 0x7F, 0, true), (7, false));
    // sraw r5,r3,r4 (XO 792): shifts of 32..63 give all sign bits
    let sraw = x(31, 3, 5, 4, 792, 0);
    assert_eq!(run(sraw, 0x8000_0000, 40, false), (u64::MAX, true));
    assert_eq!(run(sraw, 0x4000_0000, 40, false), (0, false));
    // slw/srw (XO 24/536): shift amounts 32..63 give 0
    assert_eq!(run(x(31, 3, 5, 4, 24, 0), 1, 32, false).0, 0);
    assert_eq!(run(x(31, 3, 5, 4, 536, 0), 0x8000_0000, 31, false).0, 1);
    // sld (XO 27): 64..127 give 0
    assert_eq!(run(x(31, 3, 5, 4, 27, 0), 1, 64, false).0, 0);
    assert_eq!(run(x(31, 3, 5, 4, 27, 0), 1, 63, false).0, 1 << 63);
}

#[test]
fn rotates_and_masks() {
    let rlwinm = |sh: u32, mb: u32, me: u32| (21 << 26) | (3 << 21) | (5 << 16) | (sh << 11) | (mb << 6) | (me << 1);
    // extrwi-style: rotate left 8, keep bits 24..31
    assert_eq!(run(rlwinm(8, 24, 31), 0x1234_5678, 0, false).0, 0x12);
    // wrapping mask (mb > me): bits 28..31 and 0..3
    assert_eq!(run(rlwinm(0, 28, 3), 0xFFFF_FFFF, 0, false).0, 0xF000_000F);
    // the rotate is 32-bit: the high word of the source doesn't leak in
    assert_eq!(run(rlwinm(4, 0, 31), 0xAAAA_AAAA_0000_000F, 0, false).0, 0xF0);
    // rlwimi: insert the rotated bits under the mask, keep the rest of the target
    let mut c = Cpu::default();
    let mut m = Memory::new();
    c.gpr[3] = 0xAB;
    c.gpr[5] = 0x1111_1111;
    c.exec(&mut m, (20 << 26) | (3 << 21) | (5 << 16) | (8 << 11) | (16 << 6) | (23 << 1)).unwrap();
    assert_eq!(c.gpr[5], 0x1111_AB11);
}

#[test]
fn counts_extensions_compares() {
    // cntlzw (XO 26, rs=3 ra=5) counts in the low word; cntlzd (XO 58) in all 64 bits
    assert_eq!(run(x(31, 3, 5, 0, 26, 0), 0xFFFF_FFFF_0000_0001, 0, false).0, 31);
    assert_eq!(run(x(31, 3, 5, 0, 26, 0), 0, 0, false).0, 32);
    assert_eq!(run(x(31, 3, 5, 0, 58, 0), 1, 0, false).0, 63);
    // extsb / extsh / extsw (XO 954 / 922 / 986)
    assert_eq!(run(x(31, 3, 5, 0, 954, 0), 0x80, 0, false).0, (-128i64) as u64);
    assert_eq!(run(x(31, 3, 5, 0, 922, 0), 0x7FFF, 0, false).0, 0x7FFF);
    assert_eq!(run(x(31, 3, 5, 0, 986, 0), 0x8000_0000, 0, false).0, 0xFFFF_FFFF_8000_0000);
    // cmpw cr0 compares the low words signed; cmplw unsigned (LT GT EQ in CR0's top bits)
    let cr0 = |w: u32, a: u64, b: u64| {
        let mut c = Cpu::default();
        let mut m = Memory::new();
        c.gpr[3] = a;
        c.gpr[4] = b;
        c.exec(&mut m, w).unwrap();
        c.cr_word() >> 28
    };
    let cmpw = x(31, 0, 3, 4, 0, 0);
    let cmplw = x(31, 0, 3, 4, 32, 0);
    assert_eq!(cr0(cmpw, 0xFFFF_FFFF, 1), 0b1000, "-1 < 1 as signed words");
    assert_eq!(cr0(cmplw, 0xFFFF_FFFF, 1), 0b0100, "0xFFFFFFFF > 1 unsigned");
    assert_eq!(cr0(cmpw, 0x1_0000_0005, 5), 0b0010, "only the low words count");
}

#[test]
fn multiply_divide() {
    // mullw (XO 235): low 32 x low 32, 64-bit product
    assert_eq!(run(x(31, 5, 3, 4, 235, 0), 0x1_0000_0003, 7, false).0, 21);
    // mulhwu (XO 11) / mulhw (XO 75): high word of the 32-bit product
    assert_eq!(run(x(31, 5, 3, 4, 11, 0), 0xFFFF_FFFF, 2, false).0 as u32, 1);
    assert_eq!(run(x(31, 5, 3, 4, 75, 0), 0xFFFF_FFFF, 2, false).0 as u32, 0xFFFF_FFFF, "-1 * 2 = -2: high word all ones");
    // divwu (XO 459) / divw (XO 491)
    assert_eq!(run(x(31, 5, 3, 4, 459, 0), 100, 7, false).0, 14);
    assert_eq!(run(x(31, 5, 3, 4, 491, 0), (-100i64) as u64, 7, false).0 as u32 as i32, -14, "rounds toward zero");
}
