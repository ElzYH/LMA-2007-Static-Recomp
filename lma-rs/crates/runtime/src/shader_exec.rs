//! Reference interpreter for Xenos shader microcode (semantics follow Xenia, BSD-3-Clause).

use crate::shader::*;

pub type Vec4 = [f32; 4];

/// What a shader invocation can read from the GPU / memory.
pub trait ShaderEnv {
    /// Float constant (index already offset for VS/PS: PS uses 256..511).
    fn float_const(&self, index: u32) -> Vec4;
    fn bool_const(&self, index: u32) -> bool;
    /// Loop constant: (count, start, step).
    fn loop_const(&self, index: u32) -> (u32, i32, i32);
    /// Vertex fetch constant: (byte address of the stream, endian mode).
    fn vertex_stream(&self, index: u32) -> (u32, u32);
    /// Read a dword from GPU-visible memory with the given endian mode.
    fn read_dword(&mut self, addr: u32, endian: u32) -> u32;
    /// Texture sample for a texture fetch (returns RGBA).
    fn sample(&mut self, fetch: &TextureFetch, coord: Vec4) -> Vec4;
}

#[derive(Clone, Debug, Default)]
pub struct Outputs {
    pub position: Vec4,
    pub interpolators: [Vec4; 16],
    pub point_size: f32,
    pub colors: [Vec4; 4],
    pub killed: bool,
}

pub struct Invocation {
    pub r: [Vec4; 64],
    pub pred: bool,
    pub ps: f32, // previous scalar result
    pub a0: i32,
    pub al: i32,
    pub out: Outputs,
}

impl Invocation {
    pub fn new() -> Self {
        Self { r: [[0.0; 4]; 64], pred: false, ps: 0.0, a0: 0, al: 0, out: Outputs::default() }
    }
}

impl Default for Invocation {
    fn default() -> Self {
        Self::new()
    }
}

fn mul0(a: f32, b: f32) -> f32 {
    // Direct3D 9 rule used by Xenos: 0 * anything = 0
    if a == 0.0 || b == 0.0 { 0.0 } else { a * b }
}

fn fmt_components(format: u32) -> usize {
    match format {
        33 | 36 => 1,
        25 | 31 | 34 | 37 => 2,
        16 | 17 | 57 => 3,
        _ => 4,
    }
}

pub fn half(h: u32) -> f32 {
    let h = h & 0xFFFF;
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = ((h >> 10) & 0x1F) as i32;
    let man = (h & 0x3FF) as f32;
    sign * match exp {
        0 => man * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + man / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Decode a vertex element at `addr` into up to 4 floats.
fn fetch_vertex<E: ShaderEnv>(env: &mut E, v: &VertexFetch, addr: u32, endian: u32) -> Vec4 {
    let mut o = [0.0, 0.0, 0.0, 1.0];
    let norm = |raw: u32, bits: u32| -> f32 {
        if v.signed {
            let s = ((raw << (32 - bits)) as i32 >> (32 - bits)) as f32;
            if v.normalized { (s / ((1u32 << (bits - 1)) - 1) as f32).max(-1.0) } else { s }
        } else if v.normalized {
            raw as f32 / ((1u64 << bits) - 1) as f32
        } else {
            raw as f32
        }
    };
    let mut w = |k: u32| env.read_dword(addr + 4 * k, endian);
    match v.format {
        36 | 37 | 38 | 57 => {
            for k in 0..fmt_components(v.format) {
                o[k] = f32::from_bits(w(k as u32));
            }
        }
        33..=35 => {
            for k in 0..fmt_components(v.format) {
                o[k] = norm(w(k as u32), 32);
            }
        }
        6 => {
            let d = w(0);
            for (k, slot) in o.iter_mut().enumerate() {
                *slot = norm((d >> (8 * k)) & 0xFF, 8);
            }
        }
        7 => {
            let d = w(0);
            o = [norm(d & 0x3FF, 10), norm((d >> 10) & 0x3FF, 10), norm((d >> 20) & 0x3FF, 10), norm(d >> 30, 2)];
        }
        25 | 26 => {
            for k in 0..fmt_components(v.format) {
                let d = w((k / 2) as u32);
                o[k] = norm((d >> (16 * (k % 2))) & 0xFFFF, 16);
            }
        }
        31 | 32 => {
            for k in 0..fmt_components(v.format) {
                let d = w((k / 2) as u32);
                o[k] = half(d >> (16 * (k % 2)));
            }
        }
        16 | 17 => {
            let d = w(0);
            let (bx, by) = if v.format == 16 { (10, 11) } else { (11, 11) };
            o[0] = norm(d & ((1 << bx) - 1), bx);
            o[1] = norm((d >> bx) & ((1 << by) - 1), by);
            o[2] = norm(d >> (bx + by), 32 - bx - by);
        }
        _ => {}
    }
    if v.exp_adjust != 0 {
        let s = 2f32.powi(v.exp_adjust);
        for x in o.iter_mut().take(fmt_components(v.format)) {
            *x *= s;
        }
    }
    o
}

fn apply_fetch_swizzle(dst: &mut Vec4, fetched: Vec4, swiz: u32) {
    for (i, d) in dst.iter_mut().enumerate() {
        match (swiz >> (3 * i)) & 7 {
            s @ 0..=3 => *d = fetched[s as usize],
            4 => *d = 0.0,
            5 => *d = 1.0,
            _ => {} // keep
        }
    }
}

fn b(x: bool) -> f32 {
    if x { 1.0 } else { 0.0 }
}

impl Shader {
    /// Run the shader. `pixel` selects constant banks and the meaning of exports.
    pub fn run<E: ShaderEnv>(&self, inv: &mut Invocation, env: &mut E, pixel: bool) {
        let const_base = if pixel { 256 } else { 0 };
        let mut cf_i = 0usize;
        let mut loops: Vec<(usize, u32, i32, i32)> = Vec::new(); // (body cf, remaining, saved aL, step)
        let mut calls: Vec<usize> = Vec::new();
        let mut guard = 0;
        let cond_ok = |cond: &ExecCond, inv: &Invocation, env: &E| match cond {
            ExecCond::Always => true,
            ExecCond::Bool { index, value } => env.bool_const(*index) == *value,
            ExecCond::Pred { value } => inv.pred == *value,
        };
        while cf_i < self.cf.len() {
            guard += 1;
            if guard > 100_000 {
                break; // runaway loop guard
            }
            let cf = self.cf[cf_i];
            cf_i += 1;
            match cf {
                Cf::Exec { addr, count, seq, end, cond } => {
                    if cond_ok(&cond, inv, env) {
                        for k in 0..count {
                            let is_fetch = (seq >> (2 * k)) & 1 == 1;
                            match self.instr_ref(addr + k, is_fetch) {
                                Some(Instr::Alu(a)) => exec_alu(a, inv, env, const_base, pixel),
                                Some(Instr::Fetch(f)) => exec_fetch(f, inv, env),
                                None => {}
                            }
                        }
                    }
                    if end {
                        break;
                    }
                }
                Cf::LoopStart { addr, loop_id, .. } => {
                    // the 32 loop constants are shared: the shader's loop_id already names the
                    // register (D3D gives pixel shaders 16-31); no per-stage offset
                    let (count, start, step) = env.loop_const(loop_id);
                    if count == 0 {
                        cf_i = addr as usize; // skip the loop body
                    } else {
                        loops.push((cf_i, count, inv.al, step));
                        inv.al = start;
                    }
                }
                Cf::LoopEnd { .. } => {
                    if let Some((body, remaining, saved_al, step)) = loops.pop() {
                        if remaining > 1 {
                            inv.al += step;
                            loops.push((body, remaining - 1, saved_al, step));
                            cf_i = body;
                        } else {
                            inv.al = saved_al;
                        }
                    }
                }
                Cf::CondCall { addr, cond } => {
                    if cond_ok(&cond, inv, env) {
                        calls.push(cf_i);
                        cf_i = addr as usize;
                    }
                }
                Cf::Return => match calls.pop() {
                    Some(r) => cf_i = r,
                    None => break,
                },
                Cf::CondJmp { addr, cond } => {
                    if cond_ok(&cond, inv, env) {
                        cf_i = addr as usize;
                    }
                }
                _ => {}
            }
        }
    }
}

fn exec_fetch<E: ShaderEnv>(f: &Fetch, inv: &mut Invocation, env: &mut E) {
    match f {
        Fetch::Vertex(v) => {
            if v.predicated && inv.pred != v.pred_cond {
                return;
            }
            let index = inv.r[v.src as usize][v.src_swiz as usize].round() as i64 as u32;
            let (base, endian) = env.vertex_stream(v.const_index);
            let elem = (index.wrapping_mul(v.stride) as i32).wrapping_add(v.offset);
            let addr = base.wrapping_add((elem as u32).wrapping_mul(4));
            let fetched = fetch_vertex(env, v, addr, endian);
            apply_fetch_swizzle(&mut inv.r[v.dst as usize], fetched, v.dst_swiz);
        }
        Fetch::Texture(t) => {
            if t.predicated && inv.pred != t.pred_cond {
                return;
            }
            if t.opcode != 1 {
                return; // LOD/gradient queries: not needed yet
            }
            let src = inv.r[t.src as usize];
            let coord: Vec4 = std::array::from_fn(|i| src[((t.src_swiz >> (2 * i)) & 3) as usize]);
            let texel = env.sample(t, coord);
            apply_fetch_swizzle(&mut inv.r[t.dst as usize], texel, t.dst_swiz);
        }
    }
}

fn operand<E: ShaderEnv>(a: &Alu, i: usize, inv: &Invocation, env: &E, const_base: u32) -> Vec4 {
    let reg = a.reg[i];
    let mut v = if a.is_temp[i] {
        let mut idx = (reg & 0x3F) as i32;
        if reg & 0x40 != 0 {
            idx += inv.al;
        }
        let mut v = inv.r[(idx & 63) as usize];
        if reg & 0x80 != 0 {
            v = v.map(f32::abs);
        }
        v
    } else {
        let addressed = match i {
            0 => a.const_rel[0],
            1 => {
                if a.is_temp[0] { a.const_rel[0] } else { a.const_rel[1] }
            }
            _ => {
                if a.is_temp[0] && a.is_temp[1] { a.const_rel[0] } else { a.const_rel[1] }
            }
        };
        let mut idx = reg as i32;
        if addressed {
            idx += if a.const_addr_reg_rel { inv.a0 } else { inv.al };
        }
        let mut v = env.float_const(const_base + (idx as u32 & 255));
        if a.abs_constants {
            v = v.map(f32::abs);
        }
        v
    };
    v = std::array::from_fn(|c| v[alu_component(a.swiz[i], c as u32)]);
    if a.negate[i] {
        v = v.map(|x| -x);
    }
    v
}

fn operand_scalar_const<E: ShaderEnv>(a: &Alu, inv: &Invocation, env: &E, const_base: u32) -> f32 {
    let addressed = if a.is_temp[0] && a.is_temp[1] { a.const_rel[0] } else { a.const_rel[1] };
    let mut idx = a.reg[2] as i32;
    if addressed {
        idx += if a.const_addr_reg_rel { inv.a0 } else { inv.al };
    }
    let mut c = env.float_const(const_base + (idx as u32 & 255))[alu_component(a.swiz[2], 3)];
    if a.abs_constants {
        c = c.abs();
    }
    if a.negate[2] { -c } else { c }
}

fn exec_alu<E: ShaderEnv>(a: &Alu, inv: &mut Invocation, env: &mut E, const_base: u32, pixel: bool) {
    if a.predicated && inv.pred != a.pred_cond {
        return;
    }
    let nvec = VECTOR_OPERANDS[a.vector_op as usize] as usize;
    // on the stack: a Vec per instruction was ~5% of the render thread's CPU vertex transform
    // (#304); operands the op doesn't have stay zero
    let mut src = [[0.0f32; 4]; 3];
    for (i, s) in src.iter_mut().enumerate().take(nvec.max(1)) {
        *s = operand(a, i, inv, env, const_base);
    }
    let [s0, s1, s2] = src;
    let zip = |f: &dyn Fn(f32, f32) -> f32| -> Vec4 { std::array::from_fn(|i| f(s0[i], s1[i])) };
    let mut pred_from_vec = None;
    let vres: Vec4 = match a.vector_op {
        0 => zip(&|x, y| x + y),
        1 => zip(&mul0),
        2 => zip(&|x, y| if x >= y { x } else { y }),
        3 => zip(&|x, y| if x < y { x } else { y }),
        4 => zip(&|x, y| b(x == y)),
        5 => zip(&|x, y| b(x > y)),
        6 => zip(&|x, y| b(x >= y)),
        7 => zip(&|x, y| b(x != y)),
        8 => s0.map(|x| x - x.floor()),
        9 => s0.map(f32::trunc),
        10 => s0.map(f32::floor),
        11 => std::array::from_fn(|i| mul0(s0[i], s1[i]) + s2[i]),
        12 => std::array::from_fn(|i| if s0[i] == 0.0 { s1[i] } else { s2[i] }),
        13 => std::array::from_fn(|i| if s0[i] >= 0.0 { s1[i] } else { s2[i] }),
        14 => std::array::from_fn(|i| if s0[i] > 0.0 { s1[i] } else { s2[i] }),
        15 => [(0..4).map(|i| mul0(s0[i], s1[i])).sum(); 4],
        16 => [(0..3).map(|i| mul0(s0[i], s1[i])).sum(); 4],
        17 => [mul0(s0[0], s1[0]) + mul0(s0[1], s1[1]) + s2[0]; 4],
        19 => [s0.iter().cloned().fold(f32::MIN, f32::max); 4],
        20..=23 => {
            let hit = match a.vector_op {
                20 => s0[3] == 0.0 && s1[3] == 0.0,
                21 => s0[3] != 0.0 || s1[3] != 0.0,
                22 => s0[3] > 0.0 && s1[3] > 0.0,
                _ => s0[3] >= 0.0 && s1[3] >= 0.0,
            };
            pred_from_vec = Some(hit);
            [if hit { 0.0 } else { s0[0] + 1.0 }; 4]
        }
        24..=27 => {
            let cmp = |x: f32, y: f32| match a.vector_op {
                24 => x == y,
                25 => x > y,
                26 => x >= y,
                _ => x != y,
            };
            let k = (0..4).any(|i| cmp(s0[i], s1[i]));
            if k && pixel {
                inv.out.killed = true;
            }
            [b(k); 4]
        }
        28 => [1.0, mul0(s0[1], s1[1]), s0[2], s1[3]],
        29 => {
            inv.a0 = (s0[3] + 0.5).floor().clamp(-256.0, 255.0) as i32;
            zip(&|x, y| if x >= y { x } else { y })
        }
        _ => [0.0; 4],
    };

    // scalar unit
    let sop = a.scalar_op as usize;
    let mut sres = inv.ps;
    let mut changes_ps = true;
    if sop < SCALAR_OPS.len() {
        let (x, y) = match SCALAR_OPERANDS[sop] {
            2 => {
                let c = operand_scalar_const(a, inv, env, const_base);
                let ti = ((a.scalar_op & 1) | ((a.is_temp[2] as u32) << 1) | (a.swiz[2] & 0x3C)) as usize;
                let mut t = inv.r[ti & 63][alu_component(a.swiz[2], 0)];
                if a.negate[2] {
                    t = -t;
                }
                (c, t)
            }
            1 => {
                let v = operand(a, 2, inv, env, const_base);
                // `a` is the swizzled W; `b` is X, or Z when the vector op uses 3 operands
                (v[3], v[if nvec == 3 { 2 } else { 0 }])
            }
            _ => (0.0, 0.0),
        };
        let ps = inv.ps;
        sres = match sop {
            0 => x + y,
            1 => x + ps,
            2 => mul0(x, y),
            3 => mul0(x, ps),
            4 => {
                if ps == f32::MIN || ps.is_infinite() || y <= 0.0 { f32::MIN } else { mul0(x, ps) }
            }
            5 => if x >= y { x } else { y },
            6 => if x < y { x } else { y },
            7 => b(x == 0.0),
            8 => b(x > 0.0),
            9 => b(x >= 0.0),
            10 => b(x != 0.0),
            11 => x - x.floor(),
            12 => x.trunc(),
            13 => x.floor(),
            14 => x.exp2(),
            15 => {
                let l = x.log2();
                if l == f32::NEG_INFINITY { f32::MIN } else { l }
            }
            16 => x.log2(),
            17 => {
                let r = 1.0 / x;
                if r.is_infinite() { f32::MAX.copysign(r) } else { r }
            }
            18 => {
                let r = 1.0 / x;
                if r.is_infinite() { 0.0 } else { r }
            }
            19 => 1.0 / x,
            20 => {
                let r = 1.0 / x.sqrt();
                if r.is_infinite() { f32::MAX } else { r }
            }
            21 => {
                let r = 1.0 / x.sqrt();
                if r.is_infinite() { 0.0 } else { r }
            }
            22 => 1.0 / x.sqrt(),
            23 | 24 => {
                inv.a0 = (x + 0.5).floor().clamp(-256.0, 255.0) as i32;
                if x >= y { x } else { y }
            }
            25 => x - y,
            26 => x - ps,
            27 => {
                inv.pred = x == 0.0;
                b(!inv.pred)
            }
            28 => {
                inv.pred = x != 0.0;
                b(!inv.pred)
            }
            29 => {
                inv.pred = x > 0.0;
                b(!inv.pred)
            }
            30 => {
                inv.pred = x >= 0.0;
                b(!inv.pred)
            }
            31 => {
                if x == 0.0 {
                    inv.pred = true;
                    1.0
                } else {
                    inv.pred = false;
                    if x == 1.0 { 0.0 } else { x }
                }
            }
            32 => {
                if x - 1.0 <= 0.0 {
                    inv.pred = true;
                    0.0
                } else {
                    inv.pred = false;
                    x - 1.0
                }
            }
            33 => {
                inv.pred = false;
                f32::MAX
            }
            34 => {
                inv.pred = x == 0.0;
                x
            }
            35..=39 => {
                let k = match sop {
                    35 => x == 0.0,
                    36 => x > 0.0,
                    37 => x >= 0.0,
                    38 => x != 0.0,
                    _ => x == 1.0,
                };
                if k && pixel {
                    inv.out.killed = true;
                }
                b(k)
            }
            40 => x.sqrt(),
            42 | 43 => mul0(x, y),
            44 | 45 => x + y,
            46 | 47 => x - y,
            48 => x.sin(),
            49 => x.cos(),
            50 => {
                changes_ps = false;
                ps
            }
            _ => ps,
        };
    }
    if let Some(p) = pred_from_vec {
        inv.pred = p;
    }
    let vres = if a.vector_clamp { vres.map(|v| v.clamp(0.0, 1.0)) } else { vres };
    let sres = if a.scalar_clamp { sres.clamp(0.0, 1.0) } else { sres };
    if changes_ps {
        inv.ps = sres;
    }

    if a.export {
        let vmask = a.vector_mask & !a.scalar_mask;
        let smask = a.scalar_mask & !a.vector_mask;
        let one_mask = a.vector_mask & a.scalar_mask;
        let zero_mask = if a.scalar_dest_rel { 0xF & !(a.vector_mask | a.scalar_mask) } else { 0 };
        let write = |t: &mut Vec4| {
            for i in 0..4 {
                if vmask & (1 << i) != 0 {
                    t[i] = vres[i];
                }
                if smask & (1 << i) != 0 {
                    t[i] = sres;
                }
                if one_mask & (1 << i) != 0 {
                    t[i] = 1.0;
                }
                if zero_mask & (1 << i) != 0 {
                    t[i] = 0.0;
                }
            }
        };
        match (pixel, a.vector_dest) {
            (false, 62) => write(&mut inv.out.position),
            (false, 63) => {
                let mut t = [inv.out.point_size, 0.0, 0.0, 0.0];
                write(&mut t);
                inv.out.point_size = t[0];
            }
            (false, d @ 0..=15) => write(&mut inv.out.interpolators[d as usize]),
            (true, d @ 0..=3) => write(&mut inv.out.colors[d as usize]),
            _ => {}
        }
    } else {
        let vd = (a.vector_dest as i32 + if a.vector_dest_rel { inv.al } else { 0 }) as usize & 63;
        for i in 0..4 {
            if a.vector_mask & (1 << i) != 0 {
                inv.r[vd][i] = vres[i];
            }
        }
        let sd = (a.scalar_dest as i32 + if a.scalar_dest_rel { inv.al } else { 0 }) as usize & 63;
        for i in 0..4 {
            if a.scalar_mask & (1 << i) != 0 {
                inv.r[sd][i] = sres;
            }
        }
    }
}
