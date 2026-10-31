//! Windows backend: the guest CPU runs natively under the Windows Hypervisor
//! Platform (WHP), the same facility Hyper-V and WSL2 use. Only port I/O and
//! kernel calls leave the VM.
//!
//! Requires the optional Windows feature "Windows Hypervisor Platform"
//! (Settings -> Apps -> Optional features -> More Windows features), then a
//! restart. The library is loaded at run time, so a machine without it can still
//! start the app; `available()` simply returns false.
//!
//! Limits today: memory-mapped I/O needs instruction emulation, which is not
//! written, so a guest touching an unmapped address stops with a clear error. The
//! no-BIOS kernel mode never does that.
//!
//! Status: compiled in CI but **not yet run** by its author (no Windows machine
//! was available while writing it). Treat it as untested.

#![cfg(windows)]

use ferrox_core::Bus;
use ferrox_cpu::{CpuBackend, CpuError, Exit, Regs, HLE_PORT};
use ferrox_machine::MemSlot;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{s, w, HRESULT};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Hypervisor::*;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// Prints progress to stderr when `FERROX_TRACE` is set, so a crash inside the
/// hypervisor setup can be located from the last line printed.
fn trace(msg: &str) {
    if std::env::var_os("FERROX_TRACE").is_some() {
        eprintln!("[whpx] {msg}");
    }
}

pub const CODE_SEL: u16 = 0x08;
pub const DATA_SEL: u16 = 0x10;

type P = WHV_PARTITION_HANDLE;

/// The WHP entry points, looked up when first needed.
struct Api {
    create_partition: unsafe extern "system" fn(*mut P) -> HRESULT,
    delete_partition: unsafe extern "system" fn(P) -> HRESULT,
    set_property: unsafe extern "system" fn(P, WHV_PARTITION_PROPERTY_CODE, *const c_void, u32) -> HRESULT,
    setup_partition: unsafe extern "system" fn(P) -> HRESULT,
    map_gpa: unsafe extern "system" fn(P, *const c_void, u64, u64, WHV_MAP_GPA_RANGE_FLAGS) -> HRESULT,
    create_vp: unsafe extern "system" fn(P, u32, u32) -> HRESULT,
    delete_vp: unsafe extern "system" fn(P, u32) -> HRESULT,
    run_vp: unsafe extern "system" fn(P, u32, *mut c_void, u32) -> HRESULT,
    set_regs: unsafe extern "system" fn(P, u32, *const WHV_REGISTER_NAME, u32, *const WHV_REGISTER_VALUE) -> HRESULT,
    get_regs: unsafe extern "system" fn(P, u32, *const WHV_REGISTER_NAME, u32, *mut WHV_REGISTER_VALUE) -> HRESULT,
    cancel_run: unsafe extern "system" fn(P, u32, u32) -> HRESULT,
    get_capability: unsafe extern "system" fn(WHV_CAPABILITY_CODE, *mut c_void, u32, *mut u32) -> HRESULT,
}

fn load_api() -> Option<Api> {
    // SAFETY: standard dynamic loading; each pointer is cast to the documented
    // signature of the function it was looked up by.
    unsafe {
        let lib: HMODULE = LoadLibraryW(w!("WinHvPlatform.dll")).ok()?;
        macro_rules! f {
            ($name:literal) => {
                std::mem::transmute(GetProcAddress(lib, s!($name))?)
            };
        }
        Some(Api {
            create_partition: f!("WHvCreatePartition"),
            delete_partition: f!("WHvDeletePartition"),
            set_property: f!("WHvSetPartitionProperty"),
            setup_partition: f!("WHvSetupPartition"),
            map_gpa: f!("WHvMapGpaRange"),
            create_vp: f!("WHvCreateVirtualProcessor"),
            delete_vp: f!("WHvDeleteVirtualProcessor"),
            run_vp: f!("WHvRunVirtualProcessor"),
            set_regs: f!("WHvSetVirtualProcessorRegisters"),
            get_regs: f!("WHvGetVirtualProcessorRegisters"),
            cancel_run: f!("WHvCancelRunVirtualProcessor"),
            get_capability: f!("WHvGetCapability"),
        })
    }
}

/// Is the Windows Hypervisor Platform installed, enabled and running?
pub fn available() -> bool {
    trace("loading WinHvPlatform.dll");
    let Some(api) = load_api() else {
        trace("WinHvPlatform.dll or one of its functions is missing");
        return false;
    };
    trace("asking whether the hypervisor is present");
    let mut cap = WHV_CAPABILITY::default();
    // SAFETY: cap is a valid out buffer of the size we pass.
    let hr = unsafe {
        (api.get_capability)(
            WHvCapabilityCodeHypervisorPresent,
            &mut cap as *mut _ as *mut c_void,
            std::mem::size_of::<WHV_CAPABILITY>() as u32,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: HypervisorPresent is the field this capability code fills in.
    let present = hr.is_ok() && unsafe { cap.HypervisorPresent.as_bool() };
    trace(&format!("capability result {hr:?}, present = {present}"));
    present
}

/// Same three-entry GDT as the KVM backend (null, flat code, flat data).
pub fn flat_gdt() -> [u8; 24] {
    let mut g = [0u8; 24];
    g[8..16].copy_from_slice(&0x00CF_9A00_0000_FFFFu64.to_le_bytes());
    g[16..24].copy_from_slice(&0x00CF_9200_0000_FFFFu64.to_le_bytes());
    g
}

#[derive(Debug)]
pub struct WhpError(pub String);

impl std::fmt::Display for WhpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Windows Hypervisor Platform: {}", self.0)
    }
}

impl std::error::Error for WhpError {}

fn check(hr: HRESULT, what: &str) -> Result<(), WhpError> {
    if hr.is_ok() { Ok(()) } else { Err(WhpError(format!("{what} failed ({hr:?})"))) }
}

struct Watchdog {
    running: AtomicBool,
    quit: AtomicBool,
    ticks: AtomicUsize,
    slice_ticks: AtomicUsize,
}

const TICK: Duration = Duration::from_millis(2);

pub struct WhpCpu {
    api: Arc<Api>,
    part: P,
    wd: Arc<Watchdog>,
    thread: Option<std::thread::JoinHandle<()>>,
}

// SAFETY: the partition handle is only used from one thread at a time (plus the
// cancel call from the watchdog, which WHP documents as safe to call concurrently).
unsafe impl Send for WhpCpu {}

fn seg(selector: u16, attrs: u16) -> WHV_REGISTER_VALUE {
    let mut s = WHV_X64_SEGMENT_REGISTER::default();
    s.Base = 0;
    s.Limit = 0xFFFF_FFFF;
    s.Selector = selector;
    s.Anonymous.Attributes = attrs;
    WHV_REGISTER_VALUE { Segment: s }
}

fn r64(v: u64) -> WHV_REGISTER_VALUE {
    WHV_REGISTER_VALUE { Reg64: v }
}

impl WhpCpu {
    /// Creates a partition that maps `slots`, with the vcpu at `entry` in flat
    /// 32-bit protected mode. `gdt_addr` is where [`flat_gdt`] sits in guest memory.
    ///
    /// # Safety
    /// Each slot's `host_ptr` must stay valid for the life of the returned value,
    /// and the host must not touch that memory (through `Bus`) while the guest runs.
    pub unsafe fn new(slots: &[MemSlot], entry: u32, gdt_addr: u32) -> Result<Self, WhpError> {
        let api = Arc::new(load_api().ok_or_else(|| {
            WhpError("not installed (turn on the 'Windows Hypervisor Platform' optional feature and restart)".into())
        })?);
        trace("creating partition");
        let mut part = P::default();
        check((api.create_partition)(&mut part), "create partition")?;
        trace("partition created");
        let this_fail = |api: &Api, part: P, e: WhpError| {
            (api.delete_partition)(part);
            e
        };
        let count = WHV_PARTITION_PROPERTY { ProcessorCount: 1 };
        if let Err(e) = check(
            (api.set_property)(
                part,
                WHvPartitionPropertyCodeProcessorCount,
                &count as *const _ as *const c_void,
                std::mem::size_of::<u32>() as u32,
            ),
            "set processor count",
        ) {
            return Err(this_fail(&api, part, e));
        }
        trace("setting up partition");
        if let Err(e) = check((api.setup_partition)(part), "set up partition") {
            return Err(this_fail(&api, part, e));
        }
        for s in slots {
            trace(&format!("mapping {:#x}+{:#x}", s.guest_addr, s.size));
            if s.guest_addr % 4096 != 0 || s.size % 4096 != 0 || s.host_ptr as usize % 4096 != 0 {
                return Err(this_fail(&api, part, WhpError(format!("memory region {:#x}+{:#x} is not page aligned", s.guest_addr, s.size))));
            }
            let flags = if s.read_only {
                WHV_MAP_GPA_RANGE_FLAGS(WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagExecute.0)
            } else {
                WHV_MAP_GPA_RANGE_FLAGS(WHvMapGpaRangeFlagRead.0 | WHvMapGpaRangeFlagWrite.0 | WHvMapGpaRangeFlagExecute.0)
            };
            if let Err(e) = check((api.map_gpa)(part, s.host_ptr as *const c_void, s.guest_addr, s.size, flags), "map memory") {
                return Err(this_fail(&api, part, e));
            }
        }
        trace("creating virtual processor");
        if let Err(e) = check((api.create_vp)(part, 0, 0), "create virtual processor") {
            return Err(this_fail(&api, part, e));
        }

        // code: type 0xB, non-system, present, 32-bit default size, 4 KiB granularity
        let code = 0xB | 1 << 4 | 1 << 7 | 1 << 14 | 1 << 15;
        let data = 0x3 | 1 << 4 | 1 << 7 | 1 << 14 | 1 << 15;
        let gdtr = WHV_X64_TABLE_REGISTER { Pad: [0; 3], Limit: 23, Base: gdt_addr as u64 };
        let names = [
            WHvX64RegisterCs, WHvX64RegisterDs, WHvX64RegisterEs, WHvX64RegisterFs,
            WHvX64RegisterGs, WHvX64RegisterSs, WHvX64RegisterGdtr, WHvX64RegisterCr0,
            WHvX64RegisterRip, WHvX64RegisterRflags,
        ];
        let values = [
            seg(CODE_SEL, code), seg(DATA_SEL, data), seg(DATA_SEL, data), seg(DATA_SEL, data),
            seg(DATA_SEL, data), seg(DATA_SEL, data),
            WHV_REGISTER_VALUE { Table: gdtr },
            r64(0x11), // PE | ET, no paging
            r64(entry as u64),
            r64(2),
        ];
        trace("setting initial registers");
        if let Err(e) = check(
            (api.set_regs)(part, 0, names.as_ptr(), names.len() as u32, values.as_ptr()),
            "set initial registers",
        ) {
            return Err(this_fail(&api, part, e));
        }

        let wd = Arc::new(Watchdog {
            running: AtomicBool::new(false),
            quit: AtomicBool::new(false),
            ticks: AtomicUsize::new(0),
            slice_ticks: AtomicUsize::new(5),
        });
        let (w, a, handle) = (wd.clone(), api.clone(), part.0);
        let thread = std::thread::spawn(move || {
            while !w.quit.load(Ordering::Relaxed) {
                std::thread::sleep(TICK);
                if !w.running.load(Ordering::Acquire) {
                    continue;
                }
                if w.ticks.fetch_add(1, Ordering::Relaxed) + 1 >= w.slice_ticks.load(Ordering::Relaxed) {
                    w.ticks.store(0, Ordering::Relaxed);
                    // SAFETY: cancelling a run is allowed from another thread.
                    unsafe { (a.cancel_run)(WHV_PARTITION_HANDLE(handle), 0, 0) };
                }
            }
        });
        trace("ready");
        Ok(Self { api, part, wd, thread: Some(thread) })
    }

    /// How long the guest may run without leaving the VM before `run` returns
    /// `Exit::Budget`. Rounded to 2 ms ticks.
    pub fn set_slice(&mut self, d: Duration) {
        let ticks = (d.as_millis() / TICK.as_millis()).max(1) as usize;
        self.wd.slice_ticks.store(ticks, Ordering::Relaxed);
    }

    fn set_one(&self, name: WHV_REGISTER_NAME, v: u64) {
        let value = r64(v);
        // SAFETY: one valid name/value pair.
        unsafe { (self.api.set_regs)(self.part, 0, &name, 1, &value) };
    }
}

impl Drop for WhpCpu {
    fn drop(&mut self) {
        self.wd.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the processor and partition were created in `new`.
        unsafe {
            (self.api.delete_vp)(self.part, 0);
            (self.api.delete_partition)(self.part);
        }
    }
}

const GPRS: [WHV_REGISTER_NAME; 8] = [
    WHvX64RegisterRax, WHvX64RegisterRcx, WHvX64RegisterRdx, WHvX64RegisterRbx,
    WHvX64RegisterRsp, WHvX64RegisterRbp, WHvX64RegisterRsi, WHvX64RegisterRdi,
];

fn cpu_err(m: impl Into<String>) -> CpuError {
    CpuError::Backend(m.into())
}

impl CpuBackend for WhpCpu {
    fn name(&self) -> &'static str {
        "windows hypervisor platform"
    }

    fn regs(&self) -> Regs {
        let names: Vec<WHV_REGISTER_NAME> = GPRS.iter().copied().chain([WHvX64RegisterRip, WHvX64RegisterRflags]).collect();
        let mut vals = vec![WHV_REGISTER_VALUE::default(); names.len()];
        // SAFETY: names and vals have the same length.
        unsafe { (self.api.get_regs)(self.part, 0, names.as_ptr(), names.len() as u32, vals.as_mut_ptr()) };
        // SAFETY: every register read here is a 64-bit value.
        let g = |i: usize| unsafe { vals[i].Reg64 };
        Regs {
            gpr: std::array::from_fn(|i| g(i) as u32),
            eip: g(8) as u32,
            zf: g(9) & 0x40 != 0,
        }
    }

    fn set_regs(&mut self, regs: Regs) {
        let flags_now = {
            let name = WHvX64RegisterRflags;
            let mut v = WHV_REGISTER_VALUE::default();
            // SAFETY: one valid name and value slot.
            unsafe { (self.api.get_regs)(self.part, 0, &name, 1, &mut v) };
            // SAFETY: rflags is a 64-bit value.
            unsafe { v.Reg64 }
        };
        let names: Vec<WHV_REGISTER_NAME> = GPRS.iter().copied().chain([WHvX64RegisterRip, WHvX64RegisterRflags]).collect();
        let mut vals: Vec<WHV_REGISTER_VALUE> = regs.gpr.iter().map(|&v| r64(v as u64)).collect();
        vals.push(r64(regs.eip as u64));
        vals.push(r64((flags_now & !0x40) | if regs.zf { 0x40 } else { 0 }));
        // SAFETY: names and vals have the same length.
        unsafe { (self.api.set_regs)(self.part, 0, names.as_ptr(), names.len() as u32, vals.as_ptr()) };
    }

    /// Runs until the guest halts, calls the host, or has run one time slice.
    /// `max_instructions` is ignored: hardware virtualization cannot count them.
    fn run(&mut self, bus: &mut dyn Bus, _max_instructions: u64) -> Result<Exit, CpuError> {
        loop {
            trace("running guest");
            let mut ctx = WHV_RUN_VP_EXIT_CONTEXT::default();
            self.wd.ticks.store(0, Ordering::Relaxed);
            self.wd.running.store(true, Ordering::Release);
            // SAFETY: ctx is a valid exit-context buffer of the size passed.
            let hr = unsafe {
                (self.api.run_vp)(
                    self.part,
                    0,
                    &mut ctx as *mut _ as *mut c_void,
                    std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
                )
            };
            self.wd.running.store(false, Ordering::Release);
            if !hr.is_ok() {
                return Err(cpu_err(format!("WHvRunVirtualProcessor failed ({hr:?})")));
            }
            let reason = ctx.ExitReason;
            trace(&format!("guest exited, reason {:#x}", reason.0));
            if reason == WHvRunVpExitReasonX64IoPortAccess {
                // SAFETY: the exit reason says this union member is the valid one.
                let io = unsafe { ctx.Anonymous.IoPortAccess };
                // SAFETY: reading the raw bitfield word of a plain-data union.
                let info = unsafe { io.AccessInfo.AsUINT32 };
                let is_write = info & 1 != 0;
                let size = ((info >> 1) & 7) as usize;
                if info & (3 << 4) != 0 {
                    return Err(cpu_err("string I/O instructions are not supported yet"));
                }
                if !matches!(size, 1 | 2 | 4) {
                    return Err(cpu_err("unexpected port access size"));
                }
                let next_rip = ctx.VpContext.Rip + (ctx.VpContext._bitfield & 0xF) as u64;
                let mask = if size == 4 { u32::MAX as u64 } else { (1u64 << (8 * size)) - 1 };
                if is_write {
                    let v = io.Rax & mask;
                    self.set_one(WHvX64RegisterRip, next_rip);
                    if io.PortNumber == HLE_PORT {
                        return Ok(Exit::Hypercall(v as u32));
                    }
                    bus.io_out(io.PortNumber, size, v);
                } else {
                    let v = bus.io_in(io.PortNumber, size) & mask;
                    let rax = if size == 4 { v } else { (io.Rax & !mask) | v };
                    self.set_one(WHvX64RegisterRax, rax);
                    self.set_one(WHvX64RegisterRip, next_rip);
                }
            } else if reason == WHvRunVpExitReasonX64Halt {
                return Ok(Exit::Halt);
            } else if reason == WHvRunVpExitReasonCanceled {
                return Ok(Exit::Budget);
            } else if reason == WHvRunVpExitReasonMemoryAccess {
                // SAFETY: the exit reason says this union member is the valid one.
                let gpa = unsafe { ctx.Anonymous.MemoryAccess.Gpa };
                return Err(cpu_err(format!(
                    "guest touched unmapped address {gpa:#x}; memory-mapped devices are not supported by this backend yet"
                )));
            } else if reason == WHvRunVpExitReasonUnrecoverableException {
                return Err(cpu_err("guest crashed (unrecoverable exception, no IDT installed)"));
            } else {
                return Err(cpu_err(format!("unhandled VM exit (reason {:#x})", reason.0)));
            }
        }
    }
}
