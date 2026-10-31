//! The vertex program: the NV2A's programmable transform stage.
//!
//! A program is up to 136 instructions of four 32-bit words. Each instruction
//! can run one "MAC" operation (vector maths on up to three inputs) and one
//! "ILU" operation (a scalar function) at the same time, and may write the
//! result to a temporary register, an output register or a constant. Field
//! positions come from xemu (`vsh_fields.rs` is generated from its table);
//! behaviour follows xemu's translator in `vsh-prog.c` and Microsoft's
//! documented vertex-shader semantics for the operations.

#[path = "vsh_fields.rs"]
mod f;

pub const MAX_INSTRUCTIONS: usize = 136;
pub const CONSTANTS: usize = 192;

pub type Vec4 = [f32; 4];
pub type Token = [u32; 4];

fn get(t: &Token, field: (usize, u32, u32)) -> u32 {
    (t[field.0] >> field.1) & ((1u64 << field.2) - 1) as u32
}

fn set(t: &mut Token, field: (usize, u32, u32), v: u32) {
    let mask = (((1u64 << field.2) - 1) as u32) << field.1;
    t[field.0] = (t[field.0] & !mask) | ((v << field.1) & mask);
}

// output register numbers
pub const O_POS: usize = 0;
pub const O_D0: usize = 3;
pub const O_D1: usize = 4;
pub const O_FOG: usize = 5;
pub const O_T0: usize = 9;

#[derive(Clone)]
pub struct Program {
    pub slots: Vec<Token>,
    pub constants: Vec<Vec4>,
}

impl Default for Program {
    fn default() -> Self {
        Program { slots: vec![[0; 4]; MAX_INSTRUCTIONS], constants: vec![[0.0; 4]; CONSTANTS] }
    }
}

fn swizzle(v: Vec4, t: &Token, x: (usize, u32, u32), y: (usize, u32, u32), z: (usize, u32, u32), w: (usize, u32, u32)) -> Vec4 {
    [v[get(t, x) as usize], v[get(t, y) as usize], v[get(t, z) as usize], v[get(t, w) as usize]]
}

fn neg(v: Vec4, on: bool) -> Vec4 {
    if on { [-v[0], -v[1], -v[2], -v[3]] } else { v }
}

/// Reciprocal, with the clamped variant the hardware's RCC uses.
fn rcp(x: f32) -> f32 {
    1.0 / x
}

fn rcc(x: f32) -> f32 {
    let r = 1.0 / x;
    if r.is_nan() {
        return r;
    }
    const LO: f32 = 5.42101e-20;
    const HI: f32 = 1.84467e19;
    if r > 0.0 { r.clamp(LO, HI) } else { r.clamp(-HI, -LO) }
}

fn splat(x: f32) -> Vec4 {
    [x; 4]
}

impl Program {
    /// Runs the program from `start` over the vertex attributes `v`. Returns the
    /// output registers (`O_POS`, `O_D0`, ...).
    pub fn run(&self, start: usize, v: &[Vec4; 16]) -> [Vec4; 16] {
        let mut r = [[0.0f32; 4]; 16];
        // oPos is the same storage as R12
        r[12] = [0.0, 0.0, 0.0, 1.0];
        let mut o = [[0.0f32, 0.0, 0.0, 1.0]; 16];
        let mut a0 = 0i32;
        let mut consts = self.constants.clone();
        let mut pc = start;
        while pc < MAX_INSTRUCTIONS {
            let t = self.slots[pc];
            pc += 1;
            let (mac, ilu) = (get(&t, f::MAC), get(&t, f::ILU));
            if mac != 0 || ilu != 0 {
                let rel = if get(&t, f::A0X) != 0 { a0 } else { 0 };
                let const_at = (get(&t, f::CONST) as i32 + rel).clamp(0, CONSTANTS as i32 - 1) as usize;
                let input = |mux: u32, reg: u32| -> Vec4 {
                    match mux {
                        1 => r[reg as usize],
                        2 => v[get(&t, f::V) as usize],
                        3 => consts[const_at],
                        _ => [0.0; 4],
                    }
                };
                let a = neg(
                    swizzle(input(get(&t, f::A_MUX), get(&t, f::A_R)), &t, f::A_SWZ_X, f::A_SWZ_Y, f::A_SWZ_Z, f::A_SWZ_W),
                    get(&t, f::A_NEG) != 0,
                );
                let b = neg(
                    swizzle(input(get(&t, f::B_MUX), get(&t, f::B_R)), &t, f::B_SWZ_X, f::B_SWZ_Y, f::B_SWZ_Z, f::B_SWZ_W),
                    get(&t, f::B_NEG) != 0,
                );
                let c_reg = (get(&t, f::C_R_HIGH) << 2) | get(&t, f::C_R_LOW);
                let c_raw = input(get(&t, f::C_MUX), c_reg);
                let c = neg(
                    swizzle(c_raw, &t, f::C_SWZ_X, f::C_SWZ_Y, f::C_SWZ_Z, f::C_SWZ_W),
                    get(&t, f::C_NEG) != 0,
                );
                // the scalar functions read one component: the x swizzle
                let cs = neg(splat(c_raw[get(&t, f::C_SWZ_X) as usize]), get(&t, f::C_NEG) != 0)[0];

                let mac_result = mac_op(mac, a, b, c, &mut a0);
                let ilu_result = ilu_op(ilu, cs, c);

                let out_r = get(&t, f::OUT_R) as usize;
                let write_temp = |reg: usize, mask: u32, val: Vec4, r: &mut [Vec4; 16]| {
                    for k in 0..4 {
                        if mask & (8 >> k) != 0 {
                            r[reg][k] = val[k];
                        }
                    }
                };
                if mac != 0 {
                    // a MAC paired with an ILU may not write R1
                    let mask = if ilu != 0 && out_r == 1 { 0 } else { get(&t, f::OUT_MAC_MASK) };
                    if mac != 13 {
                        write_temp(out_r, mask, mac_result, &mut r);
                    }
                }
                if ilu != 0 {
                    let reg = if mac != 0 { 1 } else { out_r };
                    write_temp(reg, get(&t, f::OUT_ILU_MASK), ilu_result, &mut r);
                }
                let o_mask = get(&t, f::OUT_O_MASK);
                if o_mask != 0 {
                    let val = if get(&t, f::OUT_MUX) == 0 { mac_result } else { ilu_result };
                    let addr = get(&t, f::OUT_ADDRESS) as usize;
                    if get(&t, f::OUT_ORB) == 0 {
                        // writable constant
                        if let Some(c) = consts.get_mut(addr) {
                            for k in 0..4 {
                                if o_mask & (8 >> k) != 0 {
                                    c[k] = val[k];
                                }
                            }
                        }
                    } else {
                        let reg = addr & 0xF;
                        let dst = if reg == O_POS { &mut r[12] } else { &mut o[reg] };
                        // writes to the fog output keep the most significant masked component in x
                        if reg == O_FOG {
                            if let Some(k) = (0..4).find(|k| o_mask & (8 >> k) != 0) {
                                dst[0] = val[k];
                            }
                        } else {
                            for k in 0..4 {
                                if o_mask & (8 >> k) != 0 {
                                    dst[k] = val[k];
                                }
                            }
                        }
                    }
                }
                if mac == 13 {
                    a0 = a[0].floor() as i32;
                }
            }
            if get(&t, f::FINAL) != 0 {
                break;
            }
        }
        o[O_POS] = r[12];
        o
    }
}

fn mac_op(op: u32, a: Vec4, b: Vec4, c: Vec4, _a0: &mut i32) -> Vec4 {
    let dp3 = a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    match op {
        1 => a,
        2 => [a[0] * b[0], a[1] * b[1], a[2] * b[2], a[3] * b[3]],
        3 => [a[0] + c[0], a[1] + c[1], a[2] + c[2], a[3] + c[3]],
        4 => [a[0] * b[0] + c[0], a[1] * b[1] + c[1], a[2] * b[2] + c[2], a[3] * b[3] + c[3]],
        5 => splat(dp3),
        6 => splat(dp3 + b[3]),
        7 => splat(dp3 + a[3] * b[3]),
        8 => [1.0, a[1] * b[1], a[2], b[3]],
        9 => [a[0].min(b[0]), a[1].min(b[1]), a[2].min(b[2]), a[3].min(b[3])],
        10 => [a[0].max(b[0]), a[1].max(b[1]), a[2].max(b[2]), a[3].max(b[3])],
        11 => [(a[0] < b[0]) as u8 as f32, (a[1] < b[1]) as u8 as f32, (a[2] < b[2]) as u8 as f32, (a[3] < b[3]) as u8 as f32],
        12 => [(a[0] >= b[0]) as u8 as f32, (a[1] >= b[1]) as u8 as f32, (a[2] >= b[2]) as u8 as f32, (a[3] >= b[3]) as u8 as f32],
        13 => [a[0].floor(), 0.0, 0.0, 0.0], // ARL: the address register is set by the caller
        _ => [0.0; 4],
    }
}

fn ilu_op(op: u32, x: f32, c: Vec4) -> Vec4 {
    match op {
        1 => c,
        2 => splat(rcp(x)),
        3 => splat(rcc(x)),
        4 => splat(1.0 / x.abs().sqrt()),
        5 => {
            let fl = x.floor();
            [fl.exp2(), x - fl, x.exp2(), 1.0]
        }
        6 => {
            let ax = x.abs();
            if ax == 0.0 {
                [f32::NEG_INFINITY, 1.0, f32::NEG_INFINITY, 1.0]
            } else {
                let e = ax.log2().floor();
                [e, ax / e.exp2(), ax.log2(), 1.0]
            }
        }
        7 => {
            // LIT: c = (n.l, n.h, _, power)
            let y = c[0].max(0.0);
            let z = if c[0] > 0.0 { c[1].max(0.0).powf(c[3].clamp(-128.0, 128.0)) } else { 0.0 };
            [1.0, y, z, 1.0]
        }
        _ => [0.0; 4],
    }
}

/// A tiny assembler, so tests and demo programs can be written without
/// hand-packing bits. Encoding uses the same field table as the interpreter.
pub mod asm {
    use super::*;

    #[derive(Clone, Copy)]
    pub enum Src {
        R(u32),
        V(u32),
        C(u32),
        None,
    }

    #[derive(Clone, Copy)]
    pub struct In {
        pub src: Src,
        pub swz: [u32; 4],
        pub neg: bool,
    }

    pub const XYZW: [u32; 4] = [0, 1, 2, 3];

    pub fn r(n: u32) -> In {
        In { src: Src::R(n), swz: XYZW, neg: false }
    }
    pub fn v(n: u32) -> In {
        In { src: Src::V(n), swz: XYZW, neg: false }
    }
    pub fn c(n: u32) -> In {
        In { src: Src::C(n), swz: XYZW, neg: false }
    }
    impl In {
        pub fn swz(mut self, s: [u32; 4]) -> In {
            self.swz = s;
            self
        }
        pub fn neg(mut self) -> In {
            self.neg = true;
            self
        }
    }

    /// Where an instruction's result goes.
    #[derive(Clone, Copy, Default)]
    pub struct Out {
        pub temp: Option<(u32, u32)>,   // (register, mask)
        pub output: Option<(u32, u32)>, // (output register, mask)
    }

    pub fn to_temp(reg: u32, mask: u32) -> Out {
        Out { temp: Some((reg, mask)), output: None }
    }
    pub fn to_out(reg: usize, mask: u32) -> Out {
        Out { temp: None, output: Some((reg as u32, mask)) }
    }

    #[derive(Clone, Copy)]
    pub struct Inst {
        pub mac: u32,
        pub ilu: u32,
        pub a: Option<In>,
        pub b: Option<In>,
        pub c: Option<In>,
        pub mac_out: Out,
        pub ilu_out: Out,
        pub a0x: bool,
        pub last: bool,
    }

    pub const MOV: u32 = 1;
    pub const MUL: u32 = 2;
    pub const ADD: u32 = 3;
    pub const MAD: u32 = 4;
    pub const DP3: u32 = 5;
    pub const DPH: u32 = 6;
    pub const DP4: u32 = 7;
    pub const ARL: u32 = 13;
    pub const ILU_MOV: u32 = 1;
    pub const RCP: u32 = 2;
    pub const RCC: u32 = 3;
    pub const RSQ: u32 = 4;
    pub const EXP: u32 = 5;
    pub const LOG: u32 = 6;
    pub const LIT: u32 = 7;
    pub const XYZ: u32 = 0b1110;
    pub const ALL: u32 = 0b1111;
    pub const X: u32 = 0b1000;
    pub const Y: u32 = 0b0100;
    pub const Z: u32 = 0b0010;
    pub const W: u32 = 0b0001;

    impl Inst {
        pub fn mac(op: u32, out: Out, a: In, b: Option<In>, c: Option<In>) -> Inst {
            Inst { mac: op, ilu: 0, a: Some(a), b, c, mac_out: out, ilu_out: Out::default(), a0x: false, last: false }
        }
        pub fn ilu(op: u32, out: Out, c: In) -> Inst {
            Inst { mac: 0, ilu: op, a: None, b: None, c: Some(c), mac_out: Out::default(), ilu_out: out, a0x: false, last: false }
        }
        pub fn last(mut self) -> Inst {
            self.last = true;
            self
        }
        pub fn relative(mut self) -> Inst {
            self.a0x = true;
            self
        }
        /// Pairs an ILU operation (its result goes to R1 or an output) with this MAC operation.
        pub fn with_ilu(mut self, op: u32, out: Out, c: In) -> Inst {
            self.ilu = op;
            self.ilu_out = out;
            self.c = Some(c);
            self
        }

        pub fn encode(&self) -> Token {
            let mut t: Token = [0; 4];
            set(&mut t, f::MAC, self.mac);
            set(&mut t, f::ILU, self.ilu);
            let put_in = |t: &mut Token, i: Option<In>, mux: (usize, u32, u32), reg: Option<(usize, u32, u32)>, negf, sw: [(usize, u32, u32); 4]| {
                let Some(i) = i else { return };
                let (m, n) = match i.src {
                    Src::R(n) => (1, n),
                    Src::V(n) => (2, n),
                    Src::C(n) => (3, n),
                    Src::None => (0, 0),
                };
                set(t, mux, m);
                // an instruction has one V field and one constant field, shared by all its inputs
                match i.src {
                    Src::V(n) => {
                        assert!(get(t, f::V) == 0 || get(t, f::V) == n, "an instruction can read only one vertex attribute");
                        set(t, f::V, n)
                    }
                    Src::C(n) => {
                        assert!(get(t, f::CONST) == 0 || get(t, f::CONST) == n, "an instruction can read only one constant");
                        set(t, f::CONST, n)
                    }
                    _ => {}
                }
                if let (Some(reg), Src::R(_)) = (reg, i.src) {
                    set(t, reg, n);
                }
                set(t, negf, i.neg as u32);
                for k in 0..4 {
                    set(t, sw[k], i.swz[k]);
                }
            };
            put_in(&mut t, self.a, f::A_MUX, Some(f::A_R), f::A_NEG, [f::A_SWZ_X, f::A_SWZ_Y, f::A_SWZ_Z, f::A_SWZ_W]);
            put_in(&mut t, self.b, f::B_MUX, Some(f::B_R), f::B_NEG, [f::B_SWZ_X, f::B_SWZ_Y, f::B_SWZ_Z, f::B_SWZ_W]);
            put_in(&mut t, self.c, f::C_MUX, None, f::C_NEG, [f::C_SWZ_X, f::C_SWZ_Y, f::C_SWZ_Z, f::C_SWZ_W]);
            if let Some(Src::R(n)) = self.c.map(|c| c.src) {
                set(&mut t, f::C_R_HIGH, n >> 2);
                set(&mut t, f::C_R_LOW, n & 3);
            }
            if let Some((reg, mask)) = self.mac_out.temp {
                set(&mut t, f::OUT_R, reg);
                set(&mut t, f::OUT_MAC_MASK, mask);
            }
            if let Some((reg, mask)) = self.ilu_out.temp {
                if self.mac == 0 {
                    set(&mut t, f::OUT_R, reg);
                }
                set(&mut t, f::OUT_ILU_MASK, mask);
            }
            // an output register write comes from the MAC or the ILU (only one can use it)
            if let Some((reg, mask)) = self.mac_out.output.or(self.ilu_out.output) {
                set(&mut t, f::OUT_ORB, 1);
                set(&mut t, f::OUT_ADDRESS, reg);
                set(&mut t, f::OUT_O_MASK, mask);
                set(&mut t, f::OUT_MUX, self.mac_out.output.is_none() as u32);
            }
            set(&mut t, f::A0X, self.a0x as u32);
            set(&mut t, f::FINAL, self.last as u32);
            t
        }
    }

    /// A typical transform: `oPos = v0 * M` (M's rows in c0..c3) and `oD0 = v3`, as the words
    /// a program would upload. Handy for demos and tests.
    pub fn matrix_program() -> Vec<u32> {
        let mut insts: Vec<Inst> = [X, Y, Z, W]
            .into_iter()
            .enumerate()
            .map(|(i, mask)| Inst::mac(DP4, to_temp(0, mask), v(0), Some(c(i as u32)), None))
            .collect();
        insts.push(Inst::mac(MOV, to_out(O_POS, ALL), r(0), None, None));
        insts.push(Inst::mac(MOV, to_out(O_D0, ALL), v(3), None, None).last());
        assemble(&insts).into_iter().flatten().collect()
    }

    pub fn assemble(prog: &[Inst]) -> Vec<Token> {
        prog.iter().map(Inst::encode).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::asm::*;
    use super::*;

    fn program(insts: &[Inst], consts: &[(usize, Vec4)]) -> Program {
        let mut p = Program::default();
        for (i, t) in assemble(insts).into_iter().enumerate() {
            p.slots[i] = t;
        }
        for &(i, c) in consts {
            p.constants[i] = c;
        }
        p
    }

    fn inputs(list: &[(usize, Vec4)]) -> [Vec4; 16] {
        let mut v = [[0.0, 0.0, 0.0, 1.0]; 16];
        for &(i, x) in list {
            v[i] = x;
        }
        v
    }

    #[test]
    fn field_table_is_complete() {
        // every field fits in a word and no two instruction fields overlap
        let all = [
            f::ILU, f::MAC, f::CONST, f::V, f::A_NEG, f::A_SWZ_X, f::A_SWZ_Y, f::A_SWZ_Z, f::A_SWZ_W, f::A_R,
            f::A_MUX, f::B_NEG, f::B_SWZ_X, f::B_SWZ_Y, f::B_SWZ_Z, f::B_SWZ_W, f::B_R, f::B_MUX, f::C_NEG,
            f::C_SWZ_X, f::C_SWZ_Y, f::C_SWZ_Z, f::C_SWZ_W, f::C_R_HIGH, f::C_R_LOW, f::C_MUX, f::OUT_MAC_MASK,
            f::OUT_R, f::OUT_ILU_MASK, f::OUT_O_MASK, f::OUT_ORB, f::OUT_ADDRESS, f::OUT_MUX, f::A0X, f::FINAL,
        ];
        let mut used = [0u32; 4];
        for (w, s, l) in all {
            assert!(w < 4 && s + l <= 32);
            let m = (((1u64 << l) - 1) as u32) << s;
            assert_eq!(used[w] & m, 0, "overlap in word {w}");
            used[w] |= m;
        }
    }

    #[test]
    fn four_dp4s_transform_a_position() {
        // oPos = v0 * matrix, with the matrix columns in c0..c3 (a scale by 2 and a shift by (10, 20))
        let rows = [([2.0, 0.0, 0.0, 10.0], X), ([0.0, 2.0, 0.0, 20.0], Y), ([0.0, 0.0, 1.0, 0.0], Z), ([0.0, 0.0, 0.0, 1.0], W)];
        let mut insts: Vec<Inst> = Vec::new();
        for (i, (_, mask)) in rows.iter().enumerate() {
            insts.push(Inst::mac(DP4, to_out(O_POS, *mask), v(0), Some(c(i as u32)), None));
        }
        insts.last_mut().unwrap().last = true;
        let consts: Vec<(usize, Vec4)> = rows.iter().enumerate().map(|(i, (r, _))| (i, *r)).collect();
        let out = program(&insts, &consts).run(0, &inputs(&[(0, [3.0, 4.0, 5.0, 1.0])]));
        assert_eq!(out[O_POS], [16.0, 28.0, 5.0, 1.0]);
    }

    #[test]
    fn colour_passes_through_and_swizzle_negate_work() {
        let p = program(
            &[
                Inst::mac(MOV, to_out(O_D0, ALL), v(3), None, None),
                Inst::mac(MOV, to_out(O_T0, ALL), v(3).swz([3, 2, 1, 0]).neg(), None, None).last(),
            ],
            &[],
        );
        let out = p.run(0, &inputs(&[(3, [0.1, 0.2, 0.3, 0.4])]));
        assert_eq!(out[O_D0], [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(out[O_T0], [-0.4, -0.3, -0.2, -0.1]);
    }

    #[test]
    fn mad_mul_add_and_masks() {
        let p = program(
            &[
                Inst::mac(MAD, to_temp(2, ALL), v(0), Some(c(0)), Some(c(0))),
                Inst::mac(MUL, to_temp(2, X), r(2), Some(c(0)), None),
                Inst::mac(ADD, to_out(O_D1, ALL), r(2), None, Some(c(0))).last(),
            ],
            &[(0, [2.0, 2.0, 2.0, 2.0])],
        );
        let out = p.run(0, &inputs(&[(0, [1.0, 2.0, 3.0, 4.0])]));
        // R2 = v0*2+2 = (4,6,8,10); R2.x = 4*2 = 8; out = R2 + 2
        assert_eq!(out[O_D1], [10.0, 8.0, 10.0, 12.0]);
    }

    #[test]
    fn dot_products_and_perspective_divide_with_a_paired_ilu() {
        // R1.x = 1/w (ILU) while the MAC computes dp3; then oPos.xyz = pos.xyz * R1.x
        let p = program(
            &[
                Inst::mac(DP3, to_temp(3, ALL), v(0), Some(v(0)), None).with_ilu(RCC, to_temp(1, X), v(0).swz([3, 3, 3, 3])),
                Inst::mac(MUL, to_out(O_POS, XYZ), v(0), Some(r(1).swz([0, 0, 0, 0])), None).last(),
            ],
            &[],
        );
        let out = p.run(0, &inputs(&[(0, [8.0, 4.0, 2.0, 2.0])]));
        assert_eq!(out[O_POS][..3], [4.0, 2.0, 1.0]);
        assert_eq!(out[O_POS][3], 1.0, "w keeps its default because the mask excludes it");
    }

    #[test]
    fn address_register_indexes_constants() {
        let p = program(
            &[
                Inst::mac(ARL, to_temp(0, 0), v(1), None, None),
                Inst::mac(MOV, to_out(O_D0, ALL), c(10), None, None).relative().last(),
            ],
            &[(10, [1.0; 4]), (12, [7.0, 8.0, 9.0, 10.0])],
        );
        let out = p.run(0, &inputs(&[(1, [2.7, 0.0, 0.0, 1.0])]));
        assert_eq!(out[O_D0], [7.0, 8.0, 9.0, 10.0]);
    }

    #[test]
    fn scalar_functions() {
        let run = |op, x: f32| {
            let p = program(&[Inst::ilu(op, to_out(O_D0, ALL), c(0)).last()], &[(0, [x, 0.5, 0.0, 2.0])]);
            p.run(0, &inputs(&[]))[O_D0]
        };
        assert_eq!(run(RCP, 4.0), [0.25; 4]);
        assert_eq!(run(RSQ, 16.0), [0.25; 4]);
        assert_eq!(run(RCC, 0.0)[0], 1.84467e19);
        assert_eq!(run(EXP, 3.5), [8.0, 0.5, 2f32.powf(3.5), 1.0]);
        assert_eq!(run(LOG, 8.0), [3.0, 1.0, 3.0, 1.0]);
        // LIT with n.l = 2, n.h = 0.5, power 2
        assert_eq!(run(LIT, 2.0), [1.0, 2.0, 0.25, 1.0]);
        assert_eq!(run(LIT, -1.0), [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn execution_stops_at_the_final_instruction_and_starts_where_told() {
        let p = program(
            &[
                Inst::mac(MOV, to_out(O_D0, ALL), c(0), None, None).last(),
                Inst::mac(MOV, to_out(O_D0, ALL), c(1), None, None).last(),
            ],
            &[(0, [1.0; 4]), (1, [2.0; 4])],
        );
        assert_eq!(p.run(0, &inputs(&[]))[O_D0], [1.0; 4]);
        assert_eq!(p.run(1, &inputs(&[]))[O_D0], [2.0; 4]);
    }

    #[test]
    fn fog_output_takes_one_component() {
        let p = program(&[Inst::mac(MOV, to_out(O_FOG, Y | Z), c(0), None, None).last()], &[(0, [1.0, 2.0, 3.0, 4.0])]);
        assert_eq!(p.run(0, &inputs(&[]))[O_FOG][0], 2.0);
    }
}
