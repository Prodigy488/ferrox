# The original-Xbox emulator landscape, and what ferrox takes from each

This is a design input, not a ranking. Sources are listed at the bottom. Where a
statement comes from general knowledge of the projects rather than a page I read
while writing this, it is marked *(unverified)*. Compatibility percentages on wiki
pages go stale quickly; treat them as rough.

## Three ways to emulate an Xbox

| Approach | What it does | Strength | Weakness |
|---|---|---|---|
| **Low-level (LLE)** | Emulates the hardware (CPU, chipset, GPU, audio) and boots the real BIOS | Most accurate; one implementation covers every game | Slow if the CPU is translated; needs the user's own BIOS and boot ROM |
| **High-level (HLE)** | Runs the game's code natively and replaces the kernel, Direct3D and audio with host code | Fast; needs no BIOS | Accuracy depends on how well each replaced API is imitated; breaks on games that touch hardware directly |
| **Hybrid** | HLE for some parts, LLE for others | Pick the cheap path where it is safe | Two codebases to keep correct |

## The projects

| Project | Approach | Status | Worth taking |
|---|---|---|---|
| **xemu** (and its ancestor **XQEMU**) | LLE, built on QEMU | Maintained; by far the most compatible | Hardware knowledge, the NV2A model, the test corpus of games that expose bugs. ferrox follows its register-level behaviour and uses it as a differential-testing oracle |
| **Cxbx-Reloaded** | Hybrid: HLE kernel, Direct3D and audio; GPU/NIC code taken from XQEMU *(unverified detail)* | Active; far lower compatibility than xemu on the wiki figures | The idea that a *kernel in the emulator* gives fast, BIOS-free start. ferrox's `ferrox-hle` is this idea |
| **Cxbx / Dxbx** | HLE | Abandoned | Cautionary: pure HLE needs a per-game patch database to stay compatible |
| **OpenXBOX → StrikeBox** | Hybrid, kernel in the emulator; Windows and Linux *(unverified)* | Inactive | Shows the HLE-kernel-plus-virtual-hardware split works as a design |
| **Zookeeper** | macOS Hypervisor.framework with its own small kernel *(unverified detail)* | Experimental | Evidence that hardware virtualization plus a host-side kernel is viable. This is the closest relative to ferrox's plan |
| **Tortoise / kvmbox / XBVM** | Hypervisor-based LLE attempts | Dead | Confirm the speed idea is old and the hard part is the chipset and boot quirks, not the CPU |
| **MAME** | Targets the Chihiro arcade board more than the console | Console driver not usable | Documentation value only |
| **Others** (MacBox, xexec, various HLE experiments) | Mixed | Mostly dormant | None needed |

## What ferrox combines

1. **From xemu: accuracy.** The default is hardware-level emulation. Anything that
   has to be exact (GPU, audio, boot) follows the hardware, with xemu as the oracle.
2. **From Cxbx-Reloaded / StrikeBox: a kernel in the emulator.** `ferrox-hle` loads
   an XBE, points its kernel imports at stubs, and answers calls on the host side.
   Needs no BIOS, so homebrew runs immediately and games get a fast-boot path.
3. **From Zookeeper and the dead hypervisor projects: native CPU speed.** Both
   modes run the guest CPU under KVM/WHPX/HVF. Only kernel calls leave the VM.
4. **From xemu's weaknesses:** one renderer (Vulkan) instead of two, device crates
   testable in isolation, save states and patches designed in from the start.

## Why the modes share one machine

The HLE stubs use a port write (`HLE_PORT`, 0xE0) to leave the guest. Every
hypervisor already reports port I/O to user space, so no special instruction or
trap is needed. In LLE mode the same machine runs with the real BIOS and the port is
simply unused. A game that fails in HLE mode can be retried in LLE mode without
changing the CPU, memory map or GPU.

## What is deliberately not done yet

* The kernel export table is **not** filled in. The registry is data-driven so exports
  are added as real games demand them, checked against a reliable reference, not
  typed from memory.
* Direct3D is not replaced. In HLE mode the game's Direct3D calls become NV2A
  pushbuffers exactly as on hardware, so the GPU path is shared with LLE mode.
* No claim is made about speed or compatibility until measured (see ROADMAP).

## Sources

* xboxdevwiki.net — Emulators page
* gametechwiki.com — Xbox emulators page (compatibility figures, possibly stale)
* Cxbx-Reloaded project README
* xemu project news (SDL3 migration, NV2A/APU updates, Vulkan backend changes)
