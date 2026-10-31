//! A small software triangle rasteriser: the reference renderer. It turns
//! already-positioned vertices into pixels in a colour surface in guest memory,
//! so the command front end can be checked end to end before any GPU API backend
//! exists. Sampling is at pixel centres with a top-left fill rule, so triangles
//! that share an edge never overdraw or leave gaps.

use ferrox_machine::RamView;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    pub x: f32,
    pub y: f32,
    /// Red, green, blue, alpha in 0..=1.
    pub color: [f32; 4],
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
pub fn fill_triangle(s: &Surface, v: [Vertex; 3]) -> u64 {
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
            let mut c = [0.0f32; 4];
            for (k, ck) in c.iter_mut().enumerate() {
                *ck = (w[0] * v[0].color[k] + w[1] * v[1].color[k] + w[2] * v[2].color[k]) / area;
            }
            let at = s.origin + (cy + y) as u64 * s.pitch + (cx + x) as u64 * bpp as u64;
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
        Surface { ram: mem.ram_view(0).unwrap(), origin: 0, pitch: W as u64 * 4, format, clip: (0, 0, W, W) }
    }

    fn px(s: &Surface, x: u32, y: u32) -> u32 {
        s.ram.read_u32((y * W + x) as u64 * 4).unwrap()
    }

    fn v(x: f32, y: f32, c: [f32; 4]) -> Vertex {
        Vertex { x, y, color: c }
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
        let n = fill_triangle(&s, [v(2.0, 2.0, RED), v(14.0, 2.0, RED), v(2.0, 14.0, RED)]);
        assert_eq!(px(&s, 3, 3), 0xFFFF_0000);
        assert_eq!(px(&s, 13, 13), 0, "beyond the hypotenuse");
        assert_eq!(px(&s, 1, 3), 0, "left of the triangle");
        assert!(n > 50 && n < 80, "{n}");
    }

    #[test]
    fn either_winding_draws_the_same_pixels() {
        let (mut m1, mut m2) = (mem(), mem());
        let (a, b) = (surface(&mut m1, 8), surface(&mut m2, 8));
        let n1 = fill_triangle(&a, [v(1.0, 1.0, RED), v(12.0, 3.0, RED), v(5.0, 13.0, RED)]);
        let n2 = fill_triangle(&b, [v(1.0, 1.0, RED), v(5.0, 13.0, RED), v(12.0, 3.0, RED)]);
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
        let a = fill_triangle(&s, [v(0.0, 0.0, RED), v(8.0, 0.0, RED), v(0.0, 8.0, RED)]);
        let b = fill_triangle(&s, [v(8.0, 0.0, RED), v(8.0, 8.0, RED), v(0.0, 8.0, RED)]);
        assert_eq!(a + b, 64);
    }

    #[test]
    fn colours_blend_between_corners() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        let (g, b) = ([0.0, 1.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        fill_triangle(&s, [v(0.0, 0.0, RED), v(16.0, 0.0, g), v(0.0, 16.0, b)]);
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
        fill_triangle(&s, [v(-50.0, -50.0, RED), v(80.0, -50.0, RED), v(-50.0, 80.0, RED)]);
        assert_eq!(px(&s, 4, 4), 0xFFFF_0000);
        assert_eq!(px(&s, 7, 7), 0xFFFF_0000);
        assert_eq!(px(&s, 8, 4), 0);
        assert_eq!(px(&s, 3, 4), 0);
    }

    #[test]
    fn degenerate_triangles_draw_nothing_and_16_bit_formats_pack() {
        let mut m = mem();
        let s = surface(&mut m, 8);
        assert_eq!(fill_triangle(&s, [v(1.0, 1.0, RED), v(5.0, 5.0, RED), v(9.0, 9.0, RED)]), 0);
        assert_eq!(pack(3, RED), 0xF800);
        assert_eq!(pack(3, [0.0, 1.0, 0.0, 1.0]), 0x07E0);
        assert_eq!(pack(5, [0.2, 0.4, 0.6, 0.0]), 0xFF33_6699);
    }
}
