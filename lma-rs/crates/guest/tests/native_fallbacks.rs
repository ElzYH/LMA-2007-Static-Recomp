//! The native helpers for instructions that matches used to run in the interpreter (perf plan
//! item 3: mfmsr, mtmsrd, lwarx/ldarx, stwcx./stdcx., stwux, twi/tdi) leave exactly what the
//! interpreter leaves: registers, CR0, the reservation and memory.

use guest::cpu::{Cpu, Stop};
use guest::mem::Memory;
use guest::rec;

fn x(op: u32, rt: u32, ra: u32, rb: u32, xo: u32, rc: u32) -> u32 {
    (op << 26) | (rt << 21) | (ra << 16) | (rb << 11) | (xo << 1) | rc
}

fn setup() -> (Cpu, Memory) {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    m.write(0x4000_0000, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    c.gpr[3] = 0x4000_0000;
    c.gpr[4] = 4;
    c.gpr[5] = 0xAABB_CCDD_EEFF_0011;
    c.so = true;
    (c, m)
}

fn same(a: &Cpu, b: &Cpu, ma: &mut Memory, mb: &mut Memory, what: &str) {
    assert_eq!(a.gpr, b.gpr, "{what}: registers");
    assert_eq!(a.cr, b.cr, "{what}: CR");
    assert_eq!(a.reserve, b.reserve, "{what}: reservation");
    let (mut x, mut y) = ([0u8; 16], [0u8; 16]);
    ma.read(0x4000_0000, &mut x);
    mb.read(0x4000_0000, &mut y);
    assert_eq!(x, y, "{what}: memory");
}

#[test]
fn reservations_and_stores_match_the_interpreter() {
    // lwarx; stwcx. (succeeds); stwcx. again (fails: no reservation); ldarx; stdcx.; stwux
    let seq: [(u32, &dyn Fn(&mut Cpu, &mut Memory)); 6] = [
        (x(31, 6, 3, 4, 20, 0), &|c, m| rec::larx(c, m, 6, 3, 4, false)),
        (x(31, 5, 3, 4, 150, 1), &|c, m| rec::stcx(c, m, 5, 3, 4, false)),
        (x(31, 5, 3, 4, 150, 1), &|c, m| rec::stcx(c, m, 5, 3, 4, false)),
        (x(31, 7, 3, 0, 84, 0), &|c, m| rec::larx(c, m, 7, 3, 0, true)),
        (x(31, 5, 3, 0, 214, 1), &|c, m| rec::stcx(c, m, 5, 3, 0, true)),
        (x(31, 5, 3, 4, 183, 0), &|c, m| rec::stwux(c, m, 5, 3, 4)),
    ];
    let (mut a, mut ma) = setup();
    let (mut b, mut mb) = setup();
    for (k, (w, native)) in seq.iter().enumerate() {
        a.exec(&mut ma, *w).unwrap();
        native(&mut b, &mut mb);
        same(&a, &b, &mut ma, &mut mb, &format!("step {k}"));
    }
    // a store-conditional to another address than the reservation fails in both
    let (mut a, mut ma) = setup();
    let (mut b, mut mb) = setup();
    a.exec(&mut ma, x(31, 6, 3, 0, 20, 0)).unwrap();
    rec::larx(&mut b, &mut mb, 6, 3, 0, false);
    a.exec(&mut ma, x(31, 5, 3, 4, 150, 1)).unwrap();
    rec::stcx(&mut b, &mut mb, 5, 3, 4, false);
    same(&a, &b, &mut ma, &mut mb, "other address");
    assert_eq!(a.cr[0] & 2, 0);
}

#[test]
fn msr_and_traps_match_the_interpreter() {
    let (mut a, mut ma) = setup();
    let (mut b, _) = setup();
    a.exec(&mut ma, x(31, 8, 0, 0, 83, 0)).unwrap(); // mfmsr r8
    rec::mfmsr(&mut b, 8);
    assert_eq!(a.gpr, b.gpr);
    a.exec(&mut ma, x(31, 8, 0, 0, 178, 0)).unwrap(); // mtmsrd: nothing changes
    assert_eq!(a.gpr, b.gpr);
    // twi / tdi: the native condition agrees with whether the interpreter traps
    for (op, to, v, simm) in [(3u32, 4u32, 7u64, 7i16), (3, 4, 7, 8), (3, 16, (-1i64) as u64, 0), (3, 1, 0xFFFF_FFFF, 0), (2, 1, 0xFFFF_FFFF, 0), (2, 31, 0, 0)] {
        let mut c = Cpu::default();
        let mut m = Memory::new();
        c.gpr[4] = v;
        let w = (op << 26) | (to << 21) | (4 << 16) | (simm as u16 as u32);
        let traps = matches!(c.exec(&mut m, w), Err(Stop::Trap { .. }));
        assert_eq!(rec::trap_hit(&c, to, 4, simm as i64, op == 3), traps, "op {op} to {to} v {v:#x} simm {simm}");
    }
}

#[test]
fn vector_ops_match_the_interpreter() {
    // every opcode 4-6 word the decoder names, and opcode 31's vector loads/stores (sampled), on registers full of varied floats,
    // NaNs, denormals and integers: `vx` leaves the same registers, CR and result as `exec`
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    let mut base = Cpu::default();
    for r in base.vr.iter_mut() {
        let lanes: [u32; 4] = std::array::from_fn(|_| match next() % 6 {
            0 => 0x7FC0_0001,                                   // NaN
            1 => (next() as u32) & 0x007F_FFFF,                 // denormal
            2 => (next() as u32 % 2000) as f32 as u32 ^ 0x8000_0000, // small integer bits
            _ => ((next() as f32 / u64::MAX as f32 - 0.5) * 1000.0).to_bits(),
        });
        *r = std::array::from_fn(|i| lanes[i / 4].to_be_bytes()[i % 4]);
    }
    base.gpr[3] = 0x4000_0000;
    base.gpr[4] = 0x13;
    let mut checked = 0;
    for _ in 0..200_000 {
        let w = [4, 5, 6, 31][(next() % 4) as usize] << 26 | (next() as u32 & 0x03FF_FFFF);
        let Some(name) = ppc::lookup(w) else { continue };
        // opcode 31: only the vector loads/stores, which `exec` also hands to its VMX routine
        if w >> 26 == 31 && !(name.starts_with("lv") || name.starts_with("stv")) {
            continue;
        }
        let (mut a, mut b) = (base.clone(), base.clone());
        let (mut ma, mut mb) = (Memory::new(), Memory::new());
        let ra = rec::vx(&mut a, &mut ma, 0x8200_0000, w, name).is_ok();
        b.pc = 0x8200_0000;
        let rb = b.exec(&mut mb, w).is_ok();
        assert_eq!(ra, rb, "{name} {w:#010x}: result");
        if !rb {
            continue;
        }
        assert!(a.vr == b.vr, "{name} {w:#010x}: vector registers");
        assert_eq!(a.cr, b.cr, "{name} {w:#010x}: CR");
        assert_eq!(a.gpr, b.gpr, "{name} {w:#010x}: registers");
        let (mut x, mut y) = ([0u8; 64], [0u8; 64]);
        ma.read(0x4000_0000, &mut x);
        mb.read(0x4000_0000, &mut y);
        assert_eq!(x, y, "{name} {w:#010x}: memory");
        checked += 1;
    }
    assert!(checked > 10_000, "only {checked} vector words checked");
}

#[test]
fn time_base_reads_match_the_interpreter() {
    // mftb rD (xo 371) and mfspr rD, TBL/TBU (xo 339), spr field split as 5+5 bits
    for (xo, spr, upper) in [(371, 268, false), (371, 269, true), (339, 268, false), (339, 269, true)] {
        let w = x(31, 7, spr & 31, spr >> 5, xo, 0);
        let (mut a, mut ma) = setup();
        a.timebase = 0x1234_5678_9ABC_DEF0;
        let (mut b, mut mb) = (a.clone(), setup().1);
        rec::mftb(&mut a, 7, upper);
        b.exec(&mut mb, w).unwrap();
        same(&a, &b, &mut ma, &mut mb, &format!("xo {xo} spr {spr}"));
    }
}

#[test]
fn byte_reversed_loads_and_stores_match_the_interpreter() {
    // lwbrx 534, lhbrx 790, ldbrx 532, stwbrx 662, sthbrx 918, stdbrx 660 (RT 6/5, RA 3, RB 4)
    type Helper = fn(&mut Cpu, &mut Memory, usize, usize, usize);
    let cases: [(u32, usize, Helper); 6] = [
        (534, 6, rec::lwbrx),
        (790, 6, rec::lhbrx),
        (532, 6, rec::ldbrx),
        (662, 5, rec::stwbrx),
        (918, 5, rec::sthbrx),
        (660, 5, rec::stdbrx),
    ];
    for (xo, rt, f) in cases {
        let (mut a, mut ma) = setup();
        let (mut b, mut mb) = setup();
        f(&mut a, &mut ma, rt, 3, 4);
        b.exec(&mut mb, x(31, rt as u32, 3, 4, xo, 0)).unwrap();
        same(&a, &b, &mut ma, &mut mb, &format!("xo {xo}"));
    }
}

#[test]
fn cr_moves_orc_rlwnm_and_spr_match_the_interpreter() {
    let fresh = || {
        let (mut c, m) = setup();
        c.gpr[6] = 0x1234_5678_9ABC_DEF0;
        c.gpr[7] = 0xE000_0003; // XER bits set; also a rotate count (3)
        c.cr = [1, 2, 4, 8, 3, 5, 0xA, 0xF];
        c.ca = true;
        (c, m)
    };
    type Helper = Box<dyn Fn(&mut Cpu)>;
    let cases: Vec<(u32, Helper)> = vec![
        // mfcr r8 (xo 19), and mfocrf r8 with FXM 0x10 (bit 11 set)
        (x(31, 8, 0, 0, 19, 0), Box::new(|c| rec::mfcr(c, 8))),
        (x(31, 8, 0, 0, 19, 0) | 1 << 20 | 0x10 << 12, Box::new(|c| rec::mfcr(c, 8))),
        // mtcrf FXM 0xA5, r6
        (x(31, 6, 0, 0, 144, 0) | 0xA5 << 12, Box::new(|c| rec::mtcrf(c, 6, 0xA5))),
        // orc. r8, r6, r7
        (x(31, 6, 8, 7, 412, 1), Box::new(|c| rec::orc(c, 8, 6, 7, true))),
        // mfspr r8, XER (spr 1) and an unknown spr (0x3F0 -> 0)
        (x(31, 8, 1, 0, 339, 0), Box::new(|c| rec::mfspr(c, 8, 1))),
        (x(31, 8, 0x10, 0x1F, 339, 0), Box::new(|c| rec::mfspr(c, 8, 0x3F0))),
        // mtspr XER, r7
        (x(31, 7, 1, 0, 467, 0), Box::new(|c| rec::mtspr(c, 7, 1))),
    ];
    for (w, f) in cases {
        let (mut a, mut ma) = fresh();
        let (mut b, mut mb) = fresh();
        f(&mut a);
        b.exec(&mut mb, w).unwrap();
        same(&a, &b, &mut ma, &mut mb, &format!("{w:#010x}"));
        assert_eq!((a.so, a.ov, a.ca), (b.so, b.ov, b.ca), "{w:#010x}: XER");
    }
    // rlwnm. r8, r6, r7, mb 4, me 27 (and a wrapping mask mb 28, me 3)
    for (mb_, me) in [(4u32, 27u32), (28, 3)] {
        let w = (23 << 26) | (6 << 21) | (8 << 16) | (7 << 11) | (mb_ << 6) | (me << 1) | 1;
        let (mut a, mut ma) = fresh();
        let (mut b, mut mb) = fresh();
        rec::rlwnm(&mut a, 8, 6, 7, guest::cpu::mask32(mb_, me), true);
        b.exec(&mut mb, w).unwrap();
        same(&a, &b, &mut ma, &mut mb, &format!("rlwnm mb {mb_} me {me}"));
    }
}
