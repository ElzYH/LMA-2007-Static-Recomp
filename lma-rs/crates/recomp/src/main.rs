//! Static recompiler: PowerPC (Xenon) functions -> Rust.
//!
//!   lma-recomp <default.xex> <hot_functions.txt|all> <game-blocks dir> [max_functions] [parts] [misses.txt|tables.toml ...]
//!
//! Code that `.pdata` does not list (hand-written routines between functions) is recompiled as
//! gap pseudo-functions. Block leaders also come from every direct branch target in `.text` and
//! from `misses.txt` (pcs `lma-cli run` saw outside recompiled code, with LMA_MISSES=1). Jump-table
//! and function-pointer table targets (runs of words pointing into .text) and stub starts after
//! padding in the gaps become entry-only leaders: enterable, but run with a budget check after
//! every instruction as the interpreter did, so slices end where they always did (bit-exact).
//!
//! Every guest function (from `.pdata`) becomes one Rust function that is entered at any of its
//! basic-block leaders and loops over its blocks until control leaves the function or the
//! instruction budget runs out. Common instructions become calls to the always-inlined helpers
//! in `guest::rec`; everything else runs through the interpreter (`Cpu::exec`), so behaviour
//! always matches it. The output is split into `parts` small crates under
//! `<game-blocks dir>/parts/` so the whole game compiles in parallel with modest memory.
//! The generated code is derived from the user's own disc and must not be redistributed.

mod emit;
mod ir;
mod live;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

fn bits(w: u32, from: u32, to: u32) -> u32 {
    (w >> (31 - to)) & ((1 << (to - from + 1)) - 1)
}

/// Branch facts for block discovery: Some(static target if any) for block-ending instructions.
fn branch_info(w: u32, pc: u32) -> Option<Option<u32>> {
    match w >> 26 {
        18 => {
            let li = ((w & 0x03FF_FFFC) << 6) as i32 >> 6;
            Some(Some(if w & 2 != 0 { li as u32 } else { pc.wrapping_add(li as u32) }))
        }
        16 => {
            let bd = (w & 0xFFFC) as i16 as i32;
            Some(Some(if w & 2 != 0 { bd as u32 } else { pc.wrapping_add(bd as u32) }))
        }
        19 if matches!(bits(w, 21, 30), 16 | 528) => Some(None),
        17 => Some(None), // sc
        _ => None,
    }
}

/// Helper call for one non-branch instruction, or None to fall back to the interpreter.
/// Vector instructions translated natively (the rest go through `interp`); registers are decoded
/// here, once, for both the standard and the VMX128 forms.
fn translate_vmx(w: u32) -> Option<String> {
    // 64-bit rotates and shifts (match loading's next most frequent interpreted ops)
    match w >> 26 {
        30 if bits(w, 27, 29) <= 4 => return Some(format!("md30(c,{w:#x});")),
        31 if matches!(bits(w, 21, 30), 27 | 539 | 58 | 794 | 826 | 827) => return Some(format!("x31s(c,{w:#x});")),
        31 if bits(w, 21, 30) == 1014 => {
            let f = if bits(w, 6, 10) == 1 { "dcbz128" } else { "dcbz" };
            return Some(format!("{f}(c,m,{},{});", bits(w, 11, 15), bits(w, 16, 20)));
        }
        35 if bits(w, 11, 15) != 0 && bits(w, 11, 15) != bits(w, 6, 10) => {
            return Some(format!("lbzu(c,m,{},{},{});", bits(w, 6, 10), bits(w, 11, 15), (w & 0xFFFF) as i16 as i32))
        }
        39 if bits(w, 11, 15) != 0 => return Some(format!("stbu(c,m,{},{},{});", bits(w, 6, 10), bits(w, 11, 15), (w & 0xFFFF) as i16 as i32)),
        _ => {}
    }
    let name = ppc::lookup(w)?;
    let is128 = name.ends_with("128");
    let base = name.strip_suffix("128").unwrap_or(name);
    let d = if is128 { bits(w, 6, 10) | bits(w, 28, 29) << 5 } else { bits(w, 6, 10) };
    let (a, b) = (bits(w, 11, 15), bits(w, 16, 20));
    // vector register fields (as `vmx.rs`: std_regs / vx128_regs)
    let (vd, va, vb, vc) = if is128 {
        (d, bits(w, 11, 15) | bits(w, 26, 26) << 5 | bits(w, 21, 21) << 6, bits(w, 16, 20) | bits(w, 30, 31) << 5, bits(w, 23, 25))
    } else {
        (d, bits(w, 11, 15), bits(w, 16, 20), bits(w, 21, 25))
    };
    match base {
        "lvx" | "lvxl" => Some(format!("vlvx(c,m,{d},{a},{b});")),
        "stvx" | "stvxl" => Some(format!("vstvx(c,m,{d},{a},{b});")),
        "lvlx" | "lvlxl" => Some(format!("vlvlx(c,m,{d},{a},{b});")),
        "lvrx" | "lvrxl" => Some(format!("vlvrx(c,m,{d},{a},{b});")),
        "vor" => Some(format!("vor(c,{vd},{va},{vb});")),
        "vand" | "vandc" | "vxor" | "vaddfp" | "vsubfp" | "vmulfp" | "vmsum3fp" => Some(format!("{base}(c,{vd},{va},{vb});")),
        // standard: a*c + b; VMX128: a*b + d
        "vmaddfp" => Some(if is128 { format!("vmaddfp(c,{vd},{va},{vb},{vd});") } else { format!("vmaddfp(c,{vd},{va},{vc},{vb});") }),
        "vsldoi" => Some(format!("vsldoi(c,{vd},{va},{vb},{});", bits(w, 22, 25))),
        "vsel" => Some(format!("vsel(c,{vd},{va},{vb},{});", if is128 { vd } else { vc })),
        "vmrghw" => Some(format!("vmrgw(c,{vd},{va},{vb},true);")),
        "vmrglw" => Some(format!("vmrgw(c,{vd},{va},{vb},false);")),
        "vslo" => Some(format!("vso(c,{vd},{va},{vb},true);")),
        "vsro" => Some(format!("vso(c,{vd},{va},{vb},false);")),
        "stvewx" => Some(format!("vstvewx(c,m,{d},{a},{b});")),
        "vspltisw" => Some(format!("vsplatw(c,{vd},{:#x});", ((a as i32) << 27 >> 27) as u32)),
        "vcfsx" | "vcsxwfp" => Some(format!("vcfx(c,{vd},{vb},{a},true);")),
        "vcfux" | "vcuxwfp" => Some(format!("vcfx(c,{vd},{vb},{a},false);")),
        "vctsxs" | "vcfpsxws" => Some(format!("vctsxs(c,{vd},{vb},{a});")),
        "vrlimi" => Some(format!("vrlimi(c,{vd},{vb},{a},{});", bits(w, 24, 25))),
        "vperm" => Some(format!("vperm(c,{vd},{va},{vb},{vc});")),
        // integer lane ops (#204: the video decoder's IDCT and reconstruction)
        "vadduhm" | "vsubuhm" | "vaddshs" | "vsubshs" | "vslh" | "vsrh" | "vsrah" | "vrlh" | "vaddsws" | "vsubsws" | "vsraw" | "vpkshus" | "vpkswss" => {
            Some(format!("{base}(c,{vd},{va},{vb});"))
        }
        "vmrghb" | "vmrglb" => Some(format!("vmrgb(c,{vd},{va},{vb},{});", base == "vmrghb")),
        "vmrghh" | "vmrglh" => Some(format!("vmrgh(c,{vd},{va},{vb},{});", base == "vmrghh")),
        "vupkhsh" | "vupklsh" => Some(format!("vupksh(c,{vd},{vb},{});", base == "vupkhsh")),
        "vspltish" => Some(format!("vsplath(c,{vd},{:#x});", ((a as i32) << 27 >> 27) as i16 as u16)),
        "vspltisb" => Some(format!("vsplatb(c,{vd},{:#x});", ((a as i32) << 27 >> 27) as i8 as u8)),
        "vspltb" | "vsplth" | "vspltw" => {
            let size = match base { "vspltb" => 1, "vsplth" => 2, _ => 4 };
            Some(format!("vsplte(c,{vd},{vb},{size},{a});"))
        }
        _ => None,
    }
}

/// Instructions `Cpu::exec` hands to its VMX routine: opcodes 4-6, and opcode 31's lv*/stv*.
fn is_vector(w: u32) -> bool {
    match ppc::lookup(w) {
        Some(name) => matches!(w >> 26, 4..=6) || (w >> 26 == 31 && (name.starts_with("lv") || name.starts_with("stv"))),
        None => false,
    }
}

/// The pcs of a `misses.txt`: the first word of each line as hex.
fn misses(text: &str) -> Vec<u32> {
    text.lines().filter_map(|l| l.split_whitespace().next().and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok())).collect()
}

/// The jump targets of a XenonAnalyse TOML: every hex word inside a `labels = [ ... ]` list.
fn switch_targets(text: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let mut in_labels = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with("labels") {
            in_labels = true;
        }
        if in_labels {
            for tok in t.split(|c: char| !c.is_ascii_hexdigit() && c != 'x' && c != 'X') {
                if let Some(h) = tok.strip_prefix("0x").or_else(|| tok.strip_prefix("0X")) {
                    if h.len() == 8 {
                        if let Ok(v) = u32::from_str_radix(h, 16) {
                            out.push(v);
                        }
                    }
                }
            }
            if t.ends_with(']') {
                in_labels = false;
            }
        }
    }
    out
}

/// The mods' run-time values `translate` may emit (#145: the IR keeps such an op's text).
const MOD_VALUES: [&str; 9] = ["base_year()", "subs_limit()", "value_cap()", "value_unit()", "wage_budget_cap()", "offer_cap()", "wage_offer_cap()", "formation_roles", "formation_names"];

pub(crate) fn translate(pc: u32, w: u32) -> Option<String> {
    if let Some(s) = translate_vmx(w) {
        return Some(s);
    }
    let d = bits(w, 6, 10);
    let a = bits(w, 11, 15);
    let b = bits(w, 16, 20);
    let simm = (w & 0xFFFF) as i16 as i32;
    let uimm = w & 0xFFFF;
    let rc = w & 1 == 1;
    // the base year (#31): a mod space may start the game in another season, so the sites that
    // load or compare the immediate 2006 read `rec::base_year()` instead, and the four sites with
    // 2026 (the last of the game's 20 diary seasons: the date validators' upper bound, which
    // otherwise rejects 1 Jan of the first season's second year) read `base_year() + 20`
    // and the three `addi rD,rA,2007` whose rA is a loaded season index (the second half of a
    // season label at 0x821bf6e8 and 0x821fcd58, the season count under the 2006 guard at
    // 0x820dccf4) read `base_year() + 1`; a `li` of 2007 is an id (#706)
    let year = |imm: i64, s: String| match imm {
        _ if guest::rec::NOT_YEAR_SITES.contains(&pc) => s,
        2006 => s.replace("2006", "base_year()"),
        2007 if w >> 26 == 14 && a != 0 => s.replace("2007", "(base_year()+1)"),
        2026 => s.replace("2026", "(base_year()+20)"),
        _ => s,
    };
    // a moddable number (#870): the immediate comes from guest::tune's table, set by mod.toml's [tune]
    if let Some(i) = guest::tune::at(pc, w) {
        let (crf, l) = (bits(w, 6, 8), bits(w, 10, 10) == 1);
        return Some(match w >> 26 {
            7 => format!("mulli(c,{d},{a},tune({i}));"),
            8 => format!("subfic(c,{d},{a},tune({i}) as u64);"),
            10 => format!("cmpli(c,{crf},{l},{a},tune({i}) as u64);"),
            11 => format!("cmpi(c,{crf},{l},{a},tune({i}));"),
            14 => format!("addi(c,{d},{a},tune({i}));"),
            24 => format!("ori(c,{a},{d},tune({i}) as u64);"),
            op => panic!("tunable {} at {pc:#x}: opcode {op} is not supported", guest::tune::TUNABLES[i].name),
        });
    }
    Some(match w >> 26 {
        // a competitive match's substitution limit (3 in vanilla), from mod.toml (#530)
        14 if pc == guest::rec::SUBS_SITE && a == 0 && simm == 3 => format!("addi(c,{d},0,subs_limit());"),
        // the quick-sim planner's 2-or-3 subs follow it: subs - 1 and subs
        14 if pc == guest::rec::SUBS_SIM_LOW && a == 0 && simm == 2 => format!("addi(c,{d},0,subs_limit()-1);"),
        14 if pc == guest::rec::SUBS_SIM_HIGH && a == 0 && simm == 3 => format!("addi(c,{d},0,subs_limit());"),
        // a mod's formations (runtime::mods, formations.csv): the role table's address at its
        // lis/addi pairs (the shared site's addi takes the whole address), the two name tables' pairs,
        // the caption's id-list pair, the diagram's spot-table pair and the nine count sites (`li rD,14`, `cmpwi ..,14`)
        14 if simm == 3928 && guest::rec::FORMATION_ROLES_SITES.iter().any(|s| s.1 == pc) => format!("addi(c,{d},{a},formation_roles_lo());"),
        14 if simm == 3928 && pc == guest::rec::FORMATION_ROLES_SHARED.1 => format!("addi(c,{d},0,formation_roles());"),
        14 if simm == 9200 && pc == guest::rec::FORMATION_NAMES_SITE.1 => format!("addi(c,{d},{a},formation_names_lo());"),
        14 if simm == 13756 && pc == guest::rec::FORMATION_NAMES2_SITE.1 => format!("addi(c,{d},{a},formation_names_lo());"),
        14 if simm == 32204 && pc == guest::rec::FORMATION_IDS_SITE.1 => format!("addi(c,{d},{a},formation_ids_lo());"),
        14 if simm == -19600 && pc == guest::rec::FORMATION_SPOTS_SITE.1 => format!("addi(c,{d},{a},formation_spots_lo());"),
        14 if simm == -13000 && guest::rec::FORMATION_ORDER_SITES.iter().any(|s| s.1 == pc) => format!("addi(c,{d},{a},formation_order_lo());"),
        14 if simm == 14 && a == 0 && guest::rec::FORMATION_COUNT_LI_SITES.contains(&pc) => format!("addi(c,{d},0,formation_count());"),
        14 => year(simm as i64, format!("addi(c,{d},{a},{simm});")),
        15 if a == 0 && uimm == 0x8271 && guest::rec::FORMATION_ROLES_SITES.iter().any(|s| s.0 == pc) => format!("addi(c,{d},0,formation_roles_hi());"),
        15 if a == 0 && uimm == 0x8270 && pc == guest::rec::FORMATION_NAMES_SITE.0 => format!("addi(c,{d},0,formation_names_hi());"),
        15 if a == 0 && uimm == 0x8271 && pc == guest::rec::FORMATION_NAMES2_SITE.0 => format!("addi(c,{d},0,formation_names_hi());"),
        15 if a == 0 && uimm == 0x8202 && pc == guest::rec::FORMATION_IDS_SITE.0 => format!("addi(c,{d},0,formation_ids_hi());"),
        15 if a == 0 && uimm == 0x8201 && pc == guest::rec::FORMATION_SPOTS_SITE.0 => format!("addi(c,{d},0,formation_spots_hi());"),
        15 if a == 0 && uimm == 0x8203 && guest::rec::FORMATION_ORDER_SITES.iter().any(|s| s.0 == pc) => format!("addi(c,{d},0,formation_order_hi());"),
        15 => format!("addi(c,{d},{a},{});", (simm as i64) << 16),
        // the game's valuation cap (GBP 40m in vanilla) scales with the stored value's unit (#31)
        24 if uimm == 0x5a00 && pc == guest::rec::VALUE_CAP_SITE => format!("addi(c,{a},0,value_cap());"),
        // the clubs' weekly wage budget cap (GBP 3.2m in vanilla), from mod.toml
        24 if uimm == 0xd400 && pc == guest::rec::WAGE_CAP_SITE => format!("addi(c,{a},0,wage_budget_cap());"),
        // the transfer offer cap (GBP 80m in vanilla, seven sites) scales with the unit too
        24 if uimm == 0xb400 && guest::rec::OFFER_CAP_SITES.iter().any(|s| s.1 == pc) => format!("addi(c,{a},0,offer_cap());"),
        // a contract offer's weekly wage cap (GBP 250,000 in vanilla, twelve sites), from mod.toml
        24 if uimm == 0xd090 && guest::rec::WAGE_OFFER_CAP_SITES.iter().any(|s| s.1 == pc) => format!("addi(c,{a},0,wage_offer_cap());"),
        // an AI club's signing wage ceiling (GBP 100,000 in vanilla, the completion check), from mod.toml
        24 if uimm == 0x86a0 && pc == guest::rec::AI_SIGNING_WAGE_CAP_SITE.1 => format!("addi(c,{a},0,ai_signing_wage_cap());"),
        24 => format!("ori(c,{a},{d},{uimm});"),
        25 => format!("ori(c,{a},{d},{});", (uimm as u64) << 16),
        26 => format!("xori(c,{a},{d},{uimm});"),
        27 => format!("xori(c,{a},{d},{});", (uimm as u64) << 16),
        28 => format!("andi(c,{a},{d},{uimm});"),
        29 => format!("andi(c,{a},{d},{});", (uimm as u64) << 16),
        // the stored player value's unit (#31): the two `mulli ..,1000` after the getter read it
        7 if simm == 1000 && guest::rec::VALUE_UNIT_SITES.contains(&pc) => format!("mulli(c,{d},{a},value_unit());"),
        7 => format!("mulli(c,{d},{a},{simm});"),
        8 => format!("subfic(c,{d},{a},{:#x});", simm as i64 as u64),
        12 | 13 => format!("addic(c,{d},{a},{:#x},{});", simm as i64 as u64, w >> 26 == 13),
        59 | 63 => {
            // same decode order as Cpu::fp: A-form arithmetic first, then the X-form table
            let single = w >> 26 == 59;
            let xo5 = bits(w, 26, 30);
            if matches!(xo5, 18 | 20 | 21 | 22 | 24 | 25 | 26 | 28 | 29 | 30 | 31) || (xo5 == 23 && !single) {
                format!("fpa(c,{xo5},{single},{d},{a},{b},{});", bits(w, 21, 25))
            } else {
                match bits(w, 21, 30) {
                    x @ (0 | 32 | 12 | 14 | 15 | 814 | 815 | 846 | 40 | 72 | 136 | 264 | 583 | 711 | 38 | 70 | 134 | 64) => {
                        format!("fpx(c,{x},{d},{},{a},{b});", bits(w, 6, 8))
                    }
                    _ => return None,
                }
            }
        }
        10 => year(uimm as i64, format!("cmpli(c,{},{},{a},{uimm});", bits(w, 6, 8), bits(w, 10, 10) == 1)).replace("base_year()", "base_year() as u64"),
        11 if simm == 1000 && pc == guest::rec::VALUE_FLOOR_SITE => format!("cmpi(c,{},{},{a},value_floor());", bits(w, 6, 8), bits(w, 10, 10) == 1),
        // a cup draw that can't fill ends instead of spinning (#735, guest::rec::DRAW_FILL_*)
        11 if simm == 28 && pc == guest::rec::DRAW_FILL_FIRST => format!("cmpi(c,{},{},{a},draw_fill_first());", bits(w, 6, 8), bits(w, 10, 10) == 1),
        11 if simm == 28 && pc == guest::rec::DRAW_FILL_LOOP => format!("cmpi(c,{},{},{a},draw_fill_target(c.gpr[{a}] as i32 as i64));", bits(w, 6, 8), bits(w, 10, 10) == 1),
        11 if simm == 14 && guest::rec::FORMATION_COUNT_CMP_SITES.contains(&pc) => format!("cmpi(c,{},{},{a},formation_count());", bits(w, 6, 8), bits(w, 10, 10) == 1),
        11 => year(simm as i64, format!("cmpi(c,{},{},{a},{simm});", bits(w, 6, 8), bits(w, 10, 10) == 1)),
        23 => {
            let mask = guest::cpu::mask32(bits(w, 21, 25), bits(w, 26, 30));
            format!("rlwnm(c,{a},{d},{b},{mask:#x},{rc});")
        }
        20 | 21 => {
            let (sh, mb, me) = (bits(w, 16, 20), bits(w, 21, 25), bits(w, 26, 30));
            let mask = guest::cpu::mask32(mb, me);
            let f = if w >> 26 == 21 { "rlwinm" } else { "rlwimi" };
            format!("{f}(c,{a},{d},{sh},{mask:#x},{rc});")
        }
        32 => format!("lwz(c,m,{d},{a},{simm});"),
        33 if a != 0 && a != d => format!("lwzu(c,m,{d},{a},{simm});"),
        34 => format!("lbz(c,m,{d},{a},{simm});"),
        40 => format!("lhz(c,m,{d},{a},{simm});"),
        42 => format!("lha(c,m,{d},{a},{simm});"),
        36 => format!("stw(c,m,{d},{a},{simm});"),
        37 if a != 0 => format!("stwu(c,m,{d},{a},{simm});"),
        38 => format!("stb(c,m,{d},{a},{simm});"),
        44 => format!("sth(c,m,{d},{a},{simm});"),
        48 => format!("lfs(c,m,{d},{a},{simm});"),
        50 => format!("lfd(c,m,{d},{a},{simm});"),
        52 => format!("stfs(c,m,{d},{a},{simm});"),
        54 => format!("stfd(c,m,{d},{a},{simm});"),
        58 | 62 => {
            let ds = (w & 0xFFFC) as i16 as i32;
            match (w >> 26, w & 3) {
                (58, 0) => format!("ld(c,m,{d},{a},{ds});"),
                (58, 1) if a != 0 && a != d => format!("ldu(c,m,{d},{a},{ds});"),
                (58, 2) => format!("lwa(c,m,{d},{a},{ds});"),
                (62, 0) => format!("std_(c,m,{d},{a},{ds});"),
                (62, 1) if a != 0 => format!("stdu(c,m,{d},{a},{ds});"),
                _ => return None,
            }
        }
        31 => {
            let xo = bits(w, 21, 30);
            // the interpreter matches XO-form arithmetic by the 9-bit XO first (OE ignored)
            if guest::rec::XO_ARITH.contains(&bits(w, 22, 30)) {
                return Some(format!("xo(c,{},{d},{a},{b},{rc});", bits(w, 22, 30)));
            }
            if bits(w, 21, 21) == 0 {
                // XO-form without OE
                let f = match bits(w, 22, 30) {
                    266 => Some("add"),
                    40 => Some("subf"),
                    235 => Some("mullw"),
                    _ => None,
                };
                if let Some(f) = f {
                    return Some(format!("{f}(c,{d},{a},{b},{rc});"));
                }
                if bits(w, 22, 30) == 104 {
                    return Some(format!("neg(c,{d},{a},{rc});"));
                }
            }
            let f = bits(w, 6, 8);
            let l = bits(w, 10, 10) == 1;
            match xo {
                // matches were left with these in the interpreter (perf plan item 3)
                83 => format!("mfmsr(c,{d});"),
                146 | 178 => String::new(), // mtmsr / mtmsrd: no effect here
                20 => format!("larx(c,m,{d},{a},{b},false);"),
                84 => format!("larx(c,m,{d},{a},{b},true);"),
                150 => format!("stcx(c,m,{d},{a},{b},false);"),
                214 => format!("stcx(c,m,{d},{a},{b},true);"),
                183 if a != 0 => format!("stwux(c,m,{d},{a},{b});"),
                824 => format!("sraw(c,true,{a},{d},{b},{b},{rc});"),
                792 => format!("sraw(c,false,{a},{d},{b},0,{rc});"),
                535 | 567 | 599 | 631 | 663 | 695 | 727 | 759 | 983 => format!("fpx_mem(c,m,{xo},{d},{a},{b});"),
                444 => format!("or(c,{a},{d},{b},{rc});"),
                28 => format!("and(c,{a},{d},{b},{rc});"),
                60 => format!("andc(c,{a},{d},{b},{rc});"),
                316 => format!("xor(c,{a},{d},{b},{rc});"),
                124 => format!("nor(c,{a},{d},{b},{rc});"),
                24 => format!("slw(c,{a},{d},{b},{rc});"),
                536 => format!("srw(c,{a},{d},{b},{rc});"),
                922 => format!("extsh(c,{a},{d},{rc});"),
                954 => format!("extsb(c,{a},{d},{rc});"),
                986 => format!("extsw(c,{a},{d},{rc});"),
                26 => format!("cntlzw(c,{a},{d},{rc});"),
                0 => format!("cmp(c,{f},{l},{a},{b});"),
                32 => format!("cmpl(c,{f},{l},{a},{b});"),
                339 if (b << 5) | a == 8 => format!("mflr(c,{d});"),
                339 if (b << 5) | a == 9 => format!("mfctr(c,{d});"),
                // mftb / mfspr TBL, TBU: the timebase as set at function entry (as the interpreter reads it)
                339 | 371 if matches!((b << 5) | a, 268 | 269) => format!("mftb(c,{d},{});", (b << 5) | a == 269),
                467 if (b << 5) | a == 8 => format!("mtlr(c,{d});"),
                467 if (b << 5) | a == 9 => format!("mtctr(c,{d});"),
                // other SPRs (XER; the rest read 0), CR moves, orc
                339 => format!("mfspr(c,{d},{});", (b << 5) | a),
                467 => format!("mtspr(c,{d},{});", (b << 5) | a),
                19 => format!("mfcr(c,{d});"),
                144 => format!("mtcrf(c,{d},{});", bits(w, 12, 19)),
                412 => format!("orc(c,{a},{d},{b},{rc});"),
                23 => format!("lwzx(c,m,{d},{a},{b});"),
                // byte-reversed loads and stores
                534 => format!("lwbrx(c,m,{d},{a},{b});"),
                790 => format!("lhbrx(c,m,{d},{a},{b});"),
                532 => format!("ldbrx(c,m,{d},{a},{b});"),
                662 => format!("stwbrx(c,m,{d},{a},{b});"),
                918 => format!("sthbrx(c,m,{d},{a},{b});"),
                660 => format!("stdbrx(c,m,{d},{a},{b});"),
                87 => format!("lbzx(c,m,{d},{a},{b});"),
                279 => format!("lhzx(c,m,{d},{a},{b});"),
                343 => format!("lhax(c,m,{d},{a},{b});"),
                21 => format!("ldx(c,m,{d},{a},{b});"),
                341 => format!("lwax(c,m,{d},{a},{b});"),
                151 => format!("stwx(c,m,{d},{a},{b});"),
                215 => format!("stbx(c,m,{d},{a},{b});"),
                407 => format!("sthx(c,m,{d},{a},{b});"),
                149 => format!("stdx(c,m,{d},{a},{b});"),
                54 | 86 | 246 | 278 | 982 => String::new(), // cache hints
                // sync / lwsync, eieio: host fences while guest threads run on several host threads (#39)
                598 => format!("barrier({});", d & 3 == 0), // sync (L = 0) full, lwsync (L = 1)
                854 => "barrier(false);".to_string(), // eieio
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// A block body with the budget in the function's local `bv` instead of behind `b` (M1, #6):
/// `*b` reads and writes become `bv`; a call gets the parameter `bp` (written back first, read
/// after), and returns and early exits (`?`) break out to the function's one exit, which writes
/// `bv` back: the caller's clock is exact on every path, errors included.
fn budget_in_local(body: &str) -> String {
    let s = body.replace("*b", "bv");
    // fallible helpers: write back before them (only `call` and `call_local` change the budget)
    let mut out = String::with_capacity(s.len() + 64);
    let mut rest = s.as_str();
    while let Some(i) = rest.find(")?") {
        // the call expression: back from `)?` to the start of its name
        let head = &rest[..i + 1];
        let start = ["call(", "call_local(", "interp(", "vx("]
            .iter()
            .filter_map(|n| head.rfind(n).filter(|&k| k == 0 || !head.as_bytes()[k - 1].is_ascii_alphanumeric() && head.as_bytes()[k - 1] != b'_'))
            .max()
            .expect("a fallible call before `)?`");
        let expr = &head[start..];
        out.push_str(&head[..start]);
        if expr.starts_with("call") {
            let expr = expr.replacen(",b)", ",bp)", 1).replacen(",b,", ",bp,", 1);
            let _ = write!(out, "match {{*bp=bv;let r={expr};bv=*bp;r}}{{Ok(v)=>v,Err(e)=>break 'f Err(e)}}");
        } else {
            let _ = write!(out, "match {expr}{{Ok(v)=>v,Err(e)=>break 'f Err(e)}}");
        }
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    // returns: `return Ok(X)` -> `break 'f Ok(X)` (the function writes `bv` back once, at its end)
    let mut res = String::with_capacity(out.len() + 64);
    let mut rest = out.as_str();
    while let Some(i) = rest.find("return Ok(") {
        let close = i + rest[i..].find(')').expect("return Ok(..)");
        let _ = write!(res, "{}break 'f {}", &rest[..i], &rest[i + 7..=close]);
        rest = &rest[close + 1..];
    }
    res.push_str(rest);
    res
}

/// `call(c,m,0xT,b)?` -> `call_local(c,m,0xT,b,fT)?` when T starts a function in the same crate.
fn local_calls(body: &str, local: &std::collections::HashSet<u32>) -> String {
    let mut out = String::with_capacity(body.len() + 32);
    let mut rest = body;
    while let Some(i) = rest.find("call(c,m,0x") {
        let after = &rest[i + "call(c,m,".len()..];
        let hex = after.split(',').next().unwrap_or("");
        let t = u32::from_str_radix(hex.trim_start_matches("0x"), 16).ok();
        let whole = format!("call(c,m,{hex},b)?");
        // only the exact direct form, and not `call_local` already
        if rest[..i].ends_with('_') || !rest[i..].starts_with(&whole) || !t.is_some_and(|t| local.contains(&t)) {
            out.push_str(&rest[..i + 1]);
            rest = &rest[i + 1..];
            continue;
        }
        out.push_str(&rest[..i]);
        out.push_str(&format!("call_local(c,m,{hex},b,f{:08x})?", t.unwrap()));
        rest = &rest[i + whole.len()..];
    }
    out.push_str(rest);
    out
}

/// The callees of the `call_local(c,m,0xT,b,fT)?` calls in a block.
fn direct_targets(body: &str) -> impl Iterator<Item = u32> + '_ {
    body.match_indices("call_local(c,m,0x").filter_map(|(i, k)| {
        let hex = body[i + k.len()..].split(',').next()?;
        u32::from_str_radix(hex, 16).ok()
    })
}

/// A part's declarations of the functions it calls in other parts (exported as `lma_f<addr>`).
fn extern_decls(ext: &BTreeSet<u32>) -> String {
    if ext.is_empty() {
        return String::new();
    }
    let mut s = String::from("unsafe extern \"Rust\" {\n");
    for t in ext {
        let _ = writeln!(s, "#[link_name=\"lma_f{t:08x}\"] safe fn f{t:08x}(c:&mut Cpu,m:&mut Memory,pc:u32,b:&mut i64)->Result<u32,Stop>;");
    }
    s.push_str("}\n");
    s
}

/// Expression for a block-ending branch, evaluating to the next guest pc.
fn translate_branch(w: u32, pc: u32) -> String {
    let next = pc.wrapping_add(4);
    let link = if w & 1 == 1 { format!("c.lr={next:#x};") } else { String::new() };
    let (bo, bi) = (bits(w, 6, 10), bits(w, 11, 15));
    match (w >> 26, bits(w, 21, 30)) {
        (18, _) | (16, _) => {
            let t = branch_info(w, pc).flatten().unwrap();
            if w >> 26 == 18 || bo & 0x14 == 0x14 {
                // unconditional call: enter the callee natively (rec::call), else jump
                if w & 1 == 1 { format!("{link}call(c,m,{t:#x},b)?") } else { format!("{link}{t:#x}") }
            } else {
                format!("let t=c.branch_cond({bo},{bi});{link}if t{{{t:#x}}}else{{{next:#x}}}")
            }
        }
        (19, 16) => format!("let t=bclr(c,{bo},{bi}).unwrap_or({next:#x});{link}t"),
        // bctrl that always branches (virtual / function-pointer calls): native call as well
        (19, 528) if w & 1 == 1 && bo & 0x14 == 0x14 => format!("let t=c.ctr as u32&!3;{link}call(c,m,t,b)?"),
        (19, 528) => format!("let t=bcctr(c,{bo},{bi}).unwrap_or({next:#x});{link}t"),
        _ => format!("interp_branch(c,m,{pc:#x},{w:#x})?"),
    }
}

/// Inline caching for one block (see the pass in `main`): a block ending in a bctrl call
/// (`...c.lr=0xRET;call(c,m,t,b)?`) at a site of `hot`, rewritten as a match on t with an arm per
/// leaf target (inlined as a direct call's leaf: the budget exit to t, the timebase on return) and
/// rec::call for the rest; with the number of arms. None: not such a block, or no leaf target.
fn cache_indirect(body: &str, hot: &BTreeMap<u32, Vec<u32>>, leaves: &std::collections::HashMap<u32, String>, save_at: &std::collections::HashMap<u32, u32>) -> Option<(String, usize)> {
    let head = body.strip_suffix("call(c,m,t,b)?")?;
    let ret = head.rsplit_once("c.lr=0x").and_then(|(_, r)| u32::from_str_radix(r.trim_end_matches(';'), 16).ok())?;
    let targets = hot.get(&ret.wrapping_sub(4))?;
    let mut arms = String::new();
    let mut n = 0;
    for &t in targets {
        // a hooked function runs its hook from the dispatcher (rec::call hands it over): never inline it
        if guest::hooks::ADDRESSES.contains(&t) {
            continue;
        }
        let Some(leaf) = leaves.get(&t) else { continue };
        let leaf = match save_at.get(&t) {
            Some(&k) => format!("if *b>{m}{{save_gprlr(c,m,{k});*b-={m};c.lr as u32&!3}}else{{{leaf}}}", m = 34 - k),
            None => leaf.clone(),
        };
        let _ = write!(arms, "{t:#x}=>{{if *b<=0{{return Ok({t:#x})}}let r={{{leaf}}};c.timebase=timebase_at(c,*b);r}}");
        n += 1;
    }
    (n > 0).then(|| (format!("{head}match t{{{arms}_=>call(c,m,t,b)?}}"), n))
}

/// The night match's hot bctrl sites and their targets (LMA_CALL_TARGETS=1 over 10 G of
/// match_night, 22 Oct: these five sites made 96% of its 27.7 M indirect calls; the D3D material
/// binder's setters and the 0x823bf134 dispatch). Used when no `.calls` file is given.
const HOT_CALLS: &[(u32, &[u32])] = &[
    (0x8222_ddb4, &[0x8233_c9a0, 0x8233_c818, 0x8233_c690, 0x8233_ce10, 0x8233_cdc0, 0x8233_cd50]),
    (0x823b_f134, &[0x823b_fee0, 0x823b_feb0, 0x8232_3950, 0x823b_fea0, 0x8238_a798, 0x8232_3940, 0x823b_f680, 0x823b_fdf8, 0x823b_fd98, 0x823b_fce8, 0x823b_f6d8, 0x823b_fbb0, 0x823b_f9a8, 0x823b_f920, 0x823b_f730, 0x823b_0480, 0x8232_3390, 0x823b_f780, 0x823b_fe10]),
    (0x8222_dc50, &[0x8233_aee8, 0x8233_b098, 0x8233_b008, 0x8233_b4c0]),
    (0x8240_0f64, &[0x8268_97f8]),
    (0x8240_0f14, &[0x8240_09d0, 0x823f_5660]),
];

struct Func {
    start: u32,
    end: u32,
    blocks: Vec<(u32, String)>, // leader -> Rust statements ending in the next-pc expression
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: lma-recomp <default.xex> <hot_functions.txt|all> <game-blocks dir> [max_functions] [parts] [misses.txt|tables.toml ...]");
        std::process::exit(2);
    }
    let x = xex::Xex::parse(&std::fs::read(&a[0]).expect("read xex")).expect("parse xex");
    let max_funcs: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(usize::MAX);
    let parts: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(32).max(1);
    if parts != 32 {
        eprintln!("note: {parts} parts: Cargo.lock will differ from the committed one, which matches the 32 stub parts (do not commit it)");
    }

    // import thunks (the runtime runs these as kernel HLE, not as recompiled code)
    let thunks: std::collections::HashSet<u32> = x.imports().iter().filter(|i| i.kind == 1).map(|i| i.address).collect();

    let (pva, psize) = x.sections().into_iter().find(|s| s.0 == ".pdata").map(|s| (s.1, s.2)).expect(".pdata");
    let mut all: BTreeMap<u32, u32> = BTreeMap::new();
    for i in 0..psize / 8 {
        let start = x.read_u32(pva + i * 8).unwrap_or(0);
        let packed = x.read_u32(pva + i * 8 + 4).unwrap_or(0);
        if start != 0 {
            all.insert(start, ((packed >> 8) & 0x3F_FFFF) * 4);
        }
    }
    // gaps in .text not covered by .pdata become pseudo-functions
    let (tva, tsize) = x.sections().into_iter().find(|s| s.0 == ".text").map(|s| (s.1, s.2)).expect(".text");
    let mut gaps: BTreeMap<u32, u32> = BTreeMap::new();
    let mut cursor = tva;
    for (&f, &len) in &all {
        if f > cursor && f <= tva + tsize {
            gaps.insert(cursor, f - cursor);
        }
        cursor = cursor.max(f + len);
    }
    if cursor < tva + tsize {
        gaps.insert(cursor, tva + tsize - cursor);
    }
    // extra leaders: every direct branch target, plus observed misses
    let mut extra: BTreeSet<u32> = BTreeSet::new();
    for pc in (tva..tva + tsize).step_by(4) {
        if let Some(Some(t)) = branch_info(x.read_u32(pc).unwrap_or(0), pc) {
            extra.insert(t);
        }
    }
    // switch tables (inline in .text) and function-pointer tables (vtables in the data sections):
    // runs of two or more words pointing into .text. Their targets become leaders, so they run
    // recompiled instead of one instruction at a time in the interpreter (jump-table targets were
    // ~300 M interpreted steps per 60 G of the night match, ~4% of its time, #6). Inside .text a
    // table entry points near itself (within 64 KB), which keeps runs of ordinary instructions that
    // look like addresses out. Targets that aren't leaders anyway become entry-only leaders (below):
    // code may jump there, but straight-line code runs through them without a budget check, so
    // slices end exactly where they did before (bit-exact).
    let in_text = |w: u32| w >= tva && w < tva + tsize && w & 3 == 0;
    let mut tables: BTreeSet<u32> = BTreeSet::new();
    for (name, va, size, _) in x.sections() {
        if name == ".pdata" {
            continue;
        }
        let code = name == ".text";
        let mut run: Vec<u32> = Vec::new();
        for at in (va..va + size).step_by(4).chain(std::iter::once(va + size)) {
            let w = if at < va + size { x.read_u32(at).unwrap_or(0) } else { 0 };
            if in_text(w) && (!code || w.abs_diff(at) < 0x1_0000) {
                run.push(w);
                continue;
            }
            if run.len() >= 2 {
                tables.extend(run.iter().copied());
            }
            run.clear();
        }
    }
    // more leaders from files: `misses.txt` (one hex pc a line, from LMA_MISSES=1 runs) become
    // block leaders; a XenonAnalyse jump-table TOML (`[[switch]] ... labels = [0x..., ...]`, #145:
    // 796 switches, 7,475 targets for LMA 2007, 42 of them no leader of ours) becomes entry-only
    // leaders like the inline tables found above, so slices stay where they were
    // hot indirect call sites (`*.calls`, from LMA_CALL_TARGETS=1 runs): `site target target ...` in
    // hex, one site a line; their leaf targets are inlined at the bctrl behind a match on the target
    let mut hot_calls: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut calls_file = false;
    for path in a.iter().skip(5) {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        if path.ends_with(".calls") {
            calls_file = true;
            for l in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
                let hex: Vec<u32> = l.split_whitespace().filter_map(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()).collect();
                if let [site, targets @ ..] = hex.as_slice() {
                    hot_calls.entry(*site).or_default().extend(targets);
                }
            }
            continue;
        }
        if path.ends_with(".toml") {
            tables.extend(switch_targets(&text).into_iter().filter(|&t| in_text(t)));
        } else {
            extra.extend(misses(&text));
        }
    }
    // hand-written stubs between functions (adjustor thunks and the like, reached through computed
    // pointers): in the gaps .pdata doesn't cover, the first instruction after zero padding starts
    // a routine. They ran in the interpreter, so they become entry-only leaders like table targets.
    let mut stub_starts = 0usize;
    for (&g, &len) in &gaps {
        for pc in (g + 4..g + len).step_by(4) {
            if x.read_u32(pc - 4) == Some(0) && x.read_u32(pc).is_some_and(|w| w != 0) && tables.insert(pc) {
                stub_starts += 1;
            }
        }
    }
    let n_gaps = gaps.len();
    let gap_bytes: u32 = gaps.values().sum();
    all.extend(gaps.iter());
    let wanted: Vec<u32> = if a[1] == "all" {
        all.keys().copied().collect()
    } else {
        std::fs::read_to_string(&a[1])
            .expect("read hot list")
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok())
            .take(max_funcs)
            .collect()
    };

    let (mut native, mut fallback) = (0u64, 0u64);
    let mut funcs: Vec<Func> = Vec::new();
    // LMA_RECOMP_IR=1 (#145 M1): blocks emitted over host locals by the IR emitter
    let ir_emit = std::env::var("LMA_RECOMP_IR").as_deref() == Ok("1");
    // LMA_RECOMP_IR_RANGE=<lo hex>-<hi hex> (bisecting an IR difference): only functions starting in
    // [lo, hi) use the IR emitter
    let ir_range = std::env::var("LMA_RECOMP_IR_RANGE").ok().and_then(|v| {
        let (a, b) = v.split_once('-')?;
        Some((u32::from_str_radix(a.trim_start_matches("0x"), 16).ok()?, u32::from_str_radix(b.trim_start_matches("0x"), 16).ok()?))
    });
    let mut entry_only_total = 0usize;
    for &f in &wanted {
        let Some(&len) = all.get(&f) else { continue };
        let end = f + len;
        let mut leaders: BTreeSet<u32> = BTreeSet::from([f]);
        leaders.extend(extra.range(f..end));
        for pc in (f..end).step_by(4) {
            if let Some(t) = branch_info(x.read_u32(pc).unwrap_or(0), pc) {
                leaders.insert(pc + 4);
                if let Some(t) = t.filter(|&t| t >= f && t < end) {
                    leaders.insert(t);
                }
            }
        }
        let entry_only: BTreeSet<u32> = tables.range(f..end).copied().filter(|t| !leaders.contains(t)).collect();
        entry_only_total += entry_only.len();
        leaders.extend(entry_only.iter().copied());
        let leaders: Vec<u32> = leaders.into_iter().filter(|&l| l < end).collect();
        // (code, instruction count, tail expression, falls through into the next leader, the code
        // with a budget check after every instruction: entry-only leaders run like the interpreter,
        // whether it ended in a branch)
        let mut raw: Vec<(String, u32, String, bool, String, bool, Vec<(ir::Op, String)>)> = Vec::new();
        for (i, &start) in leaders.iter().enumerate() {
            let stop = leaders.get(i + 1).copied().unwrap_or(end);
            let mut code = String::new();
            let mut count = 0u32;
            let mut tail = format!("{stop:#x}");
            let mut falls = true;
            let mut stepped = String::new();
            // the IR emitter's view of the same instructions (#145 M1): the op and, for a helper,
            // the text it runs through
            let mut ops: Vec<(ir::Op, String)> = Vec::new();
            for pc in (start..stop).step_by(4) {
                let w = x.read_u32(pc).unwrap_or(0);
                count += 1;
                if branch_info(w, pc).is_some() {
                    falls = false;
                    tail = translate_branch(w, pc);
                    if tail.starts_with("interp") {
                        fallback += 1
                    } else {
                        native += 1
                    }
                    break;
                }
                // twi / tdi: the condition natively; the interpreter only when the trap fires
                if matches!(w >> 26, 2 | 3) {
                    native += 1;
                    let before = code.len();
                    let mut op = ir::decode(w, pc);
                    op.kind = ir::Kind::Helper; // the condition natively; a hit leaves through the interpreter
                    op.defs = ir::Regs::NONE;
                    ops.push((op, String::new()));
                    let _ = write!(code, "if trap_hit(c,{},{},{},{}){{interp(c,m,{pc:#x},{w:#x})?;}}", bits(w, 6, 10), bits(w, 11, 15), (w & 0xFFFF) as i16 as i64, w >> 26 == 3);
                    let _ = write!(stepped, "{}*b-=1;if *b<=0{{return Ok({:#x})}}", &code[before..], pc + 4);
                    ops.last_mut().unwrap().1 = code[before..].to_string();
                    continue;
                }
                let before = code.len();
                let mut op = ir::decode(w, pc);
                if op.kind == ir::Kind::Branch {
                    op.kind = ir::Kind::Interp; // a trap form outside branch_info: the interpreter
                }
                match translate(pc, w) {
                    Some(s) => {
                        native += 1;
                        code.push_str(&s);
                    }
                    // other vector ops (and opcode 31's vector loads/stores, as `exec` routes them):
                    // the interpreter's VMX routine, called directly
                    None if is_vector(w) => {
                        native += 1;
                        let _ = write!(code, "vx(c,m,{pc:#x},{w:#x},{:?})?;", ppc::lookup(w).unwrap());
                    }
                    None => {
                        fallback += 1;
                        let _ = write!(code, "interp(c,m,{pc:#x},{w:#x})?;");
                    }
                }
                let _ = write!(stepped, "{}*b-=1;if *b<=0{{return Ok({:#x})}}", &code[before..], pc + 4);
                let text = code[before..].to_string();
                if op.kind == ir::Kind::Native && translate(pc, w).is_none() {
                    op.kind = ir::Kind::Interp; // decoded natively but not translated: stay safe
                }
                // a mod patch: the text reads a mod value at run time (base_year() at every year site,
                // subs and caps at theirs), which the IR's plain op wouldn't: the text, as a helper
                if op.kind == ir::Kind::Native && MOD_VALUES.iter().any(|f| text.contains(f)) {
                    op.kind = ir::Kind::Helper;
                }
                if op.kind == ir::Kind::Interp && !text.starts_with("interp(") {
                    op.kind = ir::Kind::Helper; // a helper the IR doesn't know: everything synced around it
                }
                ops.push((op, text));
            }
            raw.push((code, count, tail, falls && stop < end, stepped, !falls, ops));
        }
        // Extend each block through fall-throughs into the following blocks (up to SUPERBLOCK
        // instructions), so straight-line code entered at many points (e.g. the register
        // save/restore helpers) does not go round the dispatch loop once per leader. The budget
        // check at each internal leader returns exactly where the loop would have stopped.
        const SUPERBLOCK: u32 = 64;
        let mut blocks = Vec::new();
        for (k, &start) in leaders.iter().enumerate() {
            let mut body = String::new();
            let mut total = 0;
            let mut j = k;
            if entry_only.contains(&start) {
                // reached only by a jump (a table target): until the next real leader the budget is
                // checked after every instruction, as the interpreter did, so slices end where they
                // always did (bit-exact); only the per-instruction dispatch is gone
                loop {
                    let (_, _, tail, falls, stepped, branched, _) = &raw[j];
                    body.push_str(stepped);
                    if !*falls {
                        // a branch ends the segment (it isn't in `stepped`): charge it, and its check
                        // is the function loop's, after the tail, as the interpreter's after a branch.
                        // Without one (the function's end) every instruction is already charged.
                        let _ = write!(body, "{}{tail}", if *branched { "*b-=1;" } else { "" });
                        break;
                    }
                    if j + 1 < raw.len() && entry_only.contains(&leaders[j + 1]) {
                        j += 1;
                        continue;
                    }
                    // the next real leader: carry on there through the function's loop
                    let _ = write!(body, "{:#x}", leaders.get(j + 1).copied().unwrap_or(end));
                    break;
                }
                blocks.push((start, body));
                continue;
            }
            let mut segs: Vec<emit::Seg> = Vec::new();
            loop {
                let (code, count, tail, falls, _, _, ops) = &raw[j];
                body.push_str(code);
                total += count;
                if *falls && j + 1 < raw.len() && entry_only.contains(&leaders[j + 1]) {
                    // an entry-only leader: one block before it was split, so no check, no stop
                    let _ = write!(body, "*b-={count};");
                    segs.push(emit::Seg { ops, count: *count, next: emit::Next::Plain });
                    j += 1;
                } else if *falls && j + 1 < raw.len() && total + raw[j + 1].1 <= SUPERBLOCK {
                    let _ = write!(body, "*b-={count};if *b<=0{{return Ok({:#x})}}", leaders[j + 1]);
                    segs.push(emit::Seg { ops, count: *count, next: emit::Next::Check(leaders[j + 1]) });
                    j += 1;
                } else {
                    let _ = write!(body, "*b-={count};{tail}");
                    segs.push(emit::Seg { ops, count: *count, next: emit::Next::Tail });
                    if ir_emit && ir_range.is_none_or(|(lo, hi)| f >= lo && f < hi) {
                        // the same instructions over host locals (#145 M1)
                        body = emit::block(&segs, tail);
                    }
                    break;
                }
            }
            blocks.push((start, body));
        }
        funcs.push(Func { start: f, end, blocks });
    }

    // --- inline small leaf routines at their call sites ---
    // A callee entry whose block is straight-line, ends in an unconditional blr and has no
    // interpreter fallback or nested call (the register save/restore helpers, getters) is
    // emitted inline instead of `call(...)`. This is exactly what rec::call does: same
    // budget exits (the body's own `return Ok(..)` points), and the timebase set on return.
    const LEAF_MAX: u32 = 40;
    let mut leaves: std::collections::HashMap<u32, String> = std::collections::HashMap::new();
    for f in &funcs {
        for (start, body) in &f.blocks {
            // unconditional blr tail: `let t=bclr(c,20,0).unwrap_or(..);t`
            let ends_blr = body.ends_with(";t") && body.rsplit_once("let t=").is_some_and(|(_, t)| t.starts_with("bclr(c,20,0)"));
            let count: u32 = body.match_indices("*b-=").map(|(i, _)| body[i + 4..].split(';').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(99)).sum();
            if ends_blr && !body.contains("call(") && !body.contains("interp") && count <= LEAF_MAX {
                leaves.insert(*start, body.clone());
            }
        }
    }
    // Two-block leaves (#39: the day segment's hottest code is getters like 0x820dad10,
    // `lwz / cmplwi / beqlr / lwz / blr`, each paying rec::call): an entry block ending in a
    // conditional return whose fall-through block is itself a one-block leaf. Inlined as the entry
    // body, then the fall-through body when the return wasn't taken, with the budget check the
    // function's dispatch loop makes between the two blocks (`return Ok(next)`: the caller resumes
    // there exactly as it would after the callee's budget exit). LMA_LEAF2=0 turns it off.
    let mut leaves2 = 0usize;
    if std::env::var("LMA_LEAF2").as_deref() != Ok("0") {
        for f in &funcs {
            let by_start: std::collections::HashMap<u32, &String> = f.blocks.iter().map(|(s, b)| (*s, b)).collect();
            for (start, body) in &f.blocks {
                if leaves.contains_key(start) || body.contains("call(") || body.contains("interp") {
                    continue;
                }
                // `...;let t=bclr(c,X,Y).unwrap_or(0xNEXT);t` with a BO that isn't 'always' (20)
                let Some((head, tail)) = body.rsplit_once("let t=bclr(c,") else { continue };
                let Some((args, rest)) = tail.split_once(").unwrap_or(") else { continue };
                if args.starts_with("20,") || !rest.ends_with(");t") {
                    continue;
                }
                let Some(next) = rest.strip_suffix(");t").and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()) else { continue };
                let Some(fall) = by_start.get(&next).filter(|_| leaves.contains_key(&next)) else { continue };
                let count = |s: &str| -> u32 { s.match_indices("*b-=").map(|(i, _)| s[i + 4..].split(';').next().and_then(|n| n.parse::<u32>().ok()).unwrap_or(99)).sum() };
                if count(body) + count(fall) > LEAF_MAX {
                    continue;
                }
                let combined = format!("{head}let t=bclr(c,{args}).unwrap_or({next:#x});if t!={next:#x}{{t}}else{{if *b<=0{{return Ok({next:#x})}}{fall}}}");
                leaves.insert(*start, combined);
                leaves2 += 1;
            }
        }
    }
    eprintln!("{leaves2} two-block leaves (conditional return + one-block leaf)");
    // The game's register save / restore helpers (__savegprlr / __restgprlr), recognised by their
    // code: from entry r`first`, `std rK,-152+8*(K-14)(r1)` for K = first..31, then `stw r12,-8(r1)`,
    // `blr` (save), or the `ld`s, `lwz r12,-8(r1)`, `mtlr r12`, `blr` (restore). Calls to them run
    // natively (rec::save_gprlr / rest_gprlr) when the budget covers the whole helper, so no budget
    // check could have fired inside: the same bytes, registers and instruction count (#6).
    let mut save_at: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    let mut rest_at: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    {
        let ds = |op: u32, k: u32| (op << 26) | (k << 21) | (1 << 16) | ((-152 + 8 * (k as i32 - 14)) as u32 & 0xFFFC);
        let (stw_r12, lwz_r12, mtlr_r12, blr) = (0x9181_fff8u32, 0x8181_fff8u32, 0x7d88_03a6u32, 0x4e80_0020u32);
        for pc in (tva..tva + tsize).step_by(4) {
            let w = x.read_u32(pc).unwrap_or(0);
            let k = bits(w, 6, 10);
            if !(14..32).contains(&k) {
                continue;
            }
            for (op, tail, map) in [(62u32, &[stw_r12, blr][..], &mut save_at), (58, &[lwz_r12, mtlr_r12, blr][..], &mut rest_at)] {
                let seq: Vec<u32> = (k..32).map(|j| ds(op, j)).chain(tail.iter().copied()).collect();
                if seq.iter().enumerate().all(|(i, &want)| x.read_u32(pc + 4 * i as u32) == Some(want)) {
                    map.insert(pc, k);
                }
            }
        }
    }
    let mut inlined = 0usize;
    for f in funcs.iter_mut() {
        for (_, body) in f.blocks.iter_mut() {
            // call tails look like `c.lr=0xRET;call(c,m,0xT,b)?` (direct bl only; bctrl targets are dynamic)
            let Some(i) = body.rfind("call(c,m,0x") else { continue };
            let rest = &body[i + "call(c,m,".len()..];
            let Some(t) = rest.split(',').next().and_then(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).ok()) else { continue };
            if !rest.ends_with(",b)?") {
                continue;
            }
            if let Some(leaf) = leaves.get(&t) {
                let head = &body[..i];
                let leaf = match save_at.get(&t) {
                    // the stores, `stw r12`, `blr` (to the lr this call set)
                    Some(&k) => format!("if *b>{n}{{save_gprlr(c,m,{k});*b-={n};c.lr as u32&!3}}else{{{leaf}}}", n = 34 - k),
                    None => leaf.clone(),
                };
                *body = format!("{head}if *b<=0{{return Ok({t:#x})}}let r={{{leaf}}};c.timebase=timebase_at(c,*b);r");
                inlined += 1;
            }
        }
    }
    if !calls_file {
        for &(site, targets) in HOT_CALLS {
            hot_calls.insert(site, targets.to_vec());
        }
    }
    // Inline caching at hot bctrl sites (#171 / #145, the maintainer 22 Oct: the per-call bookkeeping out of
    // match code): `let t=c.ctr..;c.lr=0xRET;call(c,m,t,b)?` becomes a match on t whose arms are the
    // site's leaf targets, inlined exactly as a direct call's leaf above (the budget exit to t, the
    // timebase on return), and rec::call for any other target. Bit-exact: the same as entering the
    // leaf through rec::call.
    let mut cached_arms = 0usize;
    for f in funcs.iter_mut() {
        for (_, body) in f.blocks.iter_mut() {
            if let Some((new, n)) = cache_indirect(body, &hot_calls, &leaves, &save_at) {
                *body = new;
                cached_arms += n;
            }
        }
    }
    eprintln!("{cached_arms} leaf targets inlined at {} hot indirect call sites", hot_calls.len());
    // Epilogues' `b __restgprlr_N`: the restore natively when the budget covers it, then on to the
    // restored lr (the caller's return address, so the caller carries on natively); otherwise the
    // branch as before
    let mut restores = 0usize;
    for f in funcs.iter_mut() {
        let (lo, hi) = (f.start, f.end);
        for (_, body) in f.blocks.iter_mut() {
            let Some((head, last)) = body.rsplit_once(';') else { continue };
            let Some(t) = last.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok()) else { continue };
            let Some(&k) = rest_at.get(&t).filter(|_| !(lo..hi).contains(&t)) else { continue };
            *body = format!("{head};if *b>{n}{{rest_gprlr(c,m,{k});*b-={n};c.lr as u32&!3}}else{{{t:#x}}}", n = 35 - k);
            restores += 1;
        }
    }
    eprintln!("register save/restore helpers: {} save and {} restore entries; {restores} epilogues restore natively", save_at.len(), rest_at.len());
    // Tail branches out of the function (V2, #145), e.g. the epilogues' `b __restgprlr_N`:
    // returning the target pc unwound every native frame to the dispatcher, which ran the
    // target and re-entered the caller. Entered with `call` instead (lr untouched, as for a
    // `b`), the target's own blr returns the caller's address and the function exits normally.
    // `call` returns the target unchanged when the budget is spent or no recompiled function
    // starts there, and sets the timebase on entry and return as the dispatcher would.
    let mut tails = 0usize;
    if std::env::var("LMA_RECOMP_V2").as_deref() != Ok("0") {
        let starts: std::collections::HashSet<u32> = funcs.iter().map(|f| f.start).chain(leaves.keys().copied()).collect();
        for f in funcs.iter_mut() {
            let (lo, hi) = (f.start, f.end);
            for (_, body) in f.blocks.iter_mut() {
                let Some((head, last)) = body.rsplit_once(';') else { continue };
                let Some(t) = last.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok()) else { continue };
                if (lo..hi).contains(&t) || !starts.contains(&t) {
                    continue;
                }
                *body = format!("{head};call(c,m,{t:#x},b)?");
                tails += 1;
            }
        }
    }
    eprintln!("{} leaf entries, {inlined} call sites and {tails} tail branches inlined", leaves.len());

    // --- write the part crates and the aggregating game-blocks crate ---
    let root = std::path::Path::new(&a[2]);
    let parts_dir = root.join("parts");
    let _ = std::fs::remove_dir_all(&parts_dir);
    let per = funcs.len().div_ceil(parts).max(1);
    // calls into other parts go straight there too (#6, the maintainer 22 Oct: "a direct call wherever one
    // works, not the dispatcher"): every function is exported as `lma_f<addr>` and each part
    // declares the ones it calls (no crate dependencies), so a `bl` anywhere is `call_local` with
    // the callee itself instead of `call`'s table lookup; same checks, so bit-exact.
    // LMA_RECOMP_DIRECT=0: crate-local only, as before. Never an import thunk or a hooked function.
    let callable: std::collections::HashSet<u32> = if std::env::var("LMA_RECOMP_V2").as_deref() != Ok("0") && std::env::var("LMA_RECOMP_DIRECT").as_deref() != Ok("0") {
        funcs.iter().map(|f| f.start).filter(|a| !thunks.contains(a) && !guest::hooks::ADDRESSES.contains(a)).collect()
    } else {
        Default::default()
    };
    let mut part_names = Vec::new();
    for (pi, chunk) in funcs.chunks(per).enumerate() {
        let name = format!("gb{pi:02}");
        let dir = parts_dir.join(&name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nedition = \"2021\"\nversion = \"0.1.0\"\npublish = false\n\n\
                 [dependencies]\nguest = {{ path = \"../../../guest\" }}\n"
            ),
        )
        .unwrap();
        let mut src = String::from(
            "//! GENERATED by lma-recomp from the user's own default.xex. Do not edit or redistribute.\n\
             #![allow(unused_variables, unused_mut, unused_parens, unreachable_code, clippy::all)]\n\
             use guest::cpu::{Cpu, Stop};\nuse guest::mem::Memory;\nuse guest::rec::*;\nuse guest::native as n;\n\n",
        );
        let mut table = String::from("pub static BLOCKS: &[(u32, FuncFn)] = &[\n");
        // calls to a function in this crate go straight to it (no table lookup)
        // (rewrite step a, #6; LMA_RECOMP_V2=0 turns it and the tail calls off); never an
        // import thunk: the runtime keeps those out of the table so `call` reaches the kernel HLE
        let local: std::collections::HashSet<u32> = if std::env::var("LMA_RECOMP_V2").as_deref() != Ok("0") {
            // nor a hooked function (guest::hooks): the runtime leaves those out of the table too
            chunk.iter().map(|f| f.start).filter(|a| !thunks.contains(a) && !guest::hooks::ADDRESSES.contains(a)).collect()
        } else {
            Default::default()
        };
        // the budget in a local the function never lends out (M1, #6): LLVM keeps it in a register
        // instead of a load, subtract and store per block; LMA_RECOMP_BUDGET_LOCAL=0 turns it off
        let budget_local = std::env::var("LMA_RECOMP_BUDGET_LOCAL").as_deref() != Ok("0");
        // the functions of other parts this one calls directly (declared after the table)
        let mut ext: BTreeSet<u32> = BTreeSet::new();
        for f in chunk {
            if !callable.is_empty() {
                let _ = writeln!(src, "#[export_name=\"lma_f{:08x}\"]", f.start);
            }
            if budget_local {
                let _ = writeln!(src, "pub fn f{:08x}(c:&mut Cpu,m:&mut Memory,mut pc:u32,bp:&mut i64)->Result<u32,Stop>{{let mut bv=*bp;let r:Result<u32,Stop>='f:loop{{pc=match pc{{", f.start);
            } else {
                let _ = writeln!(src, "pub fn f{:08x}(c:&mut Cpu,m:&mut Memory,mut pc:u32,b:&mut i64)->Result<u32,Stop>{{loop{{pc=match pc{{", f.start);
            }
            for (s, body) in &f.blocks {
                let body = local_calls(body, if callable.is_empty() { &local } else { &callable });
                ext.extend(direct_targets(&body).filter(|t| !local.contains(t)));
                let body = if budget_local { budget_in_local(&body) } else { body };
                let _ = writeln!(src, "{s:#x}=>{{{body}}}");
                let _ = writeln!(table, "({s:#x},f{:08x}),", f.start);
            }
            if budget_local {
                let _ = writeln!(src, "_=>break 'f Ok(pc)}};if bv<=0||pc<{:#x}||pc>={:#x}{{break 'f Ok(pc)}}}};*bp=bv;r}}\n", f.start, f.end);
            } else {
                let _ = writeln!(src, "_=>return Ok(pc)}};if *b<=0||pc<{:#x}||pc>={:#x}{{return Ok(pc)}}}}}}\n", f.start, f.end);
            }
        }
        table.push_str("];\n");
        src.push_str(&extern_decls(&ext));
        src.push_str(&table);
        std::fs::write(dir.join("src/lib.rs"), src).unwrap();
        part_names.push(name);
    }
    let mut toml = String::from(
        "[package]\nname = \"game-blocks\"\nedition.workspace = true\nversion.workspace = true\n\
         description = \"GENERATED recompiled code for the user's own LMA 2007 copy (run lma-recomp). Never commit.\"\n\n\
         [dependencies]\nguest = { path = \"../guest\" }\n",
    );
    let mut lib = String::from(
        "//! GENERATED by lma-recomp. Run it again to regenerate; do not edit or redistribute.\n\
         use guest::rec::FuncFn;\n\n/// Every part's (block leader pc, recompiled function) table.\npub static PARTS: &[&[(u32, FuncFn)]] = &[\n",
    );
    for n in &part_names {
        let _ = writeln!(toml, "{n} = {{ path = \"parts/{n}\" }}");
        let _ = writeln!(lib, "    {n}::BLOCKS,");
    }
    lib.push_str("];\n");
    std::fs::write(root.join("Cargo.toml"), toml).unwrap();
    std::fs::write(root.join("src/lib.rs"), lib).unwrap();
    let blocks: usize = funcs.iter().map(|f| f.blocks.len()).sum();
    println!("{n_gaps} gaps ({gap_bytes} bytes) outside .pdata included; {} extra leaders ({} jump/function-pointer table targets, {stub_starts} stub starts in gaps; {entry_only_total} became entry-only leaders)", extra.len(), tables.len());
    println!(
        "{} functions, {blocks} blocks in {} crates; {native} instructions native, {fallback} via interpreter ({:.1}% native)",
        funcs.len(),
        part_names.len(),
        native as f64 * 100.0 / (native + fallback).max(1) as f64
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const PC: u32 = 0x8200_1000;

    #[test]
    fn branch_classification() {
        assert_eq!(branch_info(0x4800_0010, PC), Some(Some(PC + 0x10)), "b +0x10");
        assert_eq!(branch_info(0x4BFF_FFF0, PC), Some(Some(PC - 0x10)), "b -0x10");
        assert_eq!(branch_info(0x4800_0012, PC), Some(Some(0x10)), "ba (absolute)");
        assert_eq!(branch_info(0x4182_0008, PC), Some(Some(PC + 8)), "beq +8");
        assert_eq!(branch_info(0x4E80_0020, PC), Some(None), "blr: target unknown");
        assert_eq!(branch_info(0x4E80_0420, PC), Some(None), "bctr");
        assert_eq!(branch_info(0x4400_0002, PC), Some(None), "sc");
        assert_eq!(branch_info(0x3863_0001, PC), None, "addi is not a branch");
    }

    #[test]
    fn branches_translate() {
        assert_eq!(translate_branch(0x4800_0011, PC), format!("c.lr={:#x};call(c,m,{:#x},b)?", PC + 4, PC + 0x10), "bl: a native call");
        assert_eq!(translate_branch(0x4800_0010, PC), format!("{:#x}", PC + 0x10), "b: jump");
        assert_eq!(translate_branch(0x4182_0008, PC), format!("let t=c.branch_cond(12,2);if t{{{:#x}}}else{{{:#x}}}", PC + 8, PC + 4));
        assert_eq!(translate_branch(0x4E80_0020, PC), format!("let t=bclr(c,20,0).unwrap_or({:#x});t", PC + 4));
        assert_eq!(translate_branch(0x4E80_0421, PC), format!("let t=c.ctr as u32&!3;c.lr={:#x};call(c,m,t,b)?", PC + 4), "bctrl");
    }

    #[test]
    fn instructions_translate() {
        assert_eq!(translate(0, 0x3863_0001).as_deref(), Some("addi(c,3,3,1);"), "addi r3,r3,1");
        assert_eq!(translate(0, 0x3C60_8200).as_deref(), Some(&*format!("addi(c,3,0,{});", (0x8200u16 as i16 as i64) << 16)), "lis r3,0x8200");
        assert_eq!(translate(0, 0x6063_1234).as_deref(), Some("ori(c,3,3,4660);"), "ori r3,r3,0x1234");
    }

    /// The base year (#31): immediates 2006 read `base_year()`, the validators' 2026 (2006 + the
    /// 20 diary seasons) `base_year() + 20`; any other immediate stays a literal.
    #[test]
    fn base_year_sites_translate() {
        assert_eq!(translate(0, 0x3880_07D6).as_deref(), Some("addi(c,4,0,base_year());"), "li r4,2006");
        assert_eq!(translate(0, 0x2F1C_07D6).as_deref(), Some("cmpi(c,6,false,28,base_year());"), "cmpwi cr6,r28,2006");
        assert_eq!(translate(0, 0x2F05_07EA).as_deref(), Some("cmpi(c,6,false,5,(base_year()+20));"), "cmpwi cr6,r5,2026");
        assert_eq!(translate(0, 0x2B05_07EA).as_deref(), Some("cmpli(c,6,false,5,(base_year() as u64+20));"), "cmplwi cr6,r5,2026");
        assert_eq!(translate(0, 0x3BCB_07D7).as_deref(), Some("addi(c,30,11,(base_year()+1));"), "addi r30,r11,2007: a season label's second half (#706)");
        assert_eq!(translate(0, 0x3860_07D7).as_deref(), Some("addi(c,3,0,2007);"), "li r3,2007 is an id, not a year");
        assert_eq!(translate(0, 0x3880_07D7).as_deref(), Some("addi(c,4,0,2007);"), "li r4,2007 stays");
        assert_eq!(translate(0x8222_1090, 0x3860_07D6).as_deref(), Some("addi(c,3,0,2006);"), "the pitch's resource id 2006 stays");
        assert_eq!(translate(0x820d_79d4, 0x3880_07D6).as_deref(), Some("addi(c,4,0,2006);"), "the switch arm's id 2006 stays");
    }

    /// Inline caching (#171 / #145): a bctrl block at a hot site gets an arm per leaf target (the
    /// budget exit to it, the leaf, the timebase) and rec::call for the rest; other sites, non-leaf
    /// targets and blocks that don't end in a bctrl call stay as they are.
    #[test]
    fn hot_bctrl_sites_inline_their_leaf_targets() {
        let hot: BTreeMap<u32, Vec<u32>> = BTreeMap::from([(0x1000, vec![0x2000, 0x3000])]);
        let leaves = std::collections::HashMap::from([(0x2000u32, "stw(c,m,4,3,8);let t=bclr(c,20,0).unwrap_or(0x2008);t".to_string())]);
        let save_at = std::collections::HashMap::new();
        let body = "add(c,3,3,4);let t=c.ctr as u32&!3;c.lr=0x1004;call(c,m,t,b)?";
        let (new, n) = super::cache_indirect(body, &hot, &leaves, &save_at).expect("a hot site with a leaf target");
        assert_eq!(n, 1, "0x3000 isn't a leaf");
        assert_eq!(new, "add(c,3,3,4);let t=c.ctr as u32&!3;c.lr=0x1004;match t{0x2000=>{if *b<=0{return Ok(0x2000)}let r={stw(c,m,4,3,8);let t=bclr(c,20,0).unwrap_or(0x2008);t};c.timebase=timebase_at(c,*b);r}_=>call(c,m,t,b)?}");
        assert!(super::cache_indirect("let t=c.ctr as u32&!3;c.lr=0x5004;call(c,m,t,b)?", &hot, &leaves, &save_at).is_none(), "a site not in the list");
        assert!(super::cache_indirect("c.lr=0x1004;call(c,m,0x2000,b)?", &hot, &leaves, &save_at).is_none(), "a direct call");
        // a hooked target keeps rec::call (its hook runs from the dispatcher)
        let h = guest::hooks::ADDRESSES[0];
        let hot_h: BTreeMap<u32, Vec<u32>> = BTreeMap::from([(0x1000, vec![h])]);
        let leaves_h = std::collections::HashMap::from([(h, "let t=bclr(c,20,0).unwrap_or(0x2008);t".to_string())]);
        assert!(super::cache_indirect(body, &hot_h, &leaves_h, &save_at).is_none(), "a hooked leaf isn't inlined");
    }

    /// A mod's formations: the role table's lis/addi pairs, the shared site's absolute address,
    /// the two name tables' pairs, the id list's pair and the count sites read the atomics; the same words elsewhere stay.
    #[test]
    fn formation_sites_translate() {
        assert_eq!(translate(0x822a_2890, 0x3D60_8271).as_deref(), Some("addi(c,11,0,formation_roles_hi());"), "lis r11,0x8271 of a pair");
        assert_eq!(translate(0x822a_2898, 0x396B_0F58).as_deref(), Some("addi(c,11,11,formation_roles_lo());"), "addi r11,r11,3928 of a pair");
        assert_eq!(translate(0x822a_dfb8, 0x3D60_8271).as_deref(), Some(&*format!("addi(c,11,0,{});", (0x8271u16 as i16 as i64) << 16)), "the shared lis stays");
        assert_eq!(translate(0x822a_dfc0, 0x3BAB_0F58).as_deref(), Some("addi(c,29,0,formation_roles());"), "its addi takes the whole address");
        assert_eq!(translate(0x820d_2e84, 0x3D60_8270).as_deref(), Some("addi(c,11,0,formation_names_hi());"), "the name table's lis");
        assert_eq!(translate(0x820d_2e8c, 0x396B_23F0).as_deref(), Some("addi(c,11,11,formation_names_lo());"), "the name table's addi");
        assert_eq!(translate(0x822a_289c, 0x396B_0F58).as_deref(), Some("addi(c,11,11,3928);"), "addi 3928 elsewhere stays");
        assert_eq!(translate(0x822a_2894, 0x3D60_8271).as_deref(), Some(&*format!("addi(c,11,0,{});", (0x8271u16 as i16 as i64) << 16)), "lis 0x8271 elsewhere stays");
        assert_eq!(translate(0x8232_6290, 0x3D60_8271).as_deref(), Some("addi(c,11,0,formation_names_hi());"), "the second name table's lis");
        assert_eq!(translate(0x8232_6298, 0x396B_35BC).as_deref(), Some("addi(c,11,11,formation_names_lo());"), "the second name table's addi");
        assert_eq!(translate(0x822d_c7a8, 0x3D60_8202).as_deref(), Some("addi(c,11,0,formation_ids_hi());"), "the id list's lis");
        assert_eq!(translate(0x822d_c7b0, 0x396B_7DCC).as_deref(), Some("addi(c,11,11,formation_ids_lo());"), "the id list's addi");
        assert_eq!(translate(0x8232_62a8, 0x3860_000E).as_deref(), Some("addi(c,3,0,formation_count());"), "the count getter's li r3,14");
        assert_eq!(translate(0x821d_42e8, 0x3880_000E).as_deref(), Some("addi(c,4,0,formation_count());"), "the Formation row's li r4,14");
        assert_eq!(translate(0x8232_6280, 0x2F03_000E).as_deref(), Some("cmpi(c,6,false,3,formation_count());"), "the name getter's cmpwi cr6,r3,14");
        assert_eq!(translate(0x822a_e0b8, 0x2F1E_000E).as_deref(), Some("cmpi(c,6,false,30,formation_count());"), "the engine's cmpwi cr6,r30,14");
        assert_eq!(translate(0x8232_62ac, 0x3860_000E).as_deref(), Some("addi(c,3,0,14);"), "li r3,14 elsewhere stays");
        assert_eq!(translate(0x8232_6284, 0x2F03_000E).as_deref(), Some("cmpi(c,6,false,3,14);"), "cmpwi 14 elsewhere stays");
        assert_eq!(translate(0x8219_1ba8, 0x3D60_8201).as_deref(), Some("addi(c,11,0,formation_spots_hi());"), "the spot table's lis");
        assert_eq!(translate(0x8219_1bb4, 0x396B_B370).as_deref(), Some("addi(c,11,11,formation_spots_lo());"), "the spot table's addi");
        assert_eq!(translate(0x8232_6240, 0x3D60_8203).as_deref(), Some("addi(c,11,0,formation_order_hi());"), "the team-sheet order's lis");
        assert_eq!(translate(0x8232_6248, 0x396B_CD38).as_deref(), Some("addi(c,11,11,formation_order_lo());"), "the team-sheet order's addi");
    }

    #[test]
    fn value_unit_sites_translate() {
        assert_eq!(translate(0x8220_e200, 0x1FC3_03E8).as_deref(), Some("mulli(c,30,3,value_unit());"), "mulli r30,r3,1000 after the getter");
        assert_eq!(translate(0x8215_da98, 0x1FE3_03E8).as_deref(), Some("mulli(c,31,3,value_unit());"));
        assert_eq!(translate(0x8218_83d8, 0x1D7F_0023).as_deref(), Some("mulli(c,11,31,tune(0));"), "a tunable (#870): prime_wage_premium");
        assert_eq!(translate(0x8218_8ae4, 0x2F0B_0010).as_deref(), Some("cmpi(c,6,false,11,tune(5));"), "a second site of the same number (sale_min_squad)");
        assert_eq!(translate(0x8218_914c, 0x23CA_00A0).as_deref(), Some("subfic(c,30,10,tune(9) as u64);"), "subfic (bid_pct_top_low)");
        assert_eq!(translate(0x8218_83dc, 0x1D7F_0023).as_deref(), Some("mulli(c,11,31,35);"), "the same word elsewhere: baked in");
        assert_eq!(translate(0x8218_83d8, 0x1D7F_0024).as_deref(), Some("mulli(c,11,31,36);"), "another word at the site: baked in");
        assert_eq!(translate(0x8211_dcd8, 0x2F1F_03E8).as_deref(), Some("cmpi(c,6,false,31,value_floor());"), "the setter's cmpwi r31,1000");
    }

    /// #870: every tunable instruction, at each of its sites, loads its number from `tune(i)`.
    #[test]
    fn every_tunable_site_translates_to_its_slot() {
        for (i, t) in guest::tune::TUNABLES.iter().enumerate().filter(|(_, t)| matches!(t.kind, guest::tune::Kind::Imm(_))) {
            for (site, word) in guest::tune::sites(t) {
                let got = translate(site, word).unwrap_or_default();
                assert!(got.contains(&format!("tune({i})")), "{} at {site:#x}: {got}", t.name);
            }
        }
    }

    /// #735: the 28-club draw's two compares read the guard; any other cmpwi 28 stays.
    #[test]
    fn draw_fill_sites_translate() {
        assert_eq!(translate(0x8213_c958, 0x2F0B_001C).as_deref(), Some("cmpi(c,6,false,11,draw_fill_first());"));
        assert_eq!(translate(0x8213_c9e8, 0x2F0B_001C).as_deref(), Some("cmpi(c,6,false,11,draw_fill_target(c.gpr[11] as i32 as i64));"));
        assert_eq!(translate(0x8213_c9ec, 0x2F0B_001C).as_deref(), Some("cmpi(c,6,false,11,28);"));
        assert_eq!(translate(0x8220_e204, 0x1FC3_03E8).as_deref(), Some("mulli(c,30,3,1000);"), "any other mulli 1000 stays");
        assert_eq!(translate(0x8220_e200, 0x1FC3_03E9).as_deref(), Some("mulli(c,30,3,1001);"), "another immediate at the site stays");
        assert_eq!(translate(0x8212_0b80, 0x616B_5A00).as_deref(), Some("addi(c,11,0,value_cap());"), "the valuation's GBP 40m cap");
        assert_eq!(translate(0x8210_b8a4, 0x617E_D400).as_deref(), Some("addi(c,30,0,wage_budget_cap());"), "the wage budget's GBP 3.2m cap");
        assert_eq!(translate(0x8221_3fd4, 0x3A00_0003).as_deref(), Some("addi(c,16,0,subs_limit());"), "a match's 3 substitutions");
        assert_ne!(translate(0x8221_3fd8, 0x3A00_0003).as_deref(), Some("addi(c,16,0,subs_limit());"), "an addi elsewhere stays");
        assert_eq!(translate(0x822a_64a8, 0x3BE0_0002).as_deref(), Some("addi(c,31,0,subs_limit()-1);"), "the sim planner's 2");
        assert_eq!(translate(0x822a_64b0, 0x3BE0_0003).as_deref(), Some("addi(c,31,0,subs_limit());"), "the sim planner's 3");
        assert_eq!(translate(0x8212_0b84, 0x616B_5A00).as_deref(), Some("ori(c,11,11,23040);"), "an ori elsewhere stays");
        assert_eq!(translate(0x821f_39ac, 0x616A_B400).as_deref(), Some("addi(c,10,0,offer_cap());"), "the offer screen's GBP 80m cap");
        assert_eq!(translate(0x821f_4778, 0x617E_B400).as_deref(), Some("addi(c,30,0,offer_cap());"), "another of the seven sites");
        assert_eq!(translate(0x821f_39b0, 0x616A_B400).as_deref(), Some("ori(c,10,11,46080);"), "an ori 0xb400 elsewhere stays");
        assert_eq!(translate(0x8212_0b84, 0x616B_5A00).as_deref(), Some("ori(c,11,11,23040);"), "an ori elsewhere stays");
        assert_eq!(translate(0x821f_6f98, 0x616B_D090).as_deref(), Some("addi(c,11,0,wage_offer_cap());"), "the offer helper's GBP 250k wage cap");
        assert_eq!(translate(0x821f_72ac, 0x6163_D090).as_deref(), Some("addi(c,3,0,wage_offer_cap());"), "the offer screen's, into r3");
        assert_eq!(translate(0x8218_8d24, 0x6063_86A0).as_deref(), Some("addi(c,3,0,ai_signing_wage_cap());"), "the AI signing's GBP 100k ceiling");
        assert_eq!(translate(0x8218_8d28, 0x6063_86A0).as_deref(), Some("ori(c,3,3,34464);"), "anywhere else a plain ori");
        assert_eq!(translate(0x8211_8dd4, 0x614A_D090).as_deref(), Some("ori(c,10,10,53392);"), "the rounding step's 250,000 stays");
    }

    #[test]
    fn leader_files() {
        assert_eq!(misses("820da108\n820da720 extra words\n0x820da724\nnot hex\n"), vec![0x820d_a108, 0x820d_a720, 0x820d_a724]);
        let toml = "# Generated by XenonAnalyse\n[[switch]]\nbase = 0x820C1FE8\nr = 4\ndefault = 0x820C2158\nlabels = [\n    0x820C201C,\n    0x820C2044,\n]\n\n[[switch]]\nbase = 0x820C217C\nlabels = [0x820C21AC]\n";
        assert_eq!(switch_targets(toml), vec![0x820c_201c, 0x820c_2044, 0x820c_21ac], "labels only, not base or default");
    }

    #[test]
    fn budget_in_a_local() {
        let body = "*b-=3;if *b<=0{return Ok(0x82000010)}c.lr=0x82000014;call(c,m,0x82002000,b)?;interp(c,m,0x82000014,0x7c000000)?;c.timebase=timebase_at(c,*b);*b-=1;0x82000018";
        assert_eq!(
            budget_in_local(body),
            "bv-=3;if bv<=0{break 'f Ok(0x82000010)}c.lr=0x82000014;match {*bp=bv;let r=call(c,m,0x82002000,bp);bv=*bp;r}{Ok(v)=>v,Err(e)=>break 'f Err(e)};match interp(c,m,0x82000014,0x7c000000){Ok(v)=>v,Err(e)=>break 'f Err(e)};c.timebase=timebase_at(c,bv);bv-=1;0x82000018"
        );
        // a name ending in `vx(` is not the `vx` helper
        assert_eq!(budget_in_local("lvx(c,m,1,2,3);vx(c,m,4)?"), "lvx(c,m,1,2,3);match vx(c,m,4){Ok(v)=>v,Err(e)=>break 'f Err(e)}");
    }

    #[test]
    fn local_calls_rewritten() {
        let local: std::collections::HashSet<u32> = [0x8200_2000].into();
        let body = "c.lr=0x82001004;call(c,m,0x82002000,b)?;call(c,m,0x82003000,b)?";
        assert_eq!(local_calls(body, &local), "c.lr=0x82001004;call_local(c,m,0x82002000,b,f82002000)?;call(c,m,0x82003000,b)?");
        assert_eq!(local_calls("call_local(c,m,0x82002000,b,f82002000)?", &local), "call_local(c,m,0x82002000,b,f82002000)?", "already local");
    }

    #[test]
    fn cross_part_calls_are_declared() {
        let all: std::collections::HashSet<u32> = [0x8200_2000, 0x8200_3000].into_iter().collect();
        let body = local_calls("c.lr=0x82001004;call(c,m,0x82002000,b)?;call(c,m,0x82003000,b)?;call(c,m,0x82004000,b)?", &all);
        assert_eq!(body, "c.lr=0x82001004;call_local(c,m,0x82002000,b,f82002000)?;call_local(c,m,0x82003000,b,f82003000)?;call(c,m,0x82004000,b)?");
        let ext: BTreeSet<u32> = direct_targets(&body).filter(|&t| t != 0x8200_2000).collect();
        assert_eq!(ext, BTreeSet::from([0x8200_3000]));
        assert_eq!(extern_decls(&ext), "unsafe extern \"Rust\" {\n#[link_name=\"lma_f82003000\"] safe fn f82003000(c:&mut Cpu,m:&mut Memory,pc:u32,b:&mut i64)->Result<u32,Stop>;\n}\n");
        assert_eq!(extern_decls(&BTreeSet::new()), "");
    }

    /// Every instruction the interpreter executes is translated natively (perf plan item 3,
    /// #235): a word the recompiler would hand to `interp` fails here, unless it's a branch or a
    /// trap (both handled before `translate`), or one of `UNUSED`.
    /// The native vector float / logic helpers decode the same registers and compute the same as
    /// the interpreter, standard and VMX128 forms alike: each emitted call, run, against `exec`.
    #[test]
    fn native_vector_ops_match_the_interpreter() {
        use guest::cpu::Cpu;
        use guest::mem::Memory;
        use guest::rec;
        let names = ["vaddfp", "vsubfp", "vmulfp", "vmaddfp", "vmsum3fp", "vsldoi", "vand", "vandc", "vxor"];
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut base = Cpu::default();
        for r in base.vr.iter_mut() {
            let lanes: [f32; 4] = std::array::from_fn(|_| ((next() % 20000) as f32 - 10000.0) / 37.0);
            *r = std::array::from_fn(|i| lanes[i / 4].to_bits().to_be_bytes()[i % 4]);
        }
        let mut checked = 0;
        for _ in 0..400_000 {
            let w = (4 + (next() % 3) as u32) << 26 | (next() as u32 & 0x03FF_FFFF);
            let Some(name) = ppc::lookup(w) else { continue };
            if !names.contains(&name.strip_suffix("128").unwrap_or(name)) {
                continue;
            }
            let Some(code) = translate(0, w) else { panic!("{name} {w:#010x} not translated") };
            let (f, args) = code.trim_end_matches(");").split_once("(c,").unwrap();
            let n: Vec<usize> = args.split(',').map(|a| a.parse().unwrap()).collect();
            let (mut a, mut b) = (base.clone(), base.clone());
            match f {
                "vaddfp" => rec::vaddfp(&mut a, n[0], n[1], n[2]),
                "vsubfp" => rec::vsubfp(&mut a, n[0], n[1], n[2]),
                "vmulfp" => rec::vmulfp(&mut a, n[0], n[1], n[2]),
                "vmsum3fp" => rec::vmsum3fp(&mut a, n[0], n[1], n[2]),
                "vmaddfp" => rec::vmaddfp(&mut a, n[0], n[1], n[2], n[3]),
                "vsldoi" => rec::vsldoi(&mut a, n[0], n[1], n[2], n[3]),
                "vand" => rec::vand(&mut a, n[0], n[1], n[2]),
                "vandc" => rec::vandc(&mut a, n[0], n[1], n[2]),
                "vxor" => rec::vxor(&mut a, n[0], n[1], n[2]),
                other => panic!("unexpected helper {other}"),
            }
            b.exec(&mut Memory::new(), w).unwrap();
            assert!(a.vr == b.vr, "{name} {w:#010x}: {code}");
            checked += 1;
        }
        assert!(checked > 1000, "only {checked} words");
    }

    /// Implemented by the interpreter, never in the game's code (lma-recomp's static count of
    /// `interp` calls is only data words and traps): translate one if the game ever needs it.
    /// `mftb` here is the form with an SPR other than TBL/TBU.
    const UNUSED: &[&str] = &[
        "crand", "crandc", "creqv", "crnand", "crnor", "cror", "crorc", "crxor", "mcrf", "isync", "eqvx", "nandx", "tw", "td",
        "lbzux", "lhzu", "lhzux", "lhau", "lhaux", "lwzux", "lwaux", "ldux", "lfsu", "lfdu", "lmw",
        "stbux", "sthu", "sthux", "stdux", "stfsu", "stfdu", "stmw", "mftb",
    ];

    #[test]
    fn nothing_the_interpreter_runs_falls_back() {
        use guest::cpu::Cpu;
        use guest::mem::Memory;
        let mut words = Vec::new();
        for op in 0..64u32 {
            let fields = op << 26 | 6 << 21 | 3 << 16 | 4 << 11;
            match op {
                31 | 19 => words.extend((0..1024u32).flat_map(|xo| [fields | xo << 1, fields | xo << 1 | 1])),
                // primary opcodes: a few displacement / field patterns
                _ => words.extend([fields, fields | 0x10, fields | 0x7FFC, fields | 1]),
            }
        }
        let mut missing = Vec::new();
        for w in words {
            if branch_info(w, PC).is_some() || matches!(w >> 26, 2 | 3) || translate(0, w).is_some() || is_vector(w) {
                continue;
            }
            let (mut c, mut m) = (Cpu::default(), Memory::new());
            c.gpr[3] = 0x4000_0000;
            c.gpr[4] = 0x10;
            c.pc = PC;
            let name = ppc::lookup(w).unwrap_or("?");
            if c.exec(&mut m, w).is_ok() && !UNUSED.contains(&name) {
                missing.push(format!("{w:#010x} {name}"));
            }
        }
        assert!(missing.is_empty(), "{} interpreter-only instructions: {:?}", missing.len(), &missing);
    }
}
