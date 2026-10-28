//! The native `__savegprlr` / `__restgprlr` (rec::save_gprlr, rec::rest_gprlr) leave exactly what
//! the guest helpers leave when the interpreter runs them, from every entry point.

use guest::cpu::Cpu;
use guest::mem::Memory;
use guest::rec;

fn std_(rs: u32, off: i32) -> u32 {
    (62 << 26) | (rs << 21) | (1 << 16) | (off as u32 & 0xFFFC)
}
fn ld(rt: u32, off: i32) -> u32 {
    (58 << 26) | (rt << 21) | (1 << 16) | (off as u32 & 0xFFFC)
}
const STW_R12: u32 = (36 << 26) | (12 << 21) | (1 << 16) | (-8i32 as u32 & 0xFFFF);
const LWZ_R12: u32 = (32 << 26) | (12 << 21) | (1 << 16) | (-8i32 as u32 & 0xFFFF);
const MTLR_R12: u32 = 0x7d88_03a6;

/// Stack pointers: one whose save area crosses a 64 KB page edge (the paged backend's slow path),
/// and two whose save area straddles where the flat backends' one-compare fast path ends
/// (0xE0000000 on Apple, 0xC0000000 on Windows), so the native helpers' fallback runs too.
const SPS: [u32; 3] = [0x7000_0040, 0xE000_0060, 0xC000_0060];

fn setup(sp: u32) -> (Cpu, Memory) {
    let mut c = Cpu::default();
    let m = Memory::new();
    for k in 0..32 {
        c.gpr[k] = 0x0101_0101_0101_0101u64.wrapping_mul(k as u64 + 1) ^ 0x8000_0000_dead_0000;
    }
    c.gpr[1] = sp as u64;
    c.lr = 0x8200_1234;
    (c, m)
}

fn run(c: &mut Cpu, m: &mut Memory, words: &[u32]) {
    for &w in words {
        c.exec(m, w).unwrap();
    }
}

#[test]
fn save_matches_the_guest_helper_from_every_entry() {
    for (sp, first) in SPS.into_iter().flat_map(|sp| (14..32u32).map(move |f| (sp, f))) {
        let words: Vec<u32> = (first..32).map(|k| std_(k, -152 + 8 * (k as i32 - 14))).chain([STW_R12]).collect();
        let (mut a, mut ma) = setup(sp);
        let (mut b, mut mb) = setup(sp);
        rec::save_gprlr(&mut a, &mut ma, first as usize);
        run(&mut b, &mut mb, &words);
        let (mut x, mut y) = ([0u8; 160], [0u8; 160]);
        ma.read(sp - 160, &mut x);
        mb.read(sp - 160, &mut y);
        assert_eq!(x, y, "save from r{first}, sp {sp:#x}: memory");
        assert_eq!(a.gpr, b.gpr, "save from r{first}, sp {sp:#x}: registers");
    }
}

#[test]
fn restore_matches_the_guest_helper_from_every_entry() {
    for (sp, first) in SPS.into_iter().flat_map(|sp| (14..32u32).map(move |f| (sp, f))) {
        let words: Vec<u32> = (first..32).map(|k| ld(k, -152 + 8 * (k as i32 - 14))).chain([LWZ_R12, MTLR_R12]).collect();
        let (mut a, mut ma) = setup(sp);
        // something to restore: a save area with distinct values
        let area: Vec<u8> = (0..160u32).map(|i| (i * 37 + 11) as u8).collect();
        ma.write(sp - 160, &area);
        let (mut b, mut mb) = (a.clone(), Memory::new());
        mb.write(sp - 160, &area);
        rec::rest_gprlr(&mut a, &mut ma, first as usize);
        run(&mut b, &mut mb, &words);
        assert_eq!(a.gpr, b.gpr, "restore from r{first}, sp {sp:#x}: registers");
        assert_eq!(a.lr, b.lr, "restore from r{first}, sp {sp:#x}: lr");
    }
}
