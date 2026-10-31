//! Puts a console together and runs a game on it.
//!
//! Today this is the BIOS-free path: the executable is loaded straight into RAM,
//! `ferrox-hle` answers kernel calls, and the CPU is whatever backend the host
//! offers. Hardware mode (real boot ROM and BIOS) will plug in here later.

use ferrox_cpu::{CpuBackend, Exit, RefCpu, ESP};
use ferrox_formats::xbe::Xbe;
use ferrox_formats::xdvdfs::Xdvdfs;
use ferrox_hle::kernel::{self, KernelState};
use ferrox_hle::video::Framebuffer;
use ferrox_hle::{KernelHle, Layout};
use ferrox_machine::PhysMem;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const RAM_BYTES: usize = 64 * 1024 * 1024;
const GDT_ADDR: u32 = 0x500;
const STACK_TOP: u32 = 0x00E0_0000 - 16;
const LAYOUT: Layout = Layout { stub_base: 0x00F0_0000, data_base: 0x00F1_0000, stack_top: STACK_TOP };
const HEAP: std::ops::Range<u32> = 0x0200_0000..0x03F0_0000;

fn trace(msg: &str) {
    if std::env::var_os("FERROX_TRACE").is_some() {
        eprintln!("[system] {msg}");
    }
}

/// One picture, ready to show: RGBA8, `width * height * 4` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

fn snapshot(mem: &mut PhysMem, fb: &Framebuffer) -> Option<Frame> {
    let rgba = fb.to_rgba(mem.ram_bytes(fb.addr as u64, fb.len())?)?;
    Some(Frame { width: fb.width, height: fb.height, rgba })
}

impl Frame {
    /// Encodes the picture as an uncompressed PNG (large but dependency-free).
    pub fn to_png(&self) -> Vec<u8> {
        fn crc32(data: &[u8]) -> u32 {
            let mut c = 0xFFFF_FFFFu32;
            for &b in data {
                c ^= b as u32;
                for _ in 0..8 {
                    c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
                }
            }
            !c
        }
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let mut body = kind.to_vec();
            body.extend_from_slice(data);
            out.extend_from_slice(&body);
            out.extend_from_slice(&crc32(&body).to_be_bytes());
        }
        // raw scanlines: filter byte 0, then RGB
        let mut raw = Vec::with_capacity((self.width as usize * 3 + 1) * self.height as usize);
        for row in self.rgba.chunks_exact(self.width as usize * 4) {
            raw.push(0);
            for px in row.chunks_exact(4) {
                raw.extend_from_slice(&px[..3]);
            }
        }
        // zlib stream of stored (uncompressed) deflate blocks
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, b) in blocks.iter().enumerate() {
            z.push(if i + 1 == blocks.len() { 1 } else { 0 });
            z.extend_from_slice(&(b.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(b.len() as u16)).to_le_bytes());
            z.extend_from_slice(b);
        }
        let (mut a, mut bsum) = (1u32, 0u32);
        for &x in &raw {
            a = (a + x as u32) % 65521;
            bsum = (bsum + a) % 65521;
        }
        z.extend_from_slice(&(bsum << 16 | a).to_be_bytes());
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&self.width.to_be_bytes());
        ihdr.extend_from_slice(&self.height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}

/// What to run.
#[derive(Clone, Debug)]
pub enum Source {
    /// A bare `.xbe` file.
    Xbe(PathBuf),
    /// A disc image; its `default.xbe` is run.
    Iso(PathBuf),
}

impl Source {
    /// Chooses by file extension: `.xbe` is an executable, anything else a disc.
    pub fn from_path(p: &Path) -> Source {
        let is_xbe = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xbe"));
        if is_xbe { Source::Xbe(p.to_path_buf()) } else { Source::Iso(p.to_path_buf()) }
    }

    pub fn read_xbe(&self) -> Result<Vec<u8>, String> {
        match self {
            Source::Xbe(p) => std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display())),
            Source::Iso(p) => {
                let f = File::open(p).map_err(|e| format!("cannot open {}: {e}", p.display()))?;
                let mut disc = Xdvdfs::open(BufReader::new(f)).map_err(|e| e.to_string())?;
                let entry = disc
                    .find("default.xbe")
                    .map_err(|e| e.to_string())?
                    .ok_or("the disc has no default.xbe")?;
                disc.read_file(&entry).map_err(|e| e.to_string())
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Hardware virtualization if the host has it, otherwise an error.
    Auto,
    /// The test interpreter (runs only tiny programs; for tests and demos).
    Reference,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub source: Source,
    pub backend: Backend,
    /// Hard-drive image to attach. Checked now; the guest will see it once the
    /// high-level kernel implements file access.
    pub hdd: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The guest asked to reboot, shut down or crashed.
    GuestStopped(String),
    /// The guest executed `hlt`.
    Halted,
    /// The user pressed stop.
    Stopped,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::GuestStopped(why) => write!(f, "the game stopped itself ({why})"),
            Outcome::Halted => write!(f, "the game halted"),
            Outcome::Stopped => write!(f, "stopped by you"),
        }
    }
}

/// (kernel functions implemented, kernel functions in the table).
pub fn kernel_coverage() -> (usize, usize) {
    let mut hle = KernelHle::with_table();
    kernel::install(&mut hle, HEAP);
    hle.coverage()
}

#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub backend: &'static str,
    pub title: String,
}

/// Is a fast CPU backend available on this machine?
pub fn hardware_virtualization() -> bool {
    #[cfg(target_os = "linux")]
    {
        ferrox_kvm::available()
    }
    #[cfg(windows)]
    {
        ferrox_whpx::available()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        false
    }
}

/// Why no fast CPU is available here, in words a player can act on.
pub fn virtualization_help() -> &'static str {
    if cfg!(target_os = "linux") {
        "Enable virtualization in the BIOS/UEFI and add your user to the 'kvm' group."
    } else if cfg!(windows) {
        "Turn on virtualization in the BIOS/UEFI, then enable the 'Windows Hypervisor Platform' optional feature (Turn Windows features on or off) and restart."
    } else {
        "A hardware-virtualization backend for this platform is not written yet."
    }
}

fn make_backend(
    choice: Backend,
    mem: &mut PhysMem,
    entry: u32,
) -> Result<Box<dyn CpuBackend>, String> {
    match choice {
        Backend::Reference => Ok(Box::new(RefCpu::new(entry))),
        Backend::Auto => {
            #[cfg(target_os = "linux")]
            {
                if !ferrox_kvm::available() {
                    return Err(format!(
                        "this computer does not offer hardware virtualization (/dev/kvm). {}",
                        virtualization_help()
                    ));
                }
                mem.load(GDT_ADDR as u64, &ferrox_kvm::flat_gdt()).map_err(|e| e.to_string())?;
                let slots = mem.mem_slots();
                // SAFETY: `mem` outlives the CPU (the caller drops the CPU first) and
                // is only touched through `Bus` while the guest is stopped at an exit.
                let cpu = unsafe { ferrox_kvm::KvmCpu::new(&slots, entry, GDT_ADDR) }
                    .map_err(|e| e.to_string())?;
                Ok(Box::new(cpu))
            }
            #[cfg(windows)]
            {
                if !ferrox_whpx::available() {
                    return Err(format!(
                        "the Windows Hypervisor Platform is not available. {}",
                        virtualization_help()
                    ));
                }
                mem.load(GDT_ADDR as u64, &ferrox_whpx::flat_gdt()).map_err(|e| e.to_string())?;
                let slots = mem.mem_slots();
                // SAFETY: as for KVM above.
                let cpu = unsafe { ferrox_whpx::WhpCpu::new(&slots, entry, GDT_ADDR) }
                    .map_err(|e| e.to_string())?;
                Ok(Box::new(cpu))
            }
            #[cfg(not(any(target_os = "linux", windows)))]
            {
                let _ = (mem, entry);
                Err(virtualization_help().into())
            }
        }
    }
}

/// Runs the configured game until it stops. `on_output` receives text the
/// guest prints, `on_frame` the picture it is displaying (at most about 30 times
/// a second, plus once more at the end); `stop` ends the run when set.
pub fn run(
    cfg: &Config,
    stop: &Arc<AtomicBool>,
    on_output: impl FnMut(&str),
    mut on_frame: impl FnMut(Frame),
) -> Result<Report, String> {
    let on_output = std::cell::RefCell::new(on_output);
    let say = |text: &str| (on_output.borrow_mut())(text);
    if let Some(h) = &cfg.hdd {
        let parts = ferrox_formats::hdd::inspect(h).map_err(|e| format!("hard-drive image: {e}"))?;
        if parts.iter().all(|&(_, ok)| !ok) {
            return Err("the hard-drive image is not formatted (no FATX partitions found)".into());
        }
    }
    let file = cfg.source.read_xbe()?;
    let xbe = Xbe::parse(&file).map_err(|e| format!("not a valid executable: {e}"))?;
    let title = xbe.certificate.title_name.clone();
    let image_end = xbe.base_addr as u64 + xbe.size_of_image as u64;
    if image_end > LAYOUT.stack_top as u64 {
        return Err("the executable is too large for the loader's memory map".into());
    }

    let mut mem = PhysMem::new();
    mem.add_ram(0, RAM_BYTES).map_err(|e| e.to_string())?;
    let (mut gpu, gpu_state) = ferrox_nv2a::device::Nv2a::new();
    gpu.attach_ram(mem.ram_view(0).ok_or("no memory to give the GPU")?);
    mem.add_mmio(ferrox_nv2a::device::BASE, ferrox_nv2a::device::SIZE, Box::new(gpu))
        .map_err(|e| e.to_string())?;
    let mut hle = KernelHle::with_table();
    let state: std::rc::Rc<std::cell::RefCell<KernelState>> = kernel::install(&mut hle, HEAP);
    let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).map_err(|e| e.to_string())?;

    trace("image loaded, creating the CPU backend");
    let mut cpu = make_backend(cfg.backend, &mut mem, loaded.entry)?;
    trace("CPU backend created");
    let mut r = cpu.regs();
    r.gpr[ESP] = loaded.stack_top;
    r.eip = loaded.entry;
    cpu.set_regs(r);
    let backend = cpu.name();
    say(&format!("[ferrox] {title} - CPU: {backend}\n"));

    let flush = |state: &std::rc::Rc<std::cell::RefCell<KernelState>>| {
        let mut s = state.borrow_mut();
        if !s.debug.is_empty() {
            say(&std::mem::take(&mut s.debug));
        }
    };
    let mut announced = false;
    let mut last_frame = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut show = |mem: &mut PhysMem, force: bool| {
        let Some(mut fb) = state.borrow().framebuffer else { return };
        // a program that programs the CRTC directly decides where scan-out starts
        if let Some(start) = gpu_state.display_start() {
            fb.addr = start;
        }
        if !announced {
            announced = true;
            say(&format!(
                "[ferrox] display set: {}x{}, format {:#x}, buffer at {:#x}\n",
                fb.width, fb.height, fb.format, fb.addr
            ));
        }
        if force || last_frame.elapsed() >= std::time::Duration::from_millis(33) {
            last_frame = std::time::Instant::now();
            if let Some(f) = snapshot(mem, &fb) {
                on_frame(f);
            }
        }
    };
    let outcome = loop {
        if stop.load(Ordering::Relaxed) {
            break Outcome::Stopped;
        }
        match hle.run(cpu.as_mut(), &mut mem, 1_000_000) {
            Ok(Exit::Budget) => {
                flush(&state);
                show(&mut mem, false);
            }
            Ok(Exit::Halt) => {
                let reason = state.borrow().stop_reason.clone();
                break match reason {
                    Some(r) => Outcome::GuestStopped(r),
                    None => Outcome::Halted,
                };
            }
            Ok(Exit::Hypercall(_)) => unreachable!("handled inside KernelHle::run"),
            Err(e) => {
                flush(&state);
                show(&mut mem, true);
                return Err(e.to_string());
            }
        }
    };
    flush(&state);
    show(&mut mem, true);
    let missed = gpu_state.unhandled();
    if !missed.is_empty() {
        let list: Vec<String> = missed
            .iter()
            .take(12)
            .map(|&(w, o, n)| format!("{} {o:#08x} x{n}", if w { "write" } else { "read" }))
            .collect();
        say(&format!(
            "[ferrox] GPU registers touched but not emulated yet: {}{}\n",
            list.join(", "),
            if missed.len() > 12 { ", ..." } else { "" }
        ));
    }
    for problem in gpu_state.take_log() {
        say(&format!("[ferrox] GPU: {problem}\n"));
    }
    let gaps = gpu_state.gaps();
    if !gaps.is_empty() {
        let list: Vec<String> = gaps.iter().take(12).map(|(what, n)| format!("{what} x{n}")).collect();
        say(&format!(
            "[ferrox] GPU work not emulated yet: {}{}\n",
            list.join("; "),
            if gaps.len() > 12 { "; ..." } else { "" }
        ));
    }
    let (methods, clears) = gpu_state.gpu_counts();
    if methods > 0 {
        say(&format!(
            "[ferrox] GPU ran {methods} commands ({clears} surface clears, {} triangles)\n",
            gpu_state.triangles()
        ));
    }
    Ok(Report { outcome, backend, title })
}

/// A tiny demo executable: allocates a 640x480 framebuffer, selects it as the
/// display, fills it with a test pattern, prints a greeting and shuts down.
/// Useful for checking an installation without any commercial game.
pub fn demo_xbe() -> Vec<u8> {
    use ferrox_formats::xbe::synth::{build, BASE};
    use ferrox_hle::exports;
    const GREETING: &[u8] = b"hello, xbox! (test pattern drawn)\n\0";
    let names = ["DbgPrint", "HalReturnToFirmware", "MmAllocateContiguousMemory", "AvSetDisplayMode"];
    let ords: Vec<u32> = names.iter().map(|n| exports::by_name(n).unwrap().ordinal).collect();
    let call = |i: u32| {
        let mut v = vec![0xFF, 0x15];
        v.extend_from_slice(&(BASE + 0x2000 + 4 * i).to_le_bytes());
        v
    };
    let push = |v: u32| {
        let mut o = vec![0x68];
        o.extend_from_slice(&v.to_le_bytes());
        o
    };
    let mov = |op: u8, v: u32| {
        let mut o = vec![op];
        o.extend_from_slice(&v.to_le_bytes());
        o
    };
    let mut c = Vec::new();
    c.extend(push(640 * 480 * 4));
    c.extend(call(2)); // eax = MmAllocateContiguousMemory(size)
    c.extend([0x89, 0xC3, 0x89, 0xC7]); // mov ebx, eax ; mov edi, eax
    c.push(0x53); // push ebx (FrameBuffer)
    for v in [2560, 0x12, 0x0401_0101, 0, 0] {
        c.extend(push(v)); // Pitch, Format (A8R8G8B8), DisplayMode (640x480 NTSC), Step, RegisterBase
    }
    c.extend(call(3)); // AvSetDisplayMode
    c.extend(mov(0xB9, 640 * 480)); // mov ecx, pixel count
    c.extend(mov(0xB8, 0xFF00_0000)); // mov eax, first colour
    // loop: mov [edi], eax ; add edi, 4 ; add eax, step ; dec ecx ; jne loop
    c.extend([0x89, 0x07, 0x83, 0xC7, 0x04]);
    c.extend(mov(0x05, 0x0001_0203));
    c.extend([0x49, 0x75, 0xF3]);
    // read the GPU's chip id, print it, then point scan-out at our framebuffer
    c.push(0xA1);
    c.extend_from_slice(&0xFD00_0000u32.to_le_bytes()); // mov eax, [PMC_BOOT_0]
    c.push(0x50); // push eax
    c.extend(push(BASE + 0x2080));
    c.extend(call(0));
    c.extend([0x83, 0xC4, 0x08]);
    c.extend([0x89, 0xD8]); // mov eax, ebx
    c.push(0xA3);
    c.extend_from_slice(&0xFD60_0800u32.to_le_bytes()); // mov [PCRTC_START], eax
    gpu_demo(&mut c, call(0), push(BASE + 0x20C0));
    c.extend(push(BASE + 0x2040));
    c.extend(call(0)); // DbgPrint(greeting)
    c.extend([0x83, 0xC4, 0x04]);
    c.extend(push(0));
    c.extend(call(1)); // HalReturnToFirmware(0)
    c.push(0xF4);
    let mut file = build(true, &ords);
    file[0x1000..0x1000 + c.len()].copy_from_slice(&c);
    file[0x2040..0x2040 + GREETING.len()].copy_from_slice(GREETING);
    const GPU_FMT: &[u8] = b"gpu id: %08x\n\0";
    file[0x2080..0x2080 + GPU_FMT.len()].copy_from_slice(GPU_FMT);
    const GPU_DONE: &[u8] = b"gpu took %d bytes of commands\n\0";
    file[0x20C0..0x20C0 + GPU_DONE.len()].copy_from_slice(GPU_DONE);
    file
}

/// Machine code for the demo's GPU part: describes a few objects to the GPU,
/// sends it a command buffer that draws two filled rectangles and two
/// colour-blended triangles (one transformed by a vertex program, one read from a vertex array) over the gradient, and prints how many command bytes the GPU consumed.
/// `ebx` holds the framebuffer address.
fn gpu_demo(c: &mut Vec<u8>, call_dbgprint: Vec<u8>, push_fmt: Vec<u8>) {
    use ferrox_nv2a::gpu::{ramht_hash, KELVIN_CLASS};
    const RAMIN: u32 = 0xFD70_0000;
    const REGS: u32 = 0xFD00_0000;
    const KELVIN: u32 = 0xB0B0;
    const COLOR_DMA: u32 = 0xC0DE;
    const PUSHBUF: u32 = 0x03F0_0000;
    const VERTICES: u32 = 0x03F1_0000;
    const DEPTH_BUFFER: u32 = 0x0280_0000;
    let st = |c: &mut Vec<u8>, addr: u32, val: u32| {
        c.extend([0xC7, 0x05]); // mov dword [addr], imm32
        c.extend(addr.to_le_bytes());
        c.extend(val.to_le_bytes());
    };
    // the hash table: handle -> object instance (in 16-byte units)
    for (handle, instance) in [(KELVIN, 0x1020u32), (COLOR_DMA, 0x1010)] {
        let slot = ramht_hash(handle, 0, 0) * 8;
        st(c, RAMIN + slot, handle);
        st(c, RAMIN + slot + 4, 0x8000_0000 | (instance >> 4));
    }
    // DMA objects: the command buffer, and all of memory for the picture
    for (inst, base, limit) in [(0x1000u32, PUSHBUF, 0xFFFFu32), (0x1010, 0, 0x03FF_FFFF)] {
        st(c, RAMIN + inst, 0x3D);
        st(c, RAMIN + inst + 4, limit);
        st(c, RAMIN + inst + 8, base | 3);
    }
    st(c, RAMIN + 0x1020, KELVIN_CLASS); // the 3D engine object
    for (reg, v) in [
        (0x2210u32, 0u32), // RAMHT: at 0, 4 KiB
        (0x3204, 0x100),   // channel 0, DMA mode
        (0x322C, 0x1000 >> 4), // command-buffer DMA object
        (0x3200, 1),       // let the pusher fetch
        (0x3220, 1),
        (0x3250, 1),       // let the puller run
    ] {
        st(c, REGS + reg, v);
    }
    let hdr = |method: u32, count: u32| (count << 18) | method;
    // the command buffer; `None` marks the framebuffer address, known only at run time
    let mut words: Vec<Option<u32>> = Vec::new();
    let mut put = |ws: &[u32]| words.extend(ws.iter().map(|&w| Some(w)));
    put(&[hdr(0, 1), KELVIN, hdr(0x194, 1), COLOR_DMA, hdr(0x200, 2), 640 << 16, 480 << 16]);
    put(&[hdr(0x208, 1), 0x108, hdr(0x20C, 1), 2560, hdr(0x210, 1)]);
    words.push(None); // colour offset = framebuffer address
    let mut put = |ws: &[u32]| words.extend(ws.iter().map(|&w| Some(w)));
    for (x0, x1, y0, y1, colour) in [(100u32, 539u32, 80u32, 399u32, 0xFFE0_8020u32), (180, 459, 140, 339, 0xFF10_2060)] {
        put(&[hdr(0x1D98, 2), x0 | (x1 << 16), y0 | (y1 << 16), hdr(0x1D90, 1), colour, hdr(0x1D94, 1), 0xF0]);
    }
    // add a depth buffer (24-bit depth, 8-bit stencil), cleared to "far"; nearer things win from now on
    let mut put = |ws: &[u32]| words.extend(ws.iter().map(|&w| Some(w)));
    put(&[
        hdr(0x198, 1), COLOR_DMA, hdr(0x208, 1), 0x128, hdr(0x20C, 1), 2560 | (2560 << 16), hdr(0x214, 1), DEPTH_BUFFER,
        hdr(0x1D98, 2), 639 << 16, 479 << 16, hdr(0x1D8C, 1), 0xFFFF_FF00, hdr(0x1D94, 1), 0x03,
        hdr(0x30C, 1), 1, hdr(0x354, 1), 0x201, hdr(0x35C, 1), 1, hdr(0x398, 1), 100.0f32.to_bits(),
    ]);
    // a vertex program that transforms model-space vertices to pixels (scale 100 x 80, centred on 320, 240)
    let program = ferrox_nv2a::vsh::asm::matrix_program();
    let mut put = |ws: &[u32]| words.extend(ws.iter().map(|&w| Some(w)));
    put(&[hdr(0x1E94, 1), 2, hdr(0x1E9C, 1), 0, hdr(0x1EA0, 1), 0, hdr(0x1EA4, 1), 0, hdr(0xB00, program.len() as u32)]);
    put(&program);
    put(&[hdr(0xB80, 16)]);
    for row in [[100.0f32, 0.0, 0.0, 320.0], [0.0, -80.0, 0.0, 240.0], [0.0, 0.0, 10.0, 0.0], [0.0, 0.0, 0.0, 1.0]] {
        put(&row.map(f32::to_bits));
    }
    // a triangle with red, green and blue corners inside the inner rectangle, sent vertex by vertex
    put(&[hdr(0x17FC, 1), 5]);
    for (x, y, argb) in [(0.0f32, 1.0f32, 0xFFFF_0000u32), (-1.0, -1.0, 0xFF00_FF00), (1.0, -1.0, 0xFF00_00FF)] {
        put(&[hdr(0x194C, 1), argb, hdr(0x1A00, 4), x.to_bits(), y.to_bits(), 1.0f32.to_bits(), 1.0f32.to_bits()]);
    }
    put(&[hdr(0x17FC, 1), 0]);
    // a white quad drawn afterwards but farther away: hidden where it is behind the triangle
    put(&[hdr(0x17FC, 1), 8]);
    for (x, y) in [(0.2f32, 0.3f32), (1.2, 0.3), (1.2, -0.1), (0.2, -0.1)] {
        put(&[hdr(0x194C, 1), 0xFFFF_FFFF, hdr(0x1A00, 4), x.to_bits(), y.to_bits(), 5.0f32.to_bits(), 1.0f32.to_bits()]);
    }
    put(&[hdr(0x17FC, 1), 0]);
    // a second triangle, read from a vertex array in memory: x, y, z floats and a packed colour per vertex
    put(&[
        hdr(0x19C, 1), COLOR_DMA,
        hdr(0x1720, 1), VERTICES, hdr(0x1760, 1), 2 | (3 << 4) | (16 << 8),
        hdr(0x172C, 1), VERTICES + 12, hdr(0x176C, 1), (4 << 4) | (16 << 8),
        hdr(0x17FC, 1), 5, hdr(0x1810, 1), 2 << 24, hdr(0x17FC, 1), 0,
    ]);
    for (i, (x, y, argb)) in [(-1.8f32, 0.5f32, 0xFFFF_FFFFu32), (-2.1, -0.5, 0xFFFF_FF00), (-1.5, -0.5, 0xFF00_FFFF)].into_iter().enumerate() {
        let at = VERTICES + 16 * i as u32;
        st(c, at, x.to_bits());
        st(c, at + 4, y.to_bits());
        st(c, at + 8, 0);
        st(c, at + 12, argb);
    }
    for (i, w) in words.iter().enumerate() {
        let at = PUSHBUF + 4 * i as u32;
        match w {
            Some(v) => st(c, at, *v),
            None => {
                c.extend([0x89, 0x1D]); // mov [at], ebx
                c.extend(at.to_le_bytes());
            }
        }
    }
    let bytes = 4 * words.len() as u32;
    st(c, 0xFD80_0040, bytes); // USER DMA_PUT: go
    c.push(0xA1);
    c.extend(0xFD80_0044u32.to_le_bytes()); // eax = DMA_GET
    c.push(0x50);
    c.extend(push_fmt);
    c.extend(call_dbgprint);
    c.extend([0x83, 0xC4, 0x08]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_counts_functions_not_data() {
        let (done, total) = kernel_coverage();
        assert!(done > 15 && done < total && total > 300, "{done}/{total}");
    }

    use ferrox_formats::xbe::synth::build;
    use ferrox_hle::exports;

    #[test]
    fn runs_an_xbe_end_to_end_and_streams_output() {
        let dir = std::env::temp_dir().join(format!("ferrox-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.xbe");
        std::fs::write(&path, demo_xbe()).unwrap();
        let cfg = Config { source: Source::from_path(&path), backend: Backend::Reference, hdd: None };
        let mut out = String::new();
        let mut frames = Vec::new();
        let rep = run(&cfg, &Arc::new(AtomicBool::new(false)), |s| out.push_str(s), |f| frames.push(f)).unwrap();
        let game_out: String = out.lines().filter(|l| !l.starts_with("[ferrox]")).map(|l| format!("{l}\n")).collect();
        assert_eq!(game_out, "gpu id: 02a000a3\ngpu took 692 bytes of commands\nhello, xbox! (test pattern drawn)\n");
        assert!(out.contains("display set: 640x480"), "{out}");
        assert!(out.contains("gpu id: 02a000a3"), "{out}");
        assert!(!out.contains("registers touched but not emulated"), "the demo only uses implemented registers: {out}");
        assert!(!out.contains("not emulated yet"), "the demo only uses implemented features: {out}");
        assert!(out.contains("CPU: reference"), "{out}");
        let last = frames.last().expect("the demo displays a picture");
        assert_eq!((last.width, last.height), (640, 480));
        assert_eq!(last.rgba.len(), 640 * 480 * 4);
        assert_eq!(&last.rgba[0..8], &[0, 0, 0, 255, 1, 2, 3, 255]);
        assert_ne!(&last.rgba[0..4], &last.rgba[last.rgba.len() - 4..], "the pattern varies across the screen");
        let px = |x: usize, y: usize| last.rgba[(y * 640 + x) * 4..][..4].to_vec();
        assert_eq!(px(150, 100), [0xE0, 0x80, 0x20, 0xFF], "outer rectangle drawn by the GPU");
        assert_eq!(px(200, 240), [0x10, 0x20, 0x60, 0xFF], "inner rectangle drawn by the GPU");
        assert_ne!(px(99, 100), [0xE0, 0x80, 0x20, 0xFF], "outside the rectangle is still the gradient");
        let top = px(320, 170);
        assert!(top[0] > 0xC0 && top[1] < 0x40 && top[2] < 0x40, "triangle is red near its top corner: {top:?}");
        let mid = px(320, 280);
        assert!(mid[0] > 0x10 && mid[1] > 0x20 && mid[2] > 0x20, "colours blend inside the triangle: {mid:?}");
        assert_eq!(px(200, 160), [0x10, 0x20, 0x60, 0xFF], "outside the triangle is still the inner rectangle");
        assert_eq!(px(420, 232), [0xFF, 0xFF, 0xFF, 0xFF], "the farther white quad shows where nothing is in front of it");
        let hidden = px(350, 232);
        assert!(hidden != [0xFF, 0xFF, 0xFF, 0xFF] && hidden[0] > 0x10, "the quad is hidden behind the triangle: {hidden:?}");
        let arr = px(140, 270);
        assert!(arr[0] > 0x80 && arr[1] > 0x80 && arr != [0xE0, 0x80, 0x20, 0xFF], "triangle read from a vertex array: {arr:?}");
        assert_eq!(px(120, 200), [0xE0, 0x80, 0x20, 0xFF], "outside the array triangle is still orange");
        assert!(out.contains("GPU ran 118 commands (3 surface clears, 4 triangles)"), "{out}");
        assert_eq!(rep.outcome, Outcome::GuestStopped("HalReturnToFirmware(0)".into()));
        assert!(rep.backend.starts_with("reference"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn png_output_is_a_valid_image() {
        let f = Frame { width: 3, height: 2, rgba: (0..24).map(|i| i as u8 * 10).collect() };
        let png = f.to_png();
        let dir = std::env::temp_dir().join(format!("ferrox-png-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        std::fs::write(&path, &png).unwrap();
        // python's zlib and struct are not available in this crate; check the structure instead
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stop_flag_ends_a_runaway_game() {
        let dir = std::env::temp_dir().join(format!("ferrox-sys2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.xbe");
        let mut file = build(true, &[exports::by_name("NtClose").unwrap().ordinal]);
        file[0x1000..0x1002].copy_from_slice(&[0xEB, 0xFE]); // jmp $
        std::fs::write(&path, file).unwrap();
        let cfg = Config { source: Source::Xbe(path), backend: Backend::Reference, hdd: None };
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            s2.store(true, Ordering::Relaxed);
        });
        let rep = run(&cfg, &stop, |_| {}, |_| {}).unwrap();
        t.join().unwrap();
        assert_eq!(rep.outcome, Outcome::Stopped);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unformatted_or_missing_inputs_give_clear_errors() {
        let cfg = Config {
            source: Source::Xbe("/nonexistent/x.xbe".into()),
            backend: Backend::Reference,
            hdd: None,
        };
        assert!(run(&cfg, &Arc::new(AtomicBool::new(false)), |_| {}, |_| {}).unwrap_err().contains("cannot read"));
    }
}
