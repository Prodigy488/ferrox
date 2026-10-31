//! CPU backends. The real emulator runs the Xbox's Pentium III–class CPU on
//! the host through hardware virtualization (KVM, WHPX, HVF), which is far
//! faster than translating every instruction. This crate defines the backend
//! interface and ships a tiny reference interpreter used to test the machine
//! and device plumbing without needing a hypervisor. The interpreter is NOT
//! an x86 emulator: it understands a handful of opcodes.

use ferrox_core::{Bus, BusError};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regs {
    /// eax, ecx, edx, ebx, esp, ebp, esi, edi
    pub gpr: [u32; 8],
    pub eip: u32,
    pub zf: bool,
}

pub const EAX: usize = 0;
pub const ECX: usize = 1;
pub const EDX: usize = 2;
pub const ESP: usize = 4;

#[derive(Debug, PartialEq, Eq)]
pub enum Exit {
    /// The guest executed `hlt`.
    Halt,
    /// The instruction budget ran out.
    Budget,
    /// The guest wrote `HLE_PORT`: it is calling into the emulator (the
    /// high-level kernel). Carries the 32-bit value written (the export
    /// ordinal). Execution resumes after the `out` when `run` is called again.
    Hypercall(u32),
}

/// The I/O port the kernel stubs write to in order to call into the host.
/// Every hypervisor reports a port write to userspace, so real backends can
/// raise `Exit::Hypercall` from that exit without any special instruction.
pub const HLE_PORT: u16 = 0xE0;

#[derive(Debug, PartialEq, Eq)]
pub enum CpuError {
    Bus(BusError),
    /// The backend does not implement this opcode.
    Unimplemented { eip: u32, opcode: u8 },
    /// A hypervisor backend failed (bad VM exit, triple fault, ioctl error).
    Backend(String),
}

impl From<BusError> for CpuError {
    fn from(e: BusError) -> Self {
        CpuError::Bus(e)
    }
}

impl std::fmt::Display for CpuError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CpuError::Bus(e) => write!(f, "{e}"),
            CpuError::Backend(m) => write!(f, "{m}"),
            CpuError::Unimplemented { eip, opcode } => {
                write!(f, "opcode {opcode:#04x} at {eip:#x} is not implemented")
            }
        }
    }
}

impl std::error::Error for CpuError {}

pub trait CpuBackend {
    fn name(&self) -> &'static str;
    fn regs(&self) -> Regs;
    fn set_regs(&mut self, regs: Regs);
    /// Runs until `hlt` or until `max_instructions` have executed.
    fn run(&mut self, bus: &mut dyn Bus, max_instructions: u64) -> Result<Exit, CpuError>;
}

/// The test interpreter: nop, mov r32 imm32, mov eax/[abs32], inc/dec r32,
/// add eax imm32, jmp/jne rel8, push imm32/r32, pop r32, mov [r32]/r32 r32, add r32 imm8, call [abs32], ret / ret imm16,
/// in/out (8-bit imm8 and dx forms; `out imm8, eax` for the hypercall port), hlt.
#[derive(Default)]
pub struct RefCpu {
    regs: Regs,
}

impl RefCpu {
    pub fn new(entry: u32) -> Self {
        Self { regs: Regs { eip: entry, ..Regs::default() } }
    }

    fn fetch8(&mut self, bus: &mut dyn Bus) -> Result<u8, CpuError> {
        let v = bus.read(self.regs.eip as u64, 1)? as u8;
        self.regs.eip = self.regs.eip.wrapping_add(1);
        Ok(v)
    }

    fn fetch32(&mut self, bus: &mut dyn Bus) -> Result<u32, CpuError> {
        let v = bus.read(self.regs.eip as u64, 4)? as u32;
        self.regs.eip = self.regs.eip.wrapping_add(4);
        Ok(v)
    }

    fn push(&mut self, bus: &mut dyn Bus, v: u32) -> Result<(), CpuError> {
        self.regs.gpr[ESP] = self.regs.gpr[ESP].wrapping_sub(4);
        bus.write(self.regs.gpr[ESP] as u64, 4, v as u64)?;
        Ok(())
    }

    fn pop(&mut self, bus: &mut dyn Bus) -> Result<u32, CpuError> {
        let v = bus.read(self.regs.gpr[ESP] as u64, 4)? as u32;
        self.regs.gpr[ESP] = self.regs.gpr[ESP].wrapping_add(4);
        Ok(v)
    }

    fn set_zf(&mut self, v: u32) {
        self.regs.zf = v == 0;
    }
}

impl CpuBackend for RefCpu {
    fn name(&self) -> &'static str {
        "reference interpreter (test only)"
    }

    fn regs(&self) -> Regs {
        self.regs
    }

    fn set_regs(&mut self, regs: Regs) {
        self.regs = regs;
    }

    fn run(&mut self, bus: &mut dyn Bus, max_instructions: u64) -> Result<Exit, CpuError> {
        for _ in 0..max_instructions {
            let at = self.regs.eip;
            let op = self.fetch8(bus)?;
            match op {
                0x90 => {}
                0x50..=0x57 => {
                    let v = self.regs.gpr[(op - 0x50) as usize];
                    self.push(bus, v)?;
                }
                0x58..=0x5F => {
                    let v = self.pop(bus)?;
                    self.regs.gpr[(op - 0x58) as usize] = v;
                }
                0x89 => {
                    // mov r/m32, r32: register-to-register, or store through [reg]
                    let modrm = self.fetch8(bus)?;
                    let (md, reg, rm) = (modrm >> 6, ((modrm >> 3) & 7) as usize, (modrm & 7) as usize);
                    match md {
                        3 => self.regs.gpr[rm] = self.regs.gpr[reg],
                        0 if rm != 4 && rm != 5 => {
                            bus.write(self.regs.gpr[rm] as u64, 4, self.regs.gpr[reg] as u64)?
                        }
                        _ => return Err(CpuError::Unimplemented { eip: at, opcode: op }),
                    }
                }
                0xB8..=0xBF => {
                    let v = self.fetch32(bus)?;
                    self.regs.gpr[(op - 0xB8) as usize] = v;
                }
                0xA1 => {
                    let a = self.fetch32(bus)?;
                    self.regs.gpr[EAX] = bus.read(a as u64, 4)? as u32;
                }
                0xA3 => {
                    let a = self.fetch32(bus)?;
                    bus.write(a as u64, 4, self.regs.gpr[EAX] as u64)?;
                }
                0x40..=0x47 => {
                    let r = (op - 0x40) as usize;
                    self.regs.gpr[r] = self.regs.gpr[r].wrapping_add(1);
                    self.set_zf(self.regs.gpr[r]);
                }
                0x48..=0x4F => {
                    let r = (op - 0x48) as usize;
                    self.regs.gpr[r] = self.regs.gpr[r].wrapping_sub(1);
                    self.set_zf(self.regs.gpr[r]);
                }
                0x05 => {
                    let v = self.fetch32(bus)?;
                    self.regs.gpr[EAX] = self.regs.gpr[EAX].wrapping_add(v);
                    self.set_zf(self.regs.gpr[EAX]);
                }
                0xEB | 0x75 => {
                    let rel = self.fetch8(bus)? as i8 as i32;
                    if op == 0xEB || !self.regs.zf {
                        self.regs.eip = self.regs.eip.wrapping_add(rel as u32);
                    }
                }
                0xE4 => {
                    let p = self.fetch8(bus)? as u16;
                    self.regs.gpr[EAX] = (self.regs.gpr[EAX] & !0xff) | bus.io_in(p, 1) as u32;
                }
                0xE6 => {
                    let p = self.fetch8(bus)? as u16;
                    bus.io_out(p, 1, (self.regs.gpr[EAX] & 0xff) as u64);
                }
                0xEC => {
                    let p = self.regs.gpr[EDX] as u16;
                    self.regs.gpr[EAX] = (self.regs.gpr[EAX] & !0xff) | bus.io_in(p, 1) as u32;
                }
                0xEE => {
                    let p = self.regs.gpr[EDX] as u16;
                    bus.io_out(p, 1, (self.regs.gpr[EAX] & 0xff) as u64);
                }
                0x83 => {
                    // add r32, imm8 (sign-extended), register form only
                    let modrm = self.fetch8(bus)?;
                    if modrm & 0xF8 != 0xC0 {
                        return Err(CpuError::Unimplemented { eip: at, opcode: op });
                    }
                    let imm = self.fetch8(bus)? as i8 as i32 as u32;
                    let r = &mut self.regs.gpr[(modrm & 7) as usize];
                    *r = r.wrapping_add(imm);
                    let v = *r;
                    self.set_zf(v);
                }
                0x68 => {
                    let v = self.fetch32(bus)?;
                    self.push(bus, v)?;
                }
                0xFF => {
                    // only `call [abs32]` (ModRM 0x15)
                    let modrm = self.fetch8(bus)?;
                    if modrm != 0x15 {
                        return Err(CpuError::Unimplemented { eip: at, opcode: op });
                    }
                    let a = self.fetch32(bus)?;
                    let target = bus.read(a as u64, 4)? as u32;
                    let ret = self.regs.eip;
                    self.push(bus, ret)?;
                    self.regs.eip = target;
                }
                0xC3 | 0xC2 => {
                    let pop = if op == 0xC2 {
                        let lo = self.fetch8(bus)? as u32;
                        lo | (self.fetch8(bus)? as u32) << 8
                    } else {
                        0
                    };
                    let ret = self.pop(bus)?;
                    self.regs.gpr[ESP] = self.regs.gpr[ESP].wrapping_add(pop);
                    self.regs.eip = ret;
                }
                0xE7 => {
                    let p = self.fetch8(bus)? as u16;
                    let v = self.regs.gpr[EAX];
                    if p == HLE_PORT {
                        return Ok(Exit::Hypercall(v));
                    }
                    bus.io_out(p, 4, v as u64);
                }
                0xF4 => return Ok(Exit::Halt),
                _ => return Err(CpuError::Unimplemented { eip: at, opcode: op }),
            }
        }
        Ok(Exit::Budget)
    }
}
