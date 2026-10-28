//! Xenos (Xbox 360 GPU) shader microcode: decoding, disassembly and a reference interpreter.
//!
//! Layout follows Xenia's ucode.h (BSD-3-Clause): control flow instructions are 48-bit and
//! packed two per three dwords; ALU and fetch instructions are 96-bit (three dwords).

/// Bits `lo..lo+n` of `v`.
fn f(v: u32, lo: u32, n: u32) -> u32 {
    (v >> lo) & ((1u64 << n) as u32).wrapping_sub(1)
}

// ---------------------------------------------------------------- control flow
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cf {
    Nop,
    /// exec / cond_exec*: run `count` instructions starting at 96-bit slot `addr`.
    Exec { addr: u32, count: u32, seq: u32, end: bool, cond: ExecCond },
    LoopStart { addr: u32, loop_id: u32, repeat: bool },
    LoopEnd { addr: u32, loop_id: u32, pred_break: bool, cond: bool },
    CondCall { addr: u32, cond: ExecCond },
    Return,
    CondJmp { addr: u32, cond: ExecCond },
    Alloc { kind: u32, size: u32 },
    Other(u32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExecCond {
    Always,
    /// bool constant `index` must equal `value`
    Bool { index: u32, value: bool },
    /// predicate must equal `value`
    Pred { value: bool },
}

fn decode_cf(lo: u32, hi: u32) -> Cf {
    let op = f(hi, 12, 4);
    let addr = f(lo, 0, 12);
    let count = f(lo, 12, 3);
    let seq = f(lo, 16, 12);
    let bool_addr = f(hi, 2, 8);
    let condition = f(hi, 10, 1) == 1;
    match op {
        0 => Cf::Nop,
        1 | 2 => Cf::Exec { addr, count, seq, end: op == 2, cond: ExecCond::Always },
        3 | 4 => Cf::Exec { addr, count, seq, end: op == 4, cond: ExecCond::Bool { index: bool_addr, value: condition } },
        5 | 6 | 13 | 14 => {
            let end = op == 6 || op == 14;
            Cf::Exec { addr, count, seq, end, cond: ExecCond::Pred { value: condition } }
        }
        7 => Cf::LoopStart { addr: f(lo, 0, 13), loop_id: f(lo, 16, 5), repeat: f(hi, 10, 1) == 1 },
        8 => Cf::LoopEnd { addr: f(lo, 0, 13), loop_id: f(lo, 16, 5), pred_break: f(lo, 21, 1) == 1, cond: condition },
        9 | 11 => {
            // force_call/is_unconditional at bit 9 of word 1; predicated via bool/pred select
            let addr = f(lo, 0, 13);
            let uncond = f(hi, 1, 1) == 1;
            let is_pred = f(lo, 13, 1) == 1;
            let cond = if uncond {
                ExecCond::Always
            } else if is_pred {
                ExecCond::Pred { value: condition }
            } else {
                ExecCond::Bool { index: bool_addr, value: condition }
            };
            if op == 9 { Cf::CondCall { addr, cond } } else { Cf::CondJmp { addr, cond } }
        }
        10 => Cf::Return,
        12 => Cf::Alloc { kind: f(hi, 9, 2), size: f(lo, 0, 3) },
        other => Cf::Other(other),
    }
}

/// Decode the control-flow program (starts at dword 0 and runs until an *_end exec).
pub fn decode_control_flow(code: &[u32]) -> Vec<Cf> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= code.len() {
        let (d0, d1, d2) = (code[i], code[i + 1], code[i + 2]);
        let a = decode_cf(d0, d1 & 0xFFFF);
        let b = decode_cf((d1 >> 16) | (d2 << 16), d2 >> 16);
        out.push(a);
        out.push(b);
        let ends = |c: &Cf| matches!(c, Cf::Exec { end: true, .. });
        if ends(&a) || ends(&b) {
            break;
        }
        i += 3;
    }
    out
}

// ---------------------------------------------------------------- fetch
#[derive(Clone, Copy, Debug)]
pub struct VertexFetch {
    pub src: u32,
    pub src_swiz: u32,
    pub dst: u32,
    pub dst_swiz: u32,
    pub const_index: u32, // index into 2-dword vertex fetch constants
    pub mini: bool,
    pub format: u32,
    pub signed: bool,
    pub normalized: bool,
    pub exp_adjust: i32,
    pub stride: u32,
    pub offset: i32,
    pub predicated: bool,
    pub pred_cond: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct TextureFetch {
    pub opcode: u32,
    pub src: u32,
    pub src_swiz: u32,
    pub dst: u32,
    pub dst_swiz: u32,
    pub const_index: u32,
    pub unnormalized: bool,
    pub mag_filter: u32,
    pub min_filter: u32,
    pub dimension: u32,
    pub offset: [i32; 3],
    pub predicated: bool,
    pub pred_cond: bool,
}

fn sext(v: u32, bits: u32) -> i32 {
    ((v << (32 - bits)) as i32) >> (32 - bits)
}

#[derive(Clone, Copy, Debug)]
pub enum Fetch {
    Vertex(VertexFetch),
    Texture(TextureFetch),
}

fn decode_fetch(w: [u32; 3]) -> Fetch {
    let op = f(w[0], 0, 5);
    if op == 0 {
        Fetch::Vertex(VertexFetch {
            src: f(w[0], 5, 6),
            src_swiz: f(w[0], 30, 2),
            dst: f(w[0], 12, 6),
            dst_swiz: f(w[1], 0, 12),
            const_index: f(w[0], 20, 5) * 3 + f(w[0], 25, 2),
            mini: f(w[1], 30, 1) == 1,
            format: f(w[1], 16, 6),
            signed: f(w[1], 12, 1) == 1,
            normalized: f(w[1], 13, 1) == 0,
            exp_adjust: sext(f(w[1], 24, 6), 6),
            stride: f(w[2], 0, 8),
            offset: sext(f(w[2], 8, 23), 23),
            predicated: f(w[1], 31, 1) == 1,
            pred_cond: f(w[2], 31, 1) == 1,
        })
    } else {
        Fetch::Texture(TextureFetch {
            opcode: op,
            src: f(w[0], 5, 6),
            src_swiz: f(w[0], 26, 6),
            dst: f(w[0], 12, 6),
            dst_swiz: f(w[1], 0, 12),
            const_index: f(w[0], 20, 5),
            unnormalized: f(w[0], 25, 1) == 1,
            mag_filter: f(w[1], 12, 2),
            min_filter: f(w[1], 14, 2),
            dimension: f(w[2], 14, 2),
            offset: [sext(f(w[2], 16, 5), 5), sext(f(w[2], 21, 5), 5), sext(f(w[2], 26, 5), 5)],
            predicated: f(w[1], 31, 1) == 1,
            pred_cond: f(w[2], 31, 1) == 1,
        })
    }
}

// ---------------------------------------------------------------- ALU
#[derive(Clone, Copy, Debug)]
pub struct Alu {
    pub vector_dest: u32,
    pub vector_dest_rel: bool,
    pub abs_constants: bool,
    pub scalar_dest: u32,
    pub scalar_dest_rel: bool,
    pub export: bool,
    pub vector_mask: u32,
    pub scalar_mask: u32,
    pub vector_clamp: bool,
    pub scalar_clamp: bool,
    pub scalar_op: u32,
    pub swiz: [u32; 3],   // src1..src3
    pub negate: [bool; 3],
    pub predicated: bool,
    pub pred_cond: bool,
    pub const_addr_reg_rel: bool,
    pub const_rel: [bool; 2], // const_0_rel_abs, const_1_rel_abs
    pub reg: [u32; 3],
    pub vector_op: u32,
    pub is_temp: [bool; 3],
}

fn decode_alu(w: [u32; 3]) -> Alu {
    Alu {
        vector_dest: f(w[0], 0, 6),
        vector_dest_rel: f(w[0], 6, 1) == 1,
        abs_constants: f(w[0], 7, 1) == 1,
        scalar_dest: f(w[0], 8, 6),
        scalar_dest_rel: f(w[0], 14, 1) == 1,
        export: f(w[0], 15, 1) == 1,
        vector_mask: f(w[0], 16, 4),
        scalar_mask: f(w[0], 20, 4),
        vector_clamp: f(w[0], 24, 1) == 1,
        scalar_clamp: f(w[0], 25, 1) == 1,
        scalar_op: f(w[0], 26, 6),
        swiz: [f(w[1], 16, 8), f(w[1], 8, 8), f(w[1], 0, 8)],
        negate: [f(w[1], 26, 1) == 1, f(w[1], 25, 1) == 1, f(w[1], 24, 1) == 1],
        pred_cond: f(w[1], 27, 1) == 1,
        predicated: f(w[1], 28, 1) == 1,
        const_addr_reg_rel: f(w[1], 29, 1) == 1,
        const_rel: [f(w[1], 31, 1) == 1, f(w[1], 30, 1) == 1],
        reg: [f(w[2], 16, 8), f(w[2], 8, 8), f(w[2], 0, 8)],
        vector_op: f(w[2], 24, 5),
        is_temp: [f(w[2], 31, 1) == 1, f(w[2], 30, 1) == 1, f(w[2], 29, 1) == 1],
    }
}

pub const VECTOR_OPS: [&str; 32] = [
    "add", "mul", "max", "min", "seq", "sgt", "sge", "sne", "frc", "trunc", "floor", "mad", "cndeq", "cndge", "cndgt",
    "dp4", "dp3", "dp2add", "cube", "max4", "setp_eq_push", "setp_ne_push", "setp_gt_push", "setp_ge_push", "kill_eq",
    "kill_gt", "kill_ge", "kill_ne", "dst", "maxa", "op30", "op31",
];
pub const VECTOR_OPERANDS: [u8; 32] = [2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 3, 3, 3, 3, 2, 2, 3, 2, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 0, 0];
pub const SCALAR_OPS: [&str; 51] = [
    "adds", "adds_prev", "muls", "muls_prev", "muls_prev2", "maxs", "mins", "seqs", "sgts", "sges", "snes", "frcs",
    "truncs", "floors", "exp", "logc", "log", "rcpc", "rcpf", "rcp", "rsqc", "rsqf", "rsq", "maxas", "maxasf", "subs",
    "subs_prev", "setp_eq", "setp_ne", "setp_gt", "setp_ge", "setp_inv", "setp_pop", "setp_clr", "setp_rstr", "kills_eq",
    "kills_gt", "kills_ge", "kills_ne", "kills_one", "sqrt", "op41", "mulsc", "mulsc", "addsc", "addsc", "subsc", "subsc",
    "sin", "cos", "retain_prev",
];
/// 0 = none, 1 = one operand (W, or W+X/Z when two-component), 2 = c#.w and r#.x
pub const SCALAR_OPERANDS: [u8; 51] = [
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 1, 1, 1, 1,
    1, 1, 0, 2, 2, 2, 2, 2, 2, 1, 1, 0,
];
pub const SCALAR_TWO_COMPONENT: [bool; 51] = {
    let mut t = [false; 51];
    t[0] = true; // adds
    t[2] = true; // muls
    t[4] = true; // muls_prev2
    t[5] = true; // maxs
    t[6] = true; // mins
    t[23] = true; // maxas
    t[24] = true; // maxasf
    t[25] = true; // subs
    t
};

/// Swizzled source component for ALU operands (relative 2-bit encoding).
pub fn alu_component(swiz: u32, i: u32) -> usize {
    (((swiz >> (2 * i)) + i) & 3) as usize
}

// ---------------------------------------------------------------- whole shader
#[derive(Clone, Copy, Debug)]
pub enum Instr {
    Alu(Alu),
    Fetch(Fetch),
}

#[derive(Clone, Debug)]
pub struct Shader {
    pub cf: Vec<Cf>,
    pub code: Vec<u32>,
    /// Every 96-bit slot decoded both ways once (ALU, fetch), so execution never re-decodes.
    decoded: Vec<(Instr, Instr)>,
}

impl Shader {
    pub fn new(code: Vec<u32>) -> Self {
        let decoded = code
            .chunks_exact(3)
            .map(|c| {
                let w = [c[0], c[1], c[2]];
                (Instr::Alu(decode_alu(w)), Instr::Fetch(decode_fetch(w)))
            })
            .collect();
        Self { cf: decode_control_flow(&code), code, decoded }
    }

    /// Pre-decoded instruction at `slot` (what `instr` returns, without the decode cost).
    #[inline]
    pub fn instr_ref(&self, slot: u32, is_fetch: bool) -> Option<&Instr> {
        self.decoded.get(slot as usize).map(|(a, f)| if is_fetch { f } else { a })
    }

    /// Texture fetch constants any exec clause of this shader can sample.
    /// The first texture fetch from fetch constant `k` (its filter fields pick the sampler).
    pub fn texture_fetch(&self, k: u32) -> Option<TextureFetch> {
        for cf in &self.cf {
            if let Cf::Exec { addr, count, seq, .. } = *cf {
                for i in 0..count {
                    if (seq >> (2 * i)) & 1 == 1 {
                        if let Some(Instr::Fetch(Fetch::Texture(t))) = self.instr_ref(addr + i, true) {
                            if t.const_index == k {
                                return Some(*t);
                            }
                        }
                    }
                }
            }
        }
        None
    }

    /// The vertex fetch constants the shader reads (each a stream: base and size).
    pub fn vertex_fetch_consts(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for cf in &self.cf {
            if let Cf::Exec { addr, count, seq, .. } = *cf {
                for k in 0..count {
                    if (seq >> (2 * k)) & 1 == 1 {
                        if let Some(Instr::Fetch(Fetch::Vertex(v))) = self.instr_ref(addr + k, true) {
                            if !out.contains(&v.const_index) {
                                out.push(v.const_index);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    pub fn texture_consts(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for cf in &self.cf {
            if let Cf::Exec { addr, count, seq, .. } = *cf {
                for k in 0..count {
                    if (seq >> (2 * k)) & 1 == 1 {
                        if let Some(Instr::Fetch(Fetch::Texture(t))) = self.instr_ref(addr + k, true) {
                            if !out.contains(&t.const_index) {
                                out.push(t.const_index);
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// The 96-bit instruction at slot `slot`, typed as ALU or fetch.
    pub fn instr(&self, slot: u32, is_fetch: bool) -> Option<Instr> {
        let i = slot as usize * 3;
        let w: [u32; 3] = self.code.get(i..i + 3)?.try_into().ok()?;
        Some(if is_fetch { Instr::Fetch(decode_fetch(w)) } else { Instr::Alu(decode_alu(w)) })
    }

    pub fn disassemble(&self) -> String {
        let mut s = String::new();
        for (n, cf) in self.cf.iter().enumerate() {
            s += &format!("cf{n:02}: {cf:?}\n");
            if let Cf::Exec { addr, count, seq, .. } = *cf {
                for k in 0..count {
                    let is_fetch = (seq >> (2 * k)) & 1 == 1;
                    match self.instr(addr + k, is_fetch) {
                        Some(Instr::Alu(a)) => s += &format!("    {:3}: {}\n", addr + k, disasm_alu(&a)),
                        Some(Instr::Fetch(Fetch::Vertex(v))) => s += &format!(
                            "    {:3}: vfetch r{}.{} <- r{} const {} fmt {} stride {} offset {}{}\n",
                            addr + k, v.dst, dst_swiz(v.dst_swiz), v.src, v.const_index, v.format, v.stride, v.offset,
                            if v.mini { " (mini)" } else { "" }
                        ),
                        Some(Instr::Fetch(Fetch::Texture(t))) => s += &format!(
                            "    {:3}: tfetch{}D op{} r{}.{} <- r{} tf{}\n",
                            addr + k, t.dimension + 1, t.opcode, t.dst, dst_swiz(t.dst_swiz), t.src, t.const_index
                        ),
                        None => s += "    <out of range>\n",
                    }
                }
            }
        }
        s
    }
}

fn dst_swiz(sw: u32) -> String {
    (0..4).map(|i| ['x', 'y', 'z', 'w', '0', '1', '?', '_'][((sw >> (3 * i)) & 7) as usize]).collect()
}

fn src_str(a: &Alu, i: usize, ncomp: usize) -> String {
    let r = a.reg[i];
    let base = if a.is_temp[i] {
        if r & 0x40 != 0 { format!("r[aL+{}]", r & 0x3F) } else { format!("r{}", r & 0x3F) }
    } else {
        // same slot rule as the interpreter: which const_rel bit applies to this operand
        let addressed = match i {
            0 => a.const_rel[0],
            1 => if a.is_temp[0] { a.const_rel[0] } else { a.const_rel[1] },
            _ => if a.is_temp[0] && a.is_temp[1] { a.const_rel[0] } else { a.const_rel[1] },
        };
        match (addressed, a.const_addr_reg_rel) {
            (true, true) => format!("c[a0+{r}]"),
            (true, false) => format!("c[aL+{r}]"),
            _ => format!("c{r}"),
        }
    };
    let comps: String = (0..ncomp as u32).map(|c| ['x', 'y', 'z', 'w'][alu_component(a.swiz[i], c)]).collect();
    format!("{}{}.{}", if a.negate[i] { "-" } else { "" }, base, comps)
}

fn disasm_alu(a: &Alu) -> String {
    let dst = |d: u32, m: u32| {
        let mask: String = (0..4).map(|i| if m & (1 << i) != 0 { ['x', 'y', 'z', 'w'][i] } else { '_' }).collect();
        if a.export { format!("export{d}.{mask}") } else { format!("r{d}.{mask}") }
    };
    let vop = VECTOR_OPS[a.vector_op as usize];
    let n = VECTOR_OPERANDS[a.vector_op as usize] as usize;
    let vsrcs: Vec<String> = (0..n).map(|i| src_str(a, i, 4)).collect();
    let mut s = format!("{vop} {} <- {}", dst(a.vector_dest, a.vector_mask), vsrcs.join(", "));
    let sop = a.scalar_op as usize;
    if sop < SCALAR_OPS.len() && (a.scalar_mask != 0 || SCALAR_OPS[sop] != "retain_prev") {
        s += &format!(" | {} {} <- {}", SCALAR_OPS[sop], dst(if a.export { a.vector_dest } else { a.scalar_dest }, a.scalar_mask), src_str(a, 2, 2));
    }
    if a.predicated {
        s += &format!(" (pred {})", a.pred_cond);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-assembled shader: one control-flow exec (ending the program) over two fetch slots,
    /// a vertex fetch from fetch constant 95 and a 2D texture fetch from constant 3.
    fn shader() -> Shader {
        let exec_end = 1 | (2 << 12) | (0b0101 << 16); // addr 1, count 2, both fetches
        let cf = [exec_end, 2 << 12, 0]; // op 2 = exec end; the second CF is a nop
        let vfetch = [(1 << 5) | (2 << 12) | (31 << 20) | (2 << 25), 0x688 | (0x39 << 16), 12 | (4 << 8)];
        let tfetch = [1 | (3 << 20), 0x688 | (1 << 12) | (1 << 14), 1 << 14];
        Shader::new([cf, vfetch, tfetch].concat())
    }

    #[test]
    fn control_flow_and_fetches() {
        let s = shader();
        assert_eq!(s.cf[0], Cf::Exec { addr: 1, count: 2, seq: 0b0101, end: true, cond: ExecCond::Always });
        assert_eq!(s.vertex_fetch_consts(), vec![95]);
        assert_eq!(s.texture_consts(), vec![3]);
        let t = s.texture_fetch(3).expect("texture fetch from constant 3");
        assert_eq!((t.mag_filter, t.min_filter, t.dimension, t.dst), (1, 1, 1, 0));
        assert!(s.texture_fetch(4).is_none());
        match s.instr_ref(1, true) {
            Some(Instr::Fetch(Fetch::Vertex(v))) => {
                assert_eq!((v.src, v.dst, v.format, v.stride, v.offset), (1, 2, 0x39, 12, 4));
            }
            other => panic!("slot 1: {other:?}"),
        }
    }

    #[test]
    fn control_flow_stops_at_the_end() {
        // an exec end in the first pair: later words are instructions, not control flow
        let mut code = shader().code;
        code.extend_from_slice(&[0xFFFF_FFFF; 6]);
        assert_eq!(decode_control_flow(&code).len(), 2);
    }
}
