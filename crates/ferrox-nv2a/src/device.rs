//! The NV2A's memory-mapped registers, as far as they are implemented.
//!
//! Register numbers and reset values are cross-checked against xemu's
//! `hw/xbox/nv2a` (`nv2a_regs.h`, `pmc.c`, `pcrtc.c`). Everything not listed reads
//! as zero and ignores writes, and is counted so a front end can report which
//! registers a program touched that are not emulated yet.

use crate::gpu::{self, Gpu};
use ferrox_machine::{Mmio, RamView};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Where the register window sits in the physical address space.
pub const BASE: u64 = 0xFD00_0000;
pub const SIZE: u64 = 0x0100_0000;

// PMC: card master control (block at 0x000000)
pub const PMC_BOOT_0: u64 = 0x0000_0000;
pub const PMC_INTR_0: u64 = 0x0000_0100;
pub const PMC_INTR_EN_0: u64 = 0x0000_0140;
/// NV2A, stepping A03, revision 0 (as xemu reports).
pub const BOOT_0_VALUE: u32 = 0x02A0_00A3;

// PCRTC: CRTC controls (block at 0x600000)
pub const PCRTC_INTR_0: u64 = 0x0060_0100;
pub const PCRTC_INTR_EN_0: u64 = 0x0060_0140;
pub const PCRTC_START: u64 = 0x0060_0800;
pub const PCRTC_RASTER: u64 = 0x0060_0808;
/// VGA input status register as seen through PRMCIO (0x601000 + 0x3DA); programs
/// poll it for the vertical-retrace bit.
pub const VGA_STATUS: u64 = 0x0060_13DA;

/// State other parts of the emulator read while the guest runs.
#[derive(Default)]
pub struct Shared {
    /// Where the CRTC is scanning out from (0 until a program sets it).
    display_start: AtomicU32,
    unhandled: Mutex<BTreeMap<(bool, u32), u32>>,
    gaps: Mutex<BTreeMap<String, u32>>,
    log: Mutex<Vec<String>>,
    methods: AtomicU64,
    clears: AtomicU64,
}

impl Shared {
    pub fn display_start(&self) -> Option<u32> {
        match self.display_start.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    /// Registers touched but not emulated: (is_write, offset, count).
    pub fn unhandled(&self) -> Vec<(bool, u32, u32)> {
        self.unhandled.lock().unwrap().iter().map(|(&(w, o), &n)| (w, o, n)).collect()
    }

    /// Things the GPU was asked to do that are not emulated yet, with how often.
    pub fn gaps(&self) -> Vec<(String, u32)> {
        self.gaps.lock().unwrap().iter().map(|(k, &n)| (k.clone(), n)).collect()
    }

    /// Problems the GPU found in what the program sent it (bad handles, bad buffers).
    pub fn take_log(&self) -> Vec<String> {
        std::mem::take(&mut *self.log.lock().unwrap())
    }

    /// (graphics methods run, surface clears done)
    pub fn gpu_counts(&self) -> (u64, u64) {
        (self.methods.load(Ordering::Relaxed), self.clears.load(Ordering::Relaxed))
    }

    pub(crate) fn gap(&self, what: String) {
        *self.gaps.lock().unwrap().entry(what).or_insert(0) += 1;
    }

    pub(crate) fn log(&self, what: String) {
        let mut l = self.log.lock().unwrap();
        if l.len() < 100 {
            l.push(what);
        }
    }

    pub(crate) fn count_method(&self) {
        self.methods.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn count_clear(&self) {
        self.clears.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note(&self, write: bool, offset: u64) {
        *self.unhandled.lock().unwrap().entry((write, offset as u32)).or_insert(0) += 1;
    }
}

pub struct Nv2a {
    shared: Arc<Shared>,
    pmc_intr_en: u32,
    pcrtc_intr_en: u32,
    pcrtc_intr: u32,
    raster: u32,
    status_toggle: bool,
    gpu: Gpu,
}

impl Nv2a {
    pub fn new() -> (Nv2a, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        let dev = Nv2a {
            shared: shared.clone(),
            pmc_intr_en: 0,
            pcrtc_intr_en: 0,
            pcrtc_intr: 0,
            raster: 0,
            status_toggle: false,
            gpu: Gpu::new(shared.clone()),
        };
        (dev, shared)
    }

    /// Gives the GPU access to system memory (command buffers and surfaces live there).
    pub fn attach_ram(&mut self, ram: RamView) {
        self.gpu.attach_ram(ram);
    }
}

const PRAMIN: u64 = 0x70_0000;
const USER: u64 = 0x80_0000;

impl Mmio for Nv2a {
    fn read(&mut self, offset: u64, size: usize) -> u64 {
        if (PRAMIN..USER).contains(&offset) {
            return self.gpu.ramin_read(offset - PRAMIN, size);
        }
        if offset >= USER {
            let rel = offset - USER;
            return self.gpu.user_read((rel >> 16) as u32, (rel & 0xFFFF) as u32) as u64;
        }
        if gpu::is_pfifo(offset as u32) {
            return self.gpu.pfifo_read(offset as u32) as u64;
        }
        match offset {
            PMC_BOOT_0 => BOOT_0_VALUE as u64,
            PMC_INTR_0 => 0,
            PMC_INTR_EN_0 => self.pmc_intr_en as u64,
            PCRTC_INTR_0 => self.pcrtc_intr as u64,
            PCRTC_INTR_EN_0 => self.pcrtc_intr_en as u64,
            PCRTC_START => self.shared.display_start.load(Ordering::Relaxed) as u64,
            PCRTC_RASTER => {
                self.raster = self.raster.wrapping_add(1);
                (self.raster - 1) as u64
            }
            VGA_STATUS => {
                // Alternates so a program polling for retrace (bit 3) sees both states.
                self.status_toggle = !self.status_toggle;
                if self.status_toggle { 0x09 } else { 0x00 }
            }
            _ => {
                self.shared.note(false, offset);
                0
            }
        }
    }

    fn write(&mut self, offset: u64, size: usize, value: u64) {
        let v = value as u32;
        if (PRAMIN..USER).contains(&offset) {
            return self.gpu.ramin_write(offset - PRAMIN, size, value);
        }
        if offset >= USER {
            let rel = offset - USER;
            return self.gpu.user_write((rel >> 16) as u32, (rel & 0xFFFF) as u32, v);
        }
        if gpu::is_pfifo(offset as u32) {
            return self.gpu.pfifo_write(offset as u32, v);
        }
        match offset {
            PMC_INTR_EN_0 => self.pmc_intr_en = v,
            PCRTC_INTR_0 => self.pcrtc_intr &= !v, // write one to clear
            PCRTC_INTR_EN_0 => self.pcrtc_intr_en = v,
            PCRTC_START => self.shared.display_start.store(v & 0x07FF_FFFF, Ordering::Relaxed),
            _ => self.shared.note(true, offset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chip_id_matches_xemu() {
        let (mut d, _) = Nv2a::new();
        assert_eq!(d.read(PMC_BOOT_0, 4), 0x02A0_00A3);
    }

    #[test]
    fn display_start_is_masked_and_shared() {
        let (mut d, shared) = Nv2a::new();
        assert_eq!(shared.display_start(), None);
        d.write(PCRTC_START, 4, 0xFFFF_FFFF);
        assert_eq!(shared.display_start(), Some(0x07FF_FFFF));
        d.write(PCRTC_START, 4, 0x0200_0000);
        assert_eq!(shared.display_start(), Some(0x0200_0000));
        assert_eq!(d.read(PCRTC_START, 4), 0x0200_0000);
    }

    #[test]
    fn interrupt_enables_read_back_and_pending_bits_clear_on_write() {
        let (mut d, _) = Nv2a::new();
        d.write(PMC_INTR_EN_0, 4, 1);
        d.write(PCRTC_INTR_EN_0, 4, 1);
        assert_eq!((d.read(PMC_INTR_EN_0, 4), d.read(PCRTC_INTR_EN_0, 4)), (1, 1));
        d.pcrtc_intr = 1;
        d.write(PCRTC_INTR_0, 4, 1);
        assert_eq!(d.read(PCRTC_INTR_0, 4), 0);
    }

    #[test]
    fn retrace_bit_toggles_and_raster_counts() {
        let (mut d, _) = Nv2a::new();
        let seen: Vec<u64> = (0..4).map(|_| d.read(VGA_STATUS, 1) & 8).collect();
        assert!(seen.contains(&8) && seen.contains(&0));
        assert_eq!((d.read(PCRTC_RASTER, 4), d.read(PCRTC_RASTER, 4)), (0, 1));
    }

    #[test]
    fn unknown_registers_are_counted_not_fatal() {
        let (mut d, shared) = Nv2a::new();
        assert_eq!(d.read(0x40_0000, 4), 0);
        d.write(0x40_0000, 4, 5);
        d.write(0x40_0000, 4, 6);
        assert_eq!(shared.unhandled(), vec![(false, 0x40_0000, 1), (true, 0x40_0000, 2)]);
    }
}
