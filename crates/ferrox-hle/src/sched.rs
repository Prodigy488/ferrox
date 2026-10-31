//! Threads, waiting and the kernel objects games synchronise with.
//!
//! The game's own code runs natively on one virtual processor, so threads are scheduled
//! cooperatively: a switch happens when the running thread makes a kernel call that blocks
//! (a wait, a delay, a yield) or ends. Kernel objects live in guest memory with the layouts
//! the real kernel uses (a `DISPATCHER_HEADER` starts every one), because games initialise
//! some of them in their own memory and read their state directly. Who is waiting on what is
//! kept on the host side.
//!
//! The kernel's per-processor control block (`KPCR`) also lives in guest memory and is reached
//! through the `fs` segment, as on the console: `fs:[0]` is the exception-handler chain,
//! `fs:[0x24]` the interrupt level, `fs:[0x28]` the running thread.

use crate::HleError;
use ferrox_core::Bus;
use ferrox_cpu::{Regs, ESP};
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

pub const STATUS_WAIT_0: u32 = 0;
pub const STATUS_TIMEOUT: u32 = 0x102;
pub const STATUS_UNSUCCESSFUL: u32 = 0xC000_0001;
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
pub const STATUS_INVALID_PARAMETER: u32 = 0xC000_000D;
pub const STATUS_NO_MEMORY: u32 = 0xC000_0017;
pub const STATUS_MUTANT_NOT_OWNED: u32 = 0xC000_0046;
pub const STATUS_SEMAPHORE_LIMIT_EXCEEDED: u32 = 0xC000_0047;

/// `NtCurrentThread()`: the pseudo-handle for the calling thread.
pub const CURRENT_THREAD_HANDLE: u32 = 0xFFFF_FFFE;

// DISPATCHER_HEADER types
pub const EVENT_NOTIFICATION: u8 = 0;
pub const EVENT_SYNCHRONIZATION: u8 = 1;
pub const MUTANT: u8 = 2;
pub const SEMAPHORE: u8 = 5;
pub const THREAD: u8 = 6;
pub const TIMER_NOTIFICATION: u8 = 8;
pub const TIMER_SYNCHRONIZATION: u8 = 9;

const SIGNAL: u32 = 4; // offset of SignalState in the header
const KTHREAD_SIZE: u32 = 0x200;
const KTHREAD_TLS: u32 = 0x28;
const KPCR_IRQL: u32 = 0x24;
const KPCR_PRCB: u32 = 0x28; // KPRCB.CurrentThread is its first field
/// Where a KDPC keeps its fields.
const DPC_ROUTINE: u32 = 0xC;
const DPC_CONTEXT: u32 = 0x10;
const DPC_ARG1: u32 = 0x14;
const DPC_ARG2: u32 = 0x18;
const DPC_INSERTED: u32 = 0x1C;
const TIMER_DUE: u32 = 0x10;
const TIMER_DPC: u32 = 0x20;
const TIMER_PERIOD: u32 = 0x24;
pub const DISPATCH_LEVEL: u8 = 2;

fn rd(bus: &mut dyn Bus, addr: u32) -> u32 {
    bus.read(addr as u64, 4).unwrap_or(0) as u32
}

fn wr(bus: &mut dyn Bus, addr: u32, v: u32) {
    let _ = bus.write(addr as u64, 4, v as u64);
}

fn rd8(bus: &mut dyn Bus, addr: u32) -> u8 {
    bus.read(addr as u64, 1).unwrap_or(0) as u8
}

fn wr8(bus: &mut dyn Bus, addr: u32, v: u8) {
    let _ = bus.write(addr as u64, 1, v as u64);
}

#[derive(Clone, Debug)]
struct Wait {
    objects: Vec<u32>,
    all: bool,
    deadline: Option<u64>,
}

#[derive(Clone, Debug)]
enum State {
    Ready,
    Waiting(Wait),
    Done,
}

struct Injected {
    regs: Regs,
    irql: u8,
    /// The caller gets the routine's result in `eax` (`KeSynchronizeExecution`).
    keep_result: bool,
}

/// A device interrupt a driver asked for with `KeInitializeInterrupt`.
#[derive(Clone, Copy, Debug)]
pub struct Interrupt {
    pub routine: u32,
    pub context: u32,
    pub vector: u32,
    pub irql: u8,
    pub connected: bool,
}

pub struct Thread {
    pub id: u32,
    pub kthread: u32,
    regs: Regs,
    state: State,
    suspend: u32,
    pub exit_status: u32,
    exception_list: u32,
    stack_base: u32,
    stack_limit: u32,
    injected: Vec<Injected>,
}

enum Clock {
    /// Advances only when asked (tests): every time query moves it forward a little.
    Manual(u64),
    Real { start: Instant, skew: u64 },
}

struct TimerEntry {
    addr: u32,
    due: u64,
    period: u64,
}

pub struct Sched {
    threads: Vec<Thread>,
    current: usize,
    next_id: u32,
    handles: HashMap<u32, u32>,
    next_handle: u32,
    kobj_next: u32,
    kobj_end: u32,
    pub kpcr: u32,
    clock: Clock,
    timers: Vec<TimerEntry>,
    dpcs: VecDeque<u32>,
    /// Set by a handler that blocked or ended the running thread.
    pub switch: bool,
    /// Start-up code for threads created without a system routine.
    pub thread_stub: u32,
    /// Where an injected call (a DPC) returns to.
    pub inject_return: u32,
    pub tick_count_addr: Option<u32>,
    pub interrupts: HashMap<u32, Interrupt>,
    /// `KeInterruptTime`: a `KSYSTEM_TIME` (low, high1, high2) in 100 ns units.
    pub interrupt_time_addr: Option<u32>,
    calls_since_switch: u32,
    ready: bool,
}

impl Default for Sched {
    fn default() -> Self {
        Sched {
            threads: Vec::new(),
            current: 0,
            next_id: 1,
            handles: HashMap::new(),
            next_handle: 0x100,
            kobj_next: 0,
            kobj_end: 0,
            kpcr: 0,
            clock: Clock::Manual(0),
            timers: Vec::new(),
            dpcs: VecDeque::new(),
            switch: false,
            thread_stub: 0,
            inject_return: 0,
            tick_count_addr: None,
            interrupts: HashMap::new(),
            interrupt_time_addr: None,
            calls_since_switch: 0,
            ready: false,
        }
    }
}

/// How a new thread starts.
pub struct NewThread {
    pub start: u32,
    pub context: u32,
    pub context2: u32,
    pub stack_base: u32,
    pub stack_size: u32,
    pub tls: u32,
    pub suspended: bool,
}

impl Sched {
    /// Lays out the control block and the main thread. `kobj` is the guest area for kernel
    /// objects.
    pub fn init(&mut self, bus: &mut dyn Bus, kpcr: u32, kobj: std::ops::Range<u32>, stack_top: u32, stack_size: u32) {
        self.kpcr = kpcr;
        self.kobj_next = kobj.start;
        self.kobj_end = kobj.end;
        for off in (0..0x100).step_by(4) {
            wr(bus, kpcr + off, 0);
        }
        wr(bus, kpcr, 0xFFFF_FFFF); // end of the exception-handler chain
        wr(bus, kpcr + 0x18, kpcr); // NtTib.Self
        wr(bus, kpcr + 0x1C, kpcr); // SelfPcr
        wr(bus, kpcr + 0x20, kpcr + KPCR_PRCB); // Prcb
        let main = self.make_thread(bus, 0, stack_top - stack_size + 16, stack_top);
        if let Some(t) = main {
            self.threads.push(t);
        }
        self.current = 0;
        if let Some(t) = self.threads.first() {
            let (k, b, l) = (t.kthread, t.stack_base, t.stack_limit);
            wr(bus, kpcr + KPCR_PRCB, k);
            wr(bus, kpcr + 4, b);
            wr(bus, kpcr + 8, l);
        }
        self.ready = true;
    }

    pub fn is_ready(&self) -> bool {
        self.ready
    }

    fn kalloc(&mut self, size: u32) -> Option<u32> {
        let start = (self.kobj_next + 15) & !15;
        let end = start.checked_add(size)?;
        if end > self.kobj_end {
            return None;
        }
        self.kobj_next = end;
        Some(start)
    }

    /// A guest dispatcher object of `size` bytes with its header initialised.
    pub fn new_object(&mut self, bus: &mut dyn Bus, size: u32, kind: u8, state: i32) -> Option<u32> {
        let a = self.kalloc(size)?;
        for off in (0..size).step_by(4) {
            wr(bus, a + off, 0);
        }
        init_header(bus, a, kind, size, state);
        Some(a)
    }

    fn make_thread(&mut self, bus: &mut dyn Bus, tls: u32, stack_limit: u32, stack_base: u32) -> Option<Thread> {
        let kthread = self.new_object(bus, KTHREAD_SIZE, THREAD, 0)?;
        wr(bus, kthread + KTHREAD_TLS, tls);
        let id = self.next_id;
        self.next_id += 1;
        Some(Thread {
            id,
            kthread,
            regs: Regs::default(),
            state: State::Ready,
            suspend: 0,
            exit_status: 0,
            exception_list: 0xFFFF_FFFF,
            stack_base,
            stack_limit,
            injected: Vec::new(),
        })
    }

    pub fn current_kthread(&self) -> u32 {
        self.threads.get(self.current).map_or(0, |t| t.kthread)
    }

    pub fn current_id(&self) -> u32 {
        self.threads.get(self.current).map_or(0, |t| t.id)
    }

    pub fn thread_count(&self) -> usize {
        self.threads.iter().filter(|t| !matches!(t.state, State::Done)).count()
    }

    // ----- time -----

    pub fn use_real_clock(&mut self) {
        self.clock = Clock::Real { start: Instant::now(), skew: 0 };
    }

    /// Time since start in 100 ns units.
    pub fn now(&self) -> u64 {
        match &self.clock {
            Clock::Manual(t) => *t,
            Clock::Real { start, skew } => (start.elapsed().as_nanos() / 100) as u64 + skew,
        }
    }

    /// For clocks that only advance when asked: moves forward a little per query so loops
    /// that poll the time terminate and runs stay repeatable.
    pub fn nudge(&mut self) {
        if let Clock::Manual(t) = &mut self.clock {
            *t += 100_000;
        }
    }

    fn advance_to(&mut self, t: u64) {
        let now = self.now();
        if t <= now {
            return;
        }
        match &mut self.clock {
            Clock::Manual(c) => *c = t,
            Clock::Real { skew, .. } => *skew += t - now,
        }
    }

    /// Converts the `LARGE_INTEGER` a wait takes (negative = relative, positive = absolute
    /// system time) into a deadline on this clock. `None` waits forever.
    pub fn deadline(&self, raw: Option<i64>) -> Option<u64> {
        match raw {
            None => None,
            Some(v) if v < 0 => Some(self.now().saturating_add(v.unsigned_abs())),
            Some(_) => Some(self.now()), // absolute times: treat as already due
        }
    }

    // ----- handles -----

    pub fn new_handle(&mut self, object: u32) -> u32 {
        let h = self.next_handle;
        self.next_handle += 4;
        self.handles.insert(h, object);
        h
    }

    pub fn object_of(&self, handle: u32) -> Option<u32> {
        if handle == CURRENT_THREAD_HANDLE {
            return Some(self.current_kthread());
        }
        self.handles.get(&handle).copied()
    }

    pub fn close_handle(&mut self, handle: u32) -> bool {
        self.handles.remove(&handle).is_some()
    }

    // ----- dispatcher objects -----

    fn signaled(&self, bus: &mut dyn Bus, obj: u32, me: u32) -> bool {
        let state = rd(bus, obj + SIGNAL) as i32;
        match rd8(bus, obj) {
            MUTANT => state > 0 || rd(bus, obj + 0x18) == me,
            _ => state > 0,
        }
    }

    fn consume(&self, bus: &mut dyn Bus, obj: u32, me: u32) {
        let state = rd(bus, obj + SIGNAL) as i32;
        match rd8(bus, obj) {
            EVENT_SYNCHRONIZATION | TIMER_SYNCHRONIZATION => wr(bus, obj + SIGNAL, 0),
            SEMAPHORE => wr(bus, obj + SIGNAL, (state - 1) as u32),
            MUTANT => {
                wr(bus, obj + SIGNAL, (state - 1) as u32);
                wr(bus, obj + 0x18, me);
            }
            _ => {}
        }
    }

    fn try_acquire(&self, bus: &mut dyn Bus, objects: &[u32], all: bool, me: u32) -> Option<usize> {
        if objects.is_empty() {
            return None;
        }
        if all {
            if objects.iter().all(|&o| self.signaled(bus, o, me)) {
                for &o in objects {
                    self.consume(bus, o, me);
                }
                return Some(0);
            }
            return None;
        }
        let i = objects.iter().position(|&o| self.signaled(bus, o, me))?;
        self.consume(bus, objects[i], me);
        Some(i)
    }

    /// Waits on `objects`. Returns the status to give the caller; if the caller has to block,
    /// the value is a placeholder and `switch` is set.
    pub fn wait(&mut self, bus: &mut dyn Bus, objects: Vec<u32>, all: bool, timeout: Option<i64>) -> u32 {
        let me = self.current_kthread();
        if let Some(i) = self.try_acquire(bus, &objects, all, me) {
            return STATUS_WAIT_0 + i as u32;
        }
        if timeout == Some(0) {
            if objects.is_empty() {
                self.switch = true; // a zero-length delay is a yield
                return STATUS_WAIT_0;
            }
            return STATUS_TIMEOUT;
        }
        let deadline = self.deadline(timeout);
        if let Some(t) = self.threads.get_mut(self.current) {
            t.state = State::Waiting(Wait { objects, all, deadline });
        }
        self.switch = true;
        0
    }

    /// Whether any thread is blocked on `obj`.
    pub fn is_waited_on(&self, obj: u32) -> bool {
        self.threads.iter().any(|t| matches!(&t.state, State::Waiting(w) if w.objects.contains(&obj)))
    }

    pub fn yield_now(&mut self) {
        self.switch = true;
    }

    /// Called after every kernel call. A thread that keeps calling the kernel without ever
    /// blocking would starve the others, so after a while it is asked to give way (kernel
    /// calls are the only place a switch is safe: no half-finished instruction state to save).
    pub fn note_call(&mut self) {
        self.calls_since_switch += 1;
        if self.calls_since_switch >= 400 && self.threads.len() > 1 {
            self.switch = true;
        }
    }

    /// Re-examines every waiting thread and every timer.
    pub fn poll(&mut self, bus: &mut dyn Bus) {
        self.fire_timers(bus);
        let now = self.now();
        for i in 0..self.threads.len() {
            let State::Waiting(w) = self.threads[i].state.clone() else { continue };
            let me = self.threads[i].kthread;
            if let Some(idx) = self.try_acquire(bus, &w.objects, w.all, me) {
                self.threads[i].state = State::Ready;
                self.threads[i].regs.gpr[0] = STATUS_WAIT_0 + idx as u32;
            } else if w.deadline.is_some_and(|d| d <= now) {
                self.threads[i].state = State::Ready;
                // a pure delay has nothing to time out on: running out the clock is success
                self.threads[i].regs.gpr[0] = if w.objects.is_empty() { STATUS_WAIT_0 } else { STATUS_TIMEOUT };
            }
        }
    }

    /// Sets an object's signal state, then lets waiters run. Returns the previous state.
    pub fn set_signal(&mut self, bus: &mut dyn Bus, obj: u32, value: i32) -> i32 {
        let prev = rd(bus, obj + SIGNAL) as i32;
        wr(bus, obj + SIGNAL, value as u32);
        self.poll(bus);
        prev
    }

    /// Wakes whoever can run, then clears the state again (`KePulseEvent`).
    pub fn pulse(&mut self, bus: &mut dyn Bus, obj: u32) -> i32 {
        let prev = self.set_signal(bus, obj, 1);
        wr(bus, obj + SIGNAL, 0);
        prev
    }

    pub fn release_semaphore(&mut self, bus: &mut dyn Bus, obj: u32, count: u32) -> Result<i32, u32> {
        let prev = rd(bus, obj + SIGNAL) as i32;
        let limit = rd(bus, obj + 0x10) as i32;
        if prev.checked_add(count as i32).is_none_or(|n| n > limit) {
            return Err(STATUS_SEMAPHORE_LIMIT_EXCEEDED);
        }
        wr(bus, obj + SIGNAL, (prev + count as i32) as u32);
        self.poll(bus);
        Ok(prev)
    }

    pub fn release_mutant(&mut self, bus: &mut dyn Bus, obj: u32) -> Result<i32, u32> {
        let me = self.current_kthread();
        if rd(bus, obj + 0x18) != me {
            return Err(STATUS_MUTANT_NOT_OWNED);
        }
        let prev = rd(bus, obj + SIGNAL) as i32;
        wr(bus, obj + SIGNAL, (prev + 1) as u32);
        if prev + 1 == 1 {
            wr(bus, obj + 0x18, 0);
        }
        self.poll(bus);
        Ok(prev)
    }

    // ----- timers and DPCs -----

    /// Starts a timer (`due` is already a deadline on this clock). Returns whether it was
    /// already running.
    pub fn set_timer(&mut self, bus: &mut dyn Bus, addr: u32, due: u64, period_ms: u32, dpc: u32) -> bool {
        let was = self.timers.iter().any(|t| t.addr == addr);
        self.timers.retain(|t| t.addr != addr);
        wr(bus, addr + SIGNAL, 0);
        wr(bus, addr + TIMER_DPC, dpc);
        wr(bus, addr + TIMER_PERIOD, period_ms);
        wr(bus, addr + TIMER_DUE, due as u32);
        wr(bus, addr + TIMER_DUE + 4, (due >> 32) as u32);
        self.timers.push(TimerEntry { addr, due, period: period_ms as u64 * 10_000 });
        was
    }

    pub fn cancel_timer(&mut self, addr: u32) -> bool {
        let was = self.timers.iter().any(|t| t.addr == addr);
        self.timers.retain(|t| t.addr != addr);
        was
    }

    fn fire_timers(&mut self, bus: &mut dyn Bus) {
        let now = self.now();
        let mut i = 0;
        while i < self.timers.len() {
            if self.timers[i].due > now {
                i += 1;
                continue;
            }
            let (addr, period) = (self.timers[i].addr, self.timers[i].period);
            wr(bus, addr + SIGNAL, 1);
            let dpc = rd(bus, addr + TIMER_DPC);
            if dpc != 0 {
                self.queue_dpc(bus, dpc, now as u32, (now >> 32) as u32);
            }
            if period > 0 {
                self.timers[i].due = now + period;
                i += 1;
            } else {
                self.timers.remove(i);
            }
        }
    }

    pub fn queue_dpc(&mut self, bus: &mut dyn Bus, dpc: u32, a1: u32, a2: u32) -> bool {
        if rd(bus, dpc + DPC_INSERTED) != 0 {
            return false;
        }
        wr(bus, dpc + DPC_ARG1, a1);
        wr(bus, dpc + DPC_ARG2, a2);
        wr(bus, dpc + DPC_INSERTED, 1);
        self.dpcs.push_back(dpc);
        true
    }

    pub fn remove_dpc(&mut self, bus: &mut dyn Bus, dpc: u32) -> bool {
        let was = self.dpcs.iter().any(|&d| d == dpc);
        self.dpcs.retain(|&d| d != dpc);
        wr(bus, dpc + DPC_INSERTED, 0);
        was
    }

    pub fn irql(&self, bus: &mut dyn Bus) -> u8 {
        rd8(bus, self.kpcr + KPCR_IRQL)
    }

    pub fn set_irql(&self, bus: &mut dyn Bus, level: u8) {
        wr8(bus, self.kpcr + KPCR_IRQL, level);
    }

    /// Runs a call on the current thread's stack and then continues where the thread was:
    /// the thread's registers are saved, and `inject_return` restores them.
    pub fn inject_call(&mut self, bus: &mut dyn Bus, regs: &mut Regs, routine: u32, args: &[u32], irql: u8) {
        self.inject(bus, regs, routine, args, irql, false);
    }

    /// Like `inject_call`, but the interrupted call returns what the routine returned.
    pub fn inject_call_returning(&mut self, bus: &mut dyn Bus, regs: &mut Regs, routine: u32, args: &[u32], irql: u8) {
        self.inject(bus, regs, routine, args, irql, true);
    }

    fn inject(&mut self, bus: &mut dyn Bus, regs: &mut Regs, routine: u32, args: &[u32], irql: u8, keep_result: bool) {
        let prev = self.irql(bus);
        if let Some(t) = self.threads.get_mut(self.current) {
            t.injected.push(Injected { regs: *regs, irql: prev, keep_result });
        }
        self.set_irql(bus, irql);
        let esp = regs.gpr[ESP].wrapping_sub(4 * (args.len() as u32 + 1));
        wr(bus, esp, self.inject_return);
        for (i, a) in args.iter().enumerate() {
            wr(bus, esp + 4 + 4 * i as u32, *a);
        }
        regs.gpr[ESP] = esp;
        regs.eip = routine;
    }

    /// Called when an injected call finished: puts the interrupted state back.
    pub fn return_from_injection(&mut self, bus: &mut dyn Bus, regs: &mut Regs) -> u32 {
        let Some(saved) = self.threads.get_mut(self.current).and_then(|t| t.injected.pop()) else {
            return 0;
        };
        self.set_irql(bus, saved.irql);
        let result = regs.gpr[0];
        *regs = saved.regs;
        if saved.keep_result {
            regs.gpr[0] = result;
        }
        regs.gpr[0]
    }

    /// Starts the next queued DPC on the running thread if the interrupt level allows it.
    pub fn run_pending_dpc(&mut self, bus: &mut dyn Bus, regs: &mut Regs) -> bool {
        if self.dpcs.is_empty() || !self.ready || self.irql(bus) >= DISPATCH_LEVEL {
            return false;
        }
        let Some(dpc) = self.dpcs.pop_front() else { return false };
        wr(bus, dpc + DPC_INSERTED, 0);
        let routine = rd(bus, dpc + DPC_ROUTINE);
        if routine == 0 {
            return false;
        }
        let args = [dpc, rd(bus, dpc + DPC_CONTEXT), rd(bus, dpc + DPC_ARG1), rd(bus, dpc + DPC_ARG2)];
        self.inject_call(bus, regs, routine, &args, DISPATCH_LEVEL);
        true
    }

    pub fn dpc_pending(&self) -> bool {
        !self.dpcs.is_empty()
    }

    // ----- threads -----

    pub fn create_thread(&mut self, bus: &mut dyn Bus, n: NewThread) -> Option<(u32, u32, u32)> {
        let stack_top = n.stack_base.checked_add(n.stack_size)?;
        let mut t = self.make_thread(bus, n.tls, n.stack_base, stack_top)?;
        // the start-up stub pops the routine, then calls it with the two context values
        let esp = stack_top - 16 - 12;
        wr(bus, esp, n.start);
        wr(bus, esp + 4, n.context);
        wr(bus, esp + 8, n.context2);
        t.regs.gpr[ESP] = esp;
        t.regs.eip = self.thread_stub;
        t.suspend = n.suspended as u32;
        let (id, kthread) = (t.id, t.kthread);
        self.threads.push(t);
        let handle = self.new_handle(kthread);
        Some((handle, id, kthread))
    }

    fn index_of(&self, kthread: u32) -> Option<usize> {
        self.threads.iter().position(|t| t.kthread == kthread)
    }

    pub fn suspend(&mut self, kthread: u32) -> Option<u32> {
        let i = self.index_of(kthread)?;
        let prev = self.threads[i].suspend;
        self.threads[i].suspend += 1;
        if i == self.current {
            self.switch = true;
        }
        Some(prev)
    }

    pub fn resume(&mut self, kthread: u32) -> Option<u32> {
        let i = self.index_of(kthread)?;
        let prev = self.threads[i].suspend;
        self.threads[i].suspend = prev.saturating_sub(1);
        Some(prev)
    }

    /// Ends the running thread. Returns false when no thread is left to run.
    pub fn exit_current(&mut self, bus: &mut dyn Bus, status: u32) -> bool {
        if let Some(t) = self.threads.get_mut(self.current) {
            t.state = State::Done;
            t.exit_status = status;
            let k = t.kthread;
            wr(bus, k + SIGNAL, 1);
        }
        self.poll(bus);
        self.switch = true;
        self.threads.iter().any(|t| !matches!(t.state, State::Done))
    }

    fn runnable(&self, i: usize) -> bool {
        matches!(self.threads[i].state, State::Ready) && self.threads[i].suspend == 0
    }

    /// Saves the running thread (whose registers are `regs`) and loads the next runnable one.
    pub fn reschedule(&mut self, bus: &mut dyn Bus, regs: &mut Regs) -> Result<(), HleError> {
        self.switch = false;
        self.calls_since_switch = 0;
        let cur = self.current;
        if let Some(t) = self.threads.get_mut(cur) {
            if !matches!(t.state, State::Done) {
                t.regs = *regs;
            }
            t.exception_list = rd(bus, self.kpcr);
        }
        let n = self.threads.len();
        let next = loop {
            self.poll(bus);
            if let Some(i) = (1..=n).map(|k| (cur + k) % n).find(|&i| self.runnable(i)) {
                break i;
            }
            // nothing can run: jump the clock to the next deadline
            let soonest = self
                .threads
                .iter()
                .filter_map(|t| match &t.state {
                    State::Waiting(w) => w.deadline,
                    _ => None,
                })
                .chain(self.timers.iter().map(|t| t.due))
                .min();
            match soonest {
                Some(t) => self.advance_to(t),
                None => return Err(HleError::Deadlock(self.describe_threads())),
            }
        };
        self.current = next;
        let t = &self.threads[next];
        *regs = t.regs;
        let (k, el, sb, sl) = (t.kthread, t.exception_list, t.stack_base, t.stack_limit);
        wr(bus, self.kpcr, el);
        wr(bus, self.kpcr + KPCR_PRCB, k);
        wr(bus, self.kpcr + 4, sb);
        wr(bus, self.kpcr + 8, sl);
        Ok(())
    }

    /// One line per thread, for diagnostics.
    pub fn describe_threads(&self) -> String {
        self.threads
            .iter()
            .map(|t| {
                let s = match &t.state {
                    State::Ready if t.suspend > 0 => format!("suspended x{}", t.suspend),
                    State::Ready => "ready".to_string(),
                    State::Done => format!("finished ({:#x})", t.exit_status),
                    State::Waiting(w) => format!(
                        "waiting on {} object(s) {}{}",
                        w.objects.len(),
                        w.objects.iter().map(|o| format!("{o:#x}")).collect::<Vec<_>>().join(","),
                        w.deadline.map_or(String::new(), |d| format!(" until {d}")),
                    ),
                };
                format!("thread {} (eip {:#x}): {s}", t.id, t.regs.eip)
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Makes the clock-driven things happen: the tick counter games read, timers, wake-ups.
    pub fn tick(&mut self, bus: &mut dyn Bus) {
        if !self.ready {
            return;
        }
        let now = self.now();
        if let Some(a) = self.tick_count_addr {
            wr(bus, a, (now / 10_000) as u32);
        }
        if let Some(a) = self.interrupt_time_addr {
            wr(bus, a + 8, (now >> 32) as u32); // High2Time first, so a reader never sees a torn value
            wr(bus, a, now as u32);
            wr(bus, a + 4, (now >> 32) as u32);
        }
        self.poll(bus);
    }
}

/// Writes a dispatcher header: type, size in 32-bit words, signal state and an empty wait list.
pub fn init_header(bus: &mut dyn Bus, addr: u32, kind: u8, size_bytes: u32, state: i32) {
    wr(bus, addr, kind as u32 | ((size_bytes / 4) & 0xFF) << 16);
    wr(bus, addr + SIGNAL, state as u32);
    wr(bus, addr + 8, addr + 8);
    wr(bus, addr + 12, addr + 8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_machine::PhysMem;

    fn mem() -> PhysMem {
        let mut m = PhysMem::new();
        m.add_ram(0, 1 << 20).unwrap();
        m
    }

    fn sched(m: &mut PhysMem) -> Sched {
        let mut s = Sched::default();
        s.init(m, 0x1000, 0x2000..0x8000, 0xF_0000, 0x1_0000);
        s.thread_stub = 0x9000;
        s.inject_return = 0x9200;
        s
    }

    fn new_thread(s: &mut Sched, m: &mut PhysMem) -> u32 {
        s.create_thread(m, NewThread { start: 0x5000, context: 7, context2: 8, stack_base: 0x8_0000, stack_size: 0x1000, tls: 0, suspended: false })
            .unwrap()
            .2
    }

    #[test]
    fn a_wait_on_an_unsignalled_event_blocks_and_a_set_wakes_it() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let ev = s.new_object(&mut m, 16, EVENT_SYNCHRONIZATION, 0).unwrap();
        let other = new_thread(&mut s, &mut m);
        let mut regs = Regs::default();
        regs.eip = 0x4000;
        assert_eq!(s.wait(&mut m, vec![ev], false, None), 0);
        assert!(s.switch, "the caller had to block");
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(s.current_kthread(), other, "the other thread runs now");
        assert_eq!((regs.eip, rd(&mut m, regs.gpr[ESP])), (0x9000, 0x5000), "it starts in the stub, with its routine on the stack");
        // the other thread signals the event: the first becomes runnable with WAIT_0
        s.set_signal(&mut m, ev, 1);
        assert_eq!(rd(&mut m, ev + SIGNAL), 0, "a synchronisation event is consumed by the waiter");
        let mut r2 = regs;
        s.yield_now();
        s.reschedule(&mut m, &mut r2).unwrap();
        assert_eq!(r2.eip, 0x4000);
        assert_eq!(r2.gpr[0], STATUS_WAIT_0);
    }

    #[test]
    fn a_notification_event_stays_signalled_and_a_timeout_returns_258() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let ev = s.new_object(&mut m, 16, EVENT_NOTIFICATION, 1).unwrap();
        assert_eq!(s.wait(&mut m, vec![ev], false, Some(-10)), 0);
        assert_eq!(s.wait(&mut m, vec![ev], false, Some(-10)), 0, "still signalled");
        let off = s.new_object(&mut m, 16, EVENT_NOTIFICATION, 0).unwrap();
        assert_eq!(s.wait(&mut m, vec![off], false, Some(0)), STATUS_TIMEOUT);
    }

    #[test]
    fn a_lone_sleeper_makes_time_jump_forward() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let mut regs = Regs::default();
        regs.eip = 0x4000;
        let t0 = s.now();
        assert_eq!(s.wait(&mut m, vec![], false, Some(-5_000_000)), 0);
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(regs.eip, 0x4000, "the same thread resumes");
        assert_eq!(regs.gpr[0], STATUS_WAIT_0, "a delay ends in success");
        assert!(s.now() >= t0 + 5_000_000, "half a second passed without waiting for it");
    }

    #[test]
    fn waiting_forever_with_nobody_to_wake_you_is_a_reported_deadlock() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let ev = s.new_object(&mut m, 16, EVENT_NOTIFICATION, 0).unwrap();
        let mut regs = Regs::default();
        s.wait(&mut m, vec![ev], false, None);
        let err = s.reschedule(&mut m, &mut regs).unwrap_err();
        assert!(err.to_string().contains("waiting on 1 object"), "{err}");
    }

    #[test]
    fn semaphores_count_and_mutants_nest() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let sem = s.new_object(&mut m, 24, SEMAPHORE, 0).unwrap();
        wr(&mut m, sem + 0x10, 3);
        assert_eq!(s.release_semaphore(&mut m, sem, 2), Ok(0));
        assert_eq!(s.wait(&mut m, vec![sem], false, Some(0)), 0);
        assert_eq!(s.wait(&mut m, vec![sem], false, Some(0)), 0);
        assert_eq!(s.wait(&mut m, vec![sem], false, Some(0)), STATUS_TIMEOUT);
        assert_eq!(s.release_semaphore(&mut m, sem, 9), Err(STATUS_SEMAPHORE_LIMIT_EXCEEDED));
        let mu = s.new_object(&mut m, 32, MUTANT, 1).unwrap();
        assert_eq!(s.wait(&mut m, vec![mu], false, Some(0)), 0);
        assert_eq!(s.wait(&mut m, vec![mu], false, Some(0)), 0, "the owner may take it again");
        assert_eq!(s.release_mutant(&mut m, mu), Ok(-1));
        assert_eq!(s.release_mutant(&mut m, mu), Ok(0));
        assert_eq!(s.release_mutant(&mut m, mu), Err(STATUS_MUTANT_NOT_OWNED));
    }

    #[test]
    fn wait_all_needs_every_object() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let a = s.new_object(&mut m, 16, EVENT_NOTIFICATION, 1).unwrap();
        let b = s.new_object(&mut m, 16, EVENT_NOTIFICATION, 0).unwrap();
        assert_eq!(s.wait(&mut m, vec![a, b], true, Some(0)), STATUS_TIMEOUT);
        assert_eq!(s.wait(&mut m, vec![a, b], false, Some(0)), 0, "wait-any takes the first signalled");
        wr(&mut m, b + SIGNAL, 1);
        assert_eq!(s.wait(&mut m, vec![a, b], true, Some(0)), 0);
        assert_eq!(s.wait(&mut m, vec![b, a], false, Some(0)), 0);
    }

    #[test]
    fn a_timer_signals_and_queues_its_dpc() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let timer = s.new_object(&mut m, 40, TIMER_NOTIFICATION, 0).unwrap();
        let dpc = s.new_object(&mut m, 32, 19, 0).unwrap();
        wr(&mut m, dpc + DPC_ROUTINE, 0x6000);
        wr(&mut m, dpc + DPC_CONTEXT, 0xC0DE);
        s.set_timer(&mut m, timer, 1_000, 0, dpc);
        s.poll(&mut m);
        assert_eq!(rd(&mut m, timer + SIGNAL), 0, "not due yet");
        s.advance_to(2_000);
        s.poll(&mut m);
        assert_eq!(rd(&mut m, timer + SIGNAL), 1);
        let mut regs = Regs::default();
        regs.gpr[ESP] = 0xE_0000;
        regs.eip = 0x4444;
        regs.gpr[0] = 77;
        assert!(s.run_pending_dpc(&mut m, &mut regs));
        assert_eq!((regs.eip, rd(&mut m, regs.gpr[ESP]), rd(&mut m, regs.gpr[ESP] + 8)), (0x6000, 0x9200, 0xC0DE));
        assert_eq!(s.irql(&mut m), DISPATCH_LEVEL);
        let eax = s.return_from_injection(&mut m, &mut regs);
        assert_eq!((regs.eip, regs.gpr[ESP], eax, s.irql(&mut m)), (0x4444, 0xE_0000, 77, 0));
    }

    #[test]
    fn a_finished_thread_signals_waiters_and_the_last_one_ends_the_run() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let worker = new_thread(&mut s, &mut m);
        let mut regs = Regs::default();
        regs.eip = 0x4000;
        assert_eq!(s.wait(&mut m, vec![worker], false, None), 0);
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(regs.eip, 0x9000, "the worker runs (in the start-up stub)");
        assert!(s.exit_current(&mut m, 5), "the main thread is still alive");
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!((regs.eip, regs.gpr[0]), (0x4000, STATUS_WAIT_0));
        assert!(!s.exit_current(&mut m, 0), "nothing left");
    }

    #[test]
    fn suspended_threads_do_not_run_until_resumed() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let t = s
            .create_thread(&mut m, NewThread { start: 1, context: 2, context2: 3, stack_base: 0x8_0000, stack_size: 0x1000, tls: 0, suspended: true })
            .unwrap()
            .2;
        let mut regs = Regs::default();
        regs.eip = 0x4000;
        s.yield_now();
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(regs.eip, 0x4000, "only the main thread is runnable");
        assert_eq!(s.resume(t), Some(1));
        s.yield_now();
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(regs.eip, 0x9000, "the new thread starts in the thread stub");
        assert_eq!((rd(&mut m, regs.gpr[ESP]), rd(&mut m, regs.gpr[ESP] + 4), rd(&mut m, regs.gpr[ESP] + 8)), (1, 2, 3));
    }

    #[test]
    fn the_control_block_follows_the_running_thread() {
        let mut m = mem();
        let mut s = sched(&mut m);
        let main = s.current_kthread();
        assert_eq!(rd(&mut m, 0x1000 + KPCR_PRCB), main);
        let other = new_thread(&mut s, &mut m);
        wr(&mut m, 0x1000, 0xAAAA); // the main thread's exception chain
        let mut regs = Regs::default();
        s.yield_now();
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(rd(&mut m, 0x1000 + KPCR_PRCB), other);
        assert_eq!(rd(&mut m, 0x1000), 0xFFFF_FFFF, "a new thread starts with an empty chain");
        s.yield_now();
        s.reschedule(&mut m, &mut regs).unwrap();
        assert_eq!(rd(&mut m, 0x1000), 0xAAAA, "the main thread's chain came back");
    }
}
