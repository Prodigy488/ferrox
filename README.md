# ferrox

A new original-Xbox emulator, written in Rust, designed from the start around
the things xemu's architecture makes hard: hardware-virtualized CPU execution,
a Vulkan-first renderer, small testable device crates, and modern player
features.

**Status: foundation only. ferrox does not run games yet.** What exists today
is the groundwork that everything else sits on, with tests:

| Crate | What it does | State |
|---|---|---|
| `ferrox-formats` | XDVDFS (XISO) disc reader, XBE executable parser | working, tested; XDVDFS also checked against an image made by `extract-xiso` |
| `ferrox-machine` | Physical address space: RAM, mirrored BIOS ROM, MMIO and port devices | working, tested |
| `ferrox-cpu` | CPU backend interface + a tiny reference interpreter for tests | interface done; **no real CPU backend yet** |
| `ferrox-nv2a` | GPU command-stream (pushbuffer) decoder | working, tested against xemu's `pfifo.c` rules |
| `ferrox-cli` | `ferrox info`, `ferrox ls`, `ferrox pushbuffer` | working |

```
cargo test --workspace
cargo run -p ferrox-cli -- ls game.iso
cargo run -p ferrox-cli -- info default.xbe
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design and
[docs/ROADMAP.md](docs/ROADMAP.md) for what comes next and why.

## You will need your own BIOS

Like every Xbox emulator, ferrox needs a dump of an MCPX boot ROM, a BIOS and a
hard-disk image from a console you own. They are not included and must not be
shared.

## License

GPL-2.0-or-later (see `LICENSE`). Behaviour is cross-checked against
[xemu](https://github.com/xemu-project/xemu) (GPL-2.0), which is why ferrox uses
the same license. ferrox is not affiliated with xemu or Microsoft.
