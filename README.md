# ferrox

A new original-Xbox emulator, written in Rust, designed from the start around
the things xemu's architecture makes hard: hardware-virtualized CPU execution,
a Vulkan-first renderer, small testable device crates, and modern player
features.

**Status: early. ferrox can run a small program's own code with the kernel
provided in software, and has a desktop app, but it cannot run commercial games:
there is no 3D graphics, audio, input or file access yet. Programs that draw into a plain framebuffer do show a picture.** Everything below has
tests, except where it says otherwise.

| Crate | What it does | State |
|---|---|---|
| `ferrox-formats` | XDVDFS (XISO) disc reader, XBE parser, blank FATX hard-drive image creator | working, tested; XDVDFS checked against an image made by `extract-xiso` |
| `ferrox-machine` | Physical address space: RAM, mirrored ROM, MMIO and port devices | working, tested |
| `ferrox-cpu` | CPU backend interface + a tiny test interpreter | done |
| `ferrox-whpx` | Windows backend (Windows Hypervisor Platform): same design as KVM | **runs the demo on a real Windows 11 PC**: native CPU, kernel calls, and GPU register reads/writes through Microsoft's instruction emulator |
| `ferrox-kvm` | Linux KVM backend: guest runs natively, only I/O and kernel calls exit | **compiles; never run** (no KVM on the dev machine). One ignored test awaits a KVM host |
| `ferrox-hle` | No-BIOS kernel: loads an XBE, patches its imports, answers kernel calls | 26 of 337 kernel functions implemented; the rest stop the run and name themselves |
| `ferrox-system` | Assembles a machine, picks a CPU backend, runs a game | working with the test interpreter; KVM path untested |
| `ferrox-nv2a` | GPU: pushbuffer decoder and command front end (PFIFO/PRAMIN/RAMHT/DMA objects, surface clears), plus the basic register block (chip id, scan-out address, vblank status, interrupt enables) | working, cross-checked against xemu's source. The command queue works: instance memory, object table, DMA objects, command-buffer fetch, surface setup and colour clears (the demo's rectangles are drawn by the GPU; verified on real Windows hardware). The vertex program runs (all MAC and ILU operations, constants, address register, field layout generated from xemu's table) and feeds a software reference rasteriser: colour-blended triangles from immediate-mode vertices or vertex arrays in memory. Depth buffers (16-bit and 24-bit with stencil bytes preserved), depth test functions, depth write mask and depth/stencil clears work. Textures: swizzled, linear and DXT1/3/5 layouts, 16 colour formats, wrap/mirror/clamp/border, nearest and bilinear filtering, perspective-correct interpolation (the texture is multiplied with the diffuse colour until register combiners exist). Not done yet: mipmaps, cube/3D textures, palettes, the register combiners and pixel shaders, blending, stencil test, fixed-function transform, swizzled render targets |
| `ferrox-app` | Desktop app `ferrox-gui`: library, play/console, settings, hard-drive creator | working; screenshots checked on a virtual display; no video yet |
| `ferrox-cli` | `info`, `ls`, `pushbuffer`, `run`, `hdd`, `demo` | working |

```
cargo test --workspace
cargo run --release -p ferrox-app --bin ferrox-gui     # the desktop app
cargo run -p ferrox-cli -- demo ./games                # writes a tiny demo program
cargo run -p ferrox-cli -- run ./games/Demo/default.xbe --reference-cpu
cargo run -p ferrox-cli -- ls game.iso
```

(On Linux the app needs OpenGL and, for `run` without `--reference-cpu`, `/dev/kvm`.)

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
[docs/ROADMAP.md](docs/ROADMAP.md) for what comes next and why.

## BIOS and hard drive

* **No-BIOS mode (the one that exists):** ferrox loads the game directly and
  provides the console's kernel itself. No copyrighted files are needed.
* **BIOS mode (planned):** boots the console's own firmware; you would supply your
  own boot ROM and BIOS from a console you own. They are never included.
* **Hard drive:** `ferrox hdd xbox_hdd.img 8` (or Settings → Create) makes a blank,
  formatted, sparse image with the console's standard partitions. Games cannot use
  it yet: the kernel's file functions are not written.

## License

GPL-2.0-or-later (see `LICENSE`). Behaviour is cross-checked against
[xemu](https://github.com/xemu-project/xemu) (GPL-2.0), which is why ferrox uses
the same license. The kernel export list comes from
[nxdk](https://github.com/XboxDev/nxdk) (CC0-1.0); the FATX format logic follows
nxdk's formatter (MIT). ferrox is not affiliated with xemu or Microsoft.
