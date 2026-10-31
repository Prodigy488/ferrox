//! Windows backend: the guest CPU runs natively under the Windows Hypervisor
//! Platform (WHP), the same facility Hyper-V and WSL2 use. Only port I/O and
//! kernel calls leave the VM.
//!
//! Requires the optional Windows feature "Windows Hypervisor Platform"
//! (Settings -> Apps -> Optional features -> More Windows features), then a
//! restart. The library is loaded at run time, so a machine without it can still
//! start the app; `available()` simply returns false.
//!
//! Memory-mapped hardware (the GPU's registers, later audio and USB) is served by
//! Microsoft's instruction emulator in WinHvEmulation.dll, loaded on first use.
//! String I/O instructions are not supported yet.
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

/// Microsoft's instruction emulator (WinHvEmulation.dll): decodes the
/// instruction behind a memory-mapped I/O exit and calls back to do the access.
struct EmuApi {
    create: unsafe extern "system" fn(*const WHV_EMULATOR_CALLBACKS, *mut *mut c_void) -> HRESULT,
    destroy: unsafe extern "system" fn(*const c_void) -> HRESULT,
    try_mmio: unsafe extern "system" fn(*const c_void, *const c_void, *const WHV_VP_EXIT_CONTEXT, *const WHV_MEMORY_ACCESS_CONTEXT, *mut WHV_EMULATOR_STATUS) -> HRESULT,
}

fn load_emu_api() -> Option<EmuApi> {
    // SAFETY: as in `load_api`.
    unsafe {
        let lib: HMODULE = LoadLibraryW(w!("WinHvEmulation.dll")).ok()?;
        Some(EmuApi {
            create: std::mem::transmute(GetProcAddress(lib, s!("WHvEmulatorCreateEmulator"))?),
            destroy: std::mem::transmute(GetProcAddress(lib, s!("WHvEmulatorDestroyEmulator"))?),
            try_mmio: std::mem::transmute(GetProcAddress(lib, s!("WHvEmulatorTryMmioEmulation"))?),
        })
    }
}

/// What the emulator's callbacks need while one access is being emulated.
struct EmuCtx {
    api: *const Api,
    part: P,
    bus: *mut (dyn Bus + 'static),
    error: Option<String>,
}

const S_OK: HRESULT = HRESULT(0);
const E_FAIL: HRESULT = HRESULT(0x8000_4005u32 as i32);

unsafe extern "system" fn cb_memory(ctx: *const c_void, info: *mut WHV_EMULATOR_MEMORY_ACCESS_INFO) -> HRESULT {
    let c = &mut *(ctx as *mut EmuCtx);
    let i = &mut *info;
    let size = i.AccessSize as usize;
    let bus = &mut *c.bus;
    let r = if i.Direction == 0 {
        bus.read(i.GpaAddress, size).map(|v| i.Data[..size].copy_from_slice(&v.to_le_bytes()[..size]))
    } else {
        let mut b = [0u8; 8];
        b[..size].copy_from_slice(&i.Data[..size]);
        bus.write(i.GpaAddress, size, u64::from_le_bytes(b))
    };
    match r {
        Ok(()) => S_OK,
        Err(e) => {
            c.error = Some(e.to_string());
            E_FAIL
        }
    }
}

unsafe extern "system" fn cb_io(ctx: *const c_void, info: *mut WHV_EMULATOR_IO_ACCESS_INFO) -> HRESULT {
    let c = &mut *(ctx as *mut EmuCtx);
    let i = &mut *info;
    let bus = &mut *c.bus;
    if i.Direction == 0 {
        i.Data = bus.io_in(i.Port, i.AccessSize as usize) as u32;
    } else {
        bus.io_out(i.Port, i.AccessSize as usize, i.Data as u64);
    }
    S_OK
}

unsafe extern "system" fn cb_get_regs(ctx: *const c_void, names: *const WHV_REGISTER_NAME, count: u32, values: *mut WHV_REGISTER_VALUE) -> HRESULT {
    let c = &*(ctx as *const EmuCtx);
    let mut tmp = vec![Val(WHV_REGISTER_VALUE::default()); count as usize];
    let hr = ((*c.api).get_regs)(c.part, 0, names, count, tmp.as_mut_ptr() as *mut WHV_REGISTER_VALUE);
    std::ptr::copy_nonoverlapping(tmp.as_ptr() as *const u8, values as *mut u8, count as usize * 16);
    hr
}

unsafe extern "system" fn cb_set_regs(ctx: *const c_void, names: *const WHV_REGISTER_NAME, count: u32, values: *const WHV_REGISTER_VALUE) -> HRESULT {
    let c = &*(ctx as *const EmuCtx);
    let mut tmp = vec![Val(WHV_REGISTER_VALUE::default()); count as usize];
    std::ptr::copy_nonoverlapping(values as *const u8, tmp.as_mut_ptr() as *mut u8, count as usize * 16);
    ((*c.api).set_regs)(c.part, 0, names, count, tmp.as_ptr() as *const WHV_REGISTER_VALUE)
}

unsafe extern "system" fn cb_translate(_: *const c_void, gva: u64, _: WHV_TRANSLATE_GVA_FLAGS, result: *mut WHV_TRANSLATE_GVA_RESULT_CODE, gpa: *mut u64) -> HRESULT {
    // paging is off, so virtual == physical
    *result = WHvTranslateGvaResultSuccess;
    *gpa = gva;
    S_OK
}

pub struct WhpCpu {
    api: Arc<Api>,
    part: P,
    wd: Arc<Watchdog>,
    thread: Option<std::thread::JoinHandle<()>>,
    emu: Option<(EmuApi, *mut c_void)>,
}

// SAFETY: the partition handle is only used from one thread at a time (plus the
// cancel call from the watchdog, which WHP documents as safe to call concurrently).
unsafe impl Send for WhpCpu {}

/// `WHV_REGISTER_VALUE` must be 16-byte aligned (the C header says so; the Rust
/// binding only guarantees 8, which made the API fault on its first call).
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Val(WHV_REGISTER_VALUE);

const _: () = assert!(std::mem::size_of::<Val>() == 16 && std::mem::align_of::<Val>() == 16);

fn seg(selector: u16, attrs: u16) -> Val {
    let mut s = WHV_X64_SEGMENT_REGISTER::default();
    s.Base = 0;
    s.Limit = 0xFFFF_FFFF;
    s.Selector = selector;
    s.Anonymous.Attributes = attrs;
    Val(WHV_REGISTER_VALUE { Segment: s })
}

fn r64(v: u64) -> Val {
    let mut x = WHV_REGISTER_VALUE::default();
    x.Reg64 = v;
    Val(x)
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
            Val(WHV_REGISTER_VALUE { Table: gdtr }),
            r64(0x11), // PE | ET, no paging
            r64(entry as u64),
            r64(2),
        ];
        trace("setting initial registers");
        if let Err(e) = check(
            (api.set_regs)(part, 0, names.as_ptr(), names.len() as u32, values.as_ptr() as *const WHV_REGISTER_VALUE),
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
        Ok(Self { api, part, wd, thread: Some(thread), emu: None })
    }

    /// How long the guest may run without leaving the VM before `run` returns
    /// `Exit::Budget`. Rounded to 2 ms ticks.
    pub fn set_slice(&mut self, d: Duration) {
        let ticks = (d.as_millis() / TICK.as_millis()).max(1) as usize;
        self.wd.slice_ticks.store(ticks, Ordering::Relaxed);
    }

    /// Emulates the instruction behind a memory-mapped I/O exit.
    fn emulate_mmio(
        &mut self,
        bus: &mut dyn Bus,
        vp: &WHV_VP_EXIT_CONTEXT,
        mem: &WHV_MEMORY_ACCESS_CONTEXT,
    ) -> Result<(), CpuError> {
        if self.emu.is_none() {
            trace("loading WinHvEmulation.dll");
            let api = load_emu_api().ok_or_else(|| {
                cpu_err("WinHvEmulation.dll is missing, so memory-mapped hardware cannot be emulated")
            })?;
            let callbacks = WHV_EMULATOR_CALLBACKS {
                Size: std::mem::size_of::<WHV_EMULATOR_CALLBACKS>() as u32,
                Reserved: 0,
                WHvEmulatorIoPortCallback: Some(cb_io),
                WHvEmulatorMemoryCallback: Some(cb_memory),
                WHvEmulatorGetVirtualProcessorRegisters: Some(cb_get_regs),
                WHvEmulatorSetVirtualProcessorRegisters: Some(cb_set_regs),
                WHvEmulatorTranslateGvaPage: Some(cb_translate),
            };
            let mut handle: *mut c_void = std::ptr::null_mut();
            // SAFETY: callbacks and out pointer are valid for the call.
            let hr = unsafe { (api.create)(&callbacks, &mut handle) };
            if !hr.is_ok() {
                return Err(cpu_err(format!("could not create the instruction emulator ({hr:?})")));
            }
            self.emu = Some((api, handle));
        }
        let (api, handle) = self.emu.as_ref().expect("created above");
        // SAFETY: the bus outlives this call; the emulator only calls back during it.
        let bus_ptr: *mut (dyn Bus + 'static) = unsafe { std::mem::transmute(bus as *mut dyn Bus) };
        let mut ctx = EmuCtx { api: &*self.api, part: self.part, bus: bus_ptr, error: None };
        let mut status = WHV_EMULATOR_STATUS::default();
        // SAFETY: all pointers are valid for the call; ctx is only used by our callbacks.
        let hr = unsafe {
            (api.try_mmio)(*handle, &mut ctx as *mut EmuCtx as *const c_void, vp, mem, &mut status)
        };
        // SAFETY: AsUINT32 reads the plain bitfield word.
        let bits = unsafe { status.AsUINT32 };
        if !hr.is_ok() || bits & 1 == 0 {
            let why = ctx.error.unwrap_or_else(|| format!("emulator status {bits:#x}, result {hr:?}"));
            return Err(cpu_err(format!(
                "access to {:#x} could not be emulated: {why}",
                mem.Gpa
            )));
        }
        Ok(())
    }

    fn set_one(&self, name: WHV_REGISTER_NAME, v: u64) {
        let value = r64(v);
        // SAFETY: one valid name/value pair.
        unsafe { (self.api.set_regs)(self.part, 0, &name, 1, &value as *const Val as *const WHV_REGISTER_VALUE) };
    }
}

impl Drop for WhpCpu {
    fn drop(&mut self) {
        self.wd.quit.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some((api, handle)) = self.emu.take() {
            // SAFETY: created by `create` above.
            unsafe { (api.destroy)(handle) };
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
        let mut vals = vec![Val(WHV_REGISTER_VALUE::default()); names.len()];
        // SAFETY: names and vals have the same length.
        unsafe { (self.api.get_regs)(self.part, 0, names.as_ptr(), names.len() as u32, vals.as_mut_ptr() as *mut WHV_REGISTER_VALUE) };
        // SAFETY: every register read here is a 64-bit value.
        let g = |i: usize| unsafe { vals[i].0.Reg64 };
        Regs {
            gpr: std::array::from_fn(|i| g(i) as u32),
            eip: g(8) as u32,
            zf: g(9) & 0x40 != 0,
        }
    }

    fn set_regs(&mut self, regs: Regs) {
        let flags_now = {
            let name = WHvX64RegisterRflags;
            let mut v = Val(WHV_REGISTER_VALUE::default());
            // SAFETY: one valid name and a 16-byte-aligned value slot.
            unsafe { (self.api.get_regs)(self.part, 0, &name, 1, &mut v as *mut Val as *mut WHV_REGISTER_VALUE) };
            // SAFETY: rflags is a 64-bit value.
            unsafe { v.0.Reg64 }
        };
        let names: Vec<WHV_REGISTER_NAME> = GPRS.iter().copied().chain([WHvX64RegisterRip, WHvX64RegisterRflags]).collect();
        let mut vals: Vec<Val> = regs.gpr.iter().map(|&v| r64(v as u64)).collect();
        vals.push(r64(regs.eip as u64));
        vals.push(r64((flags_now & !0x40) | if regs.zf { 0x40 } else { 0 }));
        // SAFETY: names and vals have the same length.
        unsafe { (self.api.set_regs)(self.part, 0, names.as_ptr(), names.len() as u32, vals.as_ptr() as *const WHV_REGISTER_VALUE) };
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
                let access = unsafe { ctx.Anonymous.MemoryAccess };
                trace(&format!("memory-mapped access at {:#x}", access.Gpa));
                self.emulate_mmio(bus, &ctx.VpContext, &access)?;
            } else if reason == WHvRunVpExitReasonUnrecoverableException {
                return Err(cpu_err("guest crashed (unrecoverable exception, no IDT installed)"));
            } else {
                return Err(cpu_err(format!("unhandled VM exit (reason {:#x})", reason.0)));
            }
        }
    }
}
