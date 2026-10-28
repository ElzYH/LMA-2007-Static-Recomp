//! Load/store and condition-register semantics in the interpreter, checked against the PowerPC
//! architecture: big-endian and byte-reversed accesses, update forms, sign extension,
//! reservations (lwarx/stwcx.), mfcr/mtcrf and rlwnm.

use guest::cpu::Cpu;
use guest::mem::Memory;

fn d(op: u32, rt: u32, ra: u32, disp: i16) -> u32 {
    (op << 26) | (rt << 21) | (ra << 16) | (disp as u16 as u32)
}
fn x(rt: u32, ra: u32, rb: u32, xo: u32, rc: u32) -> u32 {
    (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1) | rc
}

const BUF: u32 = 0x4000_0000;

fn setup() -> (Cpu, Memory) {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    m.write(BUF, &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0]);
    c.gpr[3] = BUF as u64;
    c.gpr[4] = 4;
    (c, m)
}

#[test]
fn loads() {
    let (mut c, mut m) = setup();
    c.exec(&mut m, d(32, 5, 3, 0)).unwrap(); // lwz
    assert_eq!(c.gpr[5], 0x1234_5678, "big-endian");
    c.exec(&mut m, x(5, 3, 4, 534, 0)).unwrap(); // lwbrx r5,r3,r4
    assert_eq!(c.gpr[5], 0xF0DE_BC9A, "byte-reversed");
    c.exec(&mut m, d(42, 5, 3, 4)).unwrap(); // lha: sign-extends
    assert_eq!(c.gpr[5], 0xFFFF_FFFF_FFFF_9ABC);
    c.exec(&mut m, d(40, 5, 3, 4)).unwrap(); // lhz
    assert_eq!(c.gpr[5], 0x9ABC);
    c.exec(&mut m, x(5, 3, 4, 790, 0)).unwrap(); // lhbrx
    assert_eq!(c.gpr[5], 0xBC9A);
    c.exec(&mut m, (58 << 26) | (5 << 21) | (3 << 16)).unwrap(); // ld
    assert_eq!(c.gpr[5], 0x1234_5678_9ABC_DEF0);
    c.exec(&mut m, d(33, 5, 3, 4)).unwrap(); // lwzu: loads and updates r3
    assert_eq!((c.gpr[5], c.gpr[3]), (0x9ABC_DEF0, BUF as u64 + 4));
}

#[test]
fn stores() {
    let (mut c, mut m) = setup();
    c.gpr[5] = 0xAABB_CCDD;
    c.exec(&mut m, x(5, 3, 4, 662, 0)).unwrap(); // stwbrx r5,r3,r4
    assert_eq!(m.read32(BUF + 4), 0xDDCC_BBAA);
    c.exec(&mut m, d(44, 5, 3, 0)).unwrap(); // sth: low halfword
    assert_eq!(m.read32(BUF), 0xCCDD_5678);
    c.exec(&mut m, d(37, 5, 3, 8)).unwrap(); // stwu: stores and updates r3
    assert_eq!((m.read32(BUF + 8), c.gpr[3]), (0xAABB_CCDD, BUF as u64 + 8));
}

/// One guest thread: a store-conditional succeeds after its own reservation, once.
#[test]
fn reservations() {
    let (mut c, mut m) = setup();
    c.gpr[4] = 0;
    c.exec(&mut m, x(5, 3, 4, 20, 0)).unwrap(); // lwarx r5,r3,r4
    assert_eq!(c.gpr[5], 0x1234_5678);
    c.gpr[6] = 99;
    c.exec(&mut m, x(6, 3, 4, 150, 1)).unwrap(); // stwcx. r6,r3,r4
    assert_eq!((m.read32(BUF), c.cr_word() >> 28 & 2), (99, 2), "stored, CR0[EQ] set");
    c.gpr[6] = 7;
    c.exec(&mut m, x(6, 3, 4, 150, 1)).unwrap(); // no reservation left
    assert_eq!((m.read32(BUF), c.cr_word() >> 28 & 2), (99, 0), "not stored, EQ clear");
}

#[test]
fn condition_register_moves() {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    c.gpr[3] = 0x1234_5678;
    // mtcrf 0b0100_0001,r3 (FXM selects CR1 and CR7): only those fields change
    c.exec(&mut m, (31 << 26) | (3 << 21) | (0b0100_0001 << 12) | (144 << 1)).unwrap();
    c.exec(&mut m, x(5, 0, 0, 19, 0)).unwrap(); // mfcr r5
    assert_eq!(c.gpr[5], 0x0200_0008);
    // rlwnm r5,r3,r4,24,31: rotate by the low 5 bits of r4
    c.gpr[4] = 32 + 8;
    c.exec(&mut m, (23 << 26) | (3 << 21) | (5 << 16) | (4 << 11) | (24 << 6) | (31 << 1)).unwrap();
    assert_eq!(c.gpr[5], 0x12);
}
