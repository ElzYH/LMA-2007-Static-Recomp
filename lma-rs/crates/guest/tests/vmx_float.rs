//! VMX float semantics in the interpreter, checked against the AltiVec architecture (the
//! Xenon's VMX follows it for these instructions): conversions with scale and saturation,
//! reciprocal estimates at their special values, round-to-integer, min/max.

use guest::cpu::Cpu;
use guest::mem::Memory;

/// VX-form: vD, vA (or UIMM), vB, extended opcode.
fn vx(vd: u32, va: u32, vb: u32, xo: u32) -> u32 {
    (4 << 26) | (vd << 21) | (va << 16) | (vb << 11) | xo
}

fn vec(x: [f32; 4]) -> [u8; 16] {
    let mut v = [0u8; 16];
    for i in 0..4 {
        v[4 * i..4 * i + 4].copy_from_slice(&x[i].to_bits().to_be_bytes());
    }
    v
}
fn words(v: &[u8; 16]) -> [u32; 4] {
    std::array::from_fn(|i| u32::from_be_bytes(v[4 * i..4 * i + 4].try_into().unwrap()))
}

/// Runs `w` with v1 = a, v2 = b; returns v3 as words.
fn run(w: u32, a: [f32; 4], b: [f32; 4]) -> [u32; 4] {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    c.vr[1] = vec(a);
    c.vr[2] = vec(b);
    c.exec(&mut m, w).unwrap();
    words(&c.vr[3])
}
fn floats(w: [u32; 4]) -> [f32; 4] {
    w.map(f32::from_bits)
}

#[test]
fn conversions() {
    // vctsxs v3,v2,4 (XO 970): x * 2^4 to a signed word, saturating; NaN gives 0
    let r = run(vx(3, 4, 2, 970), [0.0; 4], [1.5, -1e10, 1e10, f32::NAN]);
    assert_eq!(r.map(|x| x as i32), [24, i32::MIN, i32::MAX, 0]);
    // vctuxs (XO 906): negative saturates to 0
    let r = run(vx(3, 0, 2, 906), [0.0; 4], [-3.0, 3.7, 5e9, f32::NAN]);
    assert_eq!(r, [0, 3, u32::MAX, 0]);
    // vcfsx v3,v2,2 (XO 842): signed word / 2^2
    let mut c = Cpu::default();
    let mut m = Memory::new();
    for (i, v) in [-8i32, 6, 0, 1].iter().enumerate() {
        c.vr[2][4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    c.exec(&mut m, vx(3, 2, 2, 842)).unwrap();
    assert_eq!(floats(words(&c.vr[3])), [-2.0, 1.5, 0.0, 0.25]);
}

#[test]
fn estimates_at_special_values() {
    // vrefp (XO 266): 1/0 = +inf, 1/-0 = -inf, 1/inf = 0
    let r = floats(run(vx(3, 0, 2, 266), [0.0; 4], [0.0, -0.0, f32::INFINITY, 4.0]));
    assert_eq!(r[..3], [f32::INFINITY, f32::NEG_INFINITY, 0.0]);
    assert!((r[3] - 0.25).abs() < 0.25 / 4096.0, "within the estimate's 1/4096");
    // vrsqrtefp (XO 330): 1/sqrt(0) = +inf, of a negative number NaN
    let r = floats(run(vx(3, 0, 2, 330), [0.0; 4], [0.0, -1.0, f32::INFINITY, 4.0]));
    assert_eq!(r[0], f32::INFINITY);
    assert!(r[1].is_nan());
    assert_eq!(r[2], 0.0);
    assert!((r[3] - 0.5).abs() < 0.5 / 4096.0);
}

#[test]
fn round_to_integer() {
    // vrfin (XO 522) nearest-even, vrfiz (XO 586) toward zero, vrfip (XO 650) up, vrfim (XO 714) down
    let src = [2.5, -2.5, 1.2, -1.2];
    assert_eq!(floats(run(vx(3, 0, 2, 522), [0.0; 4], src)), [2.0, -2.0, 1.0, -1.0]);
    assert_eq!(floats(run(vx(3, 0, 2, 586), [0.0; 4], src)), [2.0, -2.0, 1.0, -1.0]);
    assert_eq!(floats(run(vx(3, 0, 2, 650), [0.0; 4], src)), [3.0, -2.0, 2.0, -1.0]);
    assert_eq!(floats(run(vx(3, 0, 2, 714), [0.0; 4], src)), [2.0, -3.0, 1.0, -2.0]);
}

#[test]
fn min_max() {
    // vmaxfp (XO 1034), vminfp (XO 1098)
    assert_eq!(floats(run(vx(3, 1, 2, 1034), [1.0, -1.0, 3.0, 0.0], [2.0, -2.0, 3.0, -0.0]))[..3], [2.0, -1.0, 3.0]);
    assert_eq!(floats(run(vx(3, 1, 2, 1098), [1.0, -1.0, 3.0, 0.0], [2.0, -2.0, 3.0, -0.0]))[..3], [1.0, -2.0, 3.0]);
    // AltiVec: a NaN operand gives a NaN (Rust's f32::max would return the other operand)
    assert!(floats(run(vx(3, 1, 2, 1034), [f32::NAN; 4], [1.0; 4]))[0].is_nan(), "vmaxfp(NaN, 1)");
    assert!(floats(run(vx(3, 1, 2, 1098), [1.0; 4], [f32::NAN; 4]))[0].is_nan(), "vminfp(1, NaN)");
    // the NaN operand itself, made quiet (vA's when both are NaN); max(-0, +0) = +0, min = -0,
    // in either order (Rust's f32::max/min may return either zero, depending on the host)
    let (qa, sb) = (f32::from_bits(0x7FC0_1234), f32::from_bits(0x7F80_0005)); // quiet A, signalling B
    let a = [qa, 1.0, sb, 0.0];
    let b = [2.0, sb, qa, -0.0];
    assert_eq!(run(vx(3, 1, 2, 1034), a, b), [0x7FC0_1234, 0x7FC0_0005, 0x7FC0_0005, 0], "vmaxfp NaN payloads, max(+0, -0)");
    assert_eq!(run(vx(3, 1, 2, 1098), a, b), [0x7FC0_1234, 0x7FC0_0005, 0x7FC0_0005, 0x8000_0000], "vminfp NaN payloads, min(+0, -0)");
    assert_eq!(run(vx(3, 1, 2, 1034), [-0.0; 4], [0.0; 4])[0], 0, "max(-0, +0)");
    assert_eq!(run(vx(3, 1, 2, 1098), [-0.0; 4], [0.0; 4])[0], 0x8000_0000, "min(-0, +0)");
}

/// The recompiled code's native conversions agree with the interpreter.
#[test]
fn native_conversions_agree() {
    let vals = [1.5f32, -1e10, 1e10, f32::NAN, -0.75, 3.999, -2147483648.0, 0.0];
    for uimm in [0u32, 4, 31] {
        for chunk in vals.chunks(4) {
            let src = vec([chunk[0], chunk[1], chunk[2], chunk[3]]);
            let mut c = Cpu::default();
            let mut m = Memory::new();
            c.vr[2] = src;
            let mut r = c.clone();
            c.exec(&mut m, vx(3, uimm, 2, 970)).unwrap();
            guest::rec::vctsxs(&mut r, 3, 2, uimm);
            assert_eq!(c.vr[3], r.vr[3], "vctsxs {chunk:?} uimm {uimm}");
            for (xo, signed) in [(842, true), (778, false)] {
                let mut c = Cpu::default();
                c.vr[2] = src;
                let mut r = c.clone();
                c.exec(&mut m, vx(3, uimm, 2, xo)).unwrap();
                guest::rec::vcfx(&mut r, 3, 2, uimm, signed);
                assert_eq!(c.vr[3], r.vr[3], "vcfx signed={signed} uimm {uimm}");
            }
        }
    }
}
