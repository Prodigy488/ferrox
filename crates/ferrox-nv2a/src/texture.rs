//! Texture sampling: reads texels from guest memory in the NV2A's layouts
//! (swizzled "Z-order", linear, and DXT blocks), applies the addressing mode
//! and filter, and returns an RGBA colour.
//!
//! The swizzle algorithm and format numbers come from xemu (`swizzle.c`,
//! `texture.c`, `nv2a_regs.h`); channel conventions for the one- and
//! two-channel formats follow xemu's GL swizzles (Y8 is grey with alpha 1,
//! A8 is white with that alpha).

use ferrox_machine::RamView;

pub const FMT_SZ_Y8: u32 = 0x00;
pub const FMT_SZ_A1R5G5B5: u32 = 0x02;
pub const FMT_SZ_X1R5G5B5: u32 = 0x03;
pub const FMT_SZ_A4R4G4B4: u32 = 0x04;
pub const FMT_SZ_R5G6B5: u32 = 0x05;
pub const FMT_SZ_A8R8G8B8: u32 = 0x06;
pub const FMT_SZ_X8R8G8B8: u32 = 0x07;
pub const FMT_DXT1: u32 = 0x0C;
pub const FMT_DXT3: u32 = 0x0E;
pub const FMT_DXT5: u32 = 0x0F;
pub const FMT_LU_A1R5G5B5: u32 = 0x10;
pub const FMT_LU_R5G6B5: u32 = 0x11;
pub const FMT_LU_A8R8G8B8: u32 = 0x12;
pub const FMT_LU_Y8: u32 = 0x13;
pub const FMT_SZ_A8: u32 = 0x19;
pub const FMT_SZ_A8Y8: u32 = 0x1A;
pub const FMT_LU_X1R5G5B5: u32 = 0x1C;
pub const FMT_LU_A4R4G4B4: u32 = 0x1D;
pub const FMT_LU_X8R8G8B8: u32 = 0x1E;
pub const FMT_LU_A8: u32 = 0x1F;
pub const FMT_LU_A8Y8: u32 = 0x20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Swizzled,
    Linear,
    Blocks,
}

/// How a colour format is stored: (layout, bytes per texel), or `None` if it is not supported yet.
pub fn describe(format: u32) -> Option<(Layout, u32)> {
    use Layout::*;
    Some(match format {
        FMT_SZ_Y8 | FMT_SZ_A8 => (Swizzled, 1),
        FMT_SZ_A1R5G5B5 | FMT_SZ_X1R5G5B5 | FMT_SZ_A4R4G4B4 | FMT_SZ_R5G6B5 | FMT_SZ_A8Y8 => (Swizzled, 2),
        FMT_SZ_A8R8G8B8 | FMT_SZ_X8R8G8B8 => (Swizzled, 4),
        FMT_DXT1 => (Blocks, 8),
        FMT_DXT3 | FMT_DXT5 => (Blocks, 16),
        FMT_LU_Y8 | FMT_LU_A8 => (Linear, 1),
        FMT_LU_A1R5G5B5 | FMT_LU_R5G6B5 | FMT_LU_X1R5G5B5 | FMT_LU_A4R4G4B4 | FMT_LU_A8Y8 => (Linear, 2),
        FMT_LU_A8R8G8B8 | FMT_LU_X8R8G8B8 => (Linear, 4),
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Address {
    Wrap,
    Mirror,
    Clamp,
    Border,
}

impl Address {
    pub fn from_field(v: u32) -> Address {
        match v {
            2 => Address::Mirror,
            3 | 5 => Address::Clamp,
            4 => Address::Border,
            _ => Address::Wrap,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Texture {
    pub ram: RamView,
    pub addr: u64,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row, for linear textures.
    pub pitch: u32,
    pub wrap_u: Address,
    pub wrap_v: Address,
    pub bilinear: bool,
    pub border: [f32; 4],
}

/// Spreads the low bits of `v` into the positions set in `mask`.
fn deposit(mut v: u32, mask: u32) -> u32 {
    let mut out = 0;
    let mut m = mask;
    while m != 0 {
        let bit = m & m.wrapping_neg();
        if v & 1 != 0 {
            out |= bit;
        }
        v >>= 1;
        m &= m - 1;
    }
    out
}

/// The interleave masks for a power-of-two `width` x `height` (xemu's `generate_swizzle_masks`, depth 1).
pub fn swizzle_masks(width: u32, height: u32) -> (u32, u32) {
    let (mut x, mut y, mut mask_bit, mut bit) = (0u32, 0u32, 1u32, 1u32);
    loop {
        let mut done = true;
        if bit < width {
            x |= mask_bit;
            mask_bit <<= 1;
            done = false;
        }
        if bit < height {
            y |= mask_bit;
            mask_bit <<= 1;
            done = false;
        }
        bit <<= 1;
        if done {
            return (x, y);
        }
    }
}

/// Texel index (not bytes) of (x, y) in a swizzled texture.
pub fn swizzle_index(x: u32, y: u32, width: u32, height: u32) -> u32 {
    let (mx, my) = swizzle_masks(width, height);
    deposit(x, mx) | deposit(y, my)
}

fn rgb565(v: u16) -> [f32; 3] {
    [(v >> 11) as f32 / 31.0, ((v >> 5) & 63) as f32 / 63.0, (v & 31) as f32 / 31.0]
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// Decodes the colour half of a DXT block into 16 texels (row-major); `four` forces opaque four-colour mode.
fn dxt_colours(block: &[u8], four: bool) -> [[f32; 4]; 16] {
    let c0 = u16::from_le_bytes([block[0], block[1]]);
    let c1 = u16::from_le_bytes([block[2], block[3]]);
    let (a, b) = (rgb565(c0), rgb565(c1));
    let opaque = four || c0 > c1;
    let pal: [[f32; 4]; 4] = if opaque {
        let (p2, p3) = (lerp3(a, b, 1.0 / 3.0), lerp3(a, b, 2.0 / 3.0));
        [[a[0], a[1], a[2], 1.0], [b[0], b[1], b[2], 1.0], [p2[0], p2[1], p2[2], 1.0], [p3[0], p3[1], p3[2], 1.0]]
    } else {
        let mid = lerp3(a, b, 0.5);
        [[a[0], a[1], a[2], 1.0], [b[0], b[1], b[2], 1.0], [mid[0], mid[1], mid[2], 1.0], [0.0; 4]]
    };
    let mut out = [[0.0; 4]; 16];
    for row in 0..4 {
        for col in 0..4 {
            let idx = (block[4 + row] >> (2 * col)) & 3;
            out[row * 4 + col] = pal[idx as usize];
        }
    }
    out
}

fn dxt5_alpha(block: &[u8], texel: usize) -> f32 {
    let (a0, a1) = (block[0] as f32, block[1] as f32);
    let bits = u64::from_le_bytes([block[2], block[3], block[4], block[5], block[6], block[7], 0, 0]);
    let idx = ((bits >> (3 * texel)) & 7) as u32;
    let v = if a0 > a1 {
        match idx {
            0 => a0,
            1 => a1,
            i => ((8 - i) as f32 * a0 + (i - 1) as f32 * a1) / 7.0,
        }
    } else {
        match idx {
            0 => a0,
            1 => a1,
            6 => 0.0,
            7 => 255.0,
            i => ((6 - i) as f32 * a0 + (i - 1) as f32 * a1) / 5.0,
        }
    };
    v / 255.0
}

impl Texture {
    fn raw(&self, x: u32, y: u32) -> [f32; 4] {
        let Some((layout, bpt)) = describe(self.format) else { return [1.0, 0.0, 1.0, 1.0] };
        if layout == Layout::Blocks {
            let blocks_w = (self.width / 4).max(1);
            let at = self.addr + ((y / 4) * blocks_w + x / 4) as u64 * bpt as u64;
            let Some(b) = self.ram.bytes(at, bpt as usize) else { return [0.0; 4] };
            let t = ((y % 4) * 4 + x % 4) as usize;
            return match self.format {
                FMT_DXT1 => dxt_colours(b, false)[t],
                FMT_DXT3 => {
                    let mut c = dxt_colours(&b[8..], true)[t];
                    let nib = (b[t / 2] >> (4 * (t % 2))) & 0xF;
                    c[3] = nib as f32 / 15.0;
                    c
                }
                _ => {
                    let mut c = dxt_colours(&b[8..], true)[t];
                    c[3] = dxt5_alpha(b, t);
                    c
                }
            };
        }
        let index = match layout {
            Layout::Swizzled => swizzle_index(x, y, self.width, self.height) as u64 * bpt as u64,
            _ => y as u64 * self.pitch as u64 + x as u64 * bpt as u64,
        };
        let Some(b) = self.ram.bytes(self.addr + index, bpt as usize) else { return [0.0; 4] };
        let v16 = || u16::from_le_bytes([b[0], b[1]]);
        let n5 = |v: u16, shift: u32| ((v >> shift) & 31) as f32 / 31.0;
        match self.format {
            FMT_SZ_A8R8G8B8 | FMT_LU_A8R8G8B8 => [b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0, b[3] as f32 / 255.0],
            FMT_SZ_X8R8G8B8 | FMT_LU_X8R8G8B8 => [b[2] as f32 / 255.0, b[1] as f32 / 255.0, b[0] as f32 / 255.0, 1.0],
            FMT_SZ_R5G6B5 | FMT_LU_R5G6B5 => {
                let c = rgb565(v16());
                [c[0], c[1], c[2], 1.0]
            }
            FMT_SZ_A1R5G5B5 | FMT_LU_A1R5G5B5 => {
                let v = v16();
                [n5(v, 10), n5(v, 5), n5(v, 0), (v >> 15) as f32]
            }
            FMT_SZ_X1R5G5B5 | FMT_LU_X1R5G5B5 => {
                let v = v16();
                [n5(v, 10), n5(v, 5), n5(v, 0), 1.0]
            }
            FMT_SZ_A4R4G4B4 | FMT_LU_A4R4G4B4 => {
                let v = v16();
                let n4 = |s: u32| ((v >> s) & 15) as f32 / 15.0;
                [n4(8), n4(4), n4(0), n4(12)]
            }
            FMT_SZ_Y8 | FMT_LU_Y8 => {
                let y = b[0] as f32 / 255.0;
                [y, y, y, 1.0]
            }
            FMT_SZ_A8 | FMT_LU_A8 => [1.0, 1.0, 1.0, b[0] as f32 / 255.0],
            FMT_SZ_A8Y8 | FMT_LU_A8Y8 => {
                let y = b[0] as f32 / 255.0;
                [y, y, y, b[1] as f32 / 255.0]
            }
            _ => [1.0, 0.0, 1.0, 1.0],
        }
    }

    fn resolve(mode: Address, i: i32, n: u32) -> Option<u32> {
        let n = n as i32;
        match mode {
            Address::Wrap => Some(i.rem_euclid(n) as u32),
            Address::Mirror => {
                let m = i.rem_euclid(2 * n);
                Some(if m < n { m } else { 2 * n - 1 - m } as u32)
            }
            Address::Clamp => Some(i.clamp(0, n - 1) as u32),
            Address::Border => (0..n).contains(&i).then_some(i as u32),
        }
    }

    /// The texel at integer coordinates after the addressing mode is applied.
    pub fn fetch(&self, x: i32, y: i32) -> [f32; 4] {
        match (Self::resolve(self.wrap_u, x, self.width), Self::resolve(self.wrap_v, y, self.height)) {
            (Some(x), Some(y)) => self.raw(x, y),
            _ => self.border,
        }
    }

    pub fn is_linear_layout(&self) -> bool {
        describe(self.format).map(|d| d.0) == Some(Layout::Linear)
    }

    /// Samples at (s, t): normalised 0..1 for swizzled and block textures,
    /// in texels for linear ones (as the hardware does).
    pub fn sample(&self, s: f32, t: f32) -> [f32; 4] {
        let (u, v) = if self.is_linear_layout() { (s, t) } else { (s * self.width as f32, t * self.height as f32) };
        if !self.bilinear {
            return self.fetch(u.floor() as i32, v.floor() as i32);
        }
        let (u, v) = (u - 0.5, v - 0.5);
        let (x0, y0) = (u.floor(), v.floor());
        let (fx, fy) = (u - x0, v - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let (a, b, c, d) = (self.fetch(x0, y0), self.fetch(x0 + 1, y0), self.fetch(x0, y0 + 1), self.fetch(x0 + 1, y0 + 1));
        let mut out = [0.0; 4];
        for k in 0..4 {
            let top = a[k] + (b[k] - a[k]) * fx;
            let bottom = c[k] + (d[k] - c[k]) * fx;
            out[k] = top + (bottom - top) * fy;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_machine::PhysMem;

    fn setup(bytes: &[u8]) -> (PhysMem, RamView) {
        let mut m = PhysMem::new();
        m.add_ram(0, 1 << 16).unwrap();
        let v = m.ram_view(0).unwrap();
        assert!(v.write(0x100, bytes));
        (m, v)
    }

    fn tex(v: RamView, format: u32, w: u32, h: u32) -> Texture {
        Texture {
            ram: v,
            addr: 0x100,
            format,
            width: w,
            height: h,
            pitch: w * describe(format).unwrap().1,
            wrap_u: Address::Wrap,
            wrap_v: Address::Wrap,
            bilinear: false,
            border: [0.0, 1.0, 0.0, 1.0],
        }
    }

    #[test]
    fn swizzle_matches_the_documented_interleave() {
        // 8 x 32: "yyyxyxyx" (xemu's own example)
        let (mx, my) = swizzle_masks(8, 32);
        assert_eq!((mx, my), (0b00010101, 0b11101010));
        // 4 x 4 is plain Morton order
        let order: Vec<u32> = (0..16).map(|i| swizzle_index(i % 4, i / 4, 4, 4)).collect();
        assert_eq!(order, vec![0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15]);
        // every texel maps to a distinct slot inside the texture
        let mut seen = vec![false; 8 * 32];
        for y in 0..32 {
            for x in 0..8 {
                let i = swizzle_index(x, y, 8, 32) as usize;
                assert!(!seen[i]);
                seen[i] = true;
            }
        }
    }

    #[test]
    fn swizzled_argb_reads_back_in_place() {
        // 2x2: Morton order is (0,0) (1,0) (0,1) (1,1)
        let texels: [u32; 4] = [0xFF11_2233, 0x8044_5566, 0xFF77_8899, 0x00AA_BBCC];
        let bytes: Vec<u8> = texels.iter().flat_map(|t| t.to_le_bytes()).collect();
        let (_m, v) = setup(&bytes);
        let t = tex(v, FMT_SZ_A8R8G8B8, 2, 2);
        let c = t.fetch(1, 0);
        assert_eq!([c[0], c[1], c[2]].map(|x| (x * 255.0).round() as u32), [0x44, 0x55, 0x66]);
        assert!((c[3] - 128.0 / 255.0).abs() < 1e-3);
        assert_eq!((t.fetch(0, 1)[0] * 255.0).round() as u32, 0x77);
    }

    #[test]
    fn linear_textures_use_the_pitch_and_texel_coordinates() {
        let mut bytes = vec![0u8; 64];
        bytes[16 + 4] = 0xFF; // row 1 (pitch 16), texel 1: blue byte of 0x000000FF
        let (_m, v) = setup(&bytes);
        let mut t = tex(v, FMT_LU_A8R8G8B8, 4, 4);
        t.pitch = 16;
        assert_eq!(t.sample(1.5, 1.5)[2], 1.0, "linear textures take texel coordinates");
        assert_eq!(t.sample(0.5, 0.5)[2], 0.0);
    }

    #[test]
    fn address_modes() {
        let bytes: Vec<u8> = [0xFFFF_0000u32, 0xFF00_FF00].iter().flat_map(|t| t.to_le_bytes()).collect();
        let (_m, v) = setup(&bytes);
        let mut t = tex(v, FMT_LU_A8R8G8B8, 2, 1);
        let red = |c: [f32; 4]| c[0] > 0.9;
        t.wrap_u = Address::Wrap;
        assert!(red(t.fetch(2, 0)) && red(t.fetch(-2, 0)));
        t.wrap_u = Address::Mirror;
        assert!(red(t.fetch(0, 0)) && !red(t.fetch(2, 0)) && red(t.fetch(3, 0)));
        t.wrap_u = Address::Clamp;
        assert!(!red(t.fetch(5, 0)) && red(t.fetch(-5, 0)));
        t.wrap_u = Address::Border;
        assert_eq!(t.fetch(2, 0), [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(t.fetch(-1, 0), t.border);
    }

    #[test]
    fn bilinear_blends_neighbours() {
        let bytes: Vec<u8> = [0xFF00_0000u32, 0xFFFF_FFFF].iter().flat_map(|t| t.to_le_bytes()).collect();
        let (_m, v) = setup(&bytes);
        let mut t = tex(v, FMT_LU_A8R8G8B8, 2, 1);
        t.bilinear = true;
        t.wrap_u = Address::Clamp;
        t.wrap_v = Address::Clamp;
        let mid = t.sample(1.0, 0.5); // exactly between the two texel centres
        assert!((mid[0] - 0.5).abs() < 1e-3, "{mid:?}");
        assert!((t.sample(0.5, 0.5)[0]).abs() < 1e-3);
    }

    #[test]
    fn sixteen_bit_and_small_formats() {
        let (_m, v) = setup(&[0x00, 0xF8, 0xE0, 0x07, 0x1F, 0x00, 0x55, 0xAA]);
        let t = tex(v, FMT_LU_R5G6B5, 4, 1);
        assert_eq!(t.fetch(0, 0), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.fetch(1, 0), [0.0, 1.0, 0.0, 1.0]);
        assert_eq!(t.fetch(2, 0), [0.0, 0.0, 1.0, 1.0]);
        let y = tex(v, FMT_LU_Y8, 8, 1).fetch(7, 0);
        assert_eq!(y, [170.0 / 255.0, 170.0 / 255.0, 170.0 / 255.0, 1.0]);
        let a = tex(v, FMT_LU_A8, 8, 1).fetch(6, 0);
        assert_eq!(a, [1.0, 1.0, 1.0, 85.0 / 255.0]);
        let argb4 = tex(v, FMT_LU_A4R4G4B4, 4, 1).fetch(3, 0); // 0xAA55: a=A r=A g=5 b=5
        assert!((argb4[3] - 10.0 / 15.0).abs() < 1e-6 && (argb4[1] - 5.0 / 15.0).abs() < 1e-6);
    }

    #[test]
    fn dxt1_four_colour_and_transparent_modes() {
        // c0 = red, c1 = blue; indices: row 0 = 0,1,2,3
        let mut block = vec![0x00, 0xF8, 0x1F, 0x00, 0b11_10_01_00, 0, 0, 0];
        let (_m, v) = setup(&block);
        let t = tex(v, FMT_DXT1, 4, 4);
        assert_eq!(t.fetch(0, 0), [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(t.fetch(1, 0), [0.0, 0.0, 1.0, 1.0]);
        let p2 = t.fetch(2, 0);
        assert!(p2[0] > 0.6 && p2[2] > 0.3 && p2[3] == 1.0, "{p2:?}"); // two thirds red
        // c0 <= c1 selects the three-colour mode where index 3 is transparent
        block.splice(0..4, [0x1F, 0x00, 0x00, 0xF8]);
        let (_m2, v2) = setup(&block);
        let t2 = tex(v2, FMT_DXT1, 4, 4);
        assert_eq!(t2.fetch(3, 0), [0.0; 4]);
        assert!((t2.fetch(2, 0)[0] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dxt3_and_dxt5_alpha() {
        let colour = [0x00, 0xF8, 0x00, 0xF8, 0, 0, 0, 0]; // solid red, indices all 0
        let mut b3 = vec![0u8; 8];
        b3[0] = 0xF0; // texel 0 alpha 0, texel 1 alpha 15
        b3.extend(colour);
        let (_m, v) = setup(&b3);
        let t = tex(v, FMT_DXT3, 4, 4);
        assert_eq!(t.fetch(0, 0)[3], 0.0);
        assert_eq!(t.fetch(1, 0)[3], 1.0);
        assert_eq!(t.fetch(1, 0)[0], 1.0);
        // DXT5: a0=255, a1=0, all indices 1 -> alpha 0; indices 0 -> 255
        let mut b5 = vec![255u8, 0, 0b0000_1001, 0, 0, 0, 0, 0]; // texel 0 idx 1, texel 1 idx 1 (bits 0-2 = 1, 3-5 = 1)
        b5.extend(colour);
        let (_m5, v5) = setup(&b5);
        let t5 = tex(v5, FMT_DXT5, 4, 4);
        assert_eq!(t5.fetch(0, 0)[3], 0.0);
        assert_eq!(t5.fetch(1, 0)[3], 0.0);
        assert_eq!(t5.fetch(2, 0)[3], 1.0);
        let _ = &mut b5;
    }

    #[test]
    fn unsupported_formats_are_reported_by_describe() {
        assert!(describe(0x0B).is_none()); // palettised
        assert!(describe(0x24).is_none()); // YUV
        assert_eq!(describe(FMT_DXT5), Some((Layout::Blocks, 16)));
    }
}
