//! The Xbox's physical address space: RAM, the mirrored flash ROM and
//! memory-mapped devices, behind the `Bus` trait the CPU backends use.

use ferrox_core::{Bus, BusError, GuestAddr};
use std::sync::Arc;

/// Address-space facts. The first group is fixed by the hardware; the device
/// windows are where the BIOS normally programs the PCI base registers.
pub mod map {
    pub const RAM_BASE: u64 = 0;
    /// Retail console RAM (development kits and some mods have 128 MiB).
    pub const RAM_RETAIL: usize = 64 * 1024 * 1024;
    pub const RAM_DEVKIT: usize = 128 * 1024 * 1024;
    /// The flash ROM is mirrored upward to the very top of the 32-bit space.
    pub const ROM_TOP: u64 = 0xFFFF_FFFF;
    /// Typical PCI window bases chosen by the BIOS.
    pub const NV2A_BASE: u64 = 0xFD00_0000;
    pub const NV2A_SIZE: u64 = 0x0100_0000;
    pub const APU_BASE: u64 = 0xFE80_0000;
    pub const AC97_BASE: u64 = 0xFEC0_0000;
    pub const USB0_BASE: u64 = 0xFED0_0000;
    pub const USB1_BASE: u64 = 0xFED0_8000;
    pub const NVNET_BASE: u64 = 0xFEF0_0000;
}

/// A memory-mapped device. Offsets are relative to the start of its window.
pub trait Mmio: Send {
    fn read(&mut self, offset: u64, size: usize) -> u64;
    fn write(&mut self, offset: u64, size: usize, value: u64);
}

/// A device on the x86 I/O port space.
pub trait PortDevice: Send {
    fn io_in(&mut self, port: u16, size: usize) -> u64;
    fn io_out(&mut self, port: u16, size: usize, value: u64);
}

enum Backing {
    Ram(Vec<u8>),
    /// Read-only bytes; shared so a ROM can appear at several mirrored addresses.
    Rom(Arc<Vec<u8>>),
    Mmio(Box<dyn Mmio>),
}

struct Region {
    base: u64,
    size: u64,
    backing: Backing,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MapError {
    Overlap { base: u64, size: u64 },
    Empty,
    PastTop,
}

impl std::fmt::Display for MapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapError::Overlap { base, size } => {
                write!(f, "region {base:#x}+{size:#x} overlaps one already mapped")
            }
            MapError::Empty => write!(f, "empty region"),
            MapError::PastTop => write!(f, "region runs past the end of the 32-bit space"),
        }
    }
}

impl std::error::Error for MapError {}

/// A span of guest memory backed by host memory, for hypervisor backends that
/// map it into the VM so the guest accesses it at full speed.
#[derive(Clone, Copy, Debug)]
pub struct MemSlot {
    pub guest_addr: u64,
    pub size: u64,
    /// Start of the host bytes. Valid while the `PhysMem` it came from lives
    /// and is not mutated through `Bus` at the same time the guest runs.
    pub host_ptr: *mut u8,
    pub read_only: bool,
}

#[derive(Default)]
pub struct PhysMem {
    /// Sorted by base, never overlapping.
    regions: Vec<Region>,
    ports: Vec<(std::ops::RangeInclusive<u16>, Box<dyn PortDevice>)>,
}

impl PhysMem {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&mut self, base: u64, size: u64, backing: Backing) -> Result<(), MapError> {
        if size == 0 {
            return Err(MapError::Empty);
        }
        if base + size > (1u64 << 32) {
            return Err(MapError::PastTop);
        }
        let at = self.regions.partition_point(|r| r.base < base);
        let overlaps_prev = at > 0 && {
            let p = &self.regions[at - 1];
            p.base + p.size > base
        };
        let overlaps_next = self.regions.get(at).is_some_and(|n| base + size > n.base);
        if overlaps_prev || overlaps_next {
            return Err(MapError::Overlap { base, size });
        }
        self.regions.insert(at, Region { base, size, backing });
        Ok(())
    }

    /// RAM and ROM regions as host memory spans. MMIO regions are absent: a
    /// hypervisor reports guest accesses to them as exits, which the machine
    /// serves through `Bus`.
    pub fn mem_slots(&mut self) -> Vec<MemSlot> {
        self.regions
            .iter_mut()
            .filter_map(|r| match &mut r.backing {
                Backing::Ram(v) => Some(MemSlot {
                    guest_addr: r.base,
                    size: r.size,
                    host_ptr: v.as_mut_ptr(),
                    read_only: false,
                }),
                Backing::Rom(v) => Some(MemSlot {
                    guest_addr: r.base,
                    size: r.size,
                    host_ptr: v.as_ptr() as *mut u8,
                    read_only: true,
                }),
                Backing::Mmio(_) => None,
            })
            .collect()
    }

    pub fn add_ram(&mut self, base: u64, size: usize) -> Result<(), MapError> {
        self.insert(base, size as u64, Backing::Ram(vec![0; size]))
    }

    pub fn add_rom(&mut self, base: u64, data: Vec<u8>) -> Result<(), MapError> {
        let size = data.len() as u64;
        self.insert(base, size, Backing::Rom(Arc::new(data)))
    }

    /// Maps `data` repeatedly so that it fills `[ROM_TOP + 1 - span, ROM_TOP]`,
    /// the way the console's flash repeats up to the top of memory.
    /// `data.len()` must divide `span`.
    pub fn add_rom_mirrored(&mut self, span: u64, data: Vec<u8>) -> Result<(), MapError> {
        let len = data.len() as u64;
        if len == 0 || span % len != 0 {
            return Err(MapError::Empty);
        }
        let shared = Arc::new(data);
        let start = map::ROM_TOP + 1 - span;
        let mut base = start;
        while base <= map::ROM_TOP {
            self.insert(base, len, Backing::Rom(shared.clone()))?;
            base += len;
        }
        Ok(())
    }

    pub fn add_mmio(&mut self, base: u64, size: u64, dev: Box<dyn Mmio>) -> Result<(), MapError> {
        self.insert(base, size, Backing::Mmio(dev))
    }

    pub fn add_ports(&mut self, ports: std::ops::RangeInclusive<u16>, dev: Box<dyn PortDevice>) {
        self.ports.push((ports, dev));
    }

    fn find(&mut self, addr: u64, size: usize) -> Option<(&mut Region, u64)> {
        let i = self.regions.partition_point(|r| r.base <= addr).checked_sub(1)?;
        let r = &mut self.regions[i];
        let off = addr - r.base;
        (off + size as u64 <= r.size).then_some((r, off))
    }

    /// Copies bytes straight into RAM/ROM (loading an image before the CPU runs).
    pub fn load(&mut self, addr: GuestAddr, bytes: &[u8]) -> Result<(), BusError> {
        let (r, off) = self.find(addr, bytes.len()).ok_or(BusError::Unmapped(addr))?;
        match &mut r.backing {
            Backing::Ram(v) => {
                v[off as usize..off as usize + bytes.len()].copy_from_slice(bytes);
                Ok(())
            }
            _ => Err(BusError::ReadOnly(addr)),
        }
    }
}

fn check_size(size: usize) -> Result<(), BusError> {
    matches!(size, 1 | 2 | 4 | 8).then_some(()).ok_or(BusError::BadSize(size))
}

impl Bus for PhysMem {
    fn read(&mut self, addr: GuestAddr, size: usize) -> Result<u64, BusError> {
        check_size(size)?;
        let (r, off) = self.find(addr, size).ok_or(BusError::Unmapped(addr))?;
        let bytes = |data: &[u8]| {
            let mut v = 0u64;
            for (i, b) in data[off as usize..off as usize + size].iter().enumerate() {
                v |= (*b as u64) << (8 * i);
            }
            v
        };
        Ok(match &mut r.backing {
            Backing::Ram(d) => bytes(d),
            Backing::Rom(d) => bytes(d),
            Backing::Mmio(dev) => dev.read(off, size),
        })
    }

    fn write(&mut self, addr: GuestAddr, size: usize, value: u64) -> Result<(), BusError> {
        check_size(size)?;
        let (r, off) = self.find(addr, size).ok_or(BusError::Unmapped(addr))?;
        match &mut r.backing {
            Backing::Ram(d) => {
                for i in 0..size {
                    d[off as usize + i] = (value >> (8 * i)) as u8;
                }
                Ok(())
            }
            Backing::Rom(_) => Err(BusError::ReadOnly(addr)),
            Backing::Mmio(dev) => {
                dev.write(off, size, value);
                Ok(())
            }
        }
    }

    fn io_in(&mut self, port: u16, size: usize) -> u64 {
        for (range, dev) in &mut self.ports {
            if range.contains(&port) {
                return dev.io_in(port, size);
            }
        }
        match size {
            1 => 0xff,
            2 => 0xffff,
            _ => 0xffff_ffff,
        }
    }

    fn io_out(&mut self, port: u16, size: usize, value: u64) {
        for (range, dev) in &mut self.ports {
            if range.contains(&port) {
                dev.io_out(port, size, value);
                return;
            }
        }
    }
}

#[cfg(test)]
mod slot_tests {
    use super::*;

    #[test]
    fn slots_list_ram_and_rom_but_not_mmio() {
        struct Dev;
        impl Mmio for Dev {
            fn read(&mut self, _: u64, _: usize) -> u64 { 0 }
            fn write(&mut self, _: u64, _: usize, _: u64) {}
        }
        let mut m = PhysMem::new();
        m.add_ram(0, 4096).unwrap();
        m.add_mmio(0x1000_0000, 4096, Box::new(Dev)).unwrap();
        m.add_rom_mirrored(0x2000, vec![1; 0x1000]).unwrap();
        let slots = m.mem_slots();
        assert_eq!(slots.len(), 3);
        assert!(!slots[0].read_only && slots[1..].iter().all(|s| s.read_only));
        assert_eq!(slots[1].host_ptr, slots[2].host_ptr, "mirrors share bytes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Log(Arc<Mutex<Vec<(u64, usize, u64)>>>);
    impl Mmio for Log {
        fn read(&mut self, offset: u64, _size: usize) -> u64 {
            0xC0DE_0000 + offset
        }
        fn write(&mut self, offset: u64, size: usize, value: u64) {
            self.0.lock().unwrap().push((offset, size, value));
        }
    }

    #[test]
    fn ram_is_little_endian_at_every_size() {
        let mut m = PhysMem::new();
        m.add_ram(0, 4096).unwrap();
        m.write(0x10, 4, 0x1122_3344).unwrap();
        assert_eq!(m.read(0x10, 1).unwrap(), 0x44);
        assert_eq!(m.read(0x11, 1).unwrap(), 0x33);
        assert_eq!(m.read(0x10, 2).unwrap(), 0x3344);
        assert_eq!(m.read(0x10, 4).unwrap(), 0x1122_3344);
        m.write(0x20, 8, 0x0102_0304_0506_0708).unwrap();
        assert_eq!(m.read(0x24, 4).unwrap(), 0x0102_0304);
    }

    #[test]
    fn unmapped_and_straddling_accesses_fail() {
        let mut m = PhysMem::new();
        m.add_ram(0, 4096).unwrap();
        assert_eq!(m.read(0x2000, 4), Err(BusError::Unmapped(0x2000)));
        assert_eq!(m.read(4094, 4), Err(BusError::Unmapped(4094)));
        assert_eq!(m.read(0, 3), Err(BusError::BadSize(3)));
    }

    #[test]
    fn overlapping_regions_are_refused() {
        let mut m = PhysMem::new();
        m.add_ram(0x1000, 0x1000).unwrap();
        assert!(m.add_ram(0x1800, 0x1000).is_err());
        assert!(m.add_ram(0x0800, 0x1000).is_err());
        assert!(m.add_ram(0x2000, 0x1000).is_ok());
        assert_eq!(m.add_ram(0, 0), Err(MapError::Empty));
        assert_eq!(m.add_ram(0xFFFF_F000, 0x2000), Err(MapError::PastTop));
    }

    #[test]
    fn bios_mirrors_to_the_top_of_memory_and_is_read_only() {
        let mut m = PhysMem::new();
        let mut bios = vec![0u8; 256 * 1024];
        bios[0] = 0xEA;
        bios[0x3FFFF] = 0x5A;
        // 16 MiB window of 256 KiB mirrors
        m.add_rom_mirrored(16 * 1024 * 1024, bios).unwrap();
        assert_eq!(m.read(0xFF00_0000, 1).unwrap(), 0xEA);
        assert_eq!(m.read(0xFF04_0000, 1).unwrap(), 0xEA);
        assert_eq!(m.read(0xFFFF_FFFF, 1).unwrap(), 0x5A);
        assert_eq!(m.write(0xFFFF_FFFF, 1, 0), Err(BusError::ReadOnly(0xFFFF_FFFF)));
    }

    #[test]
    fn mmio_gets_offsets_and_sizes() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut m = PhysMem::new();
        m.add_mmio(map::NV2A_BASE, map::NV2A_SIZE, Box::new(Log(log.clone()))).unwrap();
        m.write(map::NV2A_BASE + 0x100, 4, 7).unwrap();
        assert_eq!(m.read(map::NV2A_BASE + 0x40, 4).unwrap(), 0xC0DE_0040);
        assert_eq!(log.lock().unwrap().as_slice(), &[(0x100, 4, 7)]);
    }

    #[test]
    fn load_only_targets_ram() {
        let mut m = PhysMem::new();
        m.add_ram(0, 64).unwrap();
        m.add_rom(0x1000, vec![0; 16]).unwrap();
        m.load(8, &[1, 2, 3]).unwrap();
        assert_eq!(m.read(9, 1).unwrap(), 2);
        assert!(m.load(60, &[0; 8]).is_err());
        assert_eq!(m.load(0x1000, &[1]), Err(BusError::ReadOnly(0x1000)));
    }

    #[test]
    fn unclaimed_ports_read_as_ones() {
        let mut m = PhysMem::new();
        assert_eq!(m.io_in(0x80, 1), 0xff);
        assert_eq!(m.io_in(0x80, 4), 0xffff_ffff);
    }
}
