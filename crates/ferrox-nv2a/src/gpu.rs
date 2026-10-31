//! The GPU's command front end: instance memory (PRAMIN), the object hash table
//! (RAMHT), DMA objects, the PFIFO pusher that fetches the command buffer and the
//! graphics engine's first methods (surface setup and clears).
//!
//! Structure and numbers follow xemu's `pfifo.c`, `nv2a.c` and `nv2a_regs.h`.
//! Only what a program has been seen to need is implemented; anything else the
//! engine is asked to do is recorded in `Shared::gaps` so it shows up in the
//! front end's report instead of silently doing nothing.

use crate::device::Shared;
use crate::{decode, Command};
use ferrox_machine::RamView;
use std::collections::BTreeMap;
use std::sync::Arc;

pub const RAMIN_SIZE: usize = 0x10_0000;

// PFIFO registers (absolute offsets in the register window)
pub const PFIFO_INTR_0: u32 = 0x2100;
pub const PFIFO_INTR_EN_0: u32 = 0x2140;
pub const PFIFO_RAMHT: u32 = 0x2210;
pub const CACHE1_PUSH0: u32 = 0x3200;
pub const CACHE1_PUSH1: u32 = 0x3204;
pub const CACHE1_DMA_PUSH: u32 = 0x3220;
pub const CACHE1_DMA_STATE: u32 = 0x3228;
pub const CACHE1_DMA_INSTANCE: u32 = 0x322C;
pub const CACHE1_DMA_CTL: u32 = 0x3230;
pub const CACHE1_DMA_PUT: u32 = 0x3240;
pub const CACHE1_DMA_GET: u32 = 0x3244;
pub const CACHE1_PULL0: u32 = 0x3250;

pub fn is_pfifo(off: u32) -> bool {
    (0x2000..0x4000).contains(&off)
}

fn known_pfifo(off: u32) -> bool {
    matches!(
        off,
        PFIFO_INTR_0 | PFIFO_INTR_EN_0 | PFIFO_RAMHT | CACHE1_PUSH0 | CACHE1_PUSH1 | CACHE1_DMA_PUSH
            | CACHE1_DMA_STATE | CACHE1_DMA_INSTANCE | CACHE1_DMA_CTL | CACHE1_DMA_PUT | CACHE1_DMA_GET
            | CACHE1_PULL0
    )
}

// Kelvin (class 0x97) methods
const NV097_NO_OPERATION: u32 = 0x100;
const NV097_SET_CONTEXT_DMA_NOTIFIES: u32 = 0x180;
const NV097_SET_CONTEXT_DMA_COLOR: u32 = 0x194;
const NV097_SET_CONTEXT_DMA_ZETA: u32 = 0x198;
const NV097_SET_SURFACE_CLIP_HORIZONTAL: u32 = 0x200;
const NV097_SET_SURFACE_CLIP_VERTICAL: u32 = 0x204;
const NV097_SET_SURFACE_FORMAT: u32 = 0x208;
const NV097_SET_SURFACE_PITCH: u32 = 0x20C;
const NV097_SET_SURFACE_COLOR_OFFSET: u32 = 0x210;
const NV097_SET_SURFACE_ZETA_OFFSET: u32 = 0x214;
const NV097_SET_COLOR_CLEAR_VALUE: u32 = 0x1D90;
const NV097_CLEAR_SURFACE: u32 = 0x1D94;
const NV097_SET_CLEAR_RECT_HORIZONTAL: u32 = 0x1D98;
const NV097_SET_CLEAR_RECT_VERTICAL: u32 = 0x1D9C;
pub const KELVIN_CLASS: u32 = 0x97;

/// Where the RAMHT hash of `handle` lands (an entry is 8 bytes). `size_field` is
/// the RAMHT register's SIZE field (0 = 4 KiB ... 3 = 32 KiB).
pub fn ramht_hash(handle: u32, channel: u32, size_field: u32) -> u32 {
    let size = 1u32 << (size_field + 12);
    let bits = size.trailing_zeros() - 1;
    let mut h = 0;
    let mut rest = handle;
    while rest != 0 {
        h ^= rest & ((1 << bits) - 1);
        rest >>= bits;
    }
    h ^ (channel << (bits - 4))
}

/// What a DMA object in instance memory describes: a span of memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaObject {
    pub class: u32,
    pub target: u32,
    pub address: u32,
    pub limit: u32,
}

pub struct Gpu {
    shared: Arc<Shared>,
    ram: Option<RamView>,
    pub ramin: Vec<u8>,
    regs: BTreeMap<u32, u32>,
    dma_get: u32,
    dma_put: u32,
    /// Object instance bound to each subchannel (0 = none).
    bound: [u32; 8],
    kelvin: BTreeMap<u32, u32>,
    color_base: u32,
}

impl Gpu {
    pub fn new(shared: Arc<Shared>) -> Gpu {
        Gpu {
            shared,
            ram: None,
            ramin: vec![0; RAMIN_SIZE],
            regs: BTreeMap::new(),
            dma_get: 0,
            dma_put: 0,
            bound: [0; 8],
            kelvin: BTreeMap::new(),
            color_base: 0,
        }
    }

    pub fn attach_ram(&mut self, ram: RamView) {
        self.ram = Some(ram);
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs.get(&off).copied().unwrap_or(0)
    }

    // ---- PRAMIN ----

    pub fn ramin_read(&self, off: u64, size: usize) -> u64 {
        let o = off as usize;
        match self.ramin.get(o..o + size.min(8)) {
            Some(b) => {
                let mut v = [0u8; 8];
                v[..b.len()].copy_from_slice(b);
                u64::from_le_bytes(v)
            }
            None => 0,
        }
    }

    pub fn ramin_write(&mut self, off: u64, size: usize, value: u64) {
        let o = off as usize;
        let n = size.min(8);
        if let Some(b) = self.ramin.get_mut(o..o + n) {
            b.copy_from_slice(&value.to_le_bytes()[..n]);
        }
    }

    fn ramin_u32(&self, at: u32) -> u32 {
        self.ramin_read(at as u64, 4) as u32
    }

    // ---- PFIFO ----

    pub fn pfifo_read(&mut self, off: u32) -> u32 {
        match off {
            CACHE1_DMA_PUT => self.dma_put,
            CACHE1_DMA_GET => self.dma_get,
            CACHE1_DMA_CTL => self.reg(off) | 0x8000_0000, // no cached translation: always valid
            _ => {
                if !known_pfifo(off) {
                    self.shared.note(false, off as u64);
                }
                self.reg(off)
            }
        }
    }

    pub fn pfifo_write(&mut self, off: u32, v: u32) {
        match off {
            CACHE1_DMA_PUT => self.dma_put = v & 0x1FFF_FFFC,
            CACHE1_DMA_GET => self.dma_get = v & 0x1FFF_FFFC,
            _ => {
                if !known_pfifo(off) {
                    self.shared.note(true, off as u64);
                }
                self.regs.insert(off, v);
            }
        }
        if matches!(off, CACHE1_PUSH0 | CACHE1_DMA_PUSH | CACHE1_PULL0) {
            self.run_pusher();
        }
    }

    /// The program wrote the channel's DMA_PUT in the USER area: new commands to run.
    pub fn user_write(&mut self, chid: u32, reg: u32, v: u32) {
        if chid != self.reg(CACHE1_PUSH1) & 0x1F {
            self.shared.gap(format!("USER write to channel {chid}, which is not the active channel"));
            return;
        }
        match reg {
            0x40 => {
                self.dma_put = v & 0x1FFF_FFFC;
                self.run_pusher();
            }
            0x44 => self.dma_get = v & 0x1FFF_FFFC,
            _ => self.shared.note(true, 0x80_0000 + reg as u64),
        }
    }

    pub fn user_read(&mut self, chid: u32, reg: u32) -> u32 {
        if chid != self.reg(CACHE1_PUSH1) & 0x1F {
            return 0;
        }
        match reg {
            0x40 => self.dma_put,
            0x44 => self.dma_get,
            _ => {
                self.shared.note(false, 0x80_0000 + reg as u64);
                0
            }
        }
    }

    fn ramht_lookup(&self, handle: u32) -> Option<u32> {
        let r = self.reg(PFIFO_RAMHT);
        let size_field = (r >> 16) & 3;
        let base = ((r & 0x1F0) >> 4) << 12;
        let entries = (1u32 << (size_field + 12)) / 8;
        let chid = self.reg(CACHE1_PUSH1) & 0x1F;
        let start = ramht_hash(handle, chid, size_field);
        for i in 0..8 {
            let slot = (start + i) % entries;
            let at = base + slot * 8;
            let (h, ctx) = (self.ramin_u32(at), self.ramin_u32(at + 4));
            if ctx & 0x8000_0000 != 0 && h == handle {
                return Some((ctx & 0xFFFF) << 4);
            }
        }
        None
    }

    pub fn dma_object(&self, instance: u32) -> DmaObject {
        let flags = self.ramin_u32(instance);
        let limit = self.ramin_u32(instance + 4);
        let frame = self.ramin_u32(instance + 8);
        DmaObject {
            class: flags & 0xFFF,
            target: (flags >> 16) & 3,
            address: (frame & 0xFFFF_F000) | (flags >> 20),
            limit,
        }
    }

    fn run_pusher(&mut self) {
        let enabled = self.reg(CACHE1_PUSH0) & 1 != 0
            && self.reg(CACHE1_DMA_PUSH) & 1 != 0
            && self.reg(CACHE1_PULL0) & 1 != 0;
        if !enabled || self.dma_get == self.dma_put {
            return;
        }
        let Some(ram) = self.ram else {
            self.shared.gap("command buffer read, but the GPU has no memory attached".into());
            return;
        };
        let obj = self.dma_object((self.reg(CACHE1_DMA_INSTANCE) & 0xFFFF) << 4);
        // The GPU's memory is the top of system RAM (shared memory architecture),
        // so a "video memory" address is simply a physical address.
        let base = (obj.address & 0x07FF_FFFF) as u64;
        let avail = (ram.len() as u64).saturating_sub(base - ram.base().min(base)).min(obj.limit as u64 + 1);
        let Some(buf) = ram.bytes(base, avail as usize) else {
            self.shared.log(format!("command buffer at {base:#x} is outside memory"));
            self.dma_get = self.dma_put;
            return;
        };
        match decode(buf, self.dma_get, self.dma_put, 1_000_000) {
            Ok(d) => {
                self.dma_get = d.get;
                for c in d.commands {
                    if let Command::Methods { subchannel, method, increasing, args } = c {
                        for (i, &a) in args.iter().enumerate() {
                            let m = if increasing { method + 4 * i as u32 } else { method };
                            self.method(subchannel, m, a);
                        }
                    }
                }
            }
            Err(e) => {
                self.shared.log(format!("command buffer error: {e}"));
                self.regs.insert(CACHE1_DMA_STATE, 0x2000_0000); // protection error, as the hardware reports
                self.dma_get = self.dma_put;
            }
        }
    }

    // ---- methods ----

    fn method(&mut self, sub: u8, m: u32, v: u32) {
        self.shared.count_method();
        if m == 0 {
            match self.ramht_lookup(v) {
                Some(inst) => self.bound[sub as usize & 7] = inst,
                None => self.shared.log(format!("subchannel {sub}: object handle {v:#x} is not in the hash table")),
            }
            return;
        }
        let inst = self.bound[sub as usize & 7];
        if inst == 0 {
            self.shared.log(format!("method {m:#x} on subchannel {sub} with no object bound"));
            return;
        }
        match self.ramin_u32(inst) & 0xFF {
            KELVIN_CLASS => self.kelvin_method(m, v),
            c => self.shared.gap(format!("method {m:#x} on object class {c:#x}")),
        }
    }

    fn dma_base_of(&self, handle: u32) -> Option<u32> {
        let inst = self.ramht_lookup(handle)?;
        Some(self.dma_object(inst).address & 0x07FF_FFFF)
    }

    fn kelvin_method(&mut self, m: u32, v: u32) {
        match m {
            NV097_NO_OPERATION => {}
            NV097_SET_CONTEXT_DMA_COLOR => match self.dma_base_of(v) {
                Some(b) => self.color_base = b,
                None => self.shared.log(format!("colour DMA handle {v:#x} is not in the hash table")),
            },
            NV097_SET_CONTEXT_DMA_NOTIFIES | NV097_SET_CONTEXT_DMA_ZETA => {
                self.kelvin.insert(m, v);
            }
            NV097_SET_SURFACE_CLIP_HORIZONTAL
            | NV097_SET_SURFACE_CLIP_VERTICAL
            | NV097_SET_SURFACE_FORMAT
            | NV097_SET_SURFACE_PITCH
            | NV097_SET_SURFACE_COLOR_OFFSET
            | NV097_SET_SURFACE_ZETA_OFFSET
            | NV097_SET_COLOR_CLEAR_VALUE
            | NV097_SET_CLEAR_RECT_HORIZONTAL
            | NV097_SET_CLEAR_RECT_VERTICAL => {
                self.kelvin.insert(m, v);
            }
            NV097_CLEAR_SURFACE => self.clear_surface(v),
            _ => self.shared.gap(format!("graphics method {m:#x}")),
        }
    }

    fn k(&self, m: u32) -> u32 {
        self.kelvin.get(&m).copied().unwrap_or(0)
    }

    fn clear_surface(&mut self, flags: u32) {
        if flags & 3 != 0 {
            self.shared.gap("clearing the depth/stencil buffer".into());
        }
        let colour_mask = flags & 0xF0;
        if colour_mask == 0 {
            return;
        }
        let Some(ram) = self.ram else { return };
        let bpp = match self.k(NV097_SET_SURFACE_FORMAT) & 0xF {
            1..=3 | 0xA => 2usize,
            4..=8 => 4,
            9 => 1,
            f => {
                self.shared.gap(format!("clearing a colour surface of format {f:#x}"));
                return;
            }
        };
        let clip_h = self.k(NV097_SET_SURFACE_CLIP_HORIZONTAL);
        let clip_v = self.k(NV097_SET_SURFACE_CLIP_VERTICAL);
        let (cx, cw) = (clip_h & 0xFFFF, clip_h >> 16);
        let (cy, ch) = (clip_v & 0xFFFF, clip_v >> 16);
        let rh = self.k(NV097_SET_CLEAR_RECT_HORIZONTAL);
        let rv = self.k(NV097_SET_CLEAR_RECT_VERTICAL);
        let (x0, x1) = (rh & 0xFFFF, (rh >> 16).min(cw.saturating_sub(1)));
        let (y0, y1) = (rv & 0xFFFF, (rv >> 16).min(ch.saturating_sub(1)));
        if cw == 0 || ch == 0 || x0 > x1 || y0 > y1 {
            return; // empty rectangle
        }
        if bpp != 4 && colour_mask != 0xF0 {
            self.shared.gap("a partial colour mask on a 16-bit surface".into());
            return;
        }
        let pitch = (self.k(NV097_SET_SURFACE_PITCH) & 0xFFFF) as u64;
        let origin = self.color_base as u64 + self.k(NV097_SET_SURFACE_COLOR_OFFSET) as u64;
        let value = self.k(NV097_SET_COLOR_CLEAR_VALUE).to_le_bytes();
        // memory order is B, G, R, A, and mask bits 4..7 are R, G, B, A
        let lane_on = |lane: usize| match lane {
            0 => colour_mask & 0x40 != 0,
            1 => colour_mask & 0x20 != 0,
            2 => colour_mask & 0x10 != 0,
            _ => colour_mask & 0x80 != 0,
        };
        let count = (x1 - x0 + 1) as usize;
        let mut row = Vec::with_capacity(count * bpp);
        for y in y0..=y1 {
            let at = origin + (cy + y) as u64 * pitch + (cx + x0) as u64 * bpp as u64;
            if colour_mask == 0xF0 {
                row.clear();
                for _ in 0..count {
                    row.extend_from_slice(&value[..bpp]);
                }
                if !ram.write(at, &row) {
                    self.shared.log(format!("clear outside memory at {at:#x}"));
                    return;
                }
            } else {
                let Some(old) = ram.bytes(at, count * 4) else {
                    self.shared.log(format!("clear outside memory at {at:#x}"));
                    return;
                };
                row.clear();
                for px in old.chunks_exact(4) {
                    for lane in 0..4 {
                        row.push(if lane_on(lane) { value[lane] } else { px[lane] });
                    }
                }
                ram.write(at, &row);
            }
        }
        self.shared.count_clear();
    }
}

#[cfg(test)]
mod tests {
    use crate::device::{Nv2a, Shared};
    use super::*;
    use ferrox_machine::{Mmio, PhysMem};

    const FB: u32 = 0x10_0000;
    const PB: u32 = 0x20_0000;
    const KELVIN: u32 = 0xB0B0;
    const COLOR_DMA: u32 = 0xC0DE;

    fn setup() -> (Nv2a, Arc<Shared>, PhysMem) {
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 << 20).unwrap();
        let (mut gpu, shared) = Nv2a::new();
        gpu.attach_ram(mem.ram_view(0).unwrap());
        (gpu, shared, mem)
    }

    fn put_ramin(g: &mut Nv2a, at: u64, v: u32) {
        g.write(0x70_0000 + at, 4, v as u64);
    }

    /// RAMHT at 0 (4 KiB), pushbuffer DMA object at 0x1000, colour DMA at 0x1010, Kelvin at 0x1020.
    fn init(g: &mut Nv2a) {
        let enter = |g: &mut Nv2a, h: u32, inst: u32| {
            let slot = ramht_hash(h, 0, 0) as u64 * 8;
            put_ramin(g, slot, h);
            put_ramin(g, slot + 4, 0x8000_0000 | (inst >> 4));
        };
        enter(g, KELVIN, 0x1020);
        enter(g, COLOR_DMA, 0x1010);
        for (inst, base, limit) in [(0x1000u32, PB, 0xFFFFu32), (0x1010, 0, 0x3F_FFFF)] {
            put_ramin(g, inst as u64, 0x3D);
            put_ramin(g, inst as u64 + 4, limit);
            put_ramin(g, inst as u64 + 8, base | 3);
        }
        put_ramin(g, 0x1020, KELVIN_CLASS);
        for (reg, v) in [
            (PFIFO_RAMHT, 0),
            (CACHE1_PUSH1, 0x100),
            (CACHE1_DMA_INSTANCE, 0x100),
            (CACHE1_PUSH0, 1),
            (CACHE1_DMA_PUSH, 1),
            (CACHE1_PULL0, 1),
        ] {
            g.write(reg as u64, 4, v as u64);
        }
    }

    fn kick(g: &mut Nv2a, mem: &mut PhysMem, words: &[u32]) {
        let view = mem.ram_view(0).unwrap();
        for (i, w) in words.iter().enumerate() {
            view.write_u32(PB as u64 + 4 * i as u64, *w);
        }
        g.write(0x80_0040, 4, 4 * words.len() as u64);
    }

    fn hdr(method: u32, count: u32) -> u32 {
        (count << 18) | method
    }

    fn pixel(mem: &mut PhysMem, x: u32, y: u32) -> u32 {
        mem.ram_view(0).unwrap().read_u32((FB + y * 64 * 4 + x * 4) as u64).unwrap()
    }

    #[test]
    fn hash_matches_xemu_formula() {
        // 4 KiB table: 11-bit chunks xored, channel folded in at bit 7
        assert_eq!(ramht_hash(0xB0B0, 0, 0), (0xB0B0 & 0x7FF) ^ (0xB0B0 >> 11));
        assert_eq!(ramht_hash(5, 3, 0), 5 ^ (3 << 7));
    }

    #[test]
    fn dma_object_fields_decode() {
        let mut g = Gpu::new(Arc::new(Shared::default()));
        g.ramin_write(0x40, 4, 0x0010_003D | (2 << 16)); // adjust 1, target PCI, class 3D
        g.ramin_write(0x44, 4, 0xFFF);
        g.ramin_write(0x48, 4, 0x0123_4000 | 3);
        let d = g.dma_object(0x40);
        assert_eq!(d, DmaObject { class: 0x3D, target: 2, address: 0x0123_4001, limit: 0xFFF });
    }

    #[test]
    fn clears_a_rectangle_of_the_surface() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        kick(
            &mut g,
            &mut mem,
            &[
                hdr(0, 1), KELVIN,
                hdr(0x194, 1), COLOR_DMA,
                hdr(0x200, 2), 64 << 16, 32 << 16,
                hdr(0x208, 1), 0x108,
                hdr(0x20C, 2), 256, FB,
                hdr(0x1D98, 2), 10 | (19 << 16), 4 | (7 << 16),
                hdr(0x1D90, 1), 0xFF11_2233,
                hdr(0x1D94, 1), 0xF0,
            ],
        );
        assert_eq!(pixel(&mut mem, 10, 4), 0xFF11_2233);
        assert_eq!(pixel(&mut mem, 19, 7), 0xFF11_2233);
        assert_eq!(pixel(&mut mem, 9, 4), 0, "left of the rectangle");
        assert_eq!(pixel(&mut mem, 20, 4), 0, "right of the rectangle");
        assert_eq!(pixel(&mut mem, 10, 3), 0, "above");
        assert_eq!(pixel(&mut mem, 10, 8), 0, "below");
        assert_eq!(shared.gpu_counts().1, 1);
        assert_eq!(g.read(0x80_0044, 4), 4 * 19, "the GPU consumed every word");
        assert!(shared.take_log().is_empty());
        assert!(shared.gaps().is_empty(), "{:?}", shared.gaps());
    }

    #[test]
    fn colour_mask_keeps_the_other_channels() {
        let (mut g, _, mut mem) = setup();
        init(&mut g);
        mem.ram_view(0).unwrap().write_u32(FB as u64, 0xAABB_CCDD);
        kick(
            &mut g,
            &mut mem,
            &[
                hdr(0, 1), KELVIN, hdr(0x194, 1), COLOR_DMA, hdr(0x200, 2), 64 << 16, 32 << 16,
                hdr(0x208, 1), 0x108, hdr(0x20C, 2), 256, FB,
                hdr(0x1D98, 2), 0, 0, hdr(0x1D90, 1), 0x1122_3344,
                hdr(0x1D94, 1), 0x10 | 0x80, // red and alpha only
            ],
        );
        // memory order B G R A: red is byte 2, alpha byte 3
        assert_eq!(pixel(&mut mem, 0, 0), 0x11 << 24 | 0x22 << 16 | 0xCC << 8 | 0xDD);
    }

    #[test]
    fn unknown_methods_and_bad_handles_are_reported_not_fatal() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        kick(&mut g, &mut mem, &[hdr(0x100, 1), 0, hdr(0, 1), 0x1234, hdr(0, 1), KELVIN, hdr(0x1808, 1), 7]);
        let log = shared.take_log();
        assert!(log.iter().any(|l| l.contains("no object bound")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("0x1234")), "{log:?}");
        assert!(shared.gaps().iter().any(|(k, _)| k.contains("0x1808")), "{:?}", shared.gaps());
    }

    #[test]
    fn nothing_runs_until_the_fifo_is_enabled() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        g.write(CACHE1_PULL0 as u64, 4, 0);
        kick(&mut g, &mut mem, &[hdr(0, 1), KELVIN]);
        assert_eq!(shared.gpu_counts().0, 0);
        assert_eq!(g.read(0x80_0044, 4), 0);
        g.write(CACHE1_PULL0 as u64, 4, 1); // enabling it runs what was queued
        assert_eq!(shared.gpu_counts().0, 1);
    }

    #[test]
    fn instance_memory_reads_back_at_every_size() {
        let (mut g, _, _m) = setup();
        g.write(0x70_0010, 4, 0x1122_3344);
        assert_eq!(g.read(0x70_0010, 4), 0x1122_3344);
        assert_eq!(g.read(0x70_0011, 1), 0x33);
        assert_eq!(g.read(0x70_0012, 2), 0x1122);
    }
}
