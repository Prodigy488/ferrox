//! Threads, waiting, events, semaphores, mutants, timers, deferred procedure calls and the
//! interrupt level: the part of the kernel games use to run several things at once.
//!
//! The objects live in guest memory with the real layouts (see `sched`); these handlers are
//! thin wrappers that read the arguments, call the scheduler and translate the result into the
//! status codes the kernel returns.

use crate::kernel::{KernelState, STATUS_SUCCESS};
use crate::sched::*;
use crate::{Call, KernelHle};
use std::cell::RefCell;
use std::rc::Rc;

/// KeQueryPerformanceFrequency's value: the console's 733 MHz clock.
const PERF_FREQUENCY: u64 = 733_333_333;
/// What the system clock read at the start of the run (a plausible date in 2003, in 100 ns
/// units since 1601).
const BOOT_TIME: u64 = 0x01C3_5B7A_0000_0000;
const DEFAULT_STACK: u32 = 0x10000;

fn timeout(c: &mut Call, ptr: u32) -> Option<i64> {
    if ptr == 0 {
        return None;
    }
    let lo = c.read_u32(ptr).unwrap_or(0) as u64;
    let hi = c.read_u32(ptr + 4).unwrap_or(0) as u64;
    Some((hi << 32 | lo) as i64)
}

fn arg(c: &mut Call, i: u32) -> u32 {
    c.arg(i).unwrap_or(0)
}

fn put(c: &mut Call, ptr: u32, v: u32) {
    if ptr != 0 {
        let _ = c.write_u32(ptr, v);
    }
}

fn wait_status(c: &mut Call, objects: Vec<u32>, all: bool, t: Option<i64>) -> u32 {
    c.sched.wait(c.bus, objects, all, t)
}

fn pointer_list(c: &mut Call, list: u32, count: u32) -> Vec<u32> {
    (0..count.min(64)).map(|i| c.read_u32(list + 4 * i).unwrap_or(0)).collect()
}

fn object(c: &Call, handle: u32) -> Option<u32> {
    c.sched.object_of(handle)
}

fn event_kind(t: u32) -> u8 {
    if t == 1 { EVENT_SYNCHRONIZATION } else { EVENT_NOTIFICATION }
}

pub fn install(hle: &mut KernelHle, st: Rc<RefCell<KernelState>>) {
    // ---- time ----
    hle.handle("KeQueryPerformanceFrequency", |c| {
        c.regs.gpr[ferrox_cpu::EDX] = (PERF_FREQUENCY >> 32) as u32;
        PERF_FREQUENCY as u32
    });
    hle.handle("KeQueryPerformanceCounter", |c| {
        c.sched.nudge();
        let ticks = (c.sched.now() as u128 * PERF_FREQUENCY as u128 / 10_000_000) as u64;
        c.regs.gpr[ferrox_cpu::EDX] = (ticks >> 32) as u32;
        ticks as u32
    });
    hle.handle("KeQuerySystemTime", |c| {
        let out = arg(c, 0);
        c.sched.nudge();
        let t = BOOT_TIME + c.sched.now();
        put(c, out, t as u32);
        put(c, out + 4, (t >> 32) as u32);
        0
    });
    hle.handle("KeQueryInterruptTime", |c| {
        c.sched.nudge();
        let t = c.sched.now();
        c.regs.gpr[ferrox_cpu::EDX] = (t >> 32) as u32;
        t as u32
    });
    hle.handle("KeStallExecutionProcessor", |c| {
        // microseconds of busy waiting: let the clock move
        let us = arg(c, 0) as u64;
        c.sched.nudge();
        let _ = us;
        0
    });

    // ---- the interrupt level ----
    hle.handle("KeGetCurrentIrql", |c| c.sched.irql(c.bus) as u32);
    hle.handle("KfRaiseIrql", |c| {
        let new = c.regs.gpr[ferrox_cpu::ECX] as u8;
        let old = c.sched.irql(c.bus);
        c.sched.set_irql(c.bus, new);
        old as u32
    });
    hle.handle("KfLowerIrql", |c| {
        let new = c.regs.gpr[ferrox_cpu::ECX] as u8;
        c.sched.set_irql(c.bus, new);
        0
    });
    for name in ["KeRaiseIrqlToDpcLevel", "KeRaiseIrqlToSynchLevel"] {
        hle.handle(name, |c| {
            let old = c.sched.irql(c.bus);
            c.sched.set_irql(c.bus, DISPATCH_LEVEL);
            old as u32
        });
    }
    hle.handle("KeIsExecutingDpc", |_| 0);
    hle.handle("KeEnterCriticalRegion", |_| 0);
    hle.handle("KeLeaveCriticalRegion", |_| 0);
    hle.handle("HalClearSoftwareInterrupt", |_| 0);
    hle.handle("HalRequestSoftwareInterrupt", |_| 0);

    // ---- events ----
    hle.handle("KeInitializeEvent", |c| {
        let (ev, kind, state) = (arg(c, 0), arg(c, 1), arg(c, 2));
        init_header(c.bus, ev, event_kind(kind), 16, (state != 0) as i32);
        0
    });
    hle.handle("KeSetEvent", |c| {
        let ev = arg(c, 0);
        c.sched.set_signal(c.bus, ev, 1) as u32
    });
    hle.handle("KeResetEvent", |c| {
        let ev = arg(c, 0);
        c.sched.set_signal(c.bus, ev, 0) as u32
    });
    hle.handle("KePulseEvent", |c| {
        let ev = arg(c, 0);
        c.sched.pulse(c.bus, ev) as u32
    });

    // ---- semaphores and mutants ----
    hle.handle("KeInitializeSemaphore", |c| {
        let (sem, count, limit) = (arg(c, 0), arg(c, 1), arg(c, 2));
        init_header(c.bus, sem, SEMAPHORE, 0x14, count as i32);
        let _ = c.write_u32(sem + 0x10, limit);
        0
    });
    hle.handle("KeReleaseSemaphore", |c| {
        let (sem, adjust) = (arg(c, 0), arg(c, 2));
        c.sched.release_semaphore(c.bus, sem, adjust).unwrap_or(0) as u32
    });
    hle.handle("KeInitializeMutant", |c| {
        let (m, owned) = (arg(c, 0), arg(c, 1));
        let me = c.sched.current_kthread();
        init_header(c.bus, m, MUTANT, 0x20, if owned != 0 { 0 } else { 1 });
        let _ = c.write_u32(m + 0x18, if owned != 0 { me } else { 0 });
        0
    });
    hle.handle("KeReleaseMutant", |c| {
        let m = arg(c, 0);
        c.sched.release_mutant(c.bus, m).unwrap_or(0) as u32
    });

    // ---- timers and deferred procedure calls ----
    hle.handle("KeInitializeTimerEx", |c| {
        let (t, kind) = (arg(c, 0), arg(c, 1));
        let k = if kind == 1 { TIMER_SYNCHRONIZATION } else { TIMER_NOTIFICATION };
        init_header(c.bus, t, k, 0x28, 0);
        0
    });
    hle.handle("KeSetTimer", |c| {
        let (t, lo, hi, dpc) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        let due = c.sched.deadline(Some((hi as u64) << 32 | lo as u64).map(|v| v as i64)).unwrap_or(0);
        c.sched.set_timer(c.bus, t, due, 0, dpc) as u32
    });
    hle.handle("KeSetTimerEx", |c| {
        let (t, lo, hi, period, dpc) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let due = c.sched.deadline(Some(((hi as u64) << 32 | lo as u64) as i64)).unwrap_or(0);
        c.sched.set_timer(c.bus, t, due, period, dpc) as u32
    });
    hle.handle("KeCancelTimer", |c| {
        let t = arg(c, 0);
        c.sched.cancel_timer(t) as u32
    });
    hle.handle("KeInitializeDpc", |c| {
        let (dpc, routine, context) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let _ = c.write_u32(dpc, 19);
        let _ = c.write_u32(dpc + 4, 0);
        let _ = c.write_u32(dpc + 8, 0);
        let _ = c.write_u32(dpc + 0xC, routine);
        let _ = c.write_u32(dpc + 0x10, context);
        let _ = c.write_u32(dpc + 0x1C, 0);
        0
    });
    hle.handle("KeInsertQueueDpc", |c| {
        let (dpc, a1, a2) = (arg(c, 0), arg(c, 1), arg(c, 2));
        c.sched.queue_dpc(c.bus, dpc, a1, a2) as u32
    });
    hle.handle("KeRemoveQueueDpc", |c| {
        let dpc = arg(c, 0);
        c.sched.remove_dpc(c.bus, dpc) as u32
    });

    // ---- waiting ----
    hle.handle("KeWaitForSingleObject", |c| {
        // (Object, WaitReason, WaitMode, Alertable, Timeout*)
        let (obj, t) = (arg(c, 0), arg(c, 4));
        let t = timeout(c, t);
        wait_status(c, vec![obj], false, t)
    });
    hle.handle("KeWaitForMultipleObjects", |c| {
        // (Count, Objects**, WaitType, WaitReason, WaitMode, Alertable, Timeout*, WaitBlock*)
        let (count, list, kind, t) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 6));
        let objects = pointer_list(c, list, count);
        let t = timeout(c, t);
        wait_status(c, objects, kind == 0, t)
    });
    hle.handle("KeDelayExecutionThread", |c| {
        // (WaitMode, Alertable, Interval*)
        let t = arg(c, 2);
        let t = timeout(c, t);
        wait_status(c, vec![], false, t)
    });
    hle.handle("NtYieldExecution", |c| {
        c.sched.yield_now();
        0
    });
    hle.handle("NtWaitForSingleObject", |c| {
        // (Handle, Alertable, Timeout*)
        let (h, t) = (arg(c, 0), arg(c, 2));
        let Some(obj) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let t = timeout(c, t);
        wait_status(c, vec![obj], false, t)
    });
    hle.handle("NtWaitForSingleObjectEx", |c| {
        // (Handle, WaitMode, Alertable, Timeout*)
        let (h, t) = (arg(c, 0), arg(c, 3));
        let Some(obj) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let t = timeout(c, t);
        wait_status(c, vec![obj], false, t)
    });
    hle.handle("NtWaitForMultipleObjectsEx", |c| {
        // (Count, Handles*, WaitType, WaitMode, Alertable, Timeout*)
        let (count, list, kind, t) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 5));
        let handles = pointer_list(c, list, count);
        let mut objects = Vec::new();
        for h in handles {
            match object(c, h) {
                Some(o) => objects.push(o),
                None => return STATUS_INVALID_HANDLE,
            }
        }
        let t = timeout(c, t);
        wait_status(c, objects, kind == 0, t)
    });

    // ---- objects made through handles ----
    hle.handle("NtCreateEvent", |c| {
        // (EventHandle*, ObjectAttributes*, EventType, InitialState)
        let (out, kind, state) = (arg(c, 0), arg(c, 2), arg(c, 3));
        let Some(o) = c.sched.new_object(c.bus, 16, event_kind(kind), (state & 0xFF != 0) as i32) else {
            return STATUS_NO_MEMORY;
        };
        let h = c.sched.new_handle(o);
        put(c, out, h);
        STATUS_SUCCESS
    });
    hle.handle("NtSetEvent", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let p = c.sched.set_signal(c.bus, o, 1);
        put(c, prev, p as u32);
        STATUS_SUCCESS
    });
    hle.handle("NtClearEvent", |c| {
        let h = arg(c, 0);
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        c.sched.set_signal(c.bus, o, 0);
        STATUS_SUCCESS
    });
    hle.handle("NtPulseEvent", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let p = c.sched.pulse(c.bus, o);
        put(c, prev, p as u32);
        STATUS_SUCCESS
    });
    hle.handle("NtCreateSemaphore", |c| {
        // (Handle*, ObjectAttributes*, InitialCount, MaximumCount)
        let (out, count, max) = (arg(c, 0), arg(c, 2), arg(c, 3));
        if count > max || max == 0 {
            return STATUS_INVALID_PARAMETER;
        }
        let Some(o) = c.sched.new_object(c.bus, 0x14, SEMAPHORE, count as i32) else { return STATUS_NO_MEMORY };
        let _ = c.write_u32(o + 0x10, max);
        let h = c.sched.new_handle(o);
        put(c, out, h);
        STATUS_SUCCESS
    });
    hle.handle("NtReleaseSemaphore", |c| {
        let (h, n, prev) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        match c.sched.release_semaphore(c.bus, o, n) {
            Ok(p) => {
                put(c, prev, p as u32);
                STATUS_SUCCESS
            }
            Err(status) => status,
        }
    });
    hle.handle("NtCreateMutant", |c| {
        let (out, owned) = (arg(c, 0), arg(c, 2));
        let me = c.sched.current_kthread();
        let Some(o) = c.sched.new_object(c.bus, 0x20, MUTANT, if owned & 0xFF != 0 { 0 } else { 1 }) else {
            return STATUS_NO_MEMORY;
        };
        if owned & 0xFF != 0 {
            let _ = c.write_u32(o + 0x18, me);
        }
        let h = c.sched.new_handle(o);
        put(c, out, h);
        STATUS_SUCCESS
    });
    hle.handle("NtReleaseMutant", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        match c.sched.release_mutant(c.bus, o) {
            Ok(p) => {
                put(c, prev, p as u32);
                STATUS_SUCCESS
            }
            Err(status) => status,
        }
    });
    hle.handle("NtCreateTimer", |c| {
        let (out, kind) = (arg(c, 0), arg(c, 2));
        let k = if kind == 1 { TIMER_SYNCHRONIZATION } else { TIMER_NOTIFICATION };
        let Some(o) = c.sched.new_object(c.bus, 0x28, k, 0) else { return STATUS_NO_MEMORY };
        let h = c.sched.new_handle(o);
        put(c, out, h);
        STATUS_SUCCESS
    });
    hle.handle("NtSetTimerEx", |c| {
        // (Timer, DueTime*, ApcRoutine, ProcessorMode, ApcContext, Resume, Period ms, PreviousState*)
        // timer completion routines (APCs) are not delivered yet; the timer itself signals
        let (h, due_ptr, period, prev) = (arg(c, 0), arg(c, 1), arg(c, 6), arg(c, 7));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let raw = match (c.read_u32(due_ptr), c.read_u32(due_ptr.wrapping_add(4))) {
            (Ok(lo), Ok(hi)) if due_ptr != 0 => Some(((hi as u64) << 32 | lo as u64) as i64),
            _ => None,
        };
        let due = c.sched.deadline(raw).unwrap_or(0);
        let was = c.sched.set_timer(c.bus, o, due, period, 0);
        put(c, prev, was as u32);
        STATUS_SUCCESS
    });
    hle.handle("NtCancelTimer", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(o) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let was = c.sched.cancel_timer(o);
        put(c, prev, was as u32);
        STATUS_SUCCESS
    });
    hle.handle("NtClose", |c| {
        let h = arg(c, 0);
        c.sched.close_handle(h);
        STATUS_SUCCESS
    });

    // ---- threads ----
    let state = st.clone();
    hle.handle("PsCreateSystemThreadEx", move |c| {
        // (ThreadHandle*, ThreadExtraSize, KernelStackSize, TlsDataSize, ThreadId*,
        //  StartContext1, StartContext2, CreateSuspended, DebuggerThread, StartRoutine)
        let (out, stack, tls, id_out) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 4));
        let (ctx1, ctx2, suspended, start) = (arg(c, 5), arg(c, 6), arg(c, 7), arg(c, 9));
        create_thread(c, &state, out, id_out, start, ctx1, ctx2, stack, tls, suspended & 0xFF != 0)
    });
    let state = st.clone();
    hle.handle("PsCreateSystemThread", move |c| {
        // (ThreadHandle*, ThreadId*, StartRoutine, StartContext, DebuggerThread)
        let (out, id_out, start, ctx) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3));
        create_thread(c, &state, out, id_out, start, ctx, 0, 0, 0, false)
    });
    hle.handle("PsTerminateSystemThread", |c| {
        let status = arg(c, 0);
        if !c.sched.exit_current(c.bus, status) {
            c.stop = true;
        }
        0
    });
    hle.handle("KeGetCurrentThread", |c| c.sched.current_kthread());
    hle.handle("KeResumeThread", |c| {
        let t = arg(c, 0);
        c.sched.resume(t).unwrap_or(0)
    });
    hle.handle("KeSuspendThread", |c| {
        let t = arg(c, 0);
        c.sched.suspend(t).unwrap_or(0)
    });
    hle.handle("NtResumeThread", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(t) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let p = c.sched.resume(t).unwrap_or(0);
        put(c, prev, p);
        STATUS_SUCCESS
    });
    hle.handle("NtSuspendThread", |c| {
        let (h, prev) = (arg(c, 0), arg(c, 1));
        let Some(t) = object(c, h) else { return STATUS_INVALID_HANDLE };
        let p = c.sched.suspend(t).unwrap_or(0);
        put(c, prev, p);
        STATUS_SUCCESS
    });
    for name in ["KeSetPriorityThread", "KeSetBasePriorityThread", "KeQueryBasePriorityThread", "KeBoostPriorityThread", "KeSetDisableBoostThread", "KeSetPriorityProcess"] {
        hle.handle(name, |_| 0);
    }

    // ---- critical sections: RTL_CRITICAL_SECTION = a synchronisation event, then lock count,
    // recursion count and owner ----
    for name in ["RtlInitializeCriticalSection"] {
        hle.handle(name, |c| {
            let cs = arg(c, 0);
            init_header(c.bus, cs, EVENT_SYNCHRONIZATION, 16, 0);
            let _ = c.write_u32(cs + 0x10, u32::MAX);
            let _ = c.write_u32(cs + 0x14, 0);
            let _ = c.write_u32(cs + 0x18, 0);
            0
        });
    }
    for name in ["RtlEnterCriticalSection", "RtlEnterCriticalSectionAndRegion"] {
        hle.handle(name, |c| {
            let cs = arg(c, 0);
            if !enter(c, cs) {
                // someone else holds it: sleep until it is released, then run this call again
                if c.read_cstr(cs, 1).map_or(true, |b| b.first().copied().unwrap_or(0) != EVENT_SYNCHRONIZATION) {
                    init_header(c.bus, cs, EVENT_SYNCHRONIZATION, 16, 0);
                }
                c.sched.wait(c.bus, vec![cs], false, None);
                c.retry();
            }
            0
        });
    }
    hle.handle("RtlTryEnterCriticalSection", |c| {
        let cs = arg(c, 0);
        enter(c, cs) as u32
    });
    for name in ["RtlLeaveCriticalSection", "RtlLeaveCriticalSectionAndRegion"] {
        hle.handle(name, |c| {
            let cs = arg(c, 0);
            let rec = c.read_u32(cs + 0x14).unwrap_or(0).saturating_sub(1);
            let _ = c.write_u32(cs + 0x14, rec);
            if rec == 0 {
                let _ = c.write_u32(cs + 0x18, 0);
                let _ = c.write_u32(cs + 0x10, u32::MAX);
                if c.sched.is_waited_on(cs) {
                    c.sched.set_signal(c.bus, cs, 1);
                }
            } else {
                let _ = c.write_u32(cs + 0x10, rec - 1);
            }
            0
        });
    }
}

/// Takes the critical section if it is free or already ours.
fn enter(c: &mut Call, cs: u32) -> bool {
    let me = c.sched.current_kthread();
    let owner = c.read_u32(cs + 0x18).unwrap_or(0);
    if owner != 0 && owner != me {
        return false;
    }
    let rec = c.read_u32(cs + 0x14).unwrap_or(0) + 1;
    let _ = c.write_u32(cs + 0x14, rec);
    let _ = c.write_u32(cs + 0x18, me);
    let _ = c.write_u32(cs + 0x10, rec - 1);
    true
}

#[allow(clippy::too_many_arguments)]
fn create_thread(
    c: &mut Call,
    st: &Rc<RefCell<KernelState>>,
    out: u32,
    id_out: u32,
    start: u32,
    ctx1: u32,
    ctx2: u32,
    stack: u32,
    tls: u32,
    suspended: bool,
) -> u32 {
    let size = if stack == 0 { DEFAULT_STACK } else { stack.next_multiple_of(4096) };
    let (base, tls_at) = {
        let mut s = st.borrow_mut();
        let base = s.alloc(size, 4096);
        let tls_at = if tls == 0 { Some(0) } else { s.alloc(tls, 16) };
        (base, tls_at)
    };
    let (Some(base), Some(tls_at)) = (base, tls_at) else { return STATUS_NO_MEMORY };
    let n = NewThread { start, context: ctx1, context2: ctx2, stack_base: base, stack_size: size, tls: tls_at, suspended };
    let Some((handle, id, _)) = c.sched.create_thread(c.bus, n) else { return STATUS_NO_MEMORY };
    put(c, out, handle);
    put(c, id_out, id);
    STATUS_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exports, kernel, Layout, SchedAreas};
    use ferrox_core::Bus;
    use ferrox_cpu::{CpuBackend, Exit, RefCpu, ESP};
    use ferrox_formats::xbe::synth::{build, BASE};
    use ferrox_formats::xbe::Xbe;
    use ferrox_machine::PhysMem;

    const LAYOUT: Layout = Layout { stub_base: 0x30_0000, data_base: 0x31_0000, stack_top: 0x40_0000 - 16 };
    const AREAS: SchedAreas = SchedAreas {
        kpcr: 0x32_0000,
        stubs: 0x32_1000,
        objects: 0x33_0000..0x37_0000,
        stack_top: 0x40_0000 - 16,
        stack_size: 0x1_0000,
    };
    const EV: u32 = 0x2_0000;
    const CS: u32 = 0x2_0100;
    const HANDLE: u32 = 0x2_0200;
    const OUT: u32 = 0x2_1000; // results the guest stores at OUT + 4 * n
    const CODE: u32 = BASE + 0x1000;

    fn ord(name: &str) -> u32 {
        exports::by_name(name).unwrap().ordinal
    }

    /// Assembles the few instructions the test interpreter knows.
    #[derive(Default)]
    struct Asm {
        code: Vec<u8>,
        names: Vec<&'static str>,
    }

    impl Asm {
        fn here(&self) -> u32 {
            CODE + self.code.len() as u32
        }
        fn push(&mut self, v: u32) {
            self.code.push(0x68);
            self.code.extend_from_slice(&v.to_le_bytes());
        }
        fn call(&mut self, name: &'static str, args: &[u32]) {
            for &a in args.iter().rev() {
                self.push(a);
            }
            let i = match self.names.iter().position(|&n| n == name) {
                Some(i) => i,
                None => {
                    self.names.push(name);
                    self.names.len() - 1
                }
            } as u32;
            self.code.extend_from_slice(&[0xFF, 0x15]);
            self.code.extend_from_slice(&(BASE + 0x2000 + 4 * i).to_le_bytes());
        }
        fn store_eax(&mut self, n: u32) {
            self.code.push(0xA3);
            self.code.extend_from_slice(&(OUT + 4 * n).to_le_bytes());
        }
        fn load_eax(&mut self, addr: u32) {
            self.code.push(0xA1);
            self.code.extend_from_slice(&addr.to_le_bytes());
        }
        fn store_const(&mut self, n: u32, v: u32) {
            self.code.extend_from_slice(&[0xC7, 0x05]);
            self.code.extend_from_slice(&(OUT + 4 * n).to_le_bytes());
            self.code.extend_from_slice(&v.to_le_bytes());
        }
        fn raw(&mut self, bytes: &[u8]) {
            self.code.extend_from_slice(bytes);
        }
    }

    /// Runs `asm` (entry at its start) and returns the memory afterwards.
    fn run(asm: Asm, extra: &[&'static str]) -> (PhysMem, KernelHle, Exit) {
        run_with(asm, extra, |_| {})
    }

    fn run_with(asm: Asm, extra: &[&'static str], setup: impl FnOnce(&mut KernelHle)) -> (PhysMem, KernelHle, Exit) {
        let mut names = asm.names.clone();
        names.extend_from_slice(extra);
        let ords: Vec<u32> = names.iter().map(|n| ord(n)).collect();
        let mut file = build(true, &ords);
        file[0x1000..0x1000 + asm.code.len()].copy_from_slice(&asm.code);
        let xbe = Xbe::parse(&file).unwrap();
        let mut hle = KernelHle::with_table();
        kernel::install(&mut hle, 0x38_0000..0x3F_0000);
        setup(&mut hle);
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        hle.start_scheduler(&mut mem, AREAS).unwrap();
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        let exit = hle.run(&mut cpu, &mut mem, 100_000).unwrap();
        (mem, hle, exit)
    }

    fn out(mem: &mut PhysMem, n: u32) -> u32 {
        mem.read((OUT + 4 * n) as u64, 4).unwrap() as u32
    }

    #[test]
    fn a_new_thread_runs_while_the_main_thread_waits_for_its_event() {
        let mut a = Asm::default();
        // main: event, thread, wait, then copy what the thread stored
        a.call("KeInitializeEvent", &[EV, 1, 0]);
        let routine_patch = a.code.len() + 1; // the start address is the last argument pushed first
        a.call("PsCreateSystemThreadEx", &[HANDLE, 0, 0, 0, 0, 0x1234, 0, 0, 0, 0]);
        a.call("KeWaitForSingleObject", &[EV, 0, 0, 0, 0]);
        a.store_eax(1); // the wait's status
        a.load_eax(OUT);
        a.store_eax(2); // what the thread wrote, seen after the wait
        a.raw(&[0xF4]);
        // the thread: routine(ctx1, ctx2) stores 0x1234 then sets the event
        let routine = a.here();
        a.store_const(0, 0x1234);
        a.call("KeSetEvent", &[EV, 0, 0]);
        a.raw(&[0xC2, 8, 0]);
        a.code[routine_patch..routine_patch + 4].copy_from_slice(&routine.to_le_bytes());
        let (mut mem, hle, exit) = run(a, &[]);
        assert_eq!(exit, Exit::Halt);
        assert_eq!(out(&mut mem, 1), 0, "STATUS_WAIT_0");
        assert_eq!(out(&mut mem, 2), 0x1234, "the thread ran before the wait ended");
        assert_eq!(hle.sched.thread_count(), 1, "the worker finished");
        assert!(hle.recent_calls().iter().any(|l| l.contains("KeSetEvent")));
    }

    #[test]
    fn a_contended_critical_section_hands_over_when_released() {
        let mut a = Asm::default();
        a.call("RtlInitializeCriticalSection", &[CS]);
        a.call("RtlEnterCriticalSection", &[CS]);
        let routine_patch = a.code.len() + 1;
        a.call("PsCreateSystemThreadEx", &[HANDLE, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        a.call("NtYieldExecution", &[]); // the worker runs, finds the section taken and sleeps
        a.store_const(0, 1); // main did its work inside the section
        a.call("RtlLeaveCriticalSection", &[CS]);
        a.call("NtYieldExecution", &[]); // the worker takes it now
        a.raw(&[0xF4]);
        let routine = a.here();
        a.call("RtlEnterCriticalSection", &[CS]);
        a.load_eax(OUT);
        a.store_eax(1); // saw main's work
        a.load_eax(CS + 0x18);
        a.store_eax(2); // the owner field
        a.call("RtlLeaveCriticalSection", &[CS]);
        a.raw(&[0xC2, 8, 0]);
        a.code[routine_patch..routine_patch + 4].copy_from_slice(&routine.to_le_bytes());
        let (mut mem, hle, exit) = run(a, &[]);
        assert_eq!(exit, Exit::Halt);
        assert_eq!(out(&mut mem, 1), 1, "the worker only got in after main left");
        let owner = out(&mut mem, 2);
        assert!(owner != 0 && owner != hle.sched.current_kthread(), "the worker owned it: {owner:#x}");
    }

    #[test]
    fn irql_levels_are_remembered_per_call() {
        let mut a = Asm::default();
        a.call("KeRaiseIrqlToDpcLevel", &[]);
        a.store_eax(0); // the old level
        a.call("KeGetCurrentIrql", &[]);
        a.store_eax(1);
        a.raw(&[0xB9, 0, 0, 0, 0]); // mov ecx, 0
        a.call("KfLowerIrql", &[]);
        a.call("KeGetCurrentIrql", &[]);
        a.store_eax(2);
        a.raw(&[0xF4]);
        let (mut mem, _, _) = run(a, &[]);
        assert_eq!((out(&mut mem, 0), out(&mut mem, 1), out(&mut mem, 2)), (0, 2, 0));
    }

    #[test]
    fn sleeping_advances_the_clock_not_the_wall() {
        let mut a = Asm::default();
        // interval -1 second (relative, 100 ns units), at EV
        a.call("KeDelayExecutionThread", &[0, 0, EV]);
        a.store_eax(0);
        a.call("KeQueryInterruptTime", &[]);
        a.store_eax(1);
        a.raw(&[0xF4]);
        let ords: Vec<u32> = a.names.iter().map(|n| ord(n)).collect();
        let mut file = build(true, &ords);
        file[0x1000..0x1000 + a.code.len()].copy_from_slice(&a.code);
        let xbe = Xbe::parse(&file).unwrap();
        let mut hle = KernelHle::with_table();
        kernel::install(&mut hle, 0x38_0000..0x3F_0000);
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        mem.write(EV as u64, 4, (-10_000_000i64) as u64 & 0xFFFF_FFFF).unwrap();
        mem.write(EV as u64 + 4, 4, 0xFFFF_FFFF).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        hle.start_scheduler(&mut mem, AREAS).unwrap();
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        let started = std::time::Instant::now();
        assert_eq!(hle.run(&mut cpu, &mut mem, 100_000).unwrap(), Exit::Halt);
        assert!(started.elapsed().as_millis() < 500, "no real second passed");
        assert_eq!(out(&mut mem, 0), 0, "a delay ends in success");
        assert!(out(&mut mem, 1) >= 10_000_000, "but the clock moved a second: {}", out(&mut mem, 1));
    }

    #[test]
    fn the_entry_routine_returning_ends_its_thread_and_others_carry_on() {
        let mut a = Asm::default();
        let routine_patch = a.code.len() + 1;
        a.call("PsCreateSystemThreadEx", &[HANDLE, 0, 0, 0, 0, 0x77, 0, 0, 0, 0]);
        a.call("NtClose", &[0x100]);
        a.raw(&[0xC3]); // return from the entry point
        let routine = a.here();
        a.store_const(0, 0x55);
        a.raw(&[0xC2, 8, 0]);
        a.code[routine_patch..routine_patch + 4].copy_from_slice(&routine.to_le_bytes());
        let (mut mem, hle, exit) = run(a, &[]);
        assert_eq!(exit, Exit::Halt, "the last thread ending stops the run");
        assert_eq!(out(&mut mem, 0), 0x55, "the second thread ran after the first returned");
        assert_eq!(hle.sched.thread_count(), 0);
    }

    #[test]
    fn a_connected_interrupt_handler_runs_when_its_line_is_asserted() {
        const INT: u32 = 0x2_0300;
        let mut a = Asm::default();
        let isr_patch = a.code.len() + 5 * 5 + 1; // the routine is the second argument: pushed sixth, 5 bytes per push
        a.call("KeInitializeInterrupt", &[INT, 0, 0x99, 0x33, 5, 0, 0]);
        a.call("KeConnectInterrupt", &[INT]);
        a.store_const(1, 0x77); // main carries on after the handler ran
        a.raw(&[0xF4]);
        let isr = a.here();
        a.store_const(0, 0x1234); // the handler: leave a mark, then return (stdcall, 2 arguments)
        a.call("KeGetCurrentIrql", &[]);
        a.store_eax(2);
        a.raw(&[0xC2, 8, 0]);
        a.code[isr_patch..isr_patch + 4].copy_from_slice(&isr.to_le_bytes());
        let (mut mem, _, exit) = run_with(a, &[], |h| h.add_interrupt_source(0x33, |_| true));
        assert_eq!(exit, Exit::Halt);
        assert_eq!(out(&mut mem, 0), 0x1234, "the handler ran");
        assert_eq!(out(&mut mem, 1), 0x77, "and the interrupted code resumed");
        assert_eq!(out(&mut mem, 2), 5, "at the level the driver asked for");
    }
}
