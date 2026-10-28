//! Xenon (Xbox 360 PowerPC) instruction classification.
//!
//! Step one of the recompiler: name every instruction word so we can measure coverage
//! before writing code generation for each form.

mod lookup_gen;
pub use lookup_gen::lookup;

/// Field helpers (PowerPC numbers bits from the MSB, bit 0 = 0x8000_0000).
pub fn bits(w: u32, from: u32, to: u32) -> u32 {
    (w >> (31 - to)) & ((1 << (to - from + 1)) - 1)
}
pub fn opcd(w: u32) -> u32 {
    w >> 26
}
pub fn xo10(w: u32) -> u32 {
    bits(w, 21, 30)
}
pub fn xo9(w: u32) -> u32 {
    bits(w, 22, 30)
}

/// Mnemonic for an instruction word, or None if unknown.
pub fn mnemonic(w: u32) -> Option<&'static str> {
    Some(match opcd(w) {
        2 => "tdi",
        3 => "twi",
        4 => return vmx(w),
        5 | 6 => return vmx128(w),
        7 => "mulli",
        8 => "subfic",
        10 => "cmpli",
        11 => "cmpi",
        12 => "addic",
        13 => "addic.",
        14 => "addi",
        15 => "addis",
        16 => "bc",
        17 => "sc",
        18 => "b",
        19 => match xo10(w) {
            0 => "mcrf",
            16 => "bclr",
            33 => "crnor",
            129 => "crandc",
            150 => "isync",
            193 => "crxor",
            225 => "crnand",
            257 => "crand",
            289 => "creqv",
            417 => "crorc",
            449 => "cror",
            528 => "bcctr",
            _ => return None,
        },
        20 => "rlwimi",
        21 => "rlwinm",
        23 => "rlwnm",
        24 => "ori",
        25 => "oris",
        26 => "xori",
        27 => "xoris",
        28 => "andi.",
        29 => "andis.",
        30 => match bits(w, 27, 29) {
            0 => "rldicl",
            1 => "rldicr",
            2 => "rldic",
            3 => "rldimi",
            4 => if bits(w, 30, 30) == 0 { "rldcl" } else { "rldcr" },
            _ => return None,
        },
        31 => return op31(w),
        32 => "lwz",
        33 => "lwzu",
        34 => "lbz",
        35 => "lbzu",
        36 => "stw",
        37 => "stwu",
        38 => "stb",
        39 => "stbu",
        40 => "lhz",
        41 => "lhzu",
        42 => "lha",
        43 => "lhau",
        44 => "sth",
        45 => "sthu",
        46 => "lmw",
        47 => "stmw",
        48 => "lfs",
        49 => "lfsu",
        50 => "lfd",
        51 => "lfdu",
        52 => "stfs",
        53 => "stfsu",
        54 => "stfd",
        55 => "stfdu",
        58 => match bits(w, 30, 31) {
            0 => "ld",
            1 => "ldu",
            2 => "lwa",
            _ => return None,
        },
        59 => match bits(w, 26, 30) {
            18 => "fdivs",
            20 => "fsubs",
            21 => "fadds",
            22 => "fsqrts",
            24 => "fres",
            25 => "fmuls",
            28 => "fmsubs",
            29 => "fmadds",
            30 => "fnmsubs",
            31 => "fnmadds",
            _ => return None,
        },
        62 => match bits(w, 30, 31) {
            0 => "std",
            1 => "stdu",
            _ => return None,
        },
        63 => match bits(w, 26, 30) {
            18 => "fdiv",
            20 => "fsub",
            21 => "fadd",
            22 => "fsqrt",
            23 => "fsel",
            25 => "fmul",
            26 => "frsqrte",
            28 => "fmsub",
            29 => "fmadd",
            30 => "fnmsub",
            31 => "fnmadd",
            _ => match xo10(w) {
                0 => "fcmpu",
                12 => "frsp",
                14 => "fctiw",
                15 => "fctiwz",
                32 => "fcmpo",
                38 => "mtfsb1",
                40 => "fneg",
                64 => "mcrfs",
                70 => "mtfsb0",
                72 => "fmr",
                134 => "mtfsfi",
                136 => "fnabs",
                264 => "fabs",
                583 => "mffs",
                711 => "mtfsf",
                814 => "fctid",
                815 => "fctidz",
                846 => "fcfid",
                _ => return None,
            },
        },
        _ => return None,
    })
}

fn op31(w: u32) -> Option<&'static str> {
    // XO-form arithmetic uses a 9-bit extended opcode (bit 21 is OE)
    let a = match xo9(w) {
        8 => Some("subfc"),
        9 => Some("mulhdu"),
        10 => Some("addc"),
        11 => Some("mulhwu"),
        40 => Some("subf"),
        73 => Some("mulhd"),
        75 => Some("mulhw"),
        104 => Some("neg"),
        136 => Some("subfe"),
        138 => Some("adde"),
        200 => Some("subfze"),
        202 => Some("addze"),
        232 => Some("subfme"),
        233 => Some("mulld"),
        234 => Some("addme"),
        235 => Some("mullw"),
        266 => Some("add"),
        457 => Some("divdu"),
        459 => Some("divwu"),
        489 => Some("divd"),
        491 => Some("divw"),
        _ => None,
    };
    if a.is_some() {
        return a;
    }
    Some(match xo10(w) {
        0 => "cmp",
        4 => "tw",
        6 => "lvsl",
        7 => "lvebx",
        19 => "mfcr",
        20 => "lwarx",
        21 => "ldx",
        23 => "lwzx",
        24 => "slw",
        26 => "cntlzw",
        27 => "sld",
        28 => "and",
        32 => "cmpl",
        38 => "lvsr",
        39 => "lvehx",
        53 => "ldux",
        54 => "dcbst",
        55 => "lwzux",
        58 => "cntlzd",
        60 => "andc",
        68 => "td",
        71 => "lvewx",
        83 => "mfmsr",
        84 => "ldarx",
        86 => "dcbf",
        87 => "lbzx",
        103 => "lvx",
        119 => "lbzux",
        124 => "nor",
        135 => "stvebx",
        144 => "mtcrf",
        146 => "mtmsr",
        149 => "stdx",
        150 => "stwcx.",
        151 => "stwx",
        167 => "stvehx",
        178 => "mtmsrd",
        181 => "stdux",
        183 => "stwux",
        199 => "stvewx",
        214 => "stdcx.",
        215 => "stbx",
        231 => "stvx",
        246 => "dcbtst",
        247 => "stbux",
        278 => "dcbt",
        279 => "lhzx",
        284 => "eqv",
        311 => "lhzux",
        316 => "xor",
        339 => "mfspr",
        341 => "lwax",
        343 => "lhax",
        359 => "lvxl",
        371 => "mftb",
        373 => "lwaux",
        375 => "lhaux",
        407 => "sthx",
        412 => "orc",
        439 => "sthux",
        444 => "or",
        467 => "mtspr",
        476 => "nand",
        487 => "stvxl",
        519 => "lvlx",
        532 => "ldbrx",
        534 => "lwbrx",
        535 => "lfsx",
        536 => "srw",
        539 => "srd",
        551 => "lvrx",
        567 => "lfsux",
        598 => "sync",
        599 => "lfdx",
        631 => "lfdux",
        647 => "stvlx",
        660 => "stdbrx",
        662 => "stwbrx",
        663 => "stfsx",
        679 => "stvrx",
        695 => "stfsux",
        727 => "stfdx",
        759 => "stfdux",
        775 => "lvlxl",
        790 => "lhbrx",
        792 => "sraw",
        794 => "srad",
        807 => "lvrxl",
        824 => "srawi",
        826 | 827 => "sradi",
        854 => "eieio",
        903 => "stvlxl",
        918 => "sthbrx",
        922 => "extsh",
        935 => "stvrxl",
        954 => "extsb",
        982 => "icbi",
        983 => "stfiwx",
        986 => "extsw",
        1014 => "dcbz",
        _ => return None,
    })
}

/// Standard AltiVec (opcode 4). Grouped by form; exact names come with codegen.
fn vmx(w: u32) -> Option<&'static str> {
    let va = bits(w, 26, 31);
    if (32..=47).contains(&va) {
        return Some(match va {
            32 => "vmhaddshs",
            34 => "vmladduhm",
            42 => "vsel",
            43 => "vperm",
            44 => "vsldoi",
            46 => "vmaddfp",
            47 => "vnmsubfp",
            _ => "vmx-va",
        });
    }
    Some(match bits(w, 21, 31) {
        10 => "vaddfp",
        74 => "vsubfp",
        1028 => "vand",
        1092 => "vandc",
        1156 => "vor",
        1220 => "vxor",
        1284 => "vnor",
        _ => "vmx",
    })
}

/// VMX128 (opcodes 4/5/6 with the 128-register encodings). Classified coarsely for now.
fn vmx128(w: u32) -> Option<&'static str> {
    Some(if opcd(w) == 5 { "vmx128-5" } else { "vmx128-6" })
}

#[cfg(test)]
mod tests {
    use super::mnemonic;
    #[test]
    fn known_words() {
        assert_eq!(mnemonic(0x7c0802a6), Some("mfspr")); // mflr r0
        assert_eq!(mnemonic(0x4e800020), Some("bclr")); // blr
        assert_eq!(mnemonic(0x38210010), Some("addi"));
        assert_eq!(mnemonic(0x9421ff90), Some("stwu"));
        assert_eq!(mnemonic(0x7d8802a6), Some("mfspr"));
        assert_eq!(mnemonic(0x7c632214), Some("add"));
    }
}

#[cfg(test)]
mod lookup_tests {
    #[test]
    fn generated_table_agrees() {
        assert_eq!(super::lookup(0x7c0802a6), Some("mfspr"));
        assert_eq!(super::lookup(0x4e800020), Some("bclrx"));
        assert_eq!(super::lookup(0x7c632214), Some("addx"));
        assert_eq!(super::lookup(0x7c0008ce), Some("lvx"));
    }
}
