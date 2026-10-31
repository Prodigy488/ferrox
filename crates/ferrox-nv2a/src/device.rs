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
use std::time::Instant;

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
    /// PMC_INTR_EN_0 (bit 0: hardware interrupts on), PCRTC_INTR_0 (bit 0: vertical blank
    /// happened) and PCRTC_INTR_EN_0, shared so the host can raise the interrupt line.
    pmc_intr_en: AtomicU32,
    /// Where instance memory sits in system RAM once the kernel has claimed it (0 = not yet).
    ramin_base: AtomicU32,
    pcrtc_intr: AtomicU32,
    pcrtc_intr_en: AtomicU32,
    unhandled: Mutex<BTreeMap<(bool, u32), u32>>,
    /// The latest register accesses (is_write, offset, value), for finding what a stuck game waits on.
    recent: Mutex<std::collections::VecDeque<(bool, u32, u32)>>,
    /// How often each register was touched.
    hot: Mutex<BTreeMap<(bool, u32), u64>>,
    gaps: Mutex<BTreeMap<String, u32>>,
    log: Mutex<Vec<String>>,
    methods: AtomicU64,
    clears: AtomicU64,
    triangles: AtomicU64,
}

impl Shared {
    /// Points instance memory at `base` in system RAM: from now on the game's own writes to that
    /// memory and the chip's view of it (through the PRAMIN window) are the same bytes.
    pub fn set_ramin_base(&self, base: u32) {
        self.ramin_base.store(base, Ordering::Relaxed);
    }

    pub(crate) fn ramin_base(&self) -> u32 {
        self.ramin_base.load(Ordering::Relaxed)
    }

    /// The picture finished: sets the vertical-blank flag the driver's interrupt handler reads.
    pub fn vblank(&self) {
        self.pcrtc_intr.fetch_or(1, Ordering::Relaxed);
    }

    /// What PMC_INTR_0 reads: one bit per source with a pending, enabled interrupt.
    pub fn pmc_pending(&self) -> u32 {
        let crtc = self.pcrtc_intr.load(Ordering::Relaxed) & self.pcrtc_intr_en.load(Ordering::Relaxed);
        if crtc != 0 { 1 << 24 } else { 0 }
    }

    /// Whether the chip is asking the processor for an interrupt right now.
    pub fn irq_line(&self) -> bool {
        self.pmc_intr_en.load(Ordering::Relaxed) & 1 != 0 && self.pmc_pending() != 0
    }

    pub fn display_start(&self) -> Option<u32> {
        match self.display_start.load(Ordering::Relaxed) {
            0 => None,
            v => Some(v),
        }
    }

    pub(crate) fn trace(&self, write: bool, offset: u64, value: u32) {
        let mut r = self.recent.lock().unwrap();
        if r.len() >= 48 {
            r.pop_front();
        }
        r.push_back((write, offset as u32, value));
        drop(r);
        *self.hot.lock().unwrap().entry((write, offset as u32)).or_insert(0) += 1;
    }

    /// The most recent register accesses, oldest first: (is_write, offset, value).
    pub fn recent_accesses(&self) -> Vec<(bool, u32, u32)> {
        self.recent.lock().unwrap().iter().copied().collect()
    }

    /// The `n` most-touched registers: (is_write, offset, count).
    pub fn hottest(&self, n: usize) -> Vec<(bool, u32, u64)> {
        let mut v: Vec<_> = self.hot.lock().unwrap().iter().map(|(&(w, o), &c)| (w, o, c)).collect();
        v.sort_by_key(|&(_, _, c)| std::cmp::Reverse(c));
        v.truncate(n);
        v
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

    pub(crate) fn count_triangle(&self) {
        self.triangles.fetch_add(1, Ordering::Relaxed);
    }

    /// Triangles the renderer has drawn.
    pub fn triangles(&self) -> u64 {
        self.triangles.load(Ordering::Relaxed)
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
    raster: u32,
    status_toggle: bool,
    gpu: Gpu,
    started: Instant,
    /// Registers nothing models yet read back what was written, as plain storage does.
    scratch: BTreeMap<u32, u32>,
}

// PBUS: a window onto the chip's PCI configuration
const PBUS_PCI_ID: u64 = 0x1800;
const PBUS_PCI_COMMAND: u64 = 0x1804;
const PBUS_PCI_CLASS: u64 = 0x1808;
// PTIMER
const PTIMER_NUMERATOR: u64 = 0x9200;
const PTIMER_DENOMINATOR: u64 = 0x9210;
const PTIMER_TIME_0: u64 = 0x9400;
const PTIMER_TIME_1: u64 = 0x9410;
const PFB_CSTATUS: u64 = 0x10_020C;
const PFB_WBC: u64 = 0x10_0410;
const PVIDEO_STOP: u64 = 0x8704;
const PRAMDAC_PLL_TEST_COUNTER: u64 = 0x68_0514;

/// Blocks of the chip whose registers simply hold what is written (as in the reference
/// emulator); everywhere else an unmodelled register reads as zero and ignores writes.
fn plain_storage(offset: u64) -> bool {
    matches!(offset, 0x8000..=0x8FFF | 0x10_0000..=0x10_0FFF | 0x40_0000..=0x40_0FFF | 0x68_0000..=0x68_0FFF)
}
/// The chip's core clock.
const CORE_CLOCK_HZ: u128 = 233_333_333;

impl Nv2a {
    pub fn new() -> (Nv2a, Arc<Shared>) {
        let shared = Arc::new(Shared::default());
        let dev = Nv2a {
            shared: shared.clone(),
            raster: 0,
            status_toggle: false,
            gpu: Gpu::new(shared.clone()),
            started: Instant::now(),
            scratch: BTreeMap::new(),
        };
        (dev, shared)
    }

    /// Gives the GPU access to system memory (command buffers and surfaces live there).
    pub fn attach_ram(&mut self, ram: RamView) {
        self.gpu.attach_ram(ram);
    }

    /// The timer's 64-bit count as its two registers show it: it ticks at the core clock scaled
    /// by the programmed ratio, and the registers drop the low 5 bits.
    fn timer(&self) -> u64 {
        let (num, den) = (self.scratch.get(&(PTIMER_NUMERATOR as u32)).copied().unwrap_or(1), self.scratch.get(&(PTIMER_DENOMINATOR as u32)).copied().unwrap_or(1));
        let (num, den) = (num.max(1) as u128, den.max(1) as u128);
        let ticks = self.started.elapsed().as_nanos() * CORE_CLOCK_HZ / 1_000_000_000 * den / num;
        (ticks as u64) << 5
    }
}

const PRAMIN: u64 = 0x70_0000;
const USER: u64 = 0x80_0000;

impl Mmio for Nv2a {
    fn read(&mut self, offset: u64, size: usize) -> u64 {
        let v = self.read_inner(offset, size);
        if !(PRAMIN..USER).contains(&offset) {
            self.shared.trace(false, offset, v as u32);
        }
        v
    }

    fn write(&mut self, offset: u64, size: usize, value: u64) {
        if !(PRAMIN..USER).contains(&offset) {
            self.shared.trace(true, offset, value as u32);
        }
        self.write_inner(offset, size, value);
    }
}

impl Nv2a {
    fn read_inner(&mut self, offset: u64, size: usize) -> u64 {
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
            PBUS_PCI_ID => 0x02A0_10DE,
            PBUS_PCI_COMMAND => self.scratch.get(&(PBUS_PCI_COMMAND as u32)).copied().unwrap_or(0) as u64,
            PBUS_PCI_CLASS => 0x0300_00A1,
            PTIMER_TIME_0 => (self.timer() & 0xFFFF_FFE0) as u64,
            PTIMER_TIME_1 => ((self.timer() >> 32) & 0x1FFF_FFFF) as u64,
            PTIMER_NUMERATOR | PTIMER_DENOMINATOR => self.scratch.get(&(offset as u32)).copied().unwrap_or(1) as u64,
            PMC_INTR_0 => self.shared.pmc_pending() as u64,
            PMC_INTR_EN_0 => self.shared.pmc_intr_en.load(Ordering::Relaxed) as u64,
            PCRTC_INTR_0 => self.shared.pcrtc_intr.load(Ordering::Relaxed) as u64,
            PCRTC_INTR_EN_0 => self.shared.pcrtc_intr_en.load(Ordering::Relaxed) as u64,
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
            PFB_CSTATUS => 64 << 20, // the amount of video memory
            PFB_WBC | PVIDEO_STOP => 0, // "flush not pending", "overlay stopped"
            PRAMDAC_PLL_TEST_COUNTER => 0xF800_0000, // every PLL reports locked
            _ => {
                self.shared.note(false, offset);
                if plain_storage(offset) {
                    self.scratch.get(&(offset as u32)).copied().unwrap_or(0) as u64
                } else {
                    0
                }
            }
        }
    }

    fn write_inner(&mut self, offset: u64, size: usize, value: u64) {
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
            PMC_INTR_EN_0 => self.shared.pmc_intr_en.store(v, Ordering::Relaxed),
            PBUS_PCI_COMMAND | PTIMER_NUMERATOR | PTIMER_DENOMINATOR => {
                self.scratch.insert(offset as u32, v);
            }
            PCRTC_INTR_0 => {
                self.shared.pcrtc_intr.fetch_and(!v, Ordering::Relaxed); // write one to clear
            }
            PCRTC_INTR_EN_0 => self.shared.pcrtc_intr_en.store(v, Ordering::Relaxed),
            PCRTC_START => self.shared.display_start.store(v & 0x07FF_FFFF, Ordering::Relaxed),
            _ => {
                self.shared.note(true, offset);
                if plain_storage(offset) {
                    self.scratch.insert(offset as u32, v);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pci_window_timer_and_plain_registers() {
        let (mut d, _) = Nv2a::new();
        assert_eq!(d.read(0x1800, 4), 0x02A0_10DE);
        assert_eq!(d.read(0x1808, 4) >> 8, 0x030000);
        let a = d.read(PTIMER_TIME_0, 4);
        std::thread::sleep(std::time::Duration::from_millis(5));
        let b = d.read(PTIMER_TIME_0, 4);
        assert!(b > a, "the timer runs: {a:#x} then {b:#x}");
        d.write(0x40_0750, 4, 0x1100);
        assert_eq!(d.read(0x40_0750, 4), 0x1100, "a graphics register reads back");
        d.write(0x200, 4, 0x1100);
        assert_eq!(d.read(0x200, 4), 0, "the master control block does not");
        d.write(0x10_0410, 4, 0x1_0000);
        assert_eq!(d.read(0x10_0410, 4), 0, "a write-buffer flush is never pending");
        assert_eq!(d.read(0x10_020C, 4), 64 << 20);
    }

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
        let (mut d, shared) = Nv2a::new();
        d.write(PMC_INTR_EN_0, 4, 1);
        d.write(PCRTC_INTR_EN_0, 4, 1);
        assert_eq!((d.read(PMC_INTR_EN_0, 4), d.read(PCRTC_INTR_EN_0, 4)), (1, 1));
        shared.vblank();
        assert_eq!(d.read(PCRTC_INTR_0, 4), 1);
        assert!(shared.irq_line(), "enabled and pending");
        assert_eq!(d.read(PMC_INTR_0, 4), 1 << 24, "the CRTC bit of the master register");
        d.write(PCRTC_INTR_0, 4, 1);
        assert_eq!(d.read(PCRTC_INTR_0, 4), 0);
        assert!(!shared.irq_line());
        assert_eq!(d.read(PMC_INTR_0, 4), 0);
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
