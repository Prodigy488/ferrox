# Roadmap

Honest scale: xemu is the result of years of work by many people. This list is in
the order that gets something visible on screen soonest, not the order that is
easiest. Each milestone ends with something you can run.

## M0 — Foundation (done)
Disc and executable readers, physical memory map, CPU backend interface, pushbuffer
decoder, CLI, tests.

## M0.5 — High-level kernel groundwork and app shell (done)
`ferrox-hle` (loader, stubs, 337-function export table from nxdk, 22 handlers),
`ferrox-kvm` (written, awaiting a KVM host), `ferrox-system`, the desktop app, blank
hard-drive images, CLI `run`/`hdd`/`demo`.

**Next, in order:**
1. Run `cargo test -p ferrox-kvm -- --ignored` on a KVM machine and fix what it finds. (The Windows backend has run the demo on real hardware; its first bug was register-buffer alignment.)
2. Kernel: threads, waits, events/semaphores/mutants/timers, critical sections, DPCs and the
   interrupt level are done (cooperative switching at kernel calls, a control block behind `fs`,
   a call log in the probe report). Still to do: hardware interrupts (vblank, GPU), preemption of
   code that never calls the kernel, and file access (`NtCreateFile` and friends over the disc image and FATX drive E:).
3. A homebrew program built with nxdk that only uses text output runs end to end.
4. Then M3/M4 below (GPU). Done so far: the command queue, colour clears and a software triangle rasteriser,  the vertex program and vertex arrays too; depth testing and textures too; register combiners, alpha test, blending and the colour mask too; next: game-driven work. `ferrox probe <game>` runs a real game for N seconds, keeps going past kernel functions that are not written yet and prints what it called, what is missing and where it stopped; the Windows test script runs it on a game of your choice and the reports decide the next kernel functions. Then mipmaps (sampler done, not yet wired), texture stages 1-3, the stencil test, fog, the fixed-function transform, then a Vulkan backend.
5. WHPX (Windows) and HVF (macOS) backends; qcow2 reading for xemu-made images.

## M1 — Run guest code
* A real CPU backend: KVM first (Linux), then WHPX and HVF.
* PCI configuration space, PIC/PIT, enough chipset to execute the MCPX boot ROM
  to the point where it reads the BIOS.
* Milestone check: the boot ROM runs and decrypts the BIOS (you supply both).
* In parallel, HLE mode on the same backend: a homebrew XBE runs with no BIOS and
  its first kernel calls (memory, threads, debug output) are served by `ferrox-hle`.

## M2 — Boot the BIOS
* SMBus, SMC and EEPROM, IDE with the XISO reader as the disc, flash ROM.
* Milestone check: BIOS reaches the point of initialising the GPU and the boot logo.

## M3 — GPU bring-up
* NV2A register blocks (PMC, PBUS, PFIFO, PGRAPH basics), DMA, display scan-out
  to a window. Pushbuffer decoder from M0 feeds PFIFO.
* Milestone check: the boot animation draws.

## M4 — Rendering
* Vulkan renderer: surfaces, vertex pipeline, register combiners, textures.
* Differential testing against xemu on recorded pushbuffers.

## M5 — Audio, input, USB, network
* Audio processor (APU with its DSP), AC'97, USB controller and gamepads, NIC.

## M6 — Player features
* Save states, rewind, resolution scaling and filters, patches, netplay, a
  controller-first library and settings UI.

## M7 — Compatibility push
* Game-by-game triage with automated screenshot regression tests.

## Things deliberately not promised
* No bundled BIOS, boot ROM or keys, ever.
* No claim of compatibility or speed ahead of measurements. Numbers go in this file
  when they exist.
