//! Smaller kernel services games call during start-up: strings and memory helpers, time
//! conversion, the saved console settings (the EEPROM), page protection, hashing and the
//! hardware-interrupt bookkeeping. Most of these are simple because the guest's memory is one
//! flat identity-mapped space.

use crate::kernel::{KernelState, STATUS_NO_MEMORY, STATUS_SUCCESS};
use crate::{Call, KernelHle};
use std::cell::RefCell;
use std::rc::Rc;

const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
const PAGE_READWRITE: u32 = 4;
/// Days from 1601-01-01 to 1970-01-01.
const EPOCH_DAYS: i64 = 134_774;

fn arg(c: &mut Call, i: u32) -> u32 {
    c.arg(i).unwrap_or(0)
}

fn rd16(c: &mut Call, a: u32) -> u32 {
    c.bus.read(a as u64, 2).unwrap_or(0) as u32
}

fn wr16(c: &mut Call, a: u32, v: u32) {
    let _ = c.bus.write(a as u64, 2, v as u64);
}

fn rd8(c: &mut Call, a: u32) -> u8 {
    c.bus.read(a as u64, 1).unwrap_or(0) as u8
}

/// (length in bytes, buffer) of an ANSI_STRING or UNICODE_STRING.
fn string_at(c: &mut Call, p: u32) -> (u32, u32) {
    let len = rd16(c, p);
    let buf = c.read_u32(p + 4).unwrap_or(0);
    (len, buf)
}

fn set_string(c: &mut Call, p: u32, len: u32, max: u32, buf: u32) {
    wr16(c, p, len);
    wr16(c, p + 2, max);
    let _ = c.write_u32(p + 4, buf);
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The console's saved settings, as `ExQueryNonVolatileSetting` reports them: a plausible
/// North-American console set to English with default video and audio.
fn setting(index: u32) -> Option<u32> {
    Some(match index {
        0 => 0,      // time-zone bias
        5 | 6 => 0,  // standard / daylight bias
        7 => 1,      // language: English
        8 => 0,      // video settings
        9 => 0,      // audio settings
        0xA..=0xC => 0, // parental controls
        0x11 => 0,   // miscellaneous flags
        0x12 => 1,   // DVD region: North America
        _ => return None,
    })
}

// ---- SHA-1 over a guest-held context (5 state words, 64-bit byte count, 64-byte buffer) ----

fn sha_block(h: &mut [u32; 5], block: &[u8]) {
    let mut w = [0u32; 80];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
            20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6u32),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = t;
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

fn sha_load(c: &mut Call, ctx: u32) -> ([u32; 5], u64, Vec<u8>) {
    let mut h = [0u32; 5];
    for (i, x) in h.iter_mut().enumerate() {
        *x = c.read_u32(ctx + 4 * i as u32).unwrap_or(0);
    }
    let len = c.read_u32(ctx + 20).unwrap_or(0) as u64 | (c.read_u32(ctx + 24).unwrap_or(0) as u64) << 32;
    let pending = (len % 64) as u32;
    let buf = (0..pending).map(|i| rd8(c, ctx + 28 + i)).collect();
    (h, len, buf)
}

fn sha_store(c: &mut Call, ctx: u32, h: &[u32; 5], len: u64, buf: &[u8]) {
    for (i, x) in h.iter().enumerate() {
        let _ = c.write_u32(ctx + 4 * i as u32, *x);
    }
    let _ = c.write_u32(ctx + 20, len as u32);
    let _ = c.write_u32(ctx + 24, (len >> 32) as u32);
    for (i, b) in buf.iter().enumerate() {
        let _ = c.bus.write((ctx + 28 + i as u32) as u64, 1, *b as u64);
    }
}

fn sha_update(h: &mut [u32; 5], len: &mut u64, buf: &mut Vec<u8>, data: &[u8]) {
    *len += data.len() as u64;
    buf.extend_from_slice(data);
    let whole = buf.len() / 64 * 64;
    for block in buf[..whole].chunks(64) {
        sha_block(h, block);
    }
    buf.drain(..whole);
}

pub fn install(hle: &mut KernelHle, st: Rc<RefCell<KernelState>>) {
    // ---- strings and memory ----
    hle.handle("RtlInitUnicodeString", |c| {
        let (dest, src) = (arg(c, 0), arg(c, 1));
        let mut n = 0;
        if src != 0 {
            while n < 0x7FFF && rd16(c, src + 2 * n) != 0 {
                n += 1;
            }
        }
        let (len, max) = if src == 0 { (0, 0) } else { (2 * n, 2 * n + 2) };
        set_string(c, dest, len, max, src);
        0
    });
    hle.handle("RtlEqualString", |c| {
        let (a, b, fold) = (arg(c, 0), arg(c, 1), arg(c, 2) & 0xFF != 0);
        let ((la, pa), (lb, pb)) = (string_at(c, a), string_at(c, b));
        if la != lb {
            return 0;
        }
        for i in 0..la {
            let (mut x, mut y) = (rd8(c, pa + i), rd8(c, pb + i));
            if fold {
                x = x.to_ascii_uppercase();
                y = y.to_ascii_uppercase();
            }
            if x != y {
                return 0;
            }
        }
        1
    });
    hle.handle("RtlFreeAnsiString", |_| 0);
    let s = st.clone();
    hle.handle("RtlAnsiStringToUnicodeString", move |c| {
        let (dest, src, alloc) = (arg(c, 0), arg(c, 1), arg(c, 2) & 0xFF != 0);
        let (len, buf) = string_at(c, src);
        let (mut out, max) = if alloc {
            match s.borrow_mut().alloc(2 * len + 2, 16) {
                Some(p) => (p, 2 * len + 2),
                None => return STATUS_NO_MEMORY,
            }
        } else {
            (c.read_u32(dest + 4).unwrap_or(0), rd16(c, dest + 2))
        };
        if 2 * len > max.saturating_sub(2) && !alloc {
            return STATUS_BUFFER_TOO_SMALL;
        }
        if alloc {
            let _ = c.write_u32(dest + 4, out);
        }
        for i in 0..len {
            let b = rd8(c, buf + i) as u32;
            wr16(c, out, b);
            out += 2;
        }
        wr16(c, out, 0);
        wr16(c, dest, 2 * len);
        wr16(c, dest + 2, max);
        STATUS_SUCCESS
    });
    let s = st.clone();
    hle.handle("RtlUnicodeStringToAnsiString", move |c| {
        let (dest, src, alloc) = (arg(c, 0), arg(c, 1), arg(c, 2) & 0xFF != 0);
        let (len, buf) = string_at(c, src);
        let n = len / 2;
        let (out, max) = if alloc {
            match s.borrow_mut().alloc(n + 1, 16) {
                Some(p) => (p, n + 1),
                None => return STATUS_NO_MEMORY,
            }
        } else {
            (c.read_u32(dest + 4).unwrap_or(0), rd16(c, dest + 2))
        };
        if n + 1 > max && !alloc {
            return STATUS_BUFFER_TOO_SMALL;
        }
        for i in 0..n {
            let ch = rd16(c, buf + 2 * i);
            let _ = c.bus.write((out + i) as u64, 1, (if ch < 0x100 { ch } else { b'?' as u32 }) as u64);
        }
        let _ = c.bus.write((out + n) as u64, 1, 0);
        set_string(c, dest, n, max, out);
        STATUS_SUCCESS
    });
    hle.handle("RtlCompareMemory", |c| {
        let (a, b, n) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let mut i = 0;
        while i < n && rd8(c, a + i) == rd8(c, b + i) {
            i += 1;
        }
        i
    });
    hle.handle("RtlCompareMemoryUlong", |c| {
        let (a, n, pat) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let mut i = 0;
        while i + 4 <= n && c.read_u32(a + i).unwrap_or(!pat) == pat {
            i += 4;
        }
        i
    });
    hle.handle("RtlFillMemoryUlong", |c| {
        let (a, n, pat) = (arg(c, 0), arg(c, 1), arg(c, 2));
        for i in (0..n & !3).step_by(4) {
            let _ = c.write_u32(a + i, pat);
        }
        0
    });
    hle.handle("RtlNtStatusToDosError", |c| match arg(c, 0) {
        0 => 0,
        0xC000_0034 | 0xC000_0035 => 2, // not found
        0xC000_003A => 3,               // path not found
        0xC000_0022 => 5,               // access denied
        0xC000_0008 => 6,               // invalid handle
        0xC000_0017 => 8,               // no memory
        0xC000_000D => 87,              // invalid parameter
        0x0000_0102 => 258,             // timeout
        _ => 317,
    });
    hle.handle("RtlRaiseException", |c| {
        c.stop = true;
        0
    });

    // ---- time conversion (TIME_FIELDS = year, month, day, hour, minute, second, ms, weekday) ----
    hle.handle("RtlTimeToTimeFields", |c| {
        let (t, f) = (arg(c, 0), arg(c, 1));
        let t = c.read_u32(t).unwrap_or(0) as u64; // low part; high read below
        let hi = {
            let p = arg(c, 0);
            c.read_u32(p + 4).unwrap_or(0) as u64
        };
        let ticks = hi << 32 | t;
        let ms_total = ticks / 10_000;
        let days = (ms_total / 86_400_000) as i64;
        let ms_of_day = ms_total % 86_400_000;
        let (y, m, d) = civil_from_days(days - EPOCH_DAYS);
        let fields = [
            y as u32,
            m as u32,
            d as u32,
            (ms_of_day / 3_600_000) as u32,
            (ms_of_day / 60_000 % 60) as u32,
            (ms_of_day / 1000 % 60) as u32,
            (ms_of_day % 1000) as u32,
            ((days + 1) % 7) as u32, // 1601-01-01 was a Monday
        ];
        for (i, v) in fields.iter().enumerate() {
            wr16(c, f + 2 * i as u32, *v);
        }
        0
    });
    hle.handle("RtlTimeFieldsToTime", |c| {
        let (f, out) = (arg(c, 0), arg(c, 1));
        let v: Vec<i64> = (0..7).map(|i| rd16(c, f + 2 * i) as i64).collect();
        if !(1..=12).contains(&v[1]) || !(1..=31).contains(&v[2]) || v[3] > 23 || v[4] > 59 || v[5] > 59 || v[6] > 999 {
            return 0;
        }
        let days = days_from_civil(v[0], v[1], v[2]) + EPOCH_DAYS;
        let ms = ((days * 24 + v[3]) * 60 + v[4]) * 60 + v[5];
        let ticks = (ms * 1000 + v[6]) as u64 * 10_000;
        let _ = c.write_u32(out, ticks as u32);
        let _ = c.write_u32(out + 4, (ticks >> 32) as u32);
        1
    });

    // ---- saved console settings ----
    hle.handle("ExQueryNonVolatileSetting", |c| {
        // (ValueIndex, Type*, Value*, ValueLength, ResultLength*)
        let (index, ty, val, len, res) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let Some(v) = setting(index) else { return STATUS_OBJECT_NAME_NOT_FOUND };
        if len < 4 {
            return STATUS_BUFFER_TOO_SMALL;
        }
        if ty != 0 {
            let _ = c.write_u32(ty, 4); // REG_DWORD
        }
        if val != 0 {
            let _ = c.write_u32(val, v);
        }
        if res != 0 {
            let _ = c.write_u32(res, 4);
        }
        STATUS_SUCCESS
    });

    // ---- memory manager odds and ends ----
    hle.handle("MmGetPhysicalAddress", |c| arg(c, 0) & 0x03FF_FFFF);
    hle.handle("MmQueryAllocationSize", |_| 4096);
    hle.handle("MmLockUnlockBufferPages", |_| 0);
    hle.handle("MmLockUnlockPhysicalPage", |_| 0);
    hle.handle("MmSetAddressProtect", |_| 0);
    hle.handle("MmQueryAddressProtect", |_| PAGE_READWRITE);
    let s = st.clone();
    hle.handle("MmAllocateSystemMemory", move |c| {
        let size = arg(c, 0).next_multiple_of(4096);
        s.borrow_mut().alloc(size, 4096).unwrap_or(0)
    });
    hle.handle("MmFreeSystemMemory", |_| 0);
    hle.handle("MmQueryStatistics", |c| {
        // MM_STATISTICS: Length, TotalPhysicalPages, AvailablePages, then counters
        let p = arg(c, 0);
        let _ = c.write_u32(p + 4, 16384);
        let _ = c.write_u32(p + 8, 8192);
        0
    });
    hle.handle("NtProtectVirtualMemory", |c| {
        // (BaseAddress*, RegionSize*, NewProtect, OldProtect*)
        let old = arg(c, 3);
        if old != 0 {
            let _ = c.write_u32(old, PAGE_READWRITE);
        }
        STATUS_SUCCESS
    });
    hle.handle("NtQueryVirtualMemory", |c| {
        // (BaseAddress, MEMORY_BASIC_INFORMATION*)
        let (addr, p) = (arg(c, 0), arg(c, 1));
        let base = addr & !0xFFF;
        for (i, v) in [base, base, PAGE_READWRITE, 0x1000, 0x1000, PAGE_READWRITE, 0x2_0000].iter().enumerate() {
            let _ = c.write_u32(p + 4 * i as u32, *v);
        }
        STATUS_SUCCESS
    });
    hle.handle("ExQueryPoolBlockSize", |_| 64);
    hle.handle("FscGetCacheSize", |_| 0x200);
    hle.handle("FscSetCacheSize", |_| 0);
    hle.handle("DbgBreakPoint", |_| 0);
    hle.handle("KeSaveFloatingPointState", |_| 0);
    hle.handle("KeRestoreFloatingPointState", |_| 0);

    // ---- objects ----
    hle.handle("NtDuplicateObject", |c| {
        // (SourceHandle, TargetHandle*, Options)
        let (h, out) = (arg(c, 0), arg(c, 1));
        let Some(o) = c.sched.object_of(h) else { return STATUS_INVALID_HANDLE };
        let n = c.sched.new_handle(o);
        if out != 0 {
            let _ = c.write_u32(out, n);
        }
        STATUS_SUCCESS
    });
    hle.handle("ObReferenceObjectByHandle", |c| {
        // (Handle, ObjectType, Object**)
        let (h, out) = (arg(c, 0), arg(c, 2));
        let Some(o) = c.sched.object_of(h) else { return STATUS_INVALID_HANDLE };
        let _ = c.write_u32(out, o);
        STATUS_SUCCESS
    });
    hle.handle("ObfDereferenceObject", |_| 0);

    // ---- hardware interrupts: connections are recorded (delivery comes with the devices) ----
    hle.handle("HalGetInterruptVector", |c| {
        // (BusInterruptLevel, Irql*) -> vector
        let (level, irql) = (arg(c, 0), arg(c, 1));
        if irql != 0 {
            let _ = c.bus.write(irql as u64, 1, 26u64.saturating_sub(level as u64));
        }
        0x30 + level
    });
    hle.handle("KeInitializeInterrupt", |c| {
        // (Interrupt, ServiceRoutine, ServiceContext, Vector, Irql, InterruptMode, ShareVector)
        let (obj, routine, ctx, vector, irql) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        c.sched.interrupts.insert(obj, crate::sched::Interrupt { routine, context: ctx, vector, irql: irql as u8, connected: false });
        0
    });
    hle.handle("KeConnectInterrupt", |c| {
        let obj = arg(c, 0);
        match c.sched.interrupts.get_mut(&obj) {
            Some(i) => {
                i.connected = true;
                1
            }
            None => 0,
        }
    });
    hle.handle("KeDisconnectInterrupt", |c| {
        let obj = arg(c, 0);
        match c.sched.interrupts.get_mut(&obj) {
            Some(i) => {
                i.connected = false;
                1
            }
            None => 0,
        }
    });
    hle.handle("KeSynchronizeExecution", |c| {
        // (Interrupt, SynchronizeRoutine, SynchronizeContext): run the routine, return its result
        let (routine, ctx) = (arg(c, 1), arg(c, 2));
        c.sched.inject_call_returning(c.bus, c.regs, routine, &[ctx], crate::sched::DISPATCH_LEVEL);
        0
    });
    hle.handle("HalRegisterShutdownNotification", |_| 0);
    hle.handle("HalIsResetOrShutdownPending", |_| 0);
    hle.handle("HalInitiateShutdown", |c| {
        c.stop = true;
        0
    });

    // ---- SHA-1 ----
    hle.handle("XcSHAInit", |c| {
        let ctx = arg(c, 0);
        sha_store(c, ctx, &[0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0], 0, &[]);
        0
    });
    hle.handle("XcSHAUpdate", |c| {
        let (ctx, data, n) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let (mut h, mut len, mut buf) = sha_load(c, ctx);
        let bytes: Vec<u8> = (0..n).map(|i| rd8(c, data + i)).collect();
        sha_update(&mut h, &mut len, &mut buf, &bytes);
        sha_store(c, ctx, &h, len, &buf);
        0
    });
    hle.handle("XcSHAFinal", |c| {
        let (ctx, out) = (arg(c, 0), arg(c, 1));
        let (mut h, len, mut buf) = sha_load(c, ctx);
        let mut total = len;
        let mut pad = vec![0x80u8];
        while (len as usize + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&(len * 8).to_be_bytes());
        sha_update(&mut h, &mut total, &mut buf, &pad);
        for (i, x) in h.iter().enumerate() {
            for (j, b) in x.to_be_bytes().iter().enumerate() {
                let _ = c.bus.write((out + 4 * i as u32 + j as u32) as u64, 1, *b as u64);
            }
        }
        0
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_through_the_day_count() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2003, 12, 31), (2026, 10, 31), (1601, 1, 1)] {
            assert_eq!(civil_from_days(days_from_civil(y, m, d)), (y, m, d));
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn sha1_of_abc_matches_the_standard_vector() {
        let mut h = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
        let (mut len, mut buf) = (0u64, Vec::new());
        sha_update(&mut h, &mut len, &mut buf, b"abc");
        let mut total = len;
        let mut pad = vec![0x80u8];
        while (len as usize + pad.len()) % 64 != 56 {
            pad.push(0);
        }
        pad.extend_from_slice(&(len * 8).to_be_bytes());
        sha_update(&mut h, &mut total, &mut buf, &pad);
        assert_eq!(h, [0xA999_3E36, 0x4706_816A, 0xBA3E_2571, 0x7850_C26C, 0x9CD0_D89D]);
    }

    #[test]
    fn known_settings_have_values_and_unknown_ones_do_not() {
        assert_eq!(setting(7), Some(1));
        assert_eq!(setting(0x999), None);
    }
}
