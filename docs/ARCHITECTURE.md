# Architecture

## Goals

1. **Fast on modest hardware.** Run the guest CPU through the host's hardware
   virtualization (KVM on Linux, WHPX on Windows, HVF on macOS) instead of
   translating every instruction.
2. **Testable in pieces.** Every device is an ordinary Rust crate with no global
   state, so it can be driven from a unit test, a fuzzer, or a trace replayer.
3. **Vulkan first.** One renderer, designed for it, rather than two to keep in step.
4. **Modern features are part of the design,** not patches on top: save states,
   rewind, resolution scaling, patches, netplay, a controller-first interface.
5. **Portable.** Linux, Windows, macOS, and handheld PCs from one code base.

## Layers

```
ferrox-cli / (future) ferrox-app        front ends
        |
  machine assembly                       wires devices into a console
        |
 +------+---------+-----------+----------+
 | ferrox-machine | ferrox-nv2a | ferrox-hle | (future) apu, usb, ide, smbus, nvnet ...
 +----------------+-------------+
        |
   ferrox-cpu  <--  backends: KVM, WHPX, HVF, reference interpreter
        |
   ferrox-core   (Bus trait, errors)
```

* `ferrox-core::Bus` is the one interface between a CPU and everything else:
  physical reads and writes of 1, 2, 4 or 8 bytes, plus port I/O.
* `ferrox-machine::PhysMem` implements `Bus`. RAM, ROM (mirrored, as the console's
  flash is) and MMIO regions live in a sorted, non-overlapping map with binary-search
  lookup. Overlaps are rejected when devices are added, not discovered at run time.
* `ferrox-cpu::CpuBackend` is how the machine runs code. The reference interpreter
  exists to test the plumbing; real execution comes from hypervisor backends.
* `ferrox-nv2a` starts with the part that is a pure function: decoding the
  pushbuffer into commands. Everything downstream (method dispatch, state, shader
  generation, the renderer) can be built and tested against a recorded stream.
* `ferrox-formats` reads discs and executables with every field bounds-checked;
  hostile files produce errors, never panics.

## Two modes, one machine

* **Hardware mode:** boot ROM and BIOS run on the emulated chipset (you supply them).
* **High-level kernel mode:** `ferrox-hle` loads the XBE directly and answers kernel
  calls on the host. See LANDSCAPE.md for why both exist.

The guest leaves the VM for a kernel call with `out 0xE0, eax` (`HLE_PORT`, ordinal in
`eax`). The host reads stdcall arguments from the guest stack, writes the result to
`eax`, and the stub's `ret 4*n` cleans up.

## Hardware-virtualized CPU: the main design risk

Running the Xbox's CPU natively is the biggest speed lever, but the console is not a
PC. Known hazards, each with a plan:

* **Memory-mapped devices.** A hypervisor reports guest accesses to unmapped
  addresses as exits; devices are handled in `Bus`. Cost: one VM exit per MMIO access.
  Mitigation: batch GPU work through the pushbuffer rather than per-register traffic.
* **Boot quirks.** xemu documents that retail kernels in very early boot depend on
  CPU write-back cache behaviour to store a value into ROM and read it back. A
  hypervisor sees real cache behaviour only if the region is mapped writable; the
  machine map needs an explicit "early-boot writable ROM" overlay.
* **Timing.** Guest timers (PIT, APIC-less interrupt delivery) must be driven by
  a deterministic clock, not wall time, or games that poll timers drift.
* **Host capability.** Not every PC exposes hardware virtualization (some CI runners,
  some locked-down machines). That is why a correct, slower interpreter backend is on
  the roadmap rather than being dropped.

## Testing strategy

* Unit tests per crate (27 now). Parsers get truncation and corruption tests.
* Differential testing: use xemu as an oracle. Feed the same pushbuffer to both and
  compare register state or rendered frames.
* Golden traces: record real games once, replay without the CPU.
* Fuzzing for the parsers and the decoder.
