//! XBE, the Xbox executable format: header, certificate, sections and the
//! kernel import table.
//!
//! Layout follows the public community documentation of the format; field
//! offsets used by xemu's game library were cross-checked.

use crate::{le16, le32, Error, Result};

pub const HEADER_SIZE: usize = 0x178;
const MAGIC: &[u8; 4] = b"XBEH";

// The entry point and kernel thunk address are stored XORed with a key that
// differs between retail and debug builds.
const ENTRY_KEY_RETAIL: u32 = 0xA8FC_57AB;
const ENTRY_KEY_DEBUG: u32 = 0x9485_9D4B;
const THUNK_KEY_RETAIL: u32 = 0x5B6D_40B6;
const THUNK_KEY_DEBUG: u32 = 0xEFB1_F152;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub timestamp: u32,
    pub title_id: u32,
    pub title_name: String,
    pub allowed_media: u32,
    pub region: u32,
    pub ratings: u32,
    pub disc_number: u32,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub flags: u32,
    pub virtual_addr: u32,
    pub virtual_size: u32,
    pub raw_addr: u32,
    pub raw_size: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xbe {
    pub base_addr: u32,
    pub size_of_headers: u32,
    pub size_of_image: u32,
    pub timestamp: u32,
    pub init_flags: u32,
    /// Decoded (real) entry point.
    pub entry_point: u32,
    /// Decoded (real) address of the kernel import thunk table.
    pub kernel_thunk_addr: u32,
    pub retail: bool,
    pub certificate: Certificate,
    pub sections: Vec<Section>,
}

fn read_cstr(data: &[u8], at: usize) -> String {
    let tail = data.get(at..).unwrap_or(&[]);
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len().min(64));
    String::from_utf8_lossy(&tail[..end.min(64)]).into_owned()
}

fn utf16_name(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units).trim_end().to_string()
}

impl Xbe {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE || &data[..4] != MAGIC {
            return Err(Error::Format("not an XBE (missing XBEH header)"));
        }
        let g = |at: usize| le32(data, at).ok_or(Error::Corrupt("short header"));
        let base_addr = g(0x104)?;
        let size_of_headers = g(0x108)?;
        let size_of_image = g(0x10C)?;
        if (size_of_headers as usize) < HEADER_SIZE || size_of_headers as usize > data.len() {
            return Err(Error::Corrupt("header size is wrong"));
        }
        let timestamp = g(0x114)?;
        let cert_addr = g(0x118)?;
        let num_sections = g(0x11C)?;
        let sections_addr = g(0x120)?;
        let init_flags = g(0x124)?;
        let entry_raw = g(0x128)?;
        let thunk_raw = g(0x158)?;

        // Retail if XORing with the retail key lands the entry point inside the image
        let in_image = |a: u32| a >= base_addr && (a - base_addr) < size_of_image.max(1);
        let retail = in_image(entry_raw ^ ENTRY_KEY_RETAIL);
        let (entry_point, kernel_thunk_addr) = if retail {
            (entry_raw ^ ENTRY_KEY_RETAIL, thunk_raw ^ THUNK_KEY_RETAIL)
        } else if in_image(entry_raw ^ ENTRY_KEY_DEBUG) {
            (entry_raw ^ ENTRY_KEY_DEBUG, thunk_raw ^ THUNK_KEY_DEBUG)
        } else {
            return Err(Error::Corrupt("entry point is outside the image under either key"));
        };

        let hdr = |va: u32| -> Result<usize> {
            va.checked_sub(base_addr)
                .map(|o| o as usize)
                .filter(|&o| o < size_of_headers as usize)
                .ok_or(Error::Corrupt("header pointer outside the headers"))
        };

        let c = hdr(cert_addr)?;
        let cert_bytes = data.get(c..c + 0xB0).ok_or(Error::Corrupt("certificate cut short"))?;
        let cg = |at: usize| le32(cert_bytes, at).ok_or(Error::Corrupt("certificate cut short"));
        let certificate = Certificate {
            timestamp: cg(0x04)?,
            title_id: cg(0x08)?,
            title_name: utf16_name(&cert_bytes[0x0C..0x0C + 80]),
            allowed_media: cg(0x9C)?,
            region: cg(0xA0)?,
            ratings: cg(0xA4)?,
            disc_number: cg(0xA8)?,
            version: cg(0xAC)?,
        };

        if num_sections > 1024 {
            return Err(Error::Corrupt("too many sections"));
        }
        let mut sections = Vec::new();
        for i in 0..num_sections as usize {
            let s = hdr(sections_addr)? + i * 56;
            let f = |at: usize| le32(data, s + at).ok_or(Error::Corrupt("section table cut short"));
            let name_off = hdr(f(20)?)?;
            sections.push(Section {
                flags: f(0)?,
                virtual_addr: f(4)?,
                virtual_size: f(8)?,
                raw_addr: f(12)?,
                raw_size: f(16)?,
                name: read_cstr(data, name_off),
            });
        }
        let _ = le16; // (kept for field readers that need 16-bit values)
        Ok(Self {
            base_addr,
            size_of_headers,
            size_of_image,
            timestamp,
            init_flags,
            entry_point,
            kernel_thunk_addr,
            retail,
            certificate,
            sections,
        })
    }

    /// File offset of a virtual address, if some section (or the header area) holds it.
    pub fn va_to_file_offset(&self, va: u32) -> Option<usize> {
        if va >= self.base_addr && va - self.base_addr < self.size_of_headers {
            return Some((va - self.base_addr) as usize);
        }
        self.sections.iter().find_map(|s| {
            let off = va.checked_sub(s.virtual_addr)?;
            (off < s.virtual_size.max(s.raw_size)).then(|| s.raw_addr as usize + off as usize)
        })
    }

    /// Ordinals of the kernel exports this executable imports. Entries have
    /// the top bit set; the table ends at the first zero.
    pub fn kernel_imports(&self, data: &[u8]) -> Result<Vec<u32>> {
        let start = self
            .va_to_file_offset(self.kernel_thunk_addr)
            .ok_or(Error::Corrupt("kernel thunk table is not in the file"))?;
        let mut out = Vec::new();
        let mut at = start;
        loop {
            let v = le32(data, at).ok_or(Error::Corrupt("kernel thunk table is cut short"))?;
            if v == 0 {
                break;
            }
            out.push(v & 0x7FFF_FFFF);
            at += 4;
            if out.len() > 4096 {
                return Err(Error::Corrupt("kernel thunk table does not end"));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u32 = 0x0001_0000;

    /// A minimal two-section XBE: .text at 0x11000 and a thunk table in .rdata.
    fn build(retail: bool, ordinals: &[u32]) -> Vec<u8> {
        let mut h = vec![0u8; 0x1000];
        h[..4].copy_from_slice(b"XBEH");
        let put = |h: &mut Vec<u8>, at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut h, 0x104, BASE);
        put(&mut h, 0x108, 0x1000); // headers
        put(&mut h, 0x10C, 0x3000); // image
        put(&mut h, 0x114, 0x1234_5678);
        let cert_va = BASE + 0x178;
        put(&mut h, 0x118, cert_va);
        put(&mut h, 0x11C, 2);
        let sec_va = BASE + 0x300;
        put(&mut h, 0x120, sec_va);
        let (ek, tk) = if retail { (ENTRY_KEY_RETAIL, THUNK_KEY_RETAIL) } else { (ENTRY_KEY_DEBUG, THUNK_KEY_DEBUG) };
        put(&mut h, 0x128, (BASE + 0x1000) ^ ek);
        put(&mut h, 0x158, (BASE + 0x2000) ^ tk);
        // certificate
        let c = 0x178;
        put(&mut h, c, 0xB0);
        put(&mut h, c + 0x08, 0x4D53_0004);
        for (i, u) in "Test Game".encode_utf16().enumerate() {
            h[c + 0x0C + i * 2..c + 0x0C + i * 2 + 2].copy_from_slice(&u.to_le_bytes());
        }
        put(&mut h, c + 0xA0, 7);
        put(&mut h, c + 0xA8, 1);
        put(&mut h, c + 0xAC, 3);
        // names
        h[0x400..0x405].copy_from_slice(b".text");
        h[0x410..0x417].copy_from_slice(b".rdata\0"[..7].try_into().unwrap());
        // sections: flags, va, vsize, raw, rsize, name va
        let sec = |h: &mut Vec<u8>, i: usize, va: u32, vs: u32, raw: u32, rs: u32, name: u32| {
            let s = 0x300 + i * 56;
            put(h, s, 0x6);
            put(h, s + 4, va);
            put(h, s + 8, vs);
            put(h, s + 12, raw);
            put(h, s + 16, rs);
            put(h, s + 20, BASE + name);
        };
        sec(&mut h, 0, BASE + 0x1000, 0x1000, 0x1000, 0x1000, 0x400);
        sec(&mut h, 1, BASE + 0x2000, 0x1000, 0x2000, 0x1000, 0x410);
        let mut file = h;
        file.resize(0x3000, 0);
        for (i, o) in ordinals.iter().enumerate() {
            let at = 0x2000 + i * 4;
            file[at..at + 4].copy_from_slice(&(0x8000_0000 | o).to_le_bytes());
        }
        file
    }

    #[test]
    fn parses_retail_and_debug_images() {
        for retail in [true, false] {
            let data = build(retail, &[1, 49, 255]);
            let x = Xbe::parse(&data).unwrap();
            assert_eq!(x.retail, retail);
            assert_eq!(x.entry_point, BASE + 0x1000);
            assert_eq!(x.kernel_thunk_addr, BASE + 0x2000);
            assert_eq!(x.certificate.title_id, 0x4D53_0004);
            assert_eq!(x.certificate.title_name, "Test Game");
            assert_eq!(x.certificate.region, 7);
            assert_eq!(x.certificate.disc_number, 1);
            assert_eq!(x.certificate.version, 3);
            assert_eq!(x.sections.len(), 2);
            assert_eq!(x.sections[0].name, ".text");
            assert_eq!(x.sections[1].name, ".rdata");
            assert_eq!(x.kernel_imports(&data).unwrap(), [1, 49, 255]);
        }
    }

    #[test]
    fn maps_virtual_addresses_to_file_offsets() {
        let data = build(true, &[]);
        let x = Xbe::parse(&data).unwrap();
        assert_eq!(x.va_to_file_offset(BASE + 0x10), Some(0x10));
        assert_eq!(x.va_to_file_offset(BASE + 0x1234), Some(0x1234));
        assert_eq!(x.va_to_file_offset(BASE + 0x2FFF), Some(0x2FFF));
        assert_eq!(x.va_to_file_offset(0x5000_0000), None);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        assert!(Xbe::parse(b"MZ").is_err());
        assert!(Xbe::parse(&vec![0u8; 0x400]).is_err());
        // every truncation of a good file is an error or a clean parse, never a panic
        let data = build(true, &[1]);
        for n in (0..0x400).step_by(7) {
            let _ = Xbe::parse(&data[..n]);
        }
        let mut bad = data.clone();
        bad[0x11C..0x120].copy_from_slice(&5000u32.to_le_bytes());
        assert!(Xbe::parse(&bad).is_err());
        let mut bad = data;
        bad[0x120..0x124].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        assert!(Xbe::parse(&bad).is_err());
    }
}
