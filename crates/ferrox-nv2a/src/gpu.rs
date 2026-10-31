//! The GPU's command front end: instance memory (PRAMIN), the object hash table
//! (RAMHT), DMA objects, the PFIFO pusher that fetches the command buffer and the
//! graphics engine's first methods (surface setup and clears).
//!
//! Structure and numbers follow xemu's `pfifo.c`, `nv2a.c` and `nv2a_regs.h`.
//! Only what a program has been seen to need is implemented; anything else the
//! engine is asked to do is recorded in `Shared::gaps` so it shows up in the
//! front end's report instead of silently doing nothing.

use crate::combiner::Combiner;
use crate::device::Shared;
use crate::raster::{self, Depth, Surface, Vertex};
use crate::texture::{self, Address, Mip, Texture};
use crate::vsh::{self, Program};
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
/// Both report the command queue as empty (bit 4): commands run the moment they are submitted.
pub const PFIFO_RUNOUT_STATUS: u32 = 0x2400;
pub const CACHE1_STATUS: u32 = 0x3214;
const QUEUE_EMPTY: u32 = 1 << 4;

pub fn is_pfifo(off: u32) -> bool {
    (0x2000..0x4000).contains(&off)
}

fn known_pfifo(off: u32) -> bool {
    matches!(
        off,
        PFIFO_INTR_0 | PFIFO_INTR_EN_0 | PFIFO_RAMHT | CACHE1_PUSH0 | CACHE1_PUSH1 | CACHE1_DMA_PUSH
            | CACHE1_DMA_STATE | CACHE1_DMA_INSTANCE | CACHE1_DMA_CTL | CACHE1_DMA_PUT | CACHE1_DMA_GET
            | CACHE1_PULL0 | PFIFO_RUNOUT_STATUS | CACHE1_STATUS
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
const NV097_SET_BEGIN_END: u32 = 0x17FC;
const NV097_SET_VERTEX_DATA2F_M: u32 = 0x1880;
const NV097_SET_VERTEX_DATA4UB: u32 = 0x1940;
const NV097_SET_VERTEX_DATA4F_M: u32 = 0x1A00;
const NV097_SET_CONTEXT_DMA_VERTEX_A: u32 = 0x19C;
const NV097_SET_CONTEXT_DMA_VERTEX_B: u32 = 0x1A0;
const NV097_SET_VERTEX_DATA_ARRAY_OFFSET: u32 = 0x1720;
const NV097_SET_VERTEX_DATA_ARRAY_FORMAT: u32 = 0x1760;
const NV097_ARRAY_ELEMENT16: u32 = 0x1800;
const NV097_ARRAY_ELEMENT32: u32 = 0x1808;
const NV097_DRAW_ARRAYS: u32 = 0x1810;
const NV097_SET_TRANSFORM_PROGRAM: u32 = 0xB00;
const NV097_SET_TRANSFORM_CONSTANT: u32 = 0xB80;
const NV097_SET_TRANSFORM_EXECUTION_MODE: u32 = 0x1E94;
const NV097_SET_TRANSFORM_PROGRAM_CXT_WRITE_EN: u32 = 0x1E98;
const NV097_SET_TRANSFORM_PROGRAM_LOAD: u32 = 0x1E9C;
const NV097_SET_TRANSFORM_PROGRAM_START: u32 = 0x1EA0;
const NV097_SET_TRANSFORM_CONSTANT_LOAD: u32 = 0x1EA4;
const EXEC_MODE_PROGRAM: u32 = 2;
const NV097_SET_DEPTH_TEST_ENABLE: u32 = 0x30C;
const NV097_SET_DEPTH_FUNC: u32 = 0x354;
const NV097_SET_DEPTH_MASK: u32 = 0x35C;
const NV097_SET_CLIP_MAX: u32 = 0x398;
const NV097_SET_ZSTENCIL_CLEAR_VALUE: u32 = 0x1D8C;
const NV097_SET_CONTEXT_DMA_A: u32 = 0x184;
const NV097_SET_CONTEXT_DMA_B: u32 = 0x188;
const NV097_SET_TEXTURE_BASE: u32 = 0x1B00; // four stages of 0x40 bytes
const NV097_SET_SHADER_STAGE_PROGRAM: u32 = 0x1E70;
const NV097_SET_ALPHA_TEST_ENABLE: u32 = 0x300;
const NV097_SET_BLEND_ENABLE: u32 = 0x304;
const NV097_SET_ALPHA_FUNC: u32 = 0x33C;
const NV097_SET_ALPHA_REF: u32 = 0x340;
const NV097_SET_BLEND_FUNC_SFACTOR: u32 = 0x344;
const NV097_SET_BLEND_FUNC_DFACTOR: u32 = 0x348;
const NV097_SET_BLEND_COLOR: u32 = 0x34C;
const NV097_SET_BLEND_EQUATION: u32 = 0x350;
const NV097_SET_COLOR_MASK: u32 = 0x358;
const ATTR_DIFFUSE: usize = 3;
const ATTR_SPECULAR: usize = 4;
const ATTR_TEXCOORD0: usize = 9;
const PRIM_TRIANGLES: u32 = 5;
const PRIM_TRIANGLE_STRIP: u32 = 6;
const PRIM_TRIANGLE_FAN: u32 = 7;
const PRIM_QUADS: u32 = 8;
const PRIM_QUAD_STRIP: u32 = 9;
const PRIM_POLYGON: u32 = 10;
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
    zeta_base: u32,
    /// Current value of each of the 16 vertex attributes (position is 0, diffuse 3).
    attrs: [[f32; 4]; 16],
    /// Primitive type between Begin and End, 0 outside.
    prim: u32,
    verts: Vec<Vertex>,
    vsh: Program,
    prog_load: usize,
    const_load: usize,
    vertex_base: [u32; 2],
    texture_base: [u32; 2],
    comb: Combiner,
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
            zeta_base: 0,
            attrs: Self::initial_attrs(),
            prim: 0,
            verts: Vec::new(),
            vsh: Program::default(),
            prog_load: 0,
            const_load: 0,
            vertex_base: [0; 2],
            texture_base: [0; 2],
            comb: Combiner::default(),
        }
    }

    fn initial_attrs() -> [[f32; 4]; 16] {
        let mut a = [[0.0, 0.0, 0.0, 1.0]; 16];
        a[ATTR_DIFFUSE] = [1.0; 4]; // white until a program sets a colour
        a
    }

    pub fn attach_ram(&mut self, ram: RamView) {
        self.ram = Some(ram);
    }

    fn reg(&self, off: u32) -> u32 {
        self.regs.get(&off).copied().unwrap_or(0)
    }

    // ---- PRAMIN ----

    pub fn ramin_read(&self, off: u64, size: usize) -> u64 {
        let base = self.shared.ramin_base();
        if base != 0 {
            let bytes = self.ram.as_ref().and_then(|r| r.bytes(base as u64 + off, size.min(8)));
            let mut v = [0u8; 8];
            if let Some(b) = bytes {
                v[..b.len()].copy_from_slice(b);
            }
            return u64::from_le_bytes(v);
        }
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
        let n = size.min(8);
        let base = self.shared.ramin_base();
        if base != 0 {
            if let Some(r) = &self.ram {
                r.write(base as u64 + off, &value.to_le_bytes()[..n]);
            }
            return;
        }
        let o = off as usize;
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
            PFIFO_RUNOUT_STATUS | CACHE1_STATUS => QUEUE_EMPTY,
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
            NV097_SET_CONTEXT_DMA_ZETA => match self.dma_base_of(v) {
                Some(b) => self.zeta_base = b,
                None => self.shared.log(format!("depth DMA handle {v:#x} is not in the hash table")),
            },
            NV097_SET_CONTEXT_DMA_NOTIFIES
            | NV097_SET_DEPTH_TEST_ENABLE
            | NV097_SET_DEPTH_FUNC
            | NV097_SET_DEPTH_MASK
            | NV097_SET_CLIP_MAX
            | NV097_SET_ALPHA_TEST_ENABLE
            | NV097_SET_BLEND_ENABLE
            | NV097_SET_ALPHA_FUNC
            | NV097_SET_ALPHA_REF
            | NV097_SET_BLEND_FUNC_SFACTOR
            | NV097_SET_BLEND_FUNC_DFACTOR
            | NV097_SET_BLEND_COLOR
            | NV097_SET_BLEND_EQUATION
            | NV097_SET_COLOR_MASK
            | NV097_SET_ZSTENCIL_CLEAR_VALUE => {
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
            NV097_SET_BEGIN_END => self.begin_end(v),
            NV097_SET_CONTEXT_DMA_A | NV097_SET_CONTEXT_DMA_B => match self.dma_base_of(v) {
                Some(b) => self.texture_base[((m - NV097_SET_CONTEXT_DMA_A) / 4) as usize] = b,
                None => self.shared.log(format!("texture DMA handle {v:#x} is not in the hash table")),
            },
            NV097_SET_TEXTURE_BASE..=0x1BFF => {
                self.kelvin.insert(m, v);
            }
            NV097_SET_SHADER_STAGE_PROGRAM => {
                self.kelvin.insert(m, v);
            }
            _ if self.comb.write(m, v) => {}
            NV097_SET_TRANSFORM_PROGRAM..=0xB7C => {
                let slot = ((m - NV097_SET_TRANSFORM_PROGRAM) / 4) as usize;
                if self.prog_load < vsh::MAX_INSTRUCTIONS {
                    self.vsh.slots[self.prog_load][slot % 4] = v;
                }
                if slot % 4 == 3 {
                    self.prog_load += 1;
                }
            }
            NV097_SET_TRANSFORM_CONSTANT..=0xBFC => {
                let slot = ((m - NV097_SET_TRANSFORM_CONSTANT) / 4) as usize;
                if self.const_load < vsh::CONSTANTS {
                    self.vsh.constants[self.const_load][slot % 4] = f32::from_bits(v);
                }
                if slot % 4 == 3 {
                    self.const_load += 1;
                }
            }
            NV097_SET_TRANSFORM_PROGRAM_LOAD => self.prog_load = v as usize,
            NV097_SET_TRANSFORM_CONSTANT_LOAD => self.const_load = v as usize,
            NV097_SET_TRANSFORM_EXECUTION_MODE
            | NV097_SET_TRANSFORM_PROGRAM_START
            | NV097_SET_TRANSFORM_PROGRAM_CXT_WRITE_EN => {
                self.kelvin.insert(m, v);
            }
            NV097_SET_CONTEXT_DMA_VERTEX_A | NV097_SET_CONTEXT_DMA_VERTEX_B => match self.dma_base_of(v) {
                Some(b) => self.vertex_base[((m - NV097_SET_CONTEXT_DMA_VERTEX_A) / 4) as usize] = b,
                None => self.shared.log(format!("vertex DMA handle {v:#x} is not in the hash table")),
            },
            NV097_SET_VERTEX_DATA_ARRAY_OFFSET..=0x175C | NV097_SET_VERTEX_DATA_ARRAY_FORMAT..=0x179C => {
                self.kelvin.insert(m, v);
            }
            NV097_DRAW_ARRAYS => {
                let (start, count) = (v & 0x00FF_FFFF, (v >> 24) + 1);
                for i in start..start + count {
                    self.fetch_and_emit(i);
                }
            }
            NV097_ARRAY_ELEMENT16 => {
                self.fetch_and_emit(v & 0xFFFF);
                self.fetch_and_emit(v >> 16);
            }
            NV097_ARRAY_ELEMENT32 => self.fetch_and_emit(v),
            NV097_SET_VERTEX_DATA2F_M..=0x18FF => {
                let slot = ((m - NV097_SET_VERTEX_DATA2F_M) / 4) as usize;
                let (attr, comp) = (slot / 2, slot % 2);
                self.attrs[attr][comp] = f32::from_bits(v);
                if comp == 1 {
                    self.attrs[attr][2] = 0.0;
                    self.attrs[attr][3] = 1.0;
                    if attr == 0 {
                        self.emit_vertex();
                    }
                }
            }
            NV097_SET_VERTEX_DATA4UB..=0x197F => {
                let attr = ((m - NV097_SET_VERTEX_DATA4UB) / 4) as usize;
                // packed as the Direct3D colour A, R, G, B
                self.attrs[attr] = [
                    ((v >> 16) & 0xFF) as f32 / 255.0,
                    ((v >> 8) & 0xFF) as f32 / 255.0,
                    (v & 0xFF) as f32 / 255.0,
                    (v >> 24) as f32 / 255.0,
                ];
                if attr == 0 {
                    self.emit_vertex();
                }
            }
            NV097_SET_VERTEX_DATA4F_M..=0x1BFF => {
                let slot = ((m - NV097_SET_VERTEX_DATA4F_M) / 4) as usize;
                let (attr, comp) = (slot / 4, slot % 4);
                self.attrs[attr][comp] = f32::from_bits(v);
                if attr == 0 && comp == 3 {
                    self.emit_vertex();
                }
            }
            _ => self.shared.gap(format!("graphics method {m:#x}")),
        }
    }

    fn k(&self, m: u32) -> u32 {
        self.kelvin.get(&m).copied().unwrap_or(0)
    }

    /// The colour surface as currently set up, if it is one the renderer can draw to.
    fn surface(&self) -> Option<Surface> {
        let ram = self.ram?;
        if (self.k(NV097_SET_SURFACE_FORMAT) >> 8) & 0xF == 2 {
            self.shared.gap("swizzled render targets".into());
            return None;
        }
        let clip_h = self.k(NV097_SET_SURFACE_CLIP_HORIZONTAL);
        let clip_v = self.k(NV097_SET_SURFACE_CLIP_VERTICAL);
        Some(Surface {
            ram,
            origin: self.color_base as u64 + self.k(NV097_SET_SURFACE_COLOR_OFFSET) as u64,
            pitch: (self.k(NV097_SET_SURFACE_PITCH) & 0xFFFF) as u64,
            format: self.k(NV097_SET_SURFACE_FORMAT) & 0xF,
            clip: (clip_h & 0xFFFF, clip_v & 0xFFFF, clip_h >> 16, clip_v >> 16),
            ops: raster::PixelOps::default(),
        })
    }

    /// Alpha test, blending and the colour write mask as currently set.
    fn pixel_ops(&self) -> raster::PixelOps {
        use raster::{AlphaTest, Blend, Equation, Factor};
        let mut ops = raster::PixelOps::default();
        if let Some(&m) = self.kelvin.get(&NV097_SET_COLOR_MASK) {
            ops.mask = [m >> 16 & 1 != 0, m >> 8 & 1 != 0, m & 1 != 0, m >> 24 & 1 != 0];
        }
        if self.k(NV097_SET_ALPHA_TEST_ENABLE) != 0 {
            ops.alpha_test = Some(AlphaTest {
                func: self.k(NV097_SET_ALPHA_FUNC) & 0xF,
                reference: self.k(NV097_SET_ALPHA_REF) & 0xFF,
            });
        }
        if self.k(NV097_SET_BLEND_ENABLE) != 0 {
            // before a game sets them, the reset state is source * 1 + destination * 0
            let factor = |m: u32, default: Factor| match self.kelvin.get(&m) {
                None => Some(default),
                Some(&v) => {
                    let f = Factor::from_method(v);
                    if f.is_none() {
                        self.shared.gap(format!("blend factor {v:#x}"));
                    }
                    f
                }
            };
            let equation = match self.kelvin.get(&NV097_SET_BLEND_EQUATION) {
                None => Some(Equation::Add),
                Some(&v) => {
                    let e = Equation::from_method(v);
                    if e.is_none() {
                        self.shared.gap(format!("blend equation {v:#x}"));
                    }
                    e
                }
            };
            let c = self.k(NV097_SET_BLEND_COLOR);
            if let (Some(src), Some(dst), Some(equation)) = (
                factor(NV097_SET_BLEND_FUNC_SFACTOR, Factor::One),
                factor(NV097_SET_BLEND_FUNC_DFACTOR, Factor::Zero),
                equation,
            ) {
                ops.blend = Some(Blend {
                    src,
                    dst,
                    equation,
                    constant: [
                        (c >> 16 & 0xFF) as f32 / 255.0,
                        (c >> 8 & 0xFF) as f32 / 255.0,
                        (c & 0xFF) as f32 / 255.0,
                        (c >> 24) as f32 / 255.0,
                    ],
                });
            }
        }
        ops
    }

    /// The depth buffer, if one is set up (and depth testing is on, for `testing`).
    fn depth(&self, testing: bool) -> Option<Depth> {
        let format = (self.k(NV097_SET_SURFACE_FORMAT) >> 4) & 0xF;
        if format == 0 || (testing && self.k(NV097_SET_DEPTH_TEST_ENABLE) == 0) {
            return None;
        }
        if format > 2 {
            self.shared.gap(format!("depth buffer format {format:#x}"));
            return None;
        }
        let clip_max = f32::from_bits(self.k(NV097_SET_CLIP_MAX));
        Some(Depth {
            ram: self.ram?,
            origin: self.zeta_base as u64 + self.k(NV097_SET_SURFACE_ZETA_OFFSET) as u64,
            pitch: (self.k(NV097_SET_SURFACE_PITCH) >> 16) as u64,
            format,
            func: self.k(NV097_SET_DEPTH_FUNC) & 7,
            write: self.k(NV097_SET_DEPTH_MASK) != 0,
            clip_max: if clip_max > 0.0 { clip_max } else { 0xFF_FFFF as f32 },
        })
    }

    /// Texture stage 0 as set up, if it is enabled and the draw should sample it.
    fn texture0(&self) -> Option<Texture> {
        let mode = self.k(NV097_SET_SHADER_STAGE_PROGRAM) & 0x1F;
        let control0 = self.k(NV097_SET_TEXTURE_BASE + 0x0C);
        if mode == 0 || control0 & (1 << 30) == 0 {
            return None;
        }
        if mode != 1 {
            self.shared.gap(format!("pixel shader stage mode {mode:#x}"));
            return None;
        }
        for stage in 1..4u32 {
            if self.k(NV097_SET_SHADER_STAGE_PROGRAM) >> (5 * stage) & 0x1F != 0 {
                self.shared.gap(format!("texture stage {stage}"));
                break;
            }
        }
        let base = |r: u32| self.k(NV097_SET_TEXTURE_BASE + r);
        let format = base(0x04);
        let colour = (format >> 8) & 0xFF;
        let Some((layout, _)) = texture::describe(colour) else {
            self.shared.gap(format!("texture format {colour:#x}"));
            return None;
        };
        if (format >> 2) & 1 != 0 {
            self.shared.gap("cube map textures".into());
            return None;
        }
        if (format >> 4) & 0xF != 2 {
            self.shared.gap(format!("{}-dimensional textures", (format >> 4) & 0xF));
            return None;
        }
        let (width, height) = if layout == texture::Layout::Swizzled || layout == texture::Layout::Blocks {
            (1u32 << ((format >> 20) & 0xF), 1u32 << ((format >> 24) & 0xF))
        } else {
            (base(0x1C) >> 16, base(0x1C) & 0xFFFF)
        };
        if width == 0 || height == 0 {
            return None;
        }
        let dma = match format & 3 {
            1 => 0,
            2 => 1,
            _ => {
                self.shared.gap("textures read from a context DMA other than A or B".into());
                return None;
            }
        };
        let filter = base(0x14);
        let (min, mag) = ((filter >> 16) & 0xFF, (filter >> 24) & 0xF);
        if (format >> 16) & 0xF > 1 && min >= 3 {
            self.shared.gap("mipmap level selection (level 0 is used)".into());
        }
        let addr = base(0x08);
        Some(Texture {
            ram: self.ram?,
            addr: self.texture_base[dma] as u64 + base(0x00) as u64,
            format: colour,
            width,
            height,
            pitch: base(0x10) >> 16,
            wrap_u: Address::from_field(addr & 7),
            wrap_v: Address::from_field((addr >> 8) & 7),
            bilinear: mag == 2 || mag == 4,
            min_linear: matches!(min, 2 | 4 | 6 | 7),
            mip: Mip::Off, // level selection is wired up with the per-pixel level of detail
            levels: 1,
            lod_bias: 0.0,
            border: {
                let c = base(0x24);
                [(c >> 16 & 0xFF) as f32 / 255.0, (c >> 8 & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0, (c >> 24) as f32 / 255.0]
            },
        })
    }

    fn begin_end(&mut self, op: u32) {
        if op != 0 {
            self.prim = op;
            self.verts.clear();
            return;
        }
        let verts = std::mem::take(&mut self.verts);
        let prim = std::mem::replace(&mut self.prim, 0);
        let mut tris: Vec<[usize; 3]> = Vec::new();
        let n = verts.len();
        match prim {
            PRIM_TRIANGLES => tris.extend((0..n / 3).map(|i| [3 * i, 3 * i + 1, 3 * i + 2])),
            PRIM_TRIANGLE_STRIP => tris.extend((0..n.saturating_sub(2)).map(|i| {
                if i % 2 == 0 { [i, i + 1, i + 2] } else { [i + 1, i, i + 2] }
            })),
            PRIM_TRIANGLE_FAN | PRIM_POLYGON => tris.extend((1..n.saturating_sub(1)).map(|i| [0, i, i + 1])),
            PRIM_QUADS => {
                for q in 0..n / 4 {
                    let b = 4 * q;
                    tris.push([b, b + 1, b + 2]);
                    tris.push([b, b + 2, b + 3]);
                }
            }
            PRIM_QUAD_STRIP => {
                for q in 0..n.saturating_sub(2) / 2 {
                    let b = 2 * q;
                    tris.push([b, b + 1, b + 3]);
                    tris.push([b, b + 3, b + 2]);
                }
            }
            0 => return,
            p => {
                self.shared.gap(format!("drawing primitive type {p}"));
                return;
            }
        }
        if tris.is_empty() {
            return;
        }
        let Some(mut surface) = self.surface() else { return };
        surface.ops = self.pixel_ops();
        if raster::bytes_per_pixel(surface.format).is_none() {
            self.shared.gap(format!("drawing into a colour surface of format {:#x}", surface.format));
            return;
        }
        if self.k(NV097_SET_TRANSFORM_EXECUTION_MODE) & 3 != EXEC_MODE_PROGRAM {
            self.shared.gap("fixed-function transform not emulated: positions are used as screen coordinates".into());
        }
        let depth = self.depth(true);
        let texture = self.texture0();
        let comb = if self.comb.programmed() {
            for g in self.comb.gaps() {
                self.shared.gap(g);
            }
            Some(&self.comb)
        } else {
            if texture.is_some() {
                self.shared.gap("register combiners were never programmed: the texture is multiplied with the diffuse colour".into());
            }
            None
        };
        for t in tris {
            raster::fill_triangle(&surface, depth.as_ref(), texture.as_ref(), comb, [verts[t[0]], verts[t[1]], verts[t[2]]]);
            self.shared.count_triangle();
        }
    }

    /// Reads vertex `index` from the arrays the program set up, then emits it.
    fn fetch_and_emit(&mut self, index: u32) {
        let Some(ram) = self.ram else { return };
        for a in 0..16u32 {
            let fmt = self.k(NV097_SET_VERTEX_DATA_ARRAY_FORMAT + 4 * a);
            let (ty, size, stride) = (fmt & 0xF, ((fmt >> 4) & 0xF) as usize, (fmt >> 8) as u64);
            if size == 0 {
                continue;
            }
            let off = self.k(NV097_SET_VERTEX_DATA_ARRAY_OFFSET + 4 * a);
            let base = self.vertex_base[(off >> 31) as usize] as u64 + (off & 0x7FFF_FFFF) as u64;
            let at = base + index as u64 * stride;
            let mut val = [0.0, 0.0, 0.0, 1.0];
            match ty {
                2 => {
                    for (k, slot) in val.iter_mut().enumerate().take(size.min(4)) {
                        *slot = f32::from_bits(ram.read_u32(at + 4 * k as u64).unwrap_or(0));
                    }
                }
                0 | 4 if size == 4 => {
                    let Some(b) = ram.bytes(at, 4) else { continue };
                    // UB_D3D is memory order B, G, R, A; UB_OGL is R, G, B, A
                    let (r, g, bl, al) = if ty == 0 { (b[2], b[1], b[0], b[3]) } else { (b[0], b[1], b[2], b[3]) };
                    val = [r as f32 / 255.0, g as f32 / 255.0, bl as f32 / 255.0, al as f32 / 255.0];
                }
                t => {
                    self.shared.gap(format!("vertex array of type {t} with {size} components"));
                    continue;
                }
            }
            self.attrs[a as usize] = val;
        }
        self.emit_vertex();
    }

    fn emit_vertex(&mut self) {
        if self.prim == 0 {
            return;
        }
        if self.k(NV097_SET_TRANSFORM_EXECUTION_MODE) & 3 == EXEC_MODE_PROGRAM {
            let out = self.vsh.run(self.k(NV097_SET_TRANSFORM_PROGRAM_START) as usize, &self.attrs);
            let (p, c, sp) = (out[vsh::O_POS], out[vsh::O_D0], out[vsh::O_D1]);
            // the program has already applied the viewport: oPos is in pixels
            let w = if p[3] > 0.0 { p[3] } else { 1.0 };
            self.verts.push(Vertex { x: p[0], y: p[1], z: p[2], w, color: c, spec: sp, tex: out[vsh::O_T0] });
        } else {
            let (p, c) = (self.attrs[0], self.attrs[ATTR_DIFFUSE]);
            let sp = self.attrs[ATTR_SPECULAR];
            self.verts.push(Vertex { x: p[0], y: p[1], z: p[2], w: 1.0, color: c, spec: sp, tex: self.attrs[ATTR_TEXCOORD0] });
        }
    }

    /// The clear rectangle clipped to the surface: (x0, x1, y0, y1) inclusive, plus the clip origin.
    fn clear_rect(&self) -> Option<(u32, u32, u32, u32, u32, u32)> {
        let clip_h = self.k(NV097_SET_SURFACE_CLIP_HORIZONTAL);
        let clip_v = self.k(NV097_SET_SURFACE_CLIP_VERTICAL);
        let (cx, cw) = (clip_h & 0xFFFF, clip_h >> 16);
        let (cy, ch) = (clip_v & 0xFFFF, clip_v >> 16);
        let rh = self.k(NV097_SET_CLEAR_RECT_HORIZONTAL);
        let rv = self.k(NV097_SET_CLEAR_RECT_VERTICAL);
        let (x0, x1) = (rh & 0xFFFF, (rh >> 16).min(cw.saturating_sub(1)));
        let (y0, y1) = (rv & 0xFFFF, (rv >> 16).min(ch.saturating_sub(1)));
        (cw != 0 && ch != 0 && x0 <= x1 && y0 <= y1).then_some((x0, x1, y0, y1, cx, cy))
    }

    fn clear_zeta(&mut self, flags: u32) {
        if (self.k(NV097_SET_SURFACE_FORMAT) >> 8) & 0xF == 2 {
            self.shared.gap("swizzled render targets".into());
            return;
        }
        let Some(d) = self.depth(false) else { return };
        let Some((x0, x1, y0, y1, cx, cy)) = self.clear_rect() else { return };
        let value = self.k(NV097_SET_ZSTENCIL_CLEAR_VALUE);
        let bpp = d.bytes_per_pixel();
        for y in y0..=y1 {
            for x in x0..=x1 {
                let at = d.origin + (cy + y) as u64 * d.pitch + (cx + x) as u64 * bpp;
                let ok = if d.format == 1 {
                    // 16-bit depth has no stencil
                    flags & 1 == 0 || d.ram.write(at, &(value as u16).to_le_bytes())
                } else {
                    let Some(old) = d.ram.read_u32(at) else { return };
                    let mut new = old;
                    if flags & 1 != 0 {
                        new = (new & 0xFF) | (value & 0xFFFF_FF00);
                    }
                    if flags & 2 != 0 {
                        new = (new & 0xFFFF_FF00) | (value & 0xFF);
                    }
                    d.ram.write_u32(at, new)
                };
                if !ok {
                    self.shared.log(format!("depth clear outside memory at {at:#x}"));
                    return;
                }
            }
        }
        self.shared.count_clear();
    }

    fn clear_surface(&mut self, flags: u32) {
        if flags & 3 != 0 {
            self.clear_zeta(flags);
        }
        let colour_mask = flags & 0xF0;
        if colour_mask == 0 {
            return;
        }
        if self.surface().is_none() {
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

    #[test]
    fn the_command_queue_reads_as_empty() {
        let (mut g, _, _) = setup();
        assert_eq!(g.read(CACHE1_STATUS as u64, 4) & 0x10, 0x10);
        assert_eq!(g.read(PFIFO_RUNOUT_STATUS as u64, 4) & 0x10, 0x10);
    }

    #[test]
    fn instance_memory_shares_bytes_with_ram_once_claimed() {
        let (mut g, shared, mut mem) = setup();
        let ram = mem.ram_view(0).unwrap();
        g.write(0x70_0000 + 8, 4, 0x1111_2222);
        assert_eq!(g.read(0x70_0000 + 8, 4), 0x1111_2222);
        shared.set_ramin_base(0x30_0000);
        // the private copy is gone: the window now shows what is in RAM there
        assert_eq!(g.read(0x70_0000 + 8, 4), 0);
        ram.write(0x30_0008, &0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(g.read(0x70_0000 + 8, 4), 0xDEAD_BEEF);
        g.write(0x70_0000 + 0x10, 4, 0xCAFE_F00D);
        assert_eq!(ram.read_u32(0x30_0010), Some(0xCAFE_F00D));
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
        kick(&mut g, &mut mem, &[hdr(0x100, 1), 0, hdr(0, 1), 0x1234, hdr(0, 1), KELVIN, hdr(0x1E28, 1), 7]);
        let log = shared.take_log();
        assert!(log.iter().any(|l| l.contains("no object bound")), "{log:?}");
        assert!(log.iter().any(|l| l.contains("0x1234")), "{log:?}");
        assert!(shared.gaps().iter().any(|(k, _)| k.contains("0x1e28")), "{:?}", shared.gaps());
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

    fn f(v: f32) -> u32 {
        v.to_bits()
    }

    fn setup_surface() -> Vec<u32> {
        vec![
            hdr(0, 1), KELVIN, hdr(0x194, 1), COLOR_DMA, hdr(0x200, 2), 64 << 16, 32 << 16,
            hdr(0x208, 1), 0x108, hdr(0x20C, 2), 256, FB,
        ]
    }

    fn vertex(x: f32, y: f32, argb: u32) -> Vec<u32> {
        vec![hdr(0x1940 + 4 * 3, 1), argb, hdr(0x1A00, 4), f(x), f(y), f(0.0), f(1.0)]
    }

    #[test]
    fn draws_a_triangle_from_immediate_vertices() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = setup_surface();
        w.extend([hdr(0x17FC, 1), 5]);
        w.extend(vertex(4.0, 4.0, 0xFFFF_0000));
        w.extend(vertex(60.0, 4.0, 0xFF00_FF00));
        w.extend(vertex(4.0, 28.0, 0xFF00_00FF));
        w.extend([hdr(0x17FC, 1), 0]);
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 1);
        assert!(pixel(&mut mem, 5, 5) >> 16 & 0xFF > 0xE0, "mostly red near the red corner");
        assert_eq!(pixel(&mut mem, 62, 30), 0, "outside the triangle");
        assert!(pixel(&mut mem, 20, 10) != 0);
        // one gap is recorded, and it says what is missing
        let gaps = shared.gaps();
        assert_eq!(gaps.len(), 1, "{gaps:?}");
        assert!(gaps[0].0.contains("fixed-function"), "{gaps:?}");
    }

    #[test]
    fn strips_fans_and_quads_expand_to_triangles() {
        for (prim, n, expect) in [(6u32, 5, 3), (7, 5, 3), (8, 8, 4), (5, 7, 2), (9, 6, 4), (10, 4, 2)] {
            let (mut g, shared, mut mem) = setup();
            init(&mut g);
            let mut w = setup_surface();
            w.extend([hdr(0x17FC, 1), prim]);
            for i in 0..n {
                w.extend(vertex(2.0 + (i * 5 % 17) as f32, 2.0 + (i * 7 % 13) as f32, 0xFFFF_FFFF));
            }
            w.extend([hdr(0x17FC, 1), 0]);
            kick(&mut g, &mut mem, &w);
            assert_eq!(shared.triangles(), expect, "primitive {prim} with {n} vertices");
        }
    }

    #[test]
    fn vertices_outside_begin_end_and_unsupported_primitives_are_not_drawn() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = setup_surface();
        w.extend(vertex(1.0, 1.0, 0xFFFF_FFFF)); // no Begin
        w.extend([hdr(0x17FC, 1), 2]); // lines
        w.extend(vertex(1.0, 1.0, 0xFFFF_FFFF));
        w.extend(vertex(9.0, 9.0, 0xFFFF_FFFF));
        w.extend([hdr(0x17FC, 1), 0]);
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 0);
        assert!(shared.gaps().iter().any(|(k, _)| k.contains("primitive type 2")), "{:?}", shared.gaps());
    }

    /// Program: oPos = v0 * (matrix in c0..c3), oD0 = v3.
    fn transform_program_words() -> Vec<u32> {
        use crate::vsh::{asm::*, O_D0, O_POS};
        let mut insts: Vec<Inst> = Vec::new();
        for (i, mask) in [X, Y, Z, W].into_iter().enumerate() {
            insts.push(Inst::mac(DP4, to_temp(0, mask), v(0), Some(c(i as u32)), None));
        }
        insts.push(Inst::mac(MOV, to_out(O_POS, ALL), r(0), None, None));
        insts.push(Inst::mac(MOV, to_out(O_D0, ALL), v(3), None, None).last());
        assemble(&insts).into_iter().flatten().collect()
    }

    fn matrix_words(sx: f32, sy: f32, tx: f32, ty: f32) -> Vec<u32> {
        [[sx, 0.0, 0.0, tx], [0.0, sy, 0.0, ty], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0f32]]
            .iter()
            .flatten()
            .map(|x| x.to_bits())
            .collect()
    }

    fn program_setup(m: Vec<u32>) -> Vec<u32> {
        let prog = transform_program_words();
        let mut w = vec![hdr(0x1E94, 1), 2, hdr(0x1E9C, 1), 0, hdr(0x1EA0, 1), 0, hdr(0x1EA4, 1), 0];
        w.push(hdr(0xB00, prog.len() as u32));
        w.extend(prog);
        w.push(hdr(0xB80, 16));
        w.extend(m);
        w
    }

    #[test]
    fn the_vertex_program_transforms_vertices() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = setup_surface();
        w.extend(program_setup(matrix_words(20.0, -10.0, 32.0, 20.0)));
        w.extend([hdr(0x17FC, 1), 5]);
        // model-space triangle (0,1) (-1,-1) (1,-1) lands at (32,10) (12,30) (52,30)
        w.extend(vertex(0.0, 1.0, 0xFFFF_FFFF));
        w.extend(vertex(-1.0, -1.0, 0xFFFF_FFFF));
        w.extend(vertex(1.0, -1.0, 0xFFFF_FFFF));
        w.extend([hdr(0x17FC, 1), 0]);
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 1);
        assert_eq!(pixel(&mut mem, 32, 25), 0xFFFF_FFFF, "inside the transformed triangle");
        assert_eq!(pixel(&mut mem, 5, 5), 0, "outside it");
        assert_eq!(pixel(&mut mem, 3, 29), 0, "outside it");
        assert!(shared.gaps().is_empty(), "{:?}", shared.gaps());
        assert!(shared.take_log().is_empty());
    }

    #[test]
    fn vertex_arrays_are_fetched_from_memory() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let view = mem.ram_view(0).unwrap();
        const VB: u64 = 0x30_0000;
        // three vertices of 16 bytes: x y z (floats) and a packed colour; pixel coordinates (no program)
        for (i, (x, y, argb)) in [(4.0f32, 4.0f32, 0xFFFF_0000u32), (60.0, 4.0, 0xFF00_FF00), (4.0, 28.0, 0xFF00_00FF)].iter().enumerate() {
            let at = VB + 16 * i as u64;
            view.write_u32(at, x.to_bits());
            view.write_u32(at + 4, y.to_bits());
            view.write_u32(at + 8, 0);
            view.write_u32(at + 12, *argb);
        }
        let mut w = setup_surface();
        w.extend([
            hdr(0x19C, 1), COLOR_DMA,
            hdr(0x1720, 1), VB as u32,
            hdr(0x1760, 1), 2 | (3 << 4) | (16 << 8),
            hdr(0x172C, 1), VB as u32 + 12,
            hdr(0x176C, 1), 0 | (4 << 4) | (16 << 8),
            hdr(0x17FC, 1), 5,
            hdr(0x1810, 1), 2 << 24,
            hdr(0x17FC, 1), 0,
        ]);
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 1);
        assert!(pixel(&mut mem, 5, 5) >> 16 & 0xFF > 0xE0, "red corner: {:#x}", pixel(&mut mem, 5, 5));
        assert!(shared.take_log().is_empty());
    }

    #[test]
    fn indexed_elements_use_the_same_arrays() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let view = mem.ram_view(0).unwrap();
        const VB: u64 = 0x30_0000;
        for (i, (x, y)) in [(4.0f32, 4.0f32), (60.0, 4.0), (4.0, 28.0), (60.0, 28.0)].iter().enumerate() {
            view.write_u32(VB + 8 * i as u64, x.to_bits());
            view.write_u32(VB + 8 * i as u64 + 4, y.to_bits());
        }
        let mut w = setup_surface();
        w.extend([
            hdr(0x19C, 1), COLOR_DMA, hdr(0x1720, 1), VB as u32, hdr(0x1760, 1), 2 | (2 << 4) | (8 << 8),
            hdr(0x17FC, 1), 5,
            hdr(0x1800, 2) | 0x4000_0000, 1 << 16, 3 | (2 << 16), // non-incrementing: indices (0, 1) then (3, 2)
            hdr(0x17FC, 1), 0,
        ]);
        kick(&mut g, &mut mem, &w);
        // elements 0,1,3,2 as TRIANGLES: only the first three (0,1,3) form a triangle
        assert_eq!(shared.triangles(), 1);
    }

    const ZB: u32 = 0x18_0000;

    fn depth_setup() -> Vec<u32> {
        let mut w = vec![
            hdr(0, 1), KELVIN, hdr(0x194, 1), COLOR_DMA, hdr(0x198, 1), COLOR_DMA, hdr(0x200, 2), 64 << 16, 32 << 16,
            hdr(0x208, 1), 0x128, // 8888 colour, Z24S8 depth, pitch layout
            hdr(0x20C, 2), 256 | (256 << 16), FB, // pitches and colour offset
            hdr(0x214, 1), ZB,
            hdr(0x1D98, 2), 63 << 16, 31 << 16, hdr(0x1D8C, 1), 0xFFFF_FF00, hdr(0x1D94, 1), 0x03,
            hdr(0x30C, 1), 1, hdr(0x354, 1), 0x201, hdr(0x35C, 1), 1, hdr(0x398, 1), 100.0f32.to_bits(),
        ];
        w.extend([hdr(0x1D90, 1), 0]);
        w
    }

    fn vertex3(x: f32, y: f32, z: f32, argb: u32) -> Vec<u32> {
        vec![hdr(0x194C, 1), argb, hdr(0x1A00, 4), f(x), f(y), f(z), f(1.0)]
    }

    fn tri(z: f32, argb: u32) -> Vec<u32> {
        let mut w = vec![hdr(0x17FC, 1), 5];
        w.extend(vertex3(4.0, 4.0, z, argb));
        w.extend(vertex3(60.0, 4.0, z, argb));
        w.extend(vertex3(4.0, 28.0, z, argb));
        w.extend([hdr(0x17FC, 1), 0]);
        w
    }

    #[test]
    fn depth_buffer_hides_the_farther_triangle() {
        for far_first in [true, false] {
            let (mut g, shared, mut mem) = setup();
            init(&mut g);
            let mut w = depth_setup();
            let (near, far) = (tri(10.0, 0xFFFF_0000), tri(60.0, 0xFF00_00FF));
            if far_first {
                w.extend(far);
                w.extend(near);
            } else {
                w.extend(near);
                w.extend(far);
            }
            kick(&mut g, &mut mem, &w);
            assert_eq!(pixel(&mut mem, 10, 8), 0xFFFF_0000, "far first: {far_first}");
            assert_eq!(shared.triangles(), 2);
            let zv = mem.ram_view(0).unwrap().read_u32((ZB + 8 * 256 + 10 * 4) as u64).unwrap();
            assert_eq!(zv >> 8, (10.0f32 / 100.0 * 16777215.0 + 0.5) as u32, "the near depth is stored");
        }
    }

    #[test]
    fn without_depth_testing_the_later_triangle_wins() {
        let (mut g, _, mut mem) = setup();
        init(&mut g);
        let mut w = depth_setup();
        w.extend([hdr(0x30C, 1), 0]);
        w.extend(tri(10.0, 0xFFFF_0000));
        w.extend(tri(60.0, 0xFF00_00FF));
        kick(&mut g, &mut mem, &w);
        assert_eq!(pixel(&mut mem, 10, 8), 0xFF00_00FF);
    }

    #[test]
    fn depth_clear_respects_the_rectangle_and_stencil_bits() {
        let (mut g, _, mut mem) = setup();
        init(&mut g);
        let mut w = depth_setup();
        // clear only the stencil byte of a small rectangle to 0x7F
        w.extend([hdr(0x1D98, 2), 2 | (3 << 16), 2 | (3 << 16), hdr(0x1D8C, 1), 0x0000_007F, hdr(0x1D94, 1), 0x02]);
        kick(&mut g, &mut mem, &w);
        let v = mem.ram_view(0).unwrap();
        let at = |x: u32, y: u32| v.read_u32((ZB + y * 256 + x * 4) as u64).unwrap();
        assert_eq!(at(2, 2), 0xFFFF_FF7F, "depth kept, stencil set");
        assert_eq!(at(4, 2), 0xFFFF_FF00, "outside the rectangle");
    }

    #[test]
    fn swizzled_targets_are_reported_not_drawn_wrongly() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = depth_setup();
        w.extend([hdr(0x208, 1), 0x208, hdr(0x1D90, 1), 0xFFFF_FFFF, hdr(0x1D94, 1), 0xF0]);
        kick(&mut g, &mut mem, &w);
        assert!(shared.gaps().iter().any(|(k, _)| k.contains("swizzled")), "{:?}", shared.gaps());
    }

    const TX: u32 = 0x28_0000;

    /// A 4x4 swizzled A8R8G8B8 texture whose texel (x, y) is (r = 60x+10, g = 60y+10, b = 0x80).
    fn put_texture(mem: &mut PhysMem) {
        let v = mem.ram_view(0).unwrap();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let idx = crate::texture::swizzle_index(x, y, 4, 4);
                v.write_u32((TX + 4 * idx) as u64, 0xFF00_0080 | ((60 * x + 10) << 16) | ((60 * y + 10) << 8));
            }
        }
    }

    fn texture_state(format_colour: u32) -> Vec<u32> {
        vec![
            hdr(0x184, 1), COLOR_DMA, hdr(0x1B00, 1), TX,
            hdr(0x1B04, 1), 1 | (2 << 4) | (format_colour << 8) | (1 << 16) | (2 << 20) | (2 << 24),
            hdr(0x1B08, 1), 0x0001_0101, hdr(0x1B0C, 1), 1 << 30, hdr(0x1B14, 1), (1 << 16) | (1 << 24),
            hdr(0x1E70, 1), 1,
        ]
    }

    fn tvertex(x: f32, y: f32, s: f32, t: f32) -> Vec<u32> {
        vec![hdr(0x194C, 1), 0xFFFF_FFFF, hdr(0x18C8, 2), f(s), f(t), hdr(0x1A00, 4), f(x), f(y), f(0.0), f(1.0)]
    }

    fn textured_quad() -> Vec<u32> {
        let mut w = vec![hdr(0x17FC, 1), 8];
        w.extend(tvertex(8.0, 4.0, 0.0, 0.0));
        w.extend(tvertex(40.0, 4.0, 1.0, 0.0));
        w.extend(tvertex(40.0, 20.0, 1.0, 1.0));
        w.extend(tvertex(8.0, 20.0, 0.0, 1.0));
        w.extend([hdr(0x17FC, 1), 0]);
        w
    }

    #[test]
    fn a_swizzled_texture_is_sampled_onto_a_quad() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        put_texture(&mut mem);
        let mut w = setup_surface();
        w.extend(texture_state(0x06));
        w.extend(textured_quad());
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 2);
        // 32x16 pixels over 4x4 texels: texel (x, y) covers pixels 8+8x.., 4+4y..
        for (px_, py_, tx, ty) in [(10, 5, 0u32, 0u32), (20, 14, 1, 2), (38, 18, 3, 3), (30, 6, 2, 0)] {
            let got = pixel(&mut mem, px_, py_);
            assert_eq!(got, 0xFF00_0080 | ((60 * tx + 10) << 16) | ((60 * ty + 10) << 8), "pixel ({px_},{py_}) should show texel ({tx},{ty})");
        }
        assert_eq!(pixel(&mut mem, 6, 10), 0, "outside the quad");
        let gaps = shared.gaps();
        assert!(gaps.iter().any(|(k, _)| k.contains("register combiners were never programmed")), "{gaps:?}");
    }

    #[test]
    fn programmed_combiners_replace_the_modulate_rule() {
        use crate::combiner::{icw, input, map, ocw, reg};
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        put_texture(&mut mem);
        let mut w = setup_surface();
        w.extend(texture_state(0x06));
        let inv_t0 = input(reg::T0, false, map::UNSIGNED_INVERT);
        let one = input(reg::ZERO, false, map::UNSIGNED_INVERT);
        let t0a = input(reg::T0, true, 0);
        let r0 = input(reg::R0, false, 0);
        w.extend([
            hdr(0xAC0, 1), icw(inv_t0, one, 0, 0), // colour stage 0: r0 = (1 - t0) * 1
            hdr(0x1E40, 1), ocw(0, reg::R0, 0, 0),
            hdr(0x260, 1), icw(t0a, input(reg::ZERO, false, map::UNSIGNED_INVERT), 0, 0), // alpha: r0.a = t0.a
            hdr(0xAA0, 1), ocw(0, reg::R0, 0, 0),
            hdr(0x288, 1), icw(0, 0, r0, 0), // final: colour = c = r0
            hdr(0x28C, 1), icw(0, 0, input(reg::R0, true, 0), 0), // alpha = r0.a
            hdr(0x1E60, 1), 1,
        ]);
        w.extend(textured_quad());
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 2);
        let got = pixel(&mut mem, 10, 5); // texel (0, 0): r 10, g 10, b 0x80
        let ch = |v: u32, s: u32| (v >> s & 0xFF) as i32;
        assert!((ch(got, 16) - 245).abs() <= 1 && (ch(got, 8) - 245).abs() <= 1 && (ch(got, 0) - 127).abs() <= 1, "{got:#x}");
        assert_eq!(ch(got, 24), 255);
        let gaps = shared.gaps();
        assert!(!gaps.iter().any(|(k, _)| k.contains("register combiners")), "{gaps:?}");
    }

    #[test]
    fn blend_alpha_test_and_mask_methods_reach_the_rasteriser() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = setup_surface();
        // blue background through a clear, then half-transparent red through source-alpha blending
        w.extend([hdr(0x1D98, 2), 63 << 16, 31 << 16, hdr(0x1D90, 1), 0xFF00_00FF, hdr(0x1D94, 1), 0xF0]);
        w.extend([
            hdr(0x304, 1), 1, hdr(0x344, 1), 0x302, hdr(0x348, 1), 0x303, hdr(0x350, 1), 0x8006,
            hdr(0x17FC, 1), 5,
        ]);
        for (x, y) in [(2.0, 2.0), (30.0, 2.0), (2.0, 30.0)] {
            w.extend([hdr(0x194C, 1), 0x80FF_0000, hdr(0x1A00, 4), f(x), f(y), f(0.0), f(1.0)]);
        }
        w.extend([hdr(0x17FC, 1), 0]);
        kick(&mut g, &mut mem, &w);
        assert_eq!(shared.triangles(), 1, "{:?} {:?}", shared.take_log(), shared.gaps());
        let p = pixel(&mut mem, 6, 6);
        assert!((((p >> 16) & 0xFF) as i32 - 128).abs() <= 1 && ((p & 0xFF) as i32 - 127).abs() <= 1, "{p:#x}");
        assert!(shared.gaps().iter().all(|(k, _)| !k.contains("blend")), "{:?}", shared.gaps());
        // an unknown factor is reported, not guessed
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w2 = setup_surface();
        w2.extend([hdr(0x304, 1), 1, hdr(0x344, 1), 0x1234, hdr(0x17FC, 1), 5]);
        for (x, y) in [(2.0, 2.0), (30.0, 2.0), (2.0, 30.0)] {
            w2.extend([hdr(0x1A00, 4), f(x), f(y), f(0.0), f(1.0)]);
        }
        w2.extend([hdr(0x17FC, 1), 0]);
        kick(&mut g, &mut mem, &w2);
        assert!(shared.gaps().iter().any(|(k, _)| k.contains("blend factor")), "{:?}", shared.gaps());
    }

    #[test]
    fn bilinear_filtering_blends_texels() {
        let (mut g, _, mut mem) = setup();
        init(&mut g);
        put_texture(&mut mem);
        let mut w = setup_surface();
        w.extend(texture_state(0x06));
        w.extend([hdr(0x1B14, 1), (1 << 16) | (2 << 24)]); // linear magnification
        w.extend(textured_quad());
        kick(&mut g, &mut mem, &w);
        // exactly on the border between texels 0 and 1 in x (pixel boundary at x = 16): the red channels average
        let a = pixel(&mut mem, 15, 5) >> 16 & 0xFF;
        let b = pixel(&mut mem, 16, 5) >> 16 & 0xFF;
        assert!(a > 10 && b < 70 && a < 70 && b > 10, "{a} {b}");
        assert!(a != 10 || b != 70, "filtered, not nearest: {a} {b}");
    }

    #[test]
    fn unsupported_texture_formats_and_modes_are_reported() {
        let (mut g, shared, mut mem) = setup();
        init(&mut g);
        let mut w = setup_surface();
        w.extend(texture_state(0x0B)); // palettised
        w.extend(textured_quad());
        w.extend(texture_state(0x06));
        w.extend([hdr(0x1E70, 1), 3]); // cube map shader mode
        w.extend(textured_quad());
        kick(&mut g, &mut mem, &w);
        let gaps = shared.gaps();
        assert!(gaps.iter().any(|(k, _)| k.contains("texture format 0xb")), "{gaps:?}");
        assert!(gaps.iter().any(|(k, _)| k.contains("pixel shader stage mode 0x3")), "{gaps:?}");
        assert_eq!(pixel(&mut mem, 20, 10), 0xFFFF_FFFF, "undrawable textures fall back to the plain colour");
    }
}
