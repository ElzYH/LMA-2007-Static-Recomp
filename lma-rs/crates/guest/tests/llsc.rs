//! lwarx/stwcx. and ldarx/stdcx. as value-based LL/SC (#39): the conditional store succeeds only
//! with the reservation and the word still holding the loaded value, in the interpreter and in the
//! native helpers alike, and it is atomic across host threads sharing the guest memory.

use guest::cpu::Cpu;
use guest::mem::Memory;
use guest::rec;

const A: u32 = 0x4000_0100;

fn x31(rt: u32, ra: u32, rb: u32, xo: u32, rc: u32) -> u32 {
    (31 << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1) | rc
}

/// One load-reserve / conditional-store pair on a fresh machine, with `between` run in the
/// middle; returns (CR0 EQ set, the word afterwards) for the interpreter and the native helpers.
fn pair(dbl: bool, reserve: bool, between: impl Fn(&mut Memory)) -> [(bool, u64); 2] {
    let run = |native: bool| {
        let mut c = Cpu::default();
        let mut m = Memory::new();
        if dbl { m.write64(A, 7) } else { m.write32(A, 7) }
        c.gpr[3] = A as u64;
        c.gpr[4] = 0;
        c.gpr[6] = 99;
        if reserve {
            if native { rec::larx(&mut c, &mut m, 5, 3, 4, dbl) } else { c.exec(&mut m, x31(5, 3, 4, if dbl { 84 } else { 20 }, 0)).unwrap() }
            assert_eq!(c.gpr[5], 7);
        }
        between(&mut m);
        if native { rec::stcx(&mut c, &mut m, 6, 3, 4, dbl) } else { c.exec(&mut m, x31(6, 3, 4, if dbl { 214 } else { 150 }, 1)).unwrap() }
        (c.cr[0] & 2 != 0, if dbl { m.read64(A) } else { m.read32(A) as u64 })
    };
    [run(false), run(true)]
}

#[test]
fn conditional_store_follows_the_reserved_value() {
    for dbl in [false, true] {
        assert_eq!(pair(dbl, true, |_| {}), [(true, 99); 2], "unchanged: stored");
        let changed = |m: &mut Memory| if dbl { m.write64(A, 8) } else { m.write32(A, 8) };
        assert_eq!(pair(dbl, true, changed), [(false, 8); 2], "changed by another writer: not stored");
        assert_eq!(pair(dbl, false, |_| {}), [(false, 7); 2], "no reservation: not stored");
        // ABA: changed and changed back still succeeds (accepted, as in Xenia)
        let aba = |m: &mut Memory| if dbl { m.write64(A, 8); m.write64(A, 7) } else { m.write32(A, 8); m.write32(A, 7) };
        assert_eq!(pair(dbl, true, aba), [(true, 99); 2], "ABA: stored");
    }
}

/// Guest threads on several host threads (#39): lwarx/add/stwcx. loops through shared views of the
/// same memory lose no increment.
#[cfg(any(target_vendor = "apple", target_os = "linux", target_os = "android", windows))]
#[test]
fn atomic_increments_across_host_threads() {
    const THREADS: u64 = 4;
    const EACH: u64 = 50_000;
    let m = Memory::new();
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let mut m = m.share();
            std::thread::spawn(move || {
                let mut c = Cpu::default();
                c.gpr[3] = A as u64;
                for _ in 0..EACH {
                    loop {
                        rec::larx(&mut c, &mut m, 5, 3, 0, false);
                        c.gpr[6] = c.gpr[5] + 1;
                        rec::stcx(&mut c, &mut m, 6, 3, 0, false);
                        if c.cr[0] & 2 != 0 {
                            break;
                        }
                    }
                }
            })
        })
        .collect();
    workers.into_iter().for_each(|w| w.join().unwrap());
    let mut m = m;
    assert_eq!(m.read32(A) as u64, THREADS * EACH);
}
