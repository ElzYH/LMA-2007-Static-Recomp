//! lvlx / lvrx, native (rec::vlvlx, rec::vlvrx): the same register as the interpreter's, at
//! every offset in the 16-byte line, through the fast path (a line in plain memory) and the
//! fallback (a line at 0xE0000000, past the flat backends' one-compare range).

use guest::cpu::Cpu;
use guest::mem::Memory;
use guest::rec;

fn x31(vd: u32, ra: u32, rb: u32, xo: u32) -> u32 {
    (31 << 26) | (vd << 21) | (ra << 16) | (rb << 11) | (xo << 1)
}
const LVLX: u32 = 519;
const LVRX: u32 = 551;

#[test]
fn partial_vector_loads_match_the_interpreter() {
    for line in [0x4000_1230u32, 0xE000_0000] {
        let mut m = Memory::new();
        // the line and its neighbours, distinct bytes
        let bytes: Vec<u8> = (0..48u32).map(|i| (i * 29 + 7) as u8).collect();
        m.write(line - 16, &bytes);
        for off in 0..16u32 {
            for (xo, native) in [(LVLX, rec::vlvlx as fn(&mut Cpu, &mut Memory, usize, usize, usize)), (LVRX, rec::vlvrx)] {
                let mut a = Cpu::default();
                a.gpr[3] = line as u64;
                a.gpr[4] = off as u64;
                a.vr[5] = [0xAA; 16];
                let mut b = a.clone();
                native(&mut a, &mut m, 5, 3, 4);
                b.exec(&mut m, x31(5, 3, 4, xo)).unwrap();
                assert_eq!(a.vr[5], b.vr[5], "xo {xo} line {line:#x} offset {off}");
            }
        }
    }
}
