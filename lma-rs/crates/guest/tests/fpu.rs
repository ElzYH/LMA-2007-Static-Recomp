//! Scalar floating-point semantics in the interpreter, checked against the PowerPC
//! architecture: conversion to integer (saturation, NaN, truncation), single rounding, fsel,
//! fused multiply-add and unordered compares.

use guest::cpu::Cpu;
use guest::mem::Memory;

fn a_form(op: u32, frt: u32, fra: u32, frb: u32, frc: u32, xo: u32) -> u32 {
    (op << 26) | (frt << 21) | (fra << 16) | (frb << 11) | (frc << 6) | (xo << 1)
}
fn x_form(op: u32, frt: u32, fra: u32, frb: u32, xo: u32) -> u32 {
    (op << 26) | (frt << 21) | (fra << 16) | (frb << 11) | (xo << 1)
}

/// Runs `w` with f1, f2, f3 set; returns the CPU.
fn run(w: u32, f1: f64, f2: f64, f3: f64) -> Cpu {
    let mut c = Cpu::default();
    let mut m = Memory::new();
    (c.fpr[1], c.fpr[2], c.fpr[3]) = (f1, f2, f3);
    c.exec(&mut m, w).unwrap();
    c
}

/// fctiwz f4,f2: the low word of f4's bits.
fn fctiwz(v: f64) -> u32 {
    run(x_form(63, 4, 0, 2, 15), 0.0, v, 0.0).fpr[4].to_bits() as u32
}

#[test]
fn convert_to_word() {
    assert_eq!(fctiwz(3.99) as i32, 3, "toward zero");
    assert_eq!(fctiwz(-3.99) as i32, -3);
    assert_eq!(fctiwz(3e10), 0x7FFF_FFFF, "saturates high");
    assert_eq!(fctiwz(-3e10), 0x8000_0000, "saturates low");
    assert_eq!(fctiwz(f64::NAN), 0x8000_0000, "NaN gives the most negative word");
}

#[test]
fn single_rounding_and_fsel() {
    // frsp f4,f2 (XO 12): round to single precision
    let v = 1.0 + f64::EPSILON;
    assert_eq!(run(x_form(63, 4, 0, 2, 12), 0.0, v, 0.0).fpr[4], 1.0);
    // fsel f4,f1,f2,f3 (A-form XO 23, frc = f2 here): f1 >= 0 ? frc : frb
    let fsel = a_form(63, 4, 1, 3, 2, 23);
    assert_eq!(run(fsel, 0.0, 10.0, 20.0).fpr[4], 10.0, "0 counts as >= 0");
    assert_eq!(run(fsel, -0.0, 10.0, 20.0).fpr[4], 10.0, "-0 too");
    assert_eq!(run(fsel, -1.0, 10.0, 20.0).fpr[4], 20.0);
    assert_eq!(run(fsel, f64::NAN, 10.0, 20.0).fpr[4], 20.0, "NaN is not >= 0");
}

#[test]
fn fused_multiply_add() {
    // fmadd f4,f1,f2,f3 (XO 29, frc = f2): f1*f2 + f3 with one rounding
    let fmadd = a_form(63, 4, 1, 3, 2, 29);
    let (a, b) = (1.0 + f64::EPSILON, 1.0 - f64::EPSILON);
    let fused = a.mul_add(b, -1.0);
    assert_ne!(fused, a * b - 1.0, "the example needs the single rounding");
    assert_eq!(run(fmadd, a, b, -1.0).fpr[4], fused);
    // fnmsub f4,f1,f2,f3 (XO 30): -(f1*f2 - f3)
    assert_eq!(run(a_form(63, 4, 1, 3, 2, 30), 2.0, 3.0, 1.0).fpr[4], -5.0);
}

#[test]
fn unordered_compare() {
    // fcmpu cr0,f1,f2: LT GT EQ UN in CR0
    let fcmpu = (63 << 26) | (1 << 16) | (2 << 11);
    assert_eq!(run(fcmpu, 1.0, 2.0, 0.0).cr_word() >> 28, 0b1000);
    assert_eq!(run(fcmpu, 2.0, 1.0, 0.0).cr_word() >> 28, 0b0100);
    assert_eq!(run(fcmpu, -0.0, 0.0, 0.0).cr_word() >> 28, 0b0010, "-0 == 0");
    assert_eq!(run(fcmpu, f64::NAN, 1.0, 0.0).cr_word() >> 28, 0b0001, "unordered");
}

/// fctiw / fctid round as FPSCR[RN] says; fctidz truncates; NaN gives the most negative value.
#[test]
fn rounding_conversions() {
    let conv = |xo: u32, v: f64, rn: u32| {
        let mut c = Cpu::default();
        let mut m = Memory::new();
        c.fpr[2] = v;
        c.fpscr = rn;
        c.exec(&mut m, x_form(63, 4, 0, 2, xo)).unwrap();
        c.fpr[4].to_bits()
    };
    assert_eq!(conv(14, 3.6, 0) as u32 as i32, 4, "fctiw: nearest");
    assert_eq!(conv(14, 2.5, 0) as u32 as i32, 2, "ties to even");
    assert_eq!(conv(14, 3.6, 1) as u32 as i32, 3, "toward zero");
    assert_eq!(conv(14, 3.2, 2) as u32 as i32, 4, "toward +inf");
    assert_eq!(conv(14, -3.2, 3) as u32 as i32, -4, "toward -inf");
    assert_eq!(conv(814, 3.6, 0) as i64, 4, "fctid: nearest");
    assert_eq!(conv(815, -3.6, 0) as i64, -3, "fctidz: toward zero");
    assert_eq!(conv(815, f64::NAN, 0), 0x8000_0000_0000_0000);
    assert_eq!(conv(815, 1e30, 0) as i64, i64::MAX, "saturates");
}

/// The recompiled code's helper converts exactly as the interpreter does.
#[test]
fn recompiled_helper_agrees() {
    for xo in [14, 15, 814, 815] {
        for v in [3.6, -2.5, 2.5, 1e30, -1e30, f64::NAN, -0.0] {
            for rn in 0..4 {
                let mut c = Cpu::default();
                let mut m = Memory::new();
                c.fpr[2] = v;
                c.fpscr = rn;
                let mut r = c.clone();
                c.exec(&mut m, x_form(63, 4, 0, 2, xo)).unwrap();
                guest::rec::fpx(&mut r, xo, 4, 0, 0, 2);
                assert_eq!(c.fpr[4].to_bits(), r.fpr[4].to_bits(), "xo {xo} of {v} rn {rn}");
            }
        }
    }
}
