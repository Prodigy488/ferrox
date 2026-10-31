//! The first real kernel exports. Each one is implemented from its documented
//! signature (nxdk's `xboxkrnl.h`); anything not here stops the run by name.
//!
//! These cover what a small program needs to start: memory, debug output, time,
//! a few synchronisation no-ops for a single-threaded guest, and reboot. Threads,
//! events, interrupts, files and display setup come next.

use crate::{Call, KernelHle};
use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_NO_MEMORY: u32 = 0xC000_0017;
const PAGE: u32 = 4096;
/// Reported by `KeQueryPerformanceFrequency`: the console's 733 MHz clock.
pub const PERF_FREQUENCY: u64 = 733_333_333;

/// State the handlers share, readable by the front end.
#[derive(Debug, Default)]
pub struct KernelState {
    heap_next: u32,
    heap_end: u32,
    /// Everything the guest printed through `DbgPrint`.
    pub debug: String,
    /// Deterministic clocks: they advance a fixed step per query.
    pub perf_counter: u64,
    pub system_time: u64,
    /// Why the guest asked to stop, if it did.
    pub stop_reason: Option<String>,
}

impl KernelState {
    fn alloc(&mut self, size: u32, align: u32) -> Option<u32> {
        let align = align.max(16).next_power_of_two();
        let start = self.heap_next.checked_add(align - 1)? & !(align - 1);
        let end = start.checked_add(size.max(1))?;
        if end > self.heap_end {
            return None;
        }
        self.heap_next = end;
        Some(start)
    }
}

fn pages(n: u32) -> u32 {
    n.wrapping_add(PAGE - 1) & !(PAGE - 1)
}

/// Minimal printf over guest memory: %d %i %u %x %X %c %s %p %%, with optional
/// flags, width and an ignored length modifier. `next` yields successive
/// variable arguments.
pub fn format_guest(
    c: &mut Call,
    fmt: &[u8],
    mut next: impl FnMut(&mut Call) -> u32,
) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i] as char);
            i += 1;
            continue;
        }
        i += 1;
        let mut zero = false;
        let mut left = false;
        while i < fmt.len() && matches!(fmt[i], b'0' | b'-' | b'+' | b' ' | b'#') {
            zero |= fmt[i] == b'0';
            left |= fmt[i] == b'-';
            i += 1;
        }
        let mut width = 0usize;
        while i < fmt.len() && fmt[i].is_ascii_digit() {
            width = width * 10 + (fmt[i] - b'0') as usize;
            i += 1;
        }
        while i < fmt.len() && matches!(fmt[i], b'l' | b'h' | b'z') {
            i += 1;
        }
        let Some(&conv) = fmt.get(i) else { break };
        i += 1;
        let text = match conv {
            b'%' => "%".to_string(),
            b'd' | b'i' => (next(c) as i32).to_string(),
            b'u' => next(c).to_string(),
            b'x' => format!("{:x}", next(c)),
            b'X' => format!("{:X}", next(c)),
            b'p' => format!("{:08x}", next(c)),
            b'c' => ((next(c) as u8) as char).to_string(),
            b's' => {
                let p = next(c);
                let bytes = c.read_cstr(p, 512).unwrap_or_default();
                bytes.iter().map(|&b| b as char).collect()
            }
            other => format!("%{}", other as char),
        };
        let pad = width.saturating_sub(text.chars().count());
        if left {
            out.push_str(&text);
            out.extend(std::iter::repeat(' ').take(pad));
        } else {
            let fill = if zero && conv != b's' { '0' } else { ' ' };
            out.extend(std::iter::repeat(fill).take(pad));
            out.push_str(&text);
        }
    }
    out
}

/// Registers the implemented exports. `heap` is the guest address range the
/// memory allocators hand out from.
pub fn install(hle: &mut KernelHle, heap: Range<u32>) -> Rc<RefCell<KernelState>> {
    let st = Rc::new(RefCell::new(KernelState {
        heap_next: heap.start,
        heap_end: heap.end,
        perf_counter: 1_000_000,
        system_time: 0x01D0_0000_0000_0000,
        ..Default::default()
    }));

    // --- debug output (cdecl, variable arguments) ---
    let s = st.clone();
    hle.handle("DbgPrint", move |c| {
        let fmt_ptr = c.arg(0).unwrap_or(0);
        let fmt = c.read_cstr(fmt_ptr, 1024).unwrap_or_default();
        let mut n = 0;
        let text = format_guest(c, &fmt, |c| {
            n += 1;
            c.arg(n).unwrap_or(0)
        });
        s.borrow_mut().debug.push_str(&text);
        STATUS_SUCCESS
    });

    // --- pool and contiguous memory ---
    for name in ["ExAllocatePool", "ExAllocatePoolWithTag"] {
        let s = st.clone();
        hle.handle(name, move |c| {
            let size = c.arg(0).unwrap_or(0);
            s.borrow_mut().alloc(size, 16).unwrap_or(0)
        });
    }
    hle.handle("ExFreePool", |_| 0);
    let s = st.clone();
    hle.handle("MmAllocateContiguousMemory", move |c| {
        let size = c.arg(0).unwrap_or(0);
        s.borrow_mut().alloc(pages(size), PAGE).unwrap_or(0)
    });
    let s = st.clone();
    hle.handle("MmAllocateContiguousMemoryEx", move |c| {
        // (bytes, lowest, highest, alignment, protection)
        let size = c.arg(0).unwrap_or(0);
        let align = c.arg(3).unwrap_or(0).max(PAGE);
        s.borrow_mut().alloc(pages(size), align).unwrap_or(0)
    });
    hle.handle("MmFreeContiguousMemory", |_| 0);

    // --- virtual memory (identity-mapped: an allocation is just RAM) ---
    let s = st.clone();
    hle.handle("NtAllocateVirtualMemory", move |c| {
        // (BaseAddress*, ZeroBits, RegionSize*, AllocationType, Protect)
        let (Ok(base_p), Ok(size_p)) = (c.arg(0), c.arg(2)) else { return STATUS_NO_MEMORY };
        let (Ok(want), Ok(size)) = (c.read_u32(base_p), c.read_u32(size_p)) else {
            return STATUS_NO_MEMORY;
        };
        let size = pages(size);
        let addr = if want != 0 { Some(want) } else { s.borrow_mut().alloc(size, PAGE) };
        match addr {
            Some(a) if c.write_u32(base_p, a).is_ok() && c.write_u32(size_p, size).is_ok() => {
                STATUS_SUCCESS
            }
            _ => STATUS_NO_MEMORY,
        }
    });
    hle.handle("NtFreeVirtualMemory", |_| STATUS_SUCCESS);
    hle.handle("NtClose", |_| STATUS_SUCCESS);

    // --- time ---
    hle.handle("KeQueryPerformanceFrequency", |c| {
        c.regs.gpr[ferrox_cpu::EDX] = (PERF_FREQUENCY >> 32) as u32;
        PERF_FREQUENCY as u32
    });
    let s = st.clone();
    hle.handle("KeQueryPerformanceCounter", move |c| {
        let mut st = s.borrow_mut();
        st.perf_counter += 10_000;
        c.regs.gpr[ferrox_cpu::EDX] = (st.perf_counter >> 32) as u32;
        st.perf_counter as u32
    });
    let s = st.clone();
    hle.handle("KeQuerySystemTime", move |c| {
        let out = c.arg(0).unwrap_or(0);
        let mut st = s.borrow_mut();
        st.system_time += 100_000; // 10 ms in 100 ns units
        let t = st.system_time;
        let _ = c.write_u32(out, t as u32);
        let _ = c.write_u32(out + 4, (t >> 32) as u32);
        0
    });
    hle.handle("KeStallExecutionProcessor", |_| 0);

    // --- a single-threaded guest needs no real locking ---
    hle.handle("KeGetCurrentIrql", |_| 0);
    for name in ["RtlInitializeCriticalSection", "RtlEnterCriticalSection", "RtlLeaveCriticalSection"] {
        hle.handle(name, |_| STATUS_SUCCESS);
    }

    // --- strings ---
    hle.handle("RtlInitAnsiString", |c| {
        // ANSI_STRING { u16 Length; u16 MaximumLength; u32 Buffer }
        let (Ok(dest), Ok(src)) = (c.arg(0), c.arg(1)) else { return 0 };
        let len = if src == 0 { 0 } else { c.read_cstr(src, 0xFFFF).map(|s| s.len() as u32).unwrap_or(0) };
        let max = if src == 0 { 0 } else { (len + 1).min(0xFFFF) };
        let _ = c.write_u32(dest, len | max << 16);
        let _ = c.write_u32(dest + 4, src);
        0
    });

    // --- ending the run ---
    let s = st.clone();
    hle.handle("HalReturnToFirmware", move |c| {
        let routine = c.arg(0).unwrap_or(0);
        s.borrow_mut().stop_reason = Some(format!("HalReturnToFirmware({routine})"));
        c.stop = true;
        0
    });
    for name in ["KeBugCheck", "KeBugCheckEx"] {
        let s = st.clone();
        hle.handle(name, move |c| {
            let code = c.arg(0).unwrap_or(0);
            s.borrow_mut().stop_reason = Some(format!("{name}({code:#x})"));
            c.stop = true;
            0
        });
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{exports, Layout};
    use ferrox_core::Bus;
    use ferrox_cpu::{CpuBackend, Exit, RefCpu, ESP};
    use ferrox_formats::xbe::synth::{build, BASE};
    use ferrox_formats::xbe::Xbe;
    use ferrox_machine::PhysMem;

    const LAYOUT: Layout = Layout { stub_base: 0x30_0000, data_base: 0x31_0000, stack_top: 0x40_0000 - 16 };
    const FMT: u32 = 0x2_1000;
    const RESULT: u32 = 0x2_0000;

    fn ord(name: &str) -> u32 {
        exports::by_name(name).unwrap().ordinal
    }

    fn call_slot(i: u32) -> Vec<u8> {
        let mut v = vec![0xFF, 0x15];
        v.extend_from_slice(&(BASE + 0x2000 + 4 * i).to_le_bytes());
        v
    }

    fn push(v: u32) -> Vec<u8> {
        let mut o = vec![0x68];
        o.extend_from_slice(&v.to_le_bytes());
        o
    }

    #[test]
    fn every_handler_name_exists_in_the_table() {
        // install() panics on a typo; this makes the failure obvious
        let mut hle = KernelHle::with_table();
        install(&mut hle, 0x100_0000..0x200_0000);
        assert!(hle.is_implemented("DbgPrint"));
        assert!(!hle.is_implemented("PsCreateSystemThreadEx"));
    }

    #[test]
    fn argument_sizes_match_the_signatures_used() {
        assert_eq!(exports::by_name("NtAllocateVirtualMemory").unwrap().arg_bytes, 20);
        assert_eq!(exports::by_name("MmAllocateContiguousMemoryEx").unwrap().arg_bytes, 20);
        assert_eq!(exports::by_name("RtlInitAnsiString").unwrap().arg_bytes, 8);
        assert_eq!(exports::by_name("KeQuerySystemTime").unwrap().arg_bytes, 4);
    }

    #[test]
    fn small_program_prints_allocates_and_exits() {
        // DbgPrint("n=%d hex=%x name=%s|%04d\n", 42, 255, "xbox", 7)
        // p = ExAllocatePool(100); [RESULT] = p; HalReturnToFirmware(2)
        let names = ["DbgPrint", "ExAllocatePool", "HalReturnToFirmware"];
        let ords: Vec<u32> = names.iter().map(|n| ord(n)).collect();
        let mut code = Vec::new();
        for v in [7, FMT + 64, 255, 42, FMT] {
            code.extend(push(v));
        }
        code.extend(call_slot(0));
        code.extend_from_slice(&[0x83, 0xC4, 20]); // cdecl: caller pops 5 args
        code.extend(push(100));
        code.extend(call_slot(1));
        code.push(0xA3);
        code.extend_from_slice(&RESULT.to_le_bytes());
        code.extend(push(2));
        code.extend(call_slot(2));
        code.push(0xF4);

        let mut file = build(true, &ords);
        file[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let xbe = Xbe::parse(&file).unwrap();

        let mut hle = KernelHle::with_table();
        let state = install(&mut hle, 0x100_0000..0x200_0000);
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        mem.load(FMT as u64, b"n=%d hex=%x name=%s|%04d\n\0").unwrap();
        mem.load((FMT + 64) as u64, b"xbox\0").unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();

        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        assert_eq!(hle.run(&mut cpu, &mut mem, 10_000).unwrap(), Exit::Halt);

        let st = state.borrow();
        assert_eq!(st.debug, "n=42 hex=ff name=xbox|0007\n");
        assert_eq!(st.stop_reason.as_deref(), Some("HalReturnToFirmware(2)"));
        let p = mem.read(RESULT as u64, 4).unwrap() as u32;
        assert!((0x100_0000..0x200_0000).contains(&p) && p % 16 == 0, "{p:#x}");
    }

    #[test]
    fn virtual_alloc_rounds_to_pages_and_writes_back() {
        let mut hle = KernelHle::with_table();
        install(&mut hle, 0x100_0000..0x200_0000);
        let ords = [ord("NtAllocateVirtualMemory")];
        // base variable at 0x2_0000 (0), size variable at 0x2_0004 (5000); args pushed last-first
        let mut code = Vec::new();
        for v in [4u32, 0x1000, 0x2_0004, 0, 0x2_0000] {
            code.extend(push(v));
        }
        code.extend(call_slot(0));
        code.push(0xF4);
        let mut file = build(true, &ords);
        file[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let xbe = Xbe::parse(&file).unwrap();
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        mem.write(0x2_0004, 4, 5000).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        assert_eq!(hle.run(&mut cpu, &mut mem, 10_000).unwrap(), Exit::Halt);
        assert_eq!(cpu.regs().gpr[0], STATUS_SUCCESS);
        assert_eq!(mem.read(0x2_0004, 4).unwrap(), 8192);
        let base = mem.read(0x2_0000, 4).unwrap();
        assert!(base >= 0x100_0000 && base % 4096 == 0);
    }

    #[test]
    fn unimplemented_calls_name_the_missing_export() {
        let mut hle = KernelHle::with_table();
        install(&mut hle, 0x100_0000..0x200_0000);
        let ords = [ord("PsCreateSystemThreadEx")];
        let mut code = call_slot(0);
        code.push(0xF4);
        let mut file = build(true, &ords);
        file[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let xbe = Xbe::parse(&file).unwrap();
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        let err = hle.run(&mut cpu, &mut mem, 1000).unwrap_err();
        assert!(err.to_string().contains("PsCreateSystemThreadEx"), "{err}");
    }

    #[test]
    fn data_exports_get_zeroed_guest_objects() {
        let mut hle = KernelHle::with_table();
        install(&mut hle, 0x100_0000..0x200_0000);
        let ords = [ord("KeTickCount"), ord("NtClose")];
        let mut file = build(true, &ords);
        file[0x1000] = 0xF4;
        let xbe = Xbe::parse(&file).unwrap();
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        mem.write(0x31_0000, 4, 0xDEAD).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        // slot 0 points at the data object (zeroed), slot 1 at a code stub
        assert_eq!(mem.read((BASE + 0x2000) as u64, 4).unwrap() as u32, 0x31_0000);
        assert_eq!(mem.read(0x31_0000, 4).unwrap(), 0);
        assert_eq!(mem.read((BASE + 0x2004) as u64, 4).unwrap() as u32, 0x30_0000);
        assert_eq!(loaded.stubs.len(), 2);
    }
}
