//! Display setup: what a game asked for, and how to turn its framebuffer into pixels.
//!
//! The mode table is the list of (mode value, width, height) pairs from the nxdk
//! video library (MIT), which covers the modes the console's video encoder offers.

/// (display-mode value, width, height).
pub static MODES: &[(u32, u32, u32)] = &[
    (0x04010101, 640, 480),
    (0x04010103, 640, 480),
    (0x0401010B, 640, 480),
    (0x04020202, 720, 480),
    (0x04020204, 720, 480),
    (0x0402020C, 720, 480),
    (0x0801010D, 640, 480),
    (0x08010119, 640, 480),
    (0x0802020E, 720, 480),
    (0x0802021A, 720, 480),
    (0x20010101, 640, 480),
    (0x20010103, 640, 480),
    (0x2001010B, 640, 480),
    (0x20020202, 720, 480),
    (0x20020204, 720, 480),
    (0x2002020C, 720, 480),
    (0x44030307, 640, 480),
    (0x44040408, 720, 480),
    (0x48030314, 640, 480),
    (0x48040415, 720, 480),
    (0x60030307, 640, 480),
    (0x60040408, 720, 480),
    (0x88070701, 640, 480),
    (0x88080801, 720, 480),
    (0x880B0A02, 1280, 720),
    (0x880E0C03, 1920, 1080),
    (0xC0030303, 800, 600),
    (0xC0040404, 1024, 768),
    (0xC0060601, 640, 480),
];

pub const FORMAT_A1R5G5B5: u32 = 0x10;
pub const FORMAT_R5G6B5: u32 = 0x11;
pub const FORMAT_A8R8G8B8: u32 = 0x12;

pub fn mode_size(mode: u32) -> Option<(u32, u32)> {
    MODES.iter().find(|m| m.0 == mode).map(|m| (m.1, m.2))
}

/// The framebuffer a game selected with `AvSetDisplayMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    pub addr: u32,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
}

impl Framebuffer {
    pub fn bytes_per_pixel(&self) -> u32 {
        if self.format == FORMAT_A8R8G8B8 { 4 } else { 2 }
    }

    /// Bytes of guest memory the picture occupies.
    pub fn len(&self) -> usize {
        self.pitch as usize * self.height as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Converts guest memory (`src`, `len()` bytes) to RGBA8. `None` for a format
    /// this does not know.
    pub fn to_rgba(&self, src: &[u8]) -> Option<Vec<u8>> {
        let (w, h, pitch) = (self.width as usize, self.height as usize, self.pitch as usize);
        let bpp = self.bytes_per_pixel() as usize;
        if !matches!(self.format, FORMAT_A1R5G5B5 | FORMAT_R5G6B5 | FORMAT_A8R8G8B8)
            || pitch < w * bpp
            || src.len() < pitch * h
        {
            return None;
        }
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            let row = &src[y * pitch..y * pitch + w * bpp];
            match self.format {
                FORMAT_A8R8G8B8 => {
                    for px in row.chunks_exact(4) {
                        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
                    }
                }
                FORMAT_R5G6B5 => {
                    for px in row.chunks_exact(2) {
                        let v = u16::from_le_bytes([px[0], px[1]]) as u32;
                        let (r, g, b) = ((v >> 11) & 31, (v >> 5) & 63, v & 31);
                        out.extend_from_slice(&[(r * 255 / 31) as u8, (g * 255 / 63) as u8, (b * 255 / 31) as u8, 255]);
                    }
                }
                _ => {
                    for px in row.chunks_exact(2) {
                        let v = u16::from_le_bytes([px[0], px[1]]) as u32;
                        let (r, g, b) = ((v >> 10) & 31, (v >> 5) & 31, v & 31);
                        out.extend_from_slice(&[(r * 255 / 31) as u8, (g * 255 / 31) as u8, (b * 255 / 31) as u8, 255]);
                    }
                }
            }
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_modes_resolve() {
        assert_eq!(mode_size(0x0401_0101), Some((640, 480)));
        assert_eq!(mode_size(0x0402_0202), Some((720, 480)));
        assert_eq!(mode_size(0xDEAD_BEEF), None);
    }

    #[test]
    fn converts_all_three_formats() {
        let fb = |format, pitch| Framebuffer { addr: 0, pitch, width: 2, height: 1, format };
        // A8R8G8B8 is stored B,G,R,A in memory
        let a = fb(FORMAT_A8R8G8B8, 8).to_rgba(&[1, 2, 3, 255, 10, 20, 30, 255]).unwrap();
        assert_eq!(a, [3, 2, 1, 255, 30, 20, 10, 255]);
        // R5G6B5: pure red, pure blue
        let b = fb(FORMAT_R5G6B5, 4).to_rgba(&[0x00, 0xF8, 0x1F, 0x00]).unwrap();
        assert_eq!(b, [255, 0, 0, 255, 0, 0, 255, 255]);
        // A1R5G5B5: pure green
        let c = fb(FORMAT_A1R5G5B5, 4).to_rgba(&[0xE0, 0x03, 0, 0]).unwrap();
        assert_eq!(&c[..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn rejects_short_buffers_and_unknown_formats() {
        let fb = Framebuffer { addr: 0, pitch: 8, width: 2, height: 1, format: FORMAT_A8R8G8B8 };
        assert!(fb.to_rgba(&[0; 4]).is_none());
        assert!(Framebuffer { format: 0x99, ..fb }.to_rgba(&[0; 8]).is_none());
        assert!(Framebuffer { pitch: 4, ..fb }.to_rgba(&[0; 8]).is_none());
    }
}
