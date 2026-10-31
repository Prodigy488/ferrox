//! High-level kernel mode: run an XBE without the console's boot ROM or BIOS.
//!
//! Instead of booting the real kernel, the loader copies the executable into
//! RAM and fills its kernel import table with the addresses of tiny *stubs*.
//! Each stub tells the host which kernel function was called (by writing the
//! export ordinal to `HLE_PORT`), then returns to the game with the host's
//! result in `eax`. The host-side handler can read the arguments off the
//! guest stack. The CPU still runs the game's own code natively; only calls
//! into the kernel are handled by the emulator.
//!
//! This is the approach Cxbx-Reloaded made practical for games, applied inside
//! a virtual machine so the CPU side can stay fast and exact. It needs no
//! copyrighted BIOS, so it is also the quickest way to run homebrew. Games
//! that depend on exact kernel behaviour need the low-level mode instead.

use ferrox_core::{Bus, BusError};
use ferrox_cpu::{CpuBackend, CpuError, Exit, Regs, ESP, HLE_PORT};
use ferrox_formats::xbe::Xbe;
use ferrox_machine::PhysMem;
use std::collections::HashMap;

/// A host implementation of one kernel export.
pub type Handler = Box<dyn FnMut(&mut Call) -> u32>;

/// What a handler can see and change while the guest waits.
pub struct Call<'a> {
    pub regs: &'a mut Regs,
    pub bus: &'a mut dyn Bus,
}

impl Call<'_> {
    /// The `i`th 32-bit stack argument (stdcall: arg 0 is just above the
    /// return address).
    pub fn arg(&mut self, i: u32) -> Result<u32, BusError> {
        let at = self.regs.gpr[ESP] as u64 + 4 + 4 * i as u64;
        Ok(self.bus.read(at, 4)? as u32)
    }
}

struct Export {
    name: &'static str,
    argc: u32,
    handler: Handler,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HleError {
    /// The game imports an export nobody has registered.
    MissingExport(u32),
    /// The image does not fit in RAM, or a section lies outside the file.
    BadImage(&'static str),
    Bus(BusError),
    Cpu(String),
    /// The game called an export with no handler.
    UnimplementedCall { ordinal: u32 },
}

impl std::fmt::Display for HleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HleError::MissingExport(o) => write!(f, "kernel export #{o} is not implemented"),
            HleError::BadImage(m) => write!(f, "cannot load the executable: {m}"),
            HleError::Bus(e) => write!(f, "{e}"),
            HleError::Cpu(e) => write!(f, "{e}"),
            HleError::UnimplementedCall { ordinal } => {
                write!(f, "game called kernel export #{ordinal}, which has no handler")
            }
        }
    }
}

impl std::error::Error for HleError {}

impl From<BusError> for HleError {
    fn from(e: BusError) -> Self {
        HleError::Bus(e)
    }
}

#[derive(Default)]
pub struct KernelHle {
    exports: HashMap<u32, Export>,
}

/// Where the loaded image starts running, and what the loader did.
#[derive(Debug, PartialEq, Eq)]
pub struct Loaded {
    pub entry: u32,
    pub stack_top: u32,
    /// Imports patched, as (ordinal, stub address).
    pub stubs: Vec<(u32, u32)>,
}

impl KernelHle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a kernel export: its ordinal, name, number of 32-bit stack
    /// arguments (the stub pops them on return, as stdcall requires), and the
    /// host function that implements it.
    pub fn register(
        &mut self,
        ordinal: u32,
        name: &'static str,
        argc: u32,
        handler: impl FnMut(&mut Call) -> u32 + 'static,
    ) {
        self.exports.insert(ordinal, Export { name, argc, handler: Box::new(handler) });
    }

    pub fn name_of(&self, ordinal: u32) -> Option<&'static str> {
        self.exports.get(&ordinal).map(|e| e.name)
    }

    /// Copies the image into RAM (virtual addresses used as physical), and
    /// points every kernel import at a freshly written stub in `[stub_base, ..)`.
    pub fn load(
        &self,
        mem: &mut PhysMem,
        xbe: &Xbe,
        file: &[u8],
        stub_base: u32,
        stack_top: u32,
    ) -> Result<Loaded, HleError> {
        // headers, then each section's raw data (the rest of a section is zero)
        let hdr_end = (xbe.size_of_headers as usize).min(file.len());
        mem.load(xbe.base_addr as u64, &file[..hdr_end])?;
        for s in &xbe.sections {
            let raw = file
                .get(s.raw_addr as usize..(s.raw_addr as usize).saturating_add(s.raw_size as usize))
                .ok_or(HleError::BadImage("section data lies outside the file"))?;
            let mut body = vec![0u8; s.virtual_size.max(s.raw_size) as usize];
            body[..raw.len()].copy_from_slice(raw);
            mem.load(s.virtual_addr as u64, &body)?;
        }
        let ordinals = xbe
            .kernel_imports(file)
            .map_err(|_| HleError::BadImage("kernel import table unreadable"))?;
        let mut stubs = Vec::new();
        let mut at = stub_base;
        for (i, &ord) in ordinals.iter().enumerate() {
            let ex = self.exports.get(&ord).ok_or(HleError::MissingExport(ord))?;
            // mov eax, ordinal ; out HLE_PORT, eax ; ret 4*argc
            let mut code = vec![0xB8];
            code.extend_from_slice(&ord.to_le_bytes());
            code.extend_from_slice(&[0xE7, HLE_PORT as u8]);
            if ex.argc == 0 {
                code.push(0xC3);
            } else {
                code.push(0xC2);
                code.extend_from_slice(&((4 * ex.argc) as u16).to_le_bytes());
            }
            mem.load(at as u64, &code)?;
            stubs.push((ord, at));
            let slot = xbe
                .va_to_file_offset(xbe.kernel_thunk_addr)
                .ok_or(HleError::BadImage("thunk table address"))?;
            let _ = slot; // the table lives in guest RAM now: patch it there
            mem.write(xbe.kernel_thunk_addr as u64 + 4 * i as u64, 4, at as u64)?;
            at += code.len().next_multiple_of(4) as u32;
        }
        Ok(Loaded { entry: xbe.entry_point, stack_top, stubs })
    }

    /// Runs the guest, serving kernel calls, until it halts or `budget`
    /// instructions have executed. Returns why it stopped.
    pub fn run(
        &mut self,
        cpu: &mut dyn CpuBackend,
        bus: &mut dyn Bus,
        mut budget: u64,
    ) -> Result<Exit, HleError> {
        const SLICE: u64 = 10_000;
        while budget > 0 {
            let n = budget.min(SLICE);
            match cpu.run(bus, n).map_err(|e: CpuError| HleError::Cpu(e.to_string()))? {
                Exit::Hypercall(ordinal) => {
                    let ex = self
                        .exports
                        .get_mut(&ordinal)
                        .ok_or(HleError::UnimplementedCall { ordinal })?;
                    let mut regs = cpu.regs();
                    let result = (ex.handler)(&mut Call { regs: &mut regs, bus });
                    regs.gpr[0] = result;
                    cpu.set_regs(regs);
                    budget -= 1;
                }
                Exit::Budget => budget -= n,
                Exit::Halt => return Ok(Exit::Halt),
            }
        }
        Ok(Exit::Budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_cpu::RefCpu;
    use ferrox_formats::xbe::synth::{build, BASE};
    use std::cell::RefCell;
    use std::rc::Rc;

    const ORD_ADD: u32 = 100;
    const ORD_LOG: u32 = 101;
    const RESULT: u32 = 0x2_0000;

    fn machine() -> PhysMem {
        let mut m = PhysMem::new();
        m.add_ram(0, 4 * 1024 * 1024).unwrap();
        m
    }

    /// push 4; push 3; call [add]; mov [RESULT], eax; push 7; call [log]; hlt
    fn program(add_slot: u32, log_slot: u32) -> Vec<u8> {
        let mut p = vec![0x68];
        p.extend_from_slice(&4u32.to_le_bytes());
        p.push(0x68);
        p.extend_from_slice(&3u32.to_le_bytes());
        p.extend_from_slice(&[0xFF, 0x15]);
        p.extend_from_slice(&add_slot.to_le_bytes());
        p.push(0xA3);
        p.extend_from_slice(&RESULT.to_le_bytes());
        p.push(0x68);
        p.extend_from_slice(&7u32.to_le_bytes());
        p.extend_from_slice(&[0xFF, 0x15]);
        p.extend_from_slice(&log_slot.to_le_bytes());
        p.push(0xF4);
        p
    }

    fn setup() -> (PhysMem, KernelHle, Loaded, Rc<RefCell<Vec<u32>>>) {
        let mut file = build(true, &[ORD_ADD, ORD_LOG]);
        let prog = program(BASE + 0x2000, BASE + 0x2004);
        file[0x1000..0x1000 + prog.len()].copy_from_slice(&prog);
        let xbe = Xbe::parse(&file).unwrap();
        let logged = Rc::new(RefCell::new(Vec::new()));
        let mut hle = KernelHle::new();
        hle.register(ORD_ADD, "TestAdd", 2, |c| c.arg(0).unwrap() + c.arg(1).unwrap());
        let l = logged.clone();
        hle.register(ORD_LOG, "TestLog", 1, move |c| {
            l.borrow_mut().push(c.arg(0).unwrap());
            0
        });
        let mut mem = machine();
        let loaded = hle.load(&mut mem, &xbe, &file, 0x30_0000, 0x40_0000 - 16).unwrap();
        (mem, hle, loaded, logged)
    }

    #[test]
    fn game_calls_kernel_and_gets_results() {
        let (mut mem, mut hle, loaded, logged) = setup();
        assert_eq!(loaded.stubs.len(), 2);
        // the import table now points at the stubs
        assert_eq!(mem.read((BASE + 0x2000) as u64, 4).unwrap() as u32, loaded.stubs[0].1);

        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        assert_eq!(hle.run(&mut cpu, &mut mem, 10_000).unwrap(), Exit::Halt);
        // 3 + 4, returned through eax and stored by the game
        assert_eq!(mem.read(RESULT as u64, 4).unwrap(), 7);
        assert_eq!(*logged.borrow(), [7]);
        // stdcall cleanup: the stack is back where it started
        assert_eq!(cpu.regs().gpr[ESP], loaded.stack_top);
    }

    #[test]
    fn missing_exports_are_found_at_load_time() {
        let file = build(true, &[ORD_ADD, 999]);
        let xbe = Xbe::parse(&file).unwrap();
        let mut hle = KernelHle::new();
        hle.register(ORD_ADD, "TestAdd", 2, |_| 0);
        let mut mem = machine();
        assert_eq!(
            hle.load(&mut mem, &xbe, &file, 0x30_0000, 0x40_0000).unwrap_err(),
            HleError::MissingExport(999)
        );
    }

    #[test]
    fn calls_without_handlers_stop_the_run() {
        let (mut mem, mut hle, loaded, _) = setup();
        hle.exports.remove(&ORD_ADD);
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        assert_eq!(
            hle.run(&mut cpu, &mut mem, 10_000).unwrap_err(),
            HleError::UnimplementedCall { ordinal: ORD_ADD }
        );
    }

    #[test]
    fn budget_limits_a_runaway_game() {
        let mut m = machine();
        m.load(0, &[0xEB, 0xFE]).unwrap(); // jmp $
        let mut cpu = RefCpu::new(0);
        let mut hle = KernelHle::new();
        assert_eq!(hle.run(&mut cpu, &mut m, 50_000).unwrap(), Exit::Budget);
    }
}
