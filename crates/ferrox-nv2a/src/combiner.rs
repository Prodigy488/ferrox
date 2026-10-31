//! The NV2A register combiners: the small fixed pipeline that turns the interpolated
//! colours and the sampled texture into the final pixel colour.
//!
//! Games that use fixed-function lighting and texturing reach the same hardware as games with
//! pixel shaders: the driver programs up to eight general stages plus one final stage. This
//! module decodes those registers and evaluates them per pixel in floating point.
//!
//! Layout (from the hardware registers, as the reference emulators decode them):
//!  * input word: four bytes a, b, c, d (a in the top byte); each byte is register (bits 0-3),
//!    channel (bit 4: alpha for colour inputs / blue-to-alpha select for alpha inputs) and an
//!    input mapping (bits 5-7).
//!  * output word: cd destination (bits 0-3), ab destination (4-7), sum/mux destination (8-11),
//!    then flags: bit 12 cd is a dot product, bit 13 ab is a dot product, bit 14 third output is
//!    a mux, bits 15-17 output mapping, bit 18 cd blue to alpha, bit 19 ab blue to alpha.
//!  * final stage: word 0 holds a, b, c, d, word 1 holds e, f, g and the sum flags; the result is
//!    `d + mix(c, b, a)` for colour and `g` for alpha.

/// Registers a combiner input or output byte can name.
const C0: usize = 1;
const C1: usize = 2;
const FOG: usize = 3;
const V0: usize = 4;
const V1: usize = 5;
const T0: usize = 8;
const T3: usize = 11;
const R0: usize = 12;
const R1: usize = 13;
const V1R0_SUM: usize = 14;
const EF_PROD: usize = 15;

type Rgba = [f32; 4];
type Rgb = [f32; 3];

/// Everything the combiners read from the rest of the pipeline.
#[derive(Clone, Copy, Debug)]
pub struct Inputs {
    /// Diffuse colour, interpolated.
    pub v0: Rgba,
    /// Specular colour, interpolated.
    pub v1: Rgba,
    /// Sampled colour of texture stage 0 ((0, 0, 0, 1) when the stage is off).
    pub t0: Rgba,
    /// Whether texture stage 0 is on (it decides the starting alpha of r0).
    pub t0_enabled: bool,
}

/// The combiner state a game has programmed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Combiner {
    pub alpha_icw: [u32; 8],
    pub color_icw: [u32; 8],
    pub alpha_ocw: [u32; 8],
    pub color_ocw: [u32; 8],
    pub factor0: [u32; 8],
    pub factor1: [u32; 8],
    /// Final combiner inputs a-d, then e, f, g and the flags.
    pub final0: u32,
    pub final1: u32,
    /// Constants of the final stage.
    pub spec_fog: [u32; 2],
    /// Stage count (low byte) and flags.
    pub control: u32,
}

/// Builds an input word from four bytes.
pub fn icw(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

/// Register byte for an input: register number, channel and mapping bits.
pub const fn input(reg: u8, alpha_channel: bool, mapping: u8) -> u8 {
    reg | if alpha_channel { 0x10 } else { 0 } | mapping
}

/// Input mapping bits.
pub mod map {
    pub const UNSIGNED_IDENTITY: u8 = 0x00;
    pub const UNSIGNED_INVERT: u8 = 0x20;
    pub const EXPAND_NORMAL: u8 = 0x40;
    pub const EXPAND_NEGATE: u8 = 0x60;
    pub const HALFBIAS_NORMAL: u8 = 0x80;
    pub const HALFBIAS_NEGATE: u8 = 0xA0;
    pub const SIGNED_IDENTITY: u8 = 0xC0;
    pub const SIGNED_NEGATE: u8 = 0xE0;
}

/// Register numbers, for building words.
pub mod reg {
    pub const ZERO: u8 = 0;
    pub const C0: u8 = 1;
    pub const C1: u8 = 2;
    pub const FOG: u8 = 3;
    pub const V0: u8 = 4;
    pub const V1: u8 = 5;
    pub const T0: u8 = 8;
    pub const T1: u8 = 9;
    pub const T2: u8 = 10;
    pub const T3: u8 = 11;
    pub const R0: u8 = 12;
    pub const R1: u8 = 13;
    pub const V1R0_SUM: u8 = 14;
    pub const EF_PROD: u8 = 15;
}

/// Builds an output word: destinations plus the flags above.
pub fn ocw(cd: u8, ab: u8, sum: u8, flags: u32) -> u32 {
    (cd as u32) | (ab as u32) << 4 | (sum as u32) << 8 | flags
}

/// Output flag bits (already shifted into place).
pub mod out {
    pub const CD_DOT: u32 = 1 << 12;
    pub const AB_DOT: u32 = 1 << 13;
    pub const MUX: u32 = 1 << 14;
    pub const SHIFT_LEFT_1: u32 = 2 << 15;
    pub const BIAS: u32 = 1 << 15;
    pub const SHIFT_LEFT_1_BIAS: u32 = 3 << 15;
    pub const SHIFT_LEFT_2: u32 = 4 << 15;
    pub const SHIFT_RIGHT_1: u32 = 6 << 15;
    pub const CD_BLUE_TO_ALPHA: u32 = 1 << 18;
    pub const AB_BLUE_TO_ALPHA: u32 = 1 << 19;
}

fn bytes(w: u32) -> [u8; 4] {
    w.to_be_bytes()
}

fn argb(v: u32) -> Rgba {
    [
        (v >> 16 & 0xFF) as f32 / 255.0,
        (v >> 8 & 0xFF) as f32 / 255.0,
        (v & 0xFF) as f32 / 255.0,
        (v >> 24) as f32 / 255.0,
    ]
}

fn clamp1(x: f32) -> f32 {
    x.clamp(-1.0, 1.0)
}

fn map_input(x: f32, mapping: u8) -> f32 {
    match mapping & 0xE0 {
        map::UNSIGNED_IDENTITY => x.max(0.0),
        map::UNSIGNED_INVERT => 1.0 - x.clamp(0.0, 1.0),
        map::EXPAND_NORMAL => 2.0 * x.max(0.0) - 1.0,
        map::EXPAND_NEGATE => -2.0 * x.max(0.0) + 1.0,
        map::HALFBIAS_NORMAL => x.max(0.0) - 0.5,
        map::HALFBIAS_NEGATE => -x.max(0.0) + 0.5,
        map::SIGNED_IDENTITY => x,
        _ => -x,
    }
}

fn map_output(x: f32, mapping: u32) -> f32 {
    match mapping {
        1 => x - 0.5,
        2 => x * 2.0,
        3 => (x - 0.5) * 2.0,
        4 => x * 4.0,
        6 => x / 2.0,
        _ => x,
    }
}

fn dot(a: Rgb, b: Rgb) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn mul(a: Rgb, b: Rgb) -> Rgb {
    [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
}

fn add(a: Rgb, b: Rgb) -> Rgb {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

impl Combiner {
    /// Number of general stages in use.
    pub fn stages(&self) -> usize {
        ((self.control & 0xFF) as usize).min(8)
    }

    /// True once a game has programmed anything. Before that the rasteriser falls back to the
    /// plain texture-times-diffuse rule so simple tests still show something.
    pub fn programmed(&self) -> bool {
        self.stages() > 0 || self.final0 != 0 || self.final1 != 0
    }

    /// Reads a register by method offset, ignoring ones that are not combiner state.
    pub fn write(&mut self, method: u32, value: u32) -> bool {
        let slot = |base: u32| ((method - base) / 4) as usize;
        match method {
            0x260..=0x27C => self.alpha_icw[slot(0x260)] = value,
            0x288 => self.spec_fog_inputs(0, value),
            0x28C => self.spec_fog_inputs(1, value),
            0xA60..=0xA7C => self.factor0[slot(0xA60)] = value,
            0xA80..=0xA9C => self.factor1[slot(0xA80)] = value,
            0xAA0..=0xABC => self.alpha_ocw[slot(0xAA0)] = value,
            0xAC0..=0xADC => self.color_icw[slot(0xAC0)] = value,
            0x1E20 => self.spec_fog[0] = value,
            0x1E24 => self.spec_fog[1] = value,
            0x1E40..=0x1E5C => self.color_ocw[slot(0x1E40)] = value,
            0x1E60 => self.control = value,
            _ => return false,
        }
        true
    }

    fn spec_fog_inputs(&mut self, which: usize, value: u32) {
        if which == 0 {
            self.final0 = value;
        } else {
            self.final1 = value;
        }
    }

    /// Features the programmed state uses that the evaluator cannot reproduce yet.
    pub fn gaps(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut reads = |name: &str, regs: &[u8]| {
            for &r in regs {
                let r = (r & 0xF) as usize;
                let (what, hit) = match r {
                    FOG => ("the fog register", true),
                    9..=11 => ("texture stages 1-3", true),
                    _ => ("", false),
                };
                if hit {
                    let m = format!("register combiners: {name} reads {what}, which is not emulated");
                    if !found.contains(&m) {
                        found.push(m);
                    }
                }
            }
        };
        for i in 0..self.stages() {
            reads("a stage", &bytes(self.color_icw[i]));
            reads("a stage", &bytes(self.alpha_icw[i]));
        }
        if self.final0 != 0 || self.final1 != 0 {
            reads("the final stage", &bytes(self.final0));
            reads("the final stage", &bytes(self.final1)[..3]);
        }
        found
    }

    fn constants(&self, stage: usize) -> (Rgba, Rgba) {
        let unique0 = self.control & 0x1000 != 0;
        let unique1 = self.control & 0x10000 != 0;
        (
            argb(self.factor0[if unique0 { stage } else { 0 }]),
            argb(self.factor1[if unique1 { stage } else { 0 }]),
        )
    }

    /// Runs every stage and the final combiner; returns red, green, blue, alpha in 0..=1.
    pub fn run(&self, inp: &Inputs) -> Rgba {
        let mut regs = [[0.0f32; 4]; 16];
        regs[FOG] = [0.0, 0.0, 0.0, 1.0];
        regs[V0] = inp.v0;
        regs[V1] = inp.v1;
        regs[T0] = inp.t0;
        for t in regs.iter_mut().take(T3 + 1).skip(T0 + 1) {
            *t = [0.0, 0.0, 0.0, 1.0];
        }
        regs[R0] = [0.0, 0.0, 0.0, if inp.t0_enabled { inp.t0[3] } else { 1.0 }];
        for stage in 0..self.stages() {
            let (c0, c1) = self.constants(stage);
            regs[C0] = c0;
            regs[C1] = c1;
            self.rgb_stage(&mut regs, stage);
            self.alpha_stage(&mut regs, stage);
        }
        let (c0, c1) = (argb(self.spec_fog[0]), argb(self.spec_fog[1]));
        regs[C0] = c0;
        regs[C1] = c1;
        self.final_stage(&mut regs)
    }

    fn mux_picks_cd(regs: &[Rgba; 16], control: u32) -> bool {
        let a = regs[R0][3];
        if control & 0x100 != 0 {
            a >= 0.5
        } else {
            (a * 255.0) as u32 & 1 == 1
        }
    }

    fn rgb_in(regs: &[Rgba; 16], byte: u8) -> Rgb {
        let v = regs[(byte & 0xF) as usize];
        let base = if byte & 0x10 != 0 { [v[3]; 3] } else { [v[0], v[1], v[2]] };
        [map_input(base[0], byte), map_input(base[1], byte), map_input(base[2], byte)]
    }

    fn alpha_in(regs: &[Rgba; 16], byte: u8) -> f32 {
        let v = regs[(byte & 0xF) as usize];
        map_input(if byte & 0x10 != 0 { v[3] } else { v[2] }, byte)
    }

    fn writable(r: usize) -> bool {
        matches!(r, V0 | V1 | T0..=R1)
    }

    fn rgb_stage(&self, regs: &mut [Rgba; 16], stage: usize) {
        let [a, b, c, d] = bytes(self.color_icw[stage]).map(|x| Self::rgb_in(regs, x));
        let w = self.color_ocw[stage];
        let (cd_dst, ab_dst, sum_dst) = ((w & 0xF) as usize, (w >> 4 & 0xF) as usize, (w >> 8 & 0xF) as usize);
        let mapping = w >> 15 & 7;
        let ab = if w & out::AB_DOT != 0 { [dot(a, b); 3] } else { mul(a, b) };
        let cd = if w & out::CD_DOT != 0 { [dot(c, d); 3] } else { mul(c, d) };
        let sum = if w & out::MUX != 0 {
            if Self::mux_picks_cd(regs, self.control) { cd } else { ab }
        } else {
            add(ab, cd)
        };
        let fin = |v: Rgb| v.map(|x| clamp1(map_output(x, mapping)));
        let (abm, cdm, summ) = (fin(ab), fin(cd), fin(sum));
        if Self::writable(ab_dst) {
            regs[ab_dst][..3].copy_from_slice(&abm);
            if w & out::AB_BLUE_TO_ALPHA != 0 {
                regs[ab_dst][3] = abm[2];
            }
        }
        if Self::writable(cd_dst) {
            regs[cd_dst][..3].copy_from_slice(&cdm);
            if w & out::CD_BLUE_TO_ALPHA != 0 {
                regs[cd_dst][3] = cdm[2];
            }
        }
        if Self::writable(sum_dst) {
            regs[sum_dst][..3].copy_from_slice(&summ);
        }
    }

    fn alpha_stage(&self, regs: &mut [Rgba; 16], stage: usize) {
        let [a, b, c, d] = bytes(self.alpha_icw[stage]).map(|x| Self::alpha_in(regs, x));
        let w = self.alpha_ocw[stage];
        let (cd_dst, ab_dst, sum_dst) = ((w & 0xF) as usize, (w >> 4 & 0xF) as usize, (w >> 8 & 0xF) as usize);
        let mapping = w >> 15 & 7;
        let (ab, cd) = (a * b, c * d);
        let sum = if w & out::MUX != 0 {
            if Self::mux_picks_cd(regs, self.control) { cd } else { ab }
        } else {
            ab + cd
        };
        let fin = |v: f32| clamp1(map_output(v, mapping));
        if Self::writable(ab_dst) {
            regs[ab_dst][3] = fin(ab);
        }
        if Self::writable(cd_dst) {
            regs[cd_dst][3] = fin(cd);
        }
        if Self::writable(sum_dst) {
            regs[sum_dst][3] = fin(sum);
        }
    }

    fn final_stage(&self, regs: &mut [Rgba; 16]) -> Rgba {
        if self.final0 == 0 && self.final1 == 0 {
            return [0.0; 4];
        }
        let flags = (self.final1 & 0xFF) as u8;
        let v1 = regs[V1];
        let r0 = regs[R0];
        let s1 = if flags & 0x40 != 0 { [1.0 - v1[0], 1.0 - v1[1], 1.0 - v1[2]] } else { [v1[0], v1[1], v1[2]] };
        let s2 = if flags & 0x20 != 0 { [1.0 - r0[0], 1.0 - r0[1], 1.0 - r0[2]] } else { [r0[0], r0[1], r0[2]] };
        let mut sum = add(s1, s2);
        if flags & 0x80 != 0 {
            sum = sum.map(|x| x.clamp(0.0, 1.0));
        }
        regs[V1R0_SUM] = [sum[0], sum[1], sum[2], 0.0];
        let [e, f, g, _flags] = bytes(self.final1);
        let e = Self::rgb_in(regs, e);
        let f = Self::rgb_in(regs, f);
        let ef = mul(e, f);
        regs[EF_PROD] = [ef[0], ef[1], ef[2], 0.0];
        let [a, b, c, d] = bytes(self.final0).map(|x| Self::rgb_in(regs, x));
        let mut rgb = [0.0; 3];
        for k in 0..3 {
            rgb[k] = d[k] + (c[k] * (1.0 - a[k]) + b[k] * a[k]);
        }
        let alpha = Self::alpha_in(regs, g);
        [rgb[0], rgb[1], rgb[2], alpha]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::reg::{C0, EF_PROD, FOG, R0, R1, T0, T1, V0, V1R0_SUM, ZERO};

    fn inputs() -> Inputs {
        Inputs { v0: [0.5, 0.25, 1.0, 0.5], v1: [0.1, 0.2, 0.3, 0.4], t0: [1.0, 0.5, 0.5, 0.8], t0_enabled: true }
    }

    fn rgb(r: u8) -> u8 {
        input(r, false, 0)
    }

    fn one() -> u8 {
        input(ZERO, false, map::UNSIGNED_INVERT)
    }

    /// stage 0: r0 = t0 * v0 (colour) and r0.a = t0.a * v0.a; final: r0
    fn modulate() -> Combiner {
        let mut c = Combiner::default();
        c.control = 1;
        c.color_icw[0] = icw(rgb(T0), rgb(V0), rgb(ZERO), rgb(ZERO));
        c.color_ocw[0] = ocw(ZERO, R0, ZERO, 0);
        c.alpha_icw[0] = icw(input(T0, true, 0), input(V0, true, 0), 0, 0);
        c.alpha_ocw[0] = ocw(ZERO, R0, ZERO, 0);
        c.final0 = icw(rgb(ZERO), rgb(ZERO), rgb(R0), rgb(ZERO));
        c.final1 = icw(rgb(ZERO), rgb(ZERO), input(R0, true, 0), 0);
        c
    }

    fn close(a: [f32; 4], b: [f32; 4]) {
        for k in 0..4 {
            assert!((a[k] - b[k]).abs() < 1e-4, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn nothing_programmed_is_reported_as_such() {
        let c = Combiner::default();
        assert!(!c.programmed());
        assert!(modulate().programmed());
    }

    #[test]
    fn modulate_multiplies_texture_and_diffuse() {
        close(modulate().run(&inputs()), [0.5, 0.125, 0.5, 0.4]);
    }

    #[test]
    fn the_final_stage_mixes_a_toward_b() {
        // fragment = d + mix(c, b, a) with a = 0.25 constant, b = v0, c = t0
        let mut c = modulate();
        c.control = 0;
        c.spec_fog[0] = 0x4040_4040; // 64/255 on every channel
        c.final0 = icw(rgb(C0), rgb(V0), rgb(T0), rgb(ZERO));
        c.final1 = icw(rgb(ZERO), rgb(ZERO), input(V0, true, 0), 0);
        let out = c.run(&inputs());
        let a = 64.0 / 255.0;
        close(out, [1.0 * (1.0 - a) + 0.5 * a, 0.5 * (1.0 - a) + 0.25 * a, 0.5 * (1.0 - a) + 1.0 * a, 0.5]);
    }

    #[test]
    fn input_mappings_and_constants() {
        let regs = {
            let mut r = [[0.0f32; 4]; 16];
            r[V0 as usize] = [0.25, 0.5, 0.75, 1.0];
            r
        };
        let m = |mapping| Combiner::rgb_in(&regs, input(V0, false, mapping));
        assert_eq!(m(map::UNSIGNED_IDENTITY), [0.25, 0.5, 0.75]);
        assert_eq!(m(map::UNSIGNED_INVERT), [0.75, 0.5, 0.25]);
        assert_eq!(m(map::EXPAND_NORMAL), [-0.5, 0.0, 0.5]);
        assert_eq!(m(map::HALFBIAS_NORMAL), [-0.25, 0.0, 0.25]);
        assert_eq!(m(map::SIGNED_NEGATE), [-0.25, -0.5, -0.75]);
        assert_eq!(Combiner::rgb_in(&regs, input(V0, true, 0)), [1.0; 3], "alpha channel replicated");
        // ZERO with the invert mapping is the constant one
        assert_eq!(Combiner::rgb_in(&regs, one()), [1.0; 3]);
    }

    #[test]
    fn output_scale_and_bias_clamp_to_signed_one() {
        let mut c = modulate();
        c.color_ocw[0] = ocw(ZERO, R0, ZERO, out::SHIFT_LEFT_2);
        let out = c.run(&inputs());
        // 0.5 * 4 = 2.0 clamps to 1; 0.125 * 4 = 0.5
        close(out, [1.0, 0.5, 1.0, 0.4]);
    }

    #[test]
    fn dot_products_and_blue_to_alpha() {
        let mut c = modulate();
        c.color_icw[0] = icw(rgb(T0), rgb(V0), rgb(ZERO), rgb(ZERO));
        c.color_ocw[0] = ocw(ZERO, R0, ZERO, out::AB_DOT | out::AB_BLUE_TO_ALPHA);
        // alpha stage writes nothing so the blue-to-alpha value survives
        c.alpha_ocw[0] = ocw(ZERO, ZERO, ZERO, 0);
        let out = c.run(&inputs());
        let d = 1.0 * 0.5 + 0.5 * 0.25 + 0.5 * 1.0;
        assert!(d > 1.0);
        // the dot product clamps to 1 and its blue also lands in r0.a
        close(out, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn the_third_output_can_mux_on_r0_alpha() {
        let mut c = Combiner::default();
        c.control = 1; // least-significant-bit mux
        c.color_icw[0] = icw(rgb(V0), one(), rgb(T0), one());
        c.color_ocw[0] = ocw(ZERO, ZERO, R1, out::MUX);
        c.final0 = icw(rgb(ZERO), rgb(ZERO), rgb(R1), rgb(ZERO));
        c.final1 = icw(0, 0, one(), 0);
        let mut i = inputs();
        i.t0[3] = 1.0; // r0.a = 1.0 -> bit 0 of 255 is set: pick cd (the texture)
        close(c.run(&i), [1.0, 0.5, 0.5, 1.0]);
        i.t0[3] = 0.0; // r0.a = 0 -> pick ab (the diffuse colour)
        close(c.run(&i), [0.5, 0.25, 1.0, 1.0]);
        c.control = 0x101; // most-significant-bit mux
        i.t0[3] = 0.4;
        close(c.run(&i), [0.5, 0.25, 1.0, 1.0]);
    }

    #[test]
    fn unique_constants_follow_the_stage() {
        let mut c = Combiner::default();
        c.control = 2 | 0x1000; // two stages, c0 unique per stage
        c.factor0[0] = 0xFF80_0000; // r = 0.5
        c.factor0[1] = 0xFF00_8000; // g = 0.5
        for s in 0..2 {
            c.color_icw[s] = icw(rgb(C0), one(), rgb(R0), one());
            c.color_ocw[s] = ocw(ZERO, ZERO, R0, 0);
        }
        c.final0 = icw(rgb(ZERO), rgb(ZERO), rgb(R0), rgb(ZERO));
        c.final1 = icw(0, 0, one(), 0);
        // stage 0: r0 = c0_0 + r0(0); stage 1: r0 = c0_1 + r0
        close(c.run(&inputs()), [128.0 / 255.0, 128.0 / 255.0, 0.0, 1.0]);
    }

    #[test]
    fn the_final_stage_can_use_the_sum_and_the_product() {
        let mut c = Combiner::default();
        c.final0 = icw(rgb(ZERO), rgb(ZERO), rgb(V1R0_SUM), rgb(EF_PROD));
        c.final1 = icw(rgb(V0), rgb(T0), input(V0, true, 0), 0x80);
        let i = inputs();
        let out = c.run(&i);
        // sum = v1 + r0 (0) = v1, product = v0 * t0, result = d + c
        close(out, [0.1 + 0.5, 0.2 + 0.125, 0.3 + 0.5, 0.5]);
    }

    #[test]
    fn reading_the_fog_register_or_other_texture_stages_is_a_gap() {
        let mut c = modulate();
        assert!(c.gaps().is_empty());
        c.color_icw[0] = icw(rgb(T0), rgb(FOG), rgb(T1), rgb(ZERO));
        let gaps = c.gaps();
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(gaps.iter().any(|g| g.contains("fog")));
        assert!(gaps.iter().any(|g| g.contains("texture stages 1-3")));
    }

    #[test]
    fn methods_land_in_the_right_slots() {
        let mut c = Combiner::default();
        assert!(c.write(0x260 + 4 * 3, 7));
        assert!(c.write(0xAC0, 8));
        assert!(c.write(0xAA0 + 28, 9));
        assert!(c.write(0x1E40 + 4, 10));
        assert!(c.write(0x1E60, 0x0101));
        assert!(c.write(0x288, 11) && c.write(0x28C, 12));
        assert!(!c.write(0x300, 1));
        assert_eq!((c.alpha_icw[3], c.color_icw[0], c.alpha_ocw[7], c.color_ocw[1]), (7, 8, 9, 10));
        assert_eq!((c.control, c.final0, c.final1), (0x0101, 11, 12));
    }
}
