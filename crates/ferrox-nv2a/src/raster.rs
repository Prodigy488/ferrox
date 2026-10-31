//! A small software triangle rasteriser: the reference renderer. It turns
//! already-positioned vertices into pixels in a colour surface in guest memory,
//! so the command front end can be checked end to end before any GPU API backend
//! exists. Sampling is at pixel centres with a top-left fill rule, so triangles
//! that share an edge never overdraw or leave gaps.

use crate::combiner::{Combiner, Inputs};
use crate::texture::Texture;
use ferrox_machine::RamView;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    /// Depth as the vertex program left it (window space, before scaling to the buffer).
    pub z: f32,
    /// The clip-space w the program left in oPos (1.0 when positions are already in pixels).
    /// Attributes are interpolated perspective-correctly with it.
    pub w: f32,
    /// Red, green, blue, alpha in 0..=1.
    pub color: [f32; 4],
    /// Specular colour (the second colour the combiners can read as v1).
    pub spec: [f32; 4],
    /// First texture coordinate set (s, t, r, q).
    pub tex: [f32; 4],
}

/// A colour surface: where it is, how it is laid out, and the rectangle (the
/// "clip") that drawing is confined to, in surface coordinates.
#[derive(Clone, Copy)]
pub struct Surface {
    pub ram: RamView,
    /// Guest address of pixel (0, 0).
    pub origin: u64,
    pub pitch: u64,
    pub format: u32,
    pub clip: (u32, u32, u32, u32), // x, y, width, height
    /// Per-pixel operations after the combiners: alpha test, blending, write mask.
    pub ops: PixelOps,
}

/// A blend factor, decoded from the method value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Factor {
    Zero,
    One,
    SrcColor,
    OneMinusSrcColor,
    SrcAlpha,
    OneMinusSrcAlpha,
    DstAlpha,
    OneMinusDstAlpha,
    DstColor,
    OneMinusDstColor,
    SrcAlphaSaturate,
    ConstColor,
    OneMinusConstColor,
    ConstAlpha,
    OneMinusConstAlpha,
}

impl Factor {
    pub fn from_method(v: u32) -> Option<Factor> {
        Some(match v {
            0 => Factor::Zero,
            1 => Factor::One,
            0x300 => Factor::SrcColor,
            0x301 => Factor::OneMinusSrcColor,
            0x302 => Factor::SrcAlpha,
            0x303 => Factor::OneMinusSrcAlpha,
            0x304 => Factor::DstAlpha,
            0x305 => Factor::OneMinusDstAlpha,
            0x306 => Factor::DstColor,
            0x307 => Factor::OneMinusDstColor,
            0x308 => Factor::SrcAlphaSaturate,
            0x8001 => Factor::ConstColor,
            0x8002 => Factor::OneMinusConstColor,
            0x8003 => Factor::ConstAlpha,
            0x8004 => Factor::OneMinusConstAlpha,
            _ => return None,
        })
    }
}

/// How source and destination are combined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Equation {
    Add,
    Subtract,
    ReverseSubtract,
    Min,
    Max,
}

impl Equation {
    pub fn from_method(v: u32) -> Option<Equation> {
        Some(match v {
            0x8006 => Equation::Add,
            0x800A => Equation::Subtract,
            0x800B => Equation::ReverseSubtract,
            0x8007 => Equation::Min,
            0x8008 => Equation::Max,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Blend {
    pub src: Factor,
    pub dst: Factor,
    pub equation: Equation,
    /// Constant colour, red, green, blue, alpha.
    pub constant: [f32; 4],
}

/// Alpha test: function (0 never .. 7 always, as for depth) and the 8-bit reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlphaTest {
    pub func: u32,
    pub reference: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelOps {
    pub alpha_test: Option<AlphaTest>,
    pub blend: Option<Blend>,
    /// Which channels are written: red, green, blue, alpha.
    pub mask: [bool; 4],
}

impl Default for PixelOps {
    fn default() -> Self {
        PixelOps { alpha_test: None, blend: None, mask: [true; 4] }
    }
}

fn compare(func: u32, new: u32, old: u32) -> bool {
    match func {
        0 => false,
        1 => new < old,
        2 => new == old,
        3 => new <= old,
        4 => new > old,
        5 => new != old,
        6 => new >= old,
        _ => true,
    }
}

impl Blend {
    fn factor(&self, f: Factor, src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
        let inv = |v: [f32; 4]| [1.0 - v[0], 1.0 - v[1], 1.0 - v[2], 1.0 - v[3]];
        let a = |x: f32| [x; 4];
        match f {
            Factor::Zero => [0.0; 4],
            Factor::One => [1.0; 4],
            Factor::SrcColor => src,
            Factor::OneMinusSrcColor => inv(src),
            Factor::SrcAlpha => a(src[3]),
            Factor::OneMinusSrcAlpha => a(1.0 - src[3]),
            Factor::DstAlpha => a(dst[3]),
            Factor::OneMinusDstAlpha => a(1.0 - dst[3]),
            Factor::DstColor => dst,
            Factor::OneMinusDstColor => inv(dst),
            Factor::SrcAlphaSaturate => {
                let k = src[3].min(1.0 - dst[3]);
                [k, k, k, 1.0]
            }
            Factor::ConstColor => self.constant,
            Factor::OneMinusConstColor => inv(self.constant),
            Factor::ConstAlpha => a(self.constant[3]),
            Factor::OneMinusConstAlpha => a(1.0 - self.constant[3]),
        }
    }

    /// The blended colour for a source fragment over the existing destination colour.
    pub fn apply(&self, src: [f32; 4], dst: [f32; 4]) -> [f32; 4] {
        let sf = self.factor(self.src, src, dst);
        let df = self.factor(self.dst, src, dst);
        let mut out = [0.0; 4];
        for k in 0..4 {
            let (s, d) = (src[k] * sf[k], dst[k] * df[k]);
            out[k] = match self.equation {
                Equation::Add => s + d,
                Equation::Subtract => s - d,
                Equation::ReverseSubtract => d - s,
                Equation::Min => src[k].min(dst[k]),
                Equation::Max => src[k].max(dst[k]),
            }
            .clamp(0.0, 1.0);
        }
        out
    }
}

/// A depth buffer and how to test against it.
#[derive(Clone, Copy)]
pub struct Depth {
    pub ram: RamView,
    pub origin: u64,
    pub pitch: u64,
    /// 1 = 16-bit, 2 = 24-bit depth with 8-bit stencil.
    pub format: u32,
    /// 0 never, 1 less, 2 equal, 3 less-or-equal, 4 greater, 5 not equal, 6 greater-or-equal, 7 always.
    pub func: u32,
    pub write: bool,
    /// The depth that maps to the buffer's largest value.
    pub clip_max: f32,
}

impl Depth {
    pub fn bytes_per_pixel(&self) -> u64 {
        if self.format == 1 { 2 } else { 4 }
    }

    /// Buffer value for a window-space depth.
    pub fn quantise(&self, z: f32) -> u32 {
        let max = if self.format == 1 { 0xFFFF } else { 0xFF_FFFF } as f32;
        let n = if self.clip_max > 0.0 { z / self.clip_max } else { z };
        (n.clamp(0.0, 1.0) * max + 0.5) as u32
    }

    fn passes(&self, new: u32, old: u32) -> bool {
        match self.func {
            0 => false,
            1 => new < old,
            2 => new == old,
            3 => new <= old,
            4 => new > old,
            5 => new != old,
            6 => new >= old,
            _ => true,
        }
    }

    fn read(&self, at: u64) -> Option<u32> {
        if self.format == 1 {
            self.ram.bytes(at, 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as u32)
        } else {
            self.ram.read_u32(at).map(|v| v >> 8)
        }
    }

    fn store(&self, at: u64, z: u32) {
        if self.format == 1 {
            self.ram.write(at, &(z as u16).to_le_bytes());
        } else if let Some(old) = self.ram.read_u32(at) {
            self.ram.write_u32(at, (z << 8) | (old & 0xFF)); // keep the stencil byte
        }
    }
}

/// Bytes per pixel of a colour format, or `None` if the rasteriser cannot write it.
pub fn bytes_per_pixel(format: u32) -> Option<usize> {
    match format & 0xF {
        1..=3 => Some(2),
        4..=8 => Some(4),
        _ => None,
    }
}

fn quantise(v: f32, max: u32) -> u32 {
    (v.clamp(0.0, 1.0) * max as f32 + 0.5) as u32
}

fn pack(format: u32, c: [f32; 4]) -> u32 {
    let (r8, g8, b8, a8) = (quantise(c[0], 255), quantise(c[1], 255), quantise(c[2], 255), quantise(c[3], 255));
    match format & 0xF {
        // 16-bit: X1R5G5B5 (1, 2) and R5G6B5 (3)
        1 | 2 => 0x8000 | (quantise(c[0], 31) << 10) | (quantise(c[1], 31) << 5) | quantise(c[2], 31),
        3 => (quantise(c[0], 31) << 11) | (quantise(c[1], 63) << 5) | quantise(c[2], 31),
        // 32-bit: formats with an alpha channel keep it, the others store it as unused
        6 | 7 | 8 => (a8 << 24) | (r8 << 16) | (g8 << 8) | b8,
        _ => 0xFF00_0000 | (r8 << 16) | (g8 << 8) | b8,
    }
}

/// The inverse of `pack`: a stored pixel as red, green, blue, alpha in 0..=1.
fn unpack(format: u32, bits: u32) -> [f32; 4] {
    let f = |v: u32, max: u32| v as f32 / max as f32;
    match format & 0xF {
        1 | 2 => [f(bits >> 10 & 31, 31), f(bits >> 5 & 31, 31), f(bits & 31, 31), 1.0],
        3 => [f(bits >> 11 & 31, 31), f(bits >> 5 & 63, 63), f(bits & 31, 31), 1.0],
        6 | 7 | 8 => [f(bits >> 16 & 255, 255), f(bits >> 8 & 255, 255), f(bits & 255, 255), f(bits >> 24, 255)],
        _ => [f(bits >> 16 & 255, 255), f(bits >> 8 & 255, 255), f(bits & 255, 255), 1.0],
    }
}

fn edge(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> f32 {
    (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
}

/// Whether the edge a->b owns pixels exactly on it (top or left edge, for the
/// triangle's winding after it has been made positive).
fn owns_edge(a: (f32, f32), b: (f32, f32)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    dy < 0.0 || (dy == 0.0 && dx > 0.0)
}

/// Fills one triangle with colours interpolated from the corners. Returns the
/// number of pixels written. Either winding is drawn (no culling yet).
pub fn fill_triangle(
    s: &Surface,
    depth: Option<&Depth>,
    tex: Option<&Texture>,
    comb: Option<&Combiner>,
    v: [Vertex; 3],
) -> u64 {
    let Some(bpp) = bytes_per_pixel(s.format) else { return 0 };
    let area = edge((v[0].x, v[0].y), (v[1].x, v[1].y), (v[2].x, v[2].y));
    if area == 0.0 || !area.is_finite() {
        return 0;
    }
    // make the winding positive
    let v = if area < 0.0 { [v[0], v[2], v[1]] } else { v };
    let area = area.abs();
    let p: Vec<(f32, f32)> = v.iter().map(|q| (q.x, q.y)).collect();
    let (cx, cy, cw, ch) = s.clip;
    let min_x = p.iter().map(|q| q.0).fold(f32::MAX, f32::min).floor().max(0.0) as u32;
    let min_y = p.iter().map(|q| q.1).fold(f32::MAX, f32::min).floor().max(0.0) as u32;
    let max_x = (p.iter().map(|q| q.0).fold(f32::MIN, f32::max).ceil().max(0.0) as u32).min(cw);
    let max_y = (p.iter().map(|q| q.1).fold(f32::MIN, f32::max).ceil().max(0.0) as u32).min(ch);
    let owns = [owns_edge(p[1], p[2]), owns_edge(p[2], p[0]), owns_edge(p[0], p[1])];
    let mut written = 0;
    for y in min_y..max_y {
        for x in min_x..max_x {
            let pt = (x as f32 + 0.5, y as f32 + 0.5);
            let w = [edge(p[1], p[2], pt), edge(p[2], p[0], pt), edge(p[0], p[1], pt)];
            let inside = (0..3).all(|i| w[i] > 0.0 || (w[i] == 0.0 && owns[i]));
            if !inside {
                continue;
            }
            // perspective-correct weights: edge function over each vertex's w
            let inv = [1.0 / v[0].w.max(1e-12), 1.0 / v[1].w.max(1e-12), 1.0 / v[2].w.max(1e-12)];
            let pw = [w[0] * inv[0], w[1] * inv[1], w[2] * inv[2]];
            let psum = pw[0] + pw[1] + pw[2];
            let mut c = [0.0f32; 4];
            for (k, ck) in c.iter_mut().enumerate() {
                *ck = (pw[0] * v[0].color[k] + pw[1] * v[1].color[k] + pw[2] * v[2].color[k]) / psum;
            }
            let mut texel = [0.0, 0.0, 0.0, 1.0];
            if let Some(t) = tex {
                let mut st = [0.0f32; 4];
                for (k, sk) in st.iter_mut().enumerate() {
                    *sk = (pw[0] * v[0].tex[k] + pw[1] * v[1].tex[k] + pw[2] * v[2].tex[k]) / psum;
                }
                // projective: divide by q (treat 0 as 1 for programs that leave it unset)
                let q = if st[3] == 0.0 { 1.0 } else { st[3] };
                texel = t.sample(st[0] / q, st[1] / q);
            }
            match comb {
                Some(cb) => {
                    let mut sp = [0.0f32; 4];
                    for (k, sk) in sp.iter_mut().enumerate() {
                        *sk = (pw[0] * v[0].spec[k] + pw[1] * v[1].spec[k] + pw[2] * v[2].spec[k]) / psum;
                    }
                    c = cb.run(&Inputs { v0: c, v1: sp, t0: texel, t0_enabled: tex.is_some() });
                }
                None if tex.is_some() => {
                    // nothing programmed: plain texture times diffuse
                    for k in 0..4 {
                        c[k] *= texel[k];
                    }
                }
                None => {}
            }
            if let Some(t) = s.ops.alpha_test {
                if !compare(t.func, quantise(c[3], 255), t.reference) {
                    continue;
                }
            }
            if let Some(d) = depth {
                let z = (w[0] * v[0].z + w[1] * v[1].z + w[2] * v[2].z) / area;
                let zat = d.origin + (cy + y) as u64 * d.pitch + (cx + x) as u64 * d.bytes_per_pixel();
                let new = d.quantise(z);
                let Some(old) = d.read(zat) else { continue };
                if !d.passes(new, old) {
                    continue;
                }
                if d.write {
                    d.store(zat, new);
                }
            }
            let at = s.origin + (cy + y) as u64 * s.pitch + (cx + x) as u64 * bpp as u64;
            let needs_old = s.ops.blend.is_some() || s.ops.mask != [true; 4];
            let old = if needs_old {
                let mut raw = [0u8; 4];
                match s.ram.bytes(at, bpp) {
                    Some(b) => raw[..bpp].copy_from_slice(&b),
                    None => continue,
                }
                unpack(s.format, u32::from_le_bytes(raw))
            } else {
                [0.0; 4]
            };
            if let Some(b) = &s.ops.blend {
                c = b.apply(c, old);
            }
            if needs_old {
                for k in 0..4 {
                    if !s.ops.mask[k] {
                        c[k] = old[k];
                    }
                }
            }
            let px = pack(s.format, c).to_le_bytes();
            if s.ram.write(at, &px[..bpp]) {
                written += 1;
            }
        }
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_machine::PhysMem;

    const W: u32 = 16;

    fn surface(mem: &mut PhysMem, format: u32) -> Surface {
        Surface { ram: mem.ram_view(0).unwrap(), origin: 0, pitch: W as u64 * 4, format, clip: (0, 0, W, W), ops: PixelOps::default() }
    }

    fn px(s: &Surface, x: u32, y: u32) -> u32 {
        s.ram.read_u32((y * W + x) as u64 * 4).unwrap()
    }

    fn v(x: f32, y: f32, c: [f32; 4]) -> Vertex {
        Vertex { x, y, z: 0.0, w: 1.0, color: c, spec: [0.0; 4], tex: [0.0; 4] }
    }

    const RED: [f32; 4] = [1.0, 0.0, 0.0, 1.0];

    fn mem() -> PhysMem {
        let mut m = PhysMem::new();
        m.add_ram(0, 1 << 16).unwrap();
        m
    }

    #[test]
    fn fills_the_inside_and_nothing_outside() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let n = fill_triangle(&s, None, None, None, [v(2.0, 2.0, RED), v(14.0, 2.0, RED), v(2.0, 14.0, RED)]);
        assert_eq!(px(&s, 3, 3), 0xFFFF_0000);
        assert_eq!(px(&s, 13, 13), 0, "beyond the hypotenuse");
        assert_eq!(px(&s, 1, 3), 0, "left of the triangle");
        assert!(n > 50 && n < 80, "{n}");
    }

    #[test]
    fn either_winding_draws_the_same_pixels() {
        let (mut m1, mut m2) = (mem(), mem());
        let (a, b) = (surface(&mut m1, 8), surface(&mut m2, 8));
        let n1 = fill_triangle(&a, None, None, None, [v(1.0, 1.0, RED), v(12.0, 3.0, RED), v(5.0, 13.0, RED)]);
        let n2 = fill_triangle(&b, None, None, None, [v(1.0, 1.0, RED), v(5.0, 13.0, RED), v(12.0, 3.0, RED)]);
        assert_eq!(n1, n2);
        assert!(n1 > 0);
        for y in 0..W {
            for x in 0..W {
                assert_eq!(px(&a, x, y), px(&b, x, y));
            }
        }
    }

    #[test]
    fn a_shared_edge_is_drawn_exactly_once() {
        // two triangles making a square: every pixel of it is covered once
        let mut m = mem();
        let s = surface(&mut m, 8);
        let a = fill_triangle(&s, None, None, None, [v(0.0, 0.0, RED), v(8.0, 0.0, RED), v(0.0, 8.0, RED)]);
        let b = fill_triangle(&s, None, None, None, [v(8.0, 0.0, RED), v(8.0, 8.0, RED), v(0.0, 8.0, RED)]);
        assert_eq!(a + b, 64);
    }

    #[test]
    fn colours_blend_between_corners() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let (g, b) = ([0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        fill_triangle(&s, None, None, None, [v(0.0, 0.0, RED), v(16.0, 0.0, g), v(0.0, 16.0, b)]);
        let near_red = px(&s, 1, 1);
        assert!((near_red >> 16) & 0xFF > 0xC0, "{near_red:#x}");
        let mid = px(&s, 6, 6); // roughly equal weights
        assert!((mid >> 8) & 0xFF > 0x30 && mid & 0xFF > 0x30, "{mid:#x}");
    }

    #[test]
    fn drawing_is_confined_to_the_clip_and_the_surface() {
        let mut m = mem();
        let mut s = surface(&mut m, 8);
        s.clip = (4, 4, 4, 4);
        fill_triangle(&s, None, None, None, [v(-50.0, -50.0, RED), v(80.0, -50.0, RED), v(-50.0, 80.0, RED)]);
        assert_eq!(px(&s, 4, 4), 0xFFFF_0000);
        assert_eq!(px(&s, 7, 7), 0xFFFF_0000);
        assert_eq!(px(&s, 8, 4), 0);
        assert_eq!(px(&s, 3, 4), 0);
    }

    #[test]
    fn degenerate_triangles_draw_nothing_and_16_bit_formats_pack() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        assert_eq!(fill_triangle(&s, None, None, None, [v(1.0, 1.0, RED), v(5.0, 5.0, RED), v(9.0, 9.0, RED)]), 0);
        assert_eq!(pack(3, RED), 0xF800);
        assert_eq!(pack(3, [0.0, 1.0, 0.0, 1.0]), 0x07E0);
        assert_eq!(pack(5, [0.2, 0.4, 0.6, 0.0]), 0xFF33_6699);
    }

    fn vz(x: f32, y: f32, z: f32, c: [f32; 4]) -> Vertex {
        Vertex { x, y, z, w: 1.0, color: c, spec: [0.0; 4], tex: [0.0; 4] }
    }

    fn depth_buffer(m: &mut PhysMem, format: u32, func: u32) -> Depth {
        // placed after the 16x16 colour surface (1 KiB)
        let bpp = if format == 1 { 2 } else { 4 };
        Depth { ram: m.ram_view(0).unwrap(), origin: 0x1000, pitch: 16 * bpp, format, func, write: true, clip_max: 100.0 }
    }

    fn big(z: f32, c: [f32; 4]) -> [Vertex; 3] {
        [vz(0.0, 0.0, z, c), vz(16.0, 0.0, z, c), vz(0.0, 16.0, z, c)]
    }

    const BLUE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    fn clear_depth(d: &Depth, value: u32) {
        for i in 0..16 * 16 {
            let at = d.origin + i * d.bytes_per_pixel();
            if d.format == 1 {
                d.ram.write(at, &(value as u16).to_le_bytes());
            } else {
                d.ram.write_u32(at, value << 8);
            }
        }
    }

    #[test]
    fn nearer_triangle_wins_in_either_order_with_less() {
        for format in [1, 2] {
            for near_first in [true, false] {
                let mut m = mem();
                let s = surface(&mut m, 8);
                let d = depth_buffer(&mut m, format, 1);
                clear_depth(&d, if format == 1 { 0xFFFF } else { 0xFF_FFFF });
                let (near, far) = (big(10.0, RED), big(60.0, BLUE));
                if near_first {
                    fill_triangle(&s, Some(&d), None, None, near);
                    fill_triangle(&s, Some(&d), None, None, far);
                } else {
                    fill_triangle(&s, Some(&d), None, None, far);
                    fill_triangle(&s, Some(&d), None, None, near);
                }
                assert_eq!(px(&s, 3, 3), 0xFFFF_0000, "format {format}, near first {near_first}");
            }
        }
    }

    #[test]
    fn depth_functions_and_write_mask() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let mut d = depth_buffer(&mut m, 2, 7);
        clear_depth(&d, 0xFF_FFFF);
        fill_triangle(&s, Some(&d), None, None, big(10.0, RED)); // ALWAYS: drawn
        d.func = 0;
        fill_triangle(&s, Some(&d), None, None, big(5.0, BLUE)); // NEVER: not drawn
        assert_eq!(px(&s, 3, 3), 0xFFFF_0000);
        d.func = 4; // GREATER
        fill_triangle(&s, Some(&d), None, None, big(5.0, BLUE)); // 5 is not greater than 10
        assert_eq!(px(&s, 3, 3), 0xFFFF_0000);
        fill_triangle(&s, Some(&d), None, None, big(20.0, BLUE));
        assert_eq!(px(&s, 3, 3), 0xFF00_00FF);
        d.write = false;
        d.func = 7;
        fill_triangle(&s, Some(&d), None, None, big(90.0, RED)); // drawn but the depth stays at 20
        d.func = 3; // LEQUAL against 20
        fill_triangle(&s, Some(&d), None, None, big(30.0, BLUE));
        assert_eq!(px(&s, 3, 3), 0xFFFF_0000, "the depth buffer was not changed by the write-masked draw");
    }

    #[test]
    fn stencil_byte_is_kept() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let d = depth_buffer(&mut m, 2, 1);
        clear_depth(&d, 0xFF_FFFF);
        d.ram.write(d.origin, &[0x5A]); // stencil of pixel (0,0)
        fill_triangle(&s, Some(&d), None, None, big(10.0, RED));
        assert_eq!(d.ram.bytes(d.origin, 1).unwrap()[0], 0x5A);
        assert_ne!(d.ram.read_u32(d.origin).unwrap() >> 8, 0xFF_FFFF);
    }

    #[test]
    fn depth_interpolates_across_the_triangle() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let d = depth_buffer(&mut m, 2, 7);
        clear_depth(&d, 0xFF_FFFF);
        fill_triangle(&s, Some(&d), None, None, [vz(0.0, 0.0, 0.0, RED), vz(16.0, 0.0, 100.0, RED), vz(0.0, 16.0, 0.0, RED)]);
        let at = |x: u64| d.ram.read_u32(d.origin + x * 4).unwrap() >> 8;
        assert!(at(1) < at(8) && at(8) < at(14), "{} {} {}", at(1), at(8), at(14));
    }

    #[test]
    fn perspective_correct_colour_favours_the_nearer_vertex() {
        // the far vertex (w = 4) should have less influence at the middle of the edge
        let mut m = mem();
        let s = surface(&mut m, 8);
        let near = Vertex { x: 0.0, y: 0.0, z: 0.0, w: 1.0, color: [1.0, 0.0, 0.0, 1.0], spec: [0.0; 4], tex: [0.0; 4] };
        let far = Vertex { x: 16.0, y: 0.0, z: 0.0, w: 4.0, color: [0.0, 0.0, 1.0, 1.0], spec: [0.0; 4], tex: [0.0; 4] };
        let third = Vertex { x: 0.0, y: 16.0, z: 0.0, w: 1.0, color: [1.0, 0.0, 0.0, 1.0], spec: [0.0; 4], tex: [0.0; 4] };
        fill_triangle(&s, None, None, None, [near, far, third]);
        let p = px(&s, 7, 1);
        assert!((p >> 16) & 0xFF > p & 0xFF, "red should dominate near the middle: {p:#x}");
    }

    #[test]
    fn a_texture_modulates_the_colour() {
        use crate::texture::{Address, Texture, FMT_LU_A8R8G8B8};
        let mut m = mem();
        let s = surface(&mut m, 8);
        let ram = m.ram_view(0).unwrap();
        // 2x1 texture: left texel white, right texel red, at 0x2000
        ram.write_u32(0x2000, 0xFFFF_FFFF);
        ram.write_u32(0x2004, 0xFFFF_0000);
        let t = Texture {
            ram, addr: 0x2000, format: FMT_LU_A8R8G8B8, width: 2, height: 1, pitch: 8,
            wrap_u: Address::Clamp, wrap_v: Address::Clamp, bilinear: false, border: [0.0; 4],
        };
        let mk = |x: f32, y: f32, s: f32, t: f32| Vertex { x, y, z: 0.0, w: 1.0, color: [0.0, 0.5, 1.0, 1.0], spec: [0.0; 4], tex: [s, t, 0.0, 1.0] };
        // coordinates in texels: 0 at the left edge to 2 at the right (linear textures)
        fill_triangle(&s, None, Some(&t), None, [mk(0.0, 0.0, 0.0, 0.0), mk(16.0, 0.0, 2.0, 0.0), mk(0.0, 16.0, 0.0, 0.0)]);
        let left = px(&s, 1, 1);
        assert_eq!(left & 0xFF, 0xFF, "white texel keeps the blue of the diffuse colour: {left:#x}");
        let right = px(&s, 10, 1);
        assert_eq!(right & 0xFF, 0, "red texel has no blue: {right:#x}");
    }

    fn big_tri(c: [f32; 4]) -> [Vertex; 3] {
        [v(-50.0, -50.0, c), v(80.0, -50.0, c), v(-50.0, 80.0, c)]
    }

    fn with_ops(mem: &mut PhysMem, ops: PixelOps) -> Surface {
        Surface { ops, ..surface(mem, 8) }
    }

    #[test]
    fn alpha_blending_mixes_with_what_is_there() {
        let mut m = mem();
        let mut s = surface(&mut m, 8);
        fill_triangle(&s, None, None, None, big_tri([0.0, 0.0, 1.0, 1.0])); // opaque blue
        s.ops.blend = Some(Blend { src: Factor::SrcAlpha, dst: Factor::OneMinusSrcAlpha, equation: Equation::Add, constant: [0.0; 4] });
        fill_triangle(&s, None, None, None, big_tri([1.0, 0.0, 0.0, 0.5])); // half-transparent red
        let p = px(&s, 4, 4);
        let (r, b) = (p >> 16 & 0xFF, p & 0xFF);
        assert!((r as i32 - 128).abs() <= 1 && (b as i32 - 127).abs() <= 1, "{p:#x}");
    }

    #[test]
    fn additive_and_subtractive_equations() {
        let mut m = mem();
        let mut s = surface(&mut m, 8);
        fill_triangle(&s, None, None, None, big_tri([0.25, 0.25, 0.25, 1.0]));
        s.ops.blend = Some(Blend { src: Factor::One, dst: Factor::One, equation: Equation::Add, constant: [0.0; 4] });
        fill_triangle(&s, None, None, None, big_tri([0.5, 0.0, 1.0, 1.0]));
        let p = px(&s, 2, 2);
        assert_eq!((p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF), (192, 64, 255), "{p:#x}"); // clamped at 1
        s.ops.blend = Some(Blend { src: Factor::One, dst: Factor::One, equation: Equation::ReverseSubtract, constant: [0.0; 4] });
        fill_triangle(&s, None, None, None, big_tri([0.25, 0.25, 0.25, 0.0]));
        let p = px(&s, 2, 2); // destination - source
        assert_eq!((p >> 16 & 0xFF, p >> 8 & 0xFF), (128, 0), "{p:#x}");
    }

    #[test]
    fn constant_colour_factors_use_the_blend_colour() {
        let mut m = mem();
        let mut s = surface(&mut m, 8);
        s.ops.blend = Some(Blend { src: Factor::ConstColor, dst: Factor::Zero, equation: Equation::Add, constant: [0.5, 0.0, 1.0, 1.0] });
        fill_triangle(&s, None, None, None, big_tri([1.0, 1.0, 0.5, 1.0]));
        let p = px(&s, 2, 2);
        assert_eq!((p >> 16 & 0xFF, p >> 8 & 0xFF, p & 0xFF), (128, 0, 128), "{p:#x}");
    }

    #[test]
    fn the_alpha_test_discards_fragments() {
        let mut m = mem();
        let s = with_ops(&mut m, PixelOps { alpha_test: Some(AlphaTest { func: 4, reference: 0x80 }), ..PixelOps::default() });
        fill_triangle(&s, None, None, None, big_tri([1.0, 0.0, 0.0, 0.25])); // 0x40 > 0x80? no
        assert_eq!(px(&s, 3, 3), 0);
        fill_triangle(&s, None, None, None, big_tri([0.0, 1.0, 0.0, 0.75])); // 0xC0 > 0x80: yes
        assert_eq!(px(&s, 3, 3) >> 8 & 0xFF, 0xFF);
    }

    #[test]
    fn the_colour_mask_protects_channels() {
        let mut m = mem();
        let mut s = surface(&mut m, 8);
        fill_triangle(&s, None, None, None, big_tri([0.0, 0.0, 1.0, 1.0]));
        s.ops.mask = [true, false, false, false]; // red only
        fill_triangle(&s, None, None, None, big_tri([1.0, 1.0, 0.0, 0.0]));
        assert_eq!(px(&s, 5, 5), 0xFFFF_00FF, "{:#x}", px(&s, 5, 5));
    }

    #[test]
    fn unpack_is_the_inverse_of_pack() {
        for fmt in [3u32, 6, 8] {
            let c = [1.0, 0.5, 0.0, 1.0];
            let back = unpack(fmt, pack(fmt, c));
            for k in 0..3 {
                assert!((back[k] - c[k]).abs() < 0.02, "format {fmt}: {back:?}");
            }
        }
    }
}
