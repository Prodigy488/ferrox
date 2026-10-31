//! KVM backend: the guest CPU runs natively under the host's hardware
//! virtualization; only memory-mapped I/O, port I/O and kernel calls leave the VM.
//!
//! The guest starts in 32-bit protected mode with flat segments, which is the
//! state an Xbox executable expects once the kernel has finished booting. The
//! caller supplies a small GDT in guest memory (see [`flat_gdt`]).
//!
//! Status: written against the kvm-ioctls API and compiled, but **not yet run**:
//! the machine this was written on has no `/dev/kvm`. Treat it as untested until
//! `cargo test -p ferrox-kvm -- --ignored` has passed on a KVM host.

#![cfg(target_os = "linux")]

use ferrox_core::Bus;
use ferrox_cpu::{CpuBackend, CpuError, Exit, Regs, FLAG_MASK, HLE_PORT};
use ferrox_machine::MemSlot;
use kvm_bindings::{kvm_regs, kvm_segment, kvm_userspace_memory_region, KVM_MEM_READONLY};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd, VmFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Selectors in the GDT returned by [`flat_gdt`].
pub const CODE_SEL: u16 = 0x08;
pub const DATA_SEL: u16 = 0x10;

/// Is hardware virtualization usable here (i.e. can `/dev/kvm` be opened)?
pub fn available() -> bool {
    Kvm::new().is_ok()
}

/// A three-entry GDT (null, flat 4 GiB code, flat 4 GiB data). Place it in guest
/// memory and pass its address to [`KvmCpu::new`].
pub fn flat_gdt() -> [u8; 24] {
    let mut g = [0u8; 24];
    g[8..16].copy_from_slice(&0x00CF_9A00_0000_FFFFu64.to_le_bytes());
    g[16..24].copy_from_slice(&0x00CF_9200_0000_FFFFu64.to_le_bytes());
    g
}

#[derive(Debug)]
pub struct KvmError(pub String);

impl std::fmt::Display for KvmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "KVM: {}", self.0)
    }
}

impl std::error::Error for KvmError {}

fn err<E: std::fmt::Display>(what: &str) -> impl FnOnce(E) -> KvmError + '_ {
    move |e| KvmError(format!("{what}: {e}"))
}

extern "C" fn on_kick(_: libc::c_int) {}

/// Shared with the watchdog thread, which interrupts `KVM_RUN` when the guest
/// has run for a whole time slice without exiting (e.g. a busy loop).
struct Watchdog {
    /// pthread id of the thread currently inside `run`, or 0.
    runner: AtomicUsize,
    slice_ticks: AtomicUsize,
    ticks: AtomicUsize,
    quit: AtomicBool,
    kicked: AtomicBool,
}

const TICK: Duration = Duration::from_millis(2);

pub struct KvmCpu {
    _kvm: Kvm,
    _vm: VmFd,
    vcpu: VcpuFd,
    wd: Arc<Watchdog>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KvmCpu {
    /// Creates a VM that maps `slots`, with the vcpu at `entry` in flat protected
    /// mode. `gdt_addr` is where [`flat_gdt`] has been written in guest memory.
    ///
    /// # Safety
    /// Each slot's `host_ptr` must stay valid for the life of the returned value,
    /// and the host must not touch that memory (through `Bus`) while the guest runs.
    pub unsafe fn new(slots: &[MemSlot], entry: u32, gdt_addr: u32) -> Result<Self, KvmError> {
        let kvm = Kvm::new().map_err(err("open /dev/kvm"))?;
        let vm = kvm.create_vm().map_err(err("create VM"))?;
        for (i, s) in slots.iter().enumerate() {
            if s.guest_addr % 4096 != 0 || s.size % 4096 != 0 || s.host_ptr as usize % 4096 != 0 {
                return Err(KvmError(format!("memory region {:#x}+{:#x} is not page aligned", s.guest_addr, s.size)));
            }
            let region = kvm_userspace_memory_region {
                slot: i as u32,
                flags: if s.read_only { KVM_MEM_READONLY } else { 0 },
                guest_phys_addr: s.guest_addr,
                memory_size: s.size,
                userspace_addr: s.host_ptr as u64,
            };
            vm.set_user_memory_region(region).map_err(err("map memory"))?;
        }
        let vcpu = vm.create_vcpu(0).map_err(err("create vcpu"))?;

        let mut sregs = vcpu.get_sregs().map_err(err("get sregs"))?;
        let code = kvm_segment {
            base: 0,
            limit: 0xFFFF_FFFF,
            selector: CODE_SEL,
            type_: 0xB,
            present: 1,
            dpl: 0,
            db: 1,
            s: 1,
            l: 0,
            g: 1,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        let data = kvm_segment { selector: DATA_SEL, type_: 0x3, ..code };
        sregs.cs = code;
        sregs.ds = data;
        sregs.es = data;
        sregs.fs = data;
        sregs.gs = data;
        sregs.ss = data;
        sregs.gdt.base = gdt_addr as u64;
        sregs.gdt.limit = 23;
        // protected mode, x87 reporting, no paging (guest-physical == linear), no FPU/SSE traps
        sregs.cr0 = 0x33;
        // OSFXSR | OSXMMEXCPT: the console's kernel turns SSE on before any game runs
        sregs.cr4 |= 0x600;
        vcpu.set_sregs(&sregs).map_err(err("set sregs"))?;
        vcpu.set_regs(&kvm_regs { rip: entry as u64, rflags: 2, ..Default::default() })
            .map_err(err("set regs"))?;

        // SIGUSR1 interrupts KVM_RUN (EINTR) without restarting the call.
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_kick as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());

        let wd = Arc::new(Watchdog {
            runner: AtomicUsize::new(0),
            slice_ticks: AtomicUsize::new(5),
            ticks: AtomicUsize::new(0),
            quit: AtomicBool::new(false),
            kicked: AtomicBool::new(false),
        });
        let w = wd.clone();
        let thread = std::thread::spawn(move || {
            while !w.quit.load(Ordering::Relaxed) {
                std::thread::sleep(TICK);
                let who = w.runner.load(Ordering::Acquire);
                if who == 0 {
                    continue;
                }
                if w.ticks.fetch_add(1, Ordering::Relaxed) + 1 >= w.slice_ticks.load(Ordering::Relaxed) {
                    w.ticks.store(0, Ordering::Relaxed);
                    w.kicked.store(true, Ordering::Release);
                    unsafe { libc::pthread_kill(who as libc::pthread_t, libc::SIGUSR1) };
                }
            }
        });
        Ok(Self { _kvm: kvm, _vm: vm, vcpu, wd, thread: Some(thread) })
    }

    /// How long the guest may run without leaving the VM before `run` returns
    /// `Exit::Budget`. Rounded to 2 ms ticks.
    pub fn set_slice(&mut self, d: Duration) {
        let ticks = (d.as_millis() / TICK.as_millis()).max(1) as usize;
        self.wd.slice_ticks.store(ticks, Ordering::Relaxed);
    }
}

impl Drop for KvmCpu {
    fn drop(&mut self) {
        self.wd.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn cpu_err(msg: impl Into<String>) -> CpuError {
    CpuError::Backend(msg.into())
}

impl CpuBackend for KvmCpu {
    fn name(&self) -> &'static str {
        "kvm"
    }

    fn regs(&self) -> Regs {
        let r = self.vcpu.get_regs().unwrap_or_default();
        Regs {
            gpr: [r.rax as u32, r.rcx as u32, r.rdx as u32, r.rbx as u32, r.rsp as u32, r.rbp as u32, r.rsi as u32, r.rdi as u32],
            eip: r.rip as u32,
            zf: r.rflags & 0x40 != 0,
            eflags: (r.rflags as u32) & FLAG_MASK | 2,
        }
    }

    fn set_fs_base(&mut self, base: u32) {
        if let Ok(mut s) = self.vcpu.get_sregs() {
            s.fs.base = base as u64;
            s.fs.limit = 0xFFFF_FFFF;
            let _ = self.vcpu.set_sregs(&s);
        }
    }

    fn set_regs(&mut self, regs: Regs) {
        let mut r = self.vcpu.get_regs().unwrap_or_default();
        let g = regs.gpr;
        r.rax = g[0] as u64;
        r.rcx = g[1] as u64;
        r.rdx = g[2] as u64;
        r.rbx = g[3] as u64;
        r.rsp = g[4] as u64;
        r.rbp = g[5] as u64;
        r.rsi = g[6] as u64;
        r.rdi = g[7] as u64;
        r.rip = regs.eip as u64;
        r.rflags = if regs.eflags != 0 {
            (r.rflags & !(FLAG_MASK as u64)) | (regs.eflags & FLAG_MASK) as u64 | 2
        } else {
            (r.rflags & !0x40) | if regs.zf { 0x40 } else { 0 }
        };
        let _ = self.vcpu.set_regs(&r);
    }

    /// Runs until the guest halts, calls the host, or has run one time slice.
    /// `max_instructions` is ignored: hardware virtualization cannot count them.
    fn run(&mut self, bus: &mut dyn Bus, _max_instructions: u64) -> Result<Exit, CpuError> {
        // a guest that keeps touching hardware exits before the watchdog's signal lands, so the
        // slice is also timed here
        let slice = TICK * self.wd.slice_ticks.load(Ordering::Relaxed) as u32;
        let began = std::time::Instant::now();
        loop {
            if began.elapsed() >= slice {
                return Ok(Exit::Budget);
            }
            self.wd.ticks.store(0, Ordering::Relaxed);
            self.wd.kicked.store(false, Ordering::Relaxed);
            self.wd.runner.store(unsafe { libc::pthread_self() } as usize, Ordering::Release);
            let exit = self.vcpu.run();
            self.wd.runner.store(0, Ordering::Release);
            match exit {
                Ok(VcpuExit::Hlt) => return Ok(Exit::Halt),
                Ok(VcpuExit::IoOut(port, data)) => {
                    let v = le(data);
                    if port == HLE_PORT {
                        return Ok(Exit::Hypercall(v as u32));
                    }
                    bus.io_out(port, data.len(), v);
                }
                Ok(VcpuExit::IoIn(port, data)) => {
                    let v = bus.io_in(port, data.len());
                    data.copy_from_slice(&v.to_le_bytes()[..data.len()]);
                }
                Ok(VcpuExit::MmioRead(addr, data)) => {
                    let v = bus.read(addr, data.len()).map_err(|e| cpu_err(e.to_string()))?;
                    data.copy_from_slice(&v.to_le_bytes()[..data.len()]);
                }
                Ok(VcpuExit::MmioWrite(addr, data)) => {
                    bus.write(addr, data.len(), le(data)).map_err(|e| cpu_err(e.to_string()))?;
                }
                Ok(VcpuExit::Shutdown) => {
                    return Err(cpu_err("guest triple-faulted (no IDT is installed)"));
                }
                Ok(other) => return Err(cpu_err(format!("unhandled VM exit: {other:?}"))),
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                    return Ok(Exit::Budget);
                }
                Err(e) => return Err(cpu_err(format!("KVM_RUN failed: {e}"))),
            }
        }
    }
}

fn le(data: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b[..data.len()].copy_from_slice(data);
    u64::from_le_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_machine::PhysMem;

    #[test]
    fn gdt_has_flat_code_and_data() {
        let g = flat_gdt();
        assert_eq!(&g[..8], &[0; 8]);
        assert_eq!(u64::from_le_bytes(g[8..16].try_into().unwrap()) >> 40 & 0xFF, 0x9A);
        assert_eq!(u64::from_le_bytes(g[16..24].try_into().unwrap()) >> 40 & 0xFF, 0x92);
    }

    /// Needs a KVM host: `cargo test -p ferrox-kvm -- --ignored`
    #[test]
    #[ignore]
    fn runs_native_code_and_reports_a_hypercall() {
        assert!(available(), "no /dev/kvm on this machine");
        let mut mem = PhysMem::new();
        mem.add_ram(0, 1 << 20).unwrap();
        // mov eax, 77 ; out 0xE0, eax ; hlt
        mem.load(0x1000, &[0xB8, 77, 0, 0, 0, 0xE7, 0xE0, 0xF4]).unwrap();
        mem.load(0x800, &flat_gdt()).unwrap();
        let slots = mem.mem_slots();
        let mut cpu = unsafe { KvmCpu::new(&slots, 0x1000, 0x800) }.unwrap();
        let mut r = cpu.regs();
        r.gpr[4] = 0x8000;
        cpu.set_regs(r);
        assert_eq!(cpu.run(&mut mem, 0).unwrap(), Exit::Hypercall(77));
        assert_eq!(cpu.run(&mut mem, 0).unwrap(), Exit::Halt);
    }
}
