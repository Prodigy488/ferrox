//! Puts a console together and runs a game on it.
//!
//! Today this is the BIOS-free path: the executable is loaded straight into RAM,
//! `ferrox-hle` answers kernel calls, and the CPU is whatever backend the host
//! offers. Hardware mode (real boot ROM and BIOS) will plug in here later.

use ferrox_core::Bus;
use ferrox_cpu::{CpuBackend, Exit, RefCpu, ESP};
use ferrox_formats::xbe::Xbe;
use ferrox_formats::xdvdfs::Xdvdfs;
use ferrox_hle::kernel::{self, KernelState};
use ferrox_hle::video::Framebuffer;
use ferrox_hle::{io, KernelHle, Layout, SchedAreas};
use ferrox_machine::PhysMem;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const RAM_BYTES: usize = 64 * 1024 * 1024;
const GDT_ADDR: u32 = 0x500;
const STACK_TOP: u32 = 0x00E0_0000 - 16;
/// Where the scheduler's control block, code stubs and kernel objects live (between the
/// main stack and the import stubs).
const SCHED: SchedAreas = SchedAreas {
    kpcr: 0x00E1_0000,
    stubs: 0x00E1_1000,
    objects: 0x00E2_0000..0x00EF_0000,
    stack_top: STACK_TOP,
    stack_size: 0x4_0000,
};
/// Memory-mapped chips that are not emulated yet: (name, base, size).
const PLACEHOLDERS: [(&str, u64, u64); 5] = [
    ("audio processor", 0xFE80_0000, 0x8_0000),
    ("AC97 audio", 0xFEC0_0000, 0x1000),
    ("USB 0", 0xFED0_0000, 0x1000),
    ("USB 1", 0xFED0_8000, 0x1000),
    ("network", 0xFEF0_0000, 0x400),
];
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
    /// A folder holding an extracted game; its `default.xbe` is run.
    Folder(PathBuf),
}

impl Source {
    /// Chooses by file extension: `.xbe` is an executable, anything else a disc.
    pub fn from_path(p: &Path) -> Source {
        if p.is_dir() {
            return Source::Folder(p.to_path_buf());
        }
        let is_xbe = p.extension().is_some_and(|e| e.eq_ignore_ascii_case("xbe"));
        if is_xbe { Source::Xbe(p.to_path_buf()) } else { Source::Iso(p.to_path_buf()) }
    }

    /// Finds the game behind a path the user typed: a file, a folder with `default.xbe`, a
    /// folder holding a disc image, or a name whose extension was left off.
    pub fn resolve(p: &Path) -> Result<Source, String> {
        let is_disc = |q: &Path| {
            q.extension().is_some_and(|e| ["iso", "xiso", "xbe"].iter().any(|x| e.eq_ignore_ascii_case(x)))
        };
        let list = |d: &Path| -> Vec<PathBuf> {
            let mut v: Vec<PathBuf> = std::fs::read_dir(d).map(|r| r.filter_map(|e| e.ok()).map(|e| e.path()).collect()).unwrap_or_default();
            v.sort();
            v
        };
        if p.is_file() {
            return Ok(Source::from_path(p));
        }
        if p.is_dir() {
            let entries = list(p);
            let has_xbe = entries.iter().any(|q| q.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("default.xbe")));
            if has_xbe {
                return Ok(Source::Folder(p.to_path_buf()));
            }
            if let Some(q) = entries.iter().find(|q| q.is_file() && is_disc(q)) {
                return Ok(Source::from_path(q));
            }
            for sub in entries.iter().filter(|q| q.is_dir()) {
                let inner = list(sub);
                if inner.iter().any(|q| q.file_name().is_some_and(|n| n.to_string_lossy().eq_ignore_ascii_case("default.xbe"))) {
                    return Ok(Source::Folder(sub.clone()));
                }
                if let Some(q) = inner.iter().find(|q| q.is_file() && is_disc(q)) {
                    return Ok(Source::from_path(q));
                }
            }
            let names: Vec<String> = entries.iter().take(12).map(|q| q.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).collect();
            return Err(format!("no default.xbe, .xbe, .iso or .xiso found in {} (it holds: {})", p.display(), names.join(", ")));
        }
        for ext in ["iso", "xiso", "xbe", "ISO"] {
            let mut name = p.as_os_str().to_os_string();
            name.push(".");
            name.push(ext);
            let q = PathBuf::from(name);
            if q.is_file() {
                return Ok(Source::from_path(&q));
            }
        }
        let near: Vec<String> = match (p.parent(), p.file_name()) {
            (Some(dir), Some(stem)) => {
                let stem = stem.to_string_lossy().to_lowercase();
                let first = stem.chars().take(6).collect::<String>();
                list(dir).iter().filter_map(|q| q.file_name().map(|n| n.to_string_lossy().into_owned())).filter(|n| n.to_lowercase().starts_with(&first)).take(8).collect()
            }
            _ => Vec::new(),
        };
        Err(format!("{} was not found{}", p.display(), if near.is_empty() { String::new() } else { format!(" (similar names there: {})", near.join(", ")) }))
    }

    /// The files the game sees as drive `D:`.
    pub fn disc_volume(&self) -> Option<Box<dyn io::Volume>> {
        match self {
            Source::Xbe(p) => Some(Box::new(io::HostVolume::new(p.parent()?.to_path_buf()))),
            Source::Folder(p) => Some(Box::new(io::HostVolume::new(p.clone()))),
            Source::Iso(p) => {
                let f = File::open(p).ok()?;
                let disc = Xdvdfs::open(BufReader::new(f)).ok()?;
                Some(Box::new(io::DiscVolume::new(disc)))
            }
        }
    }

    pub fn read_xbe(&self) -> Result<Vec<u8>, String> {
        match self {
            Source::Xbe(p) => std::fs::read(p).map_err(|e| format!("cannot read {}: {e}", p.display())),
            Source::Folder(p) => {
                let found = std::fs::read_dir(p)
                    .map_err(|e| format!("cannot read folder {}: {e}", p.display()))?
                    .filter_map(|e| e.ok())
                    .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case("default.xbe"))
                    .ok_or_else(|| format!("{} has no default.xbe", p.display()))?;
                std::fs::read(found.path()).map_err(|e| format!("cannot read {}: {e}", found.path().display()))
            }
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
    /// Compatibility probe: keep going past kernel functions that are not written yet, stop
    /// after this long, and print a report of what the game did.
    pub probe: Option<std::time::Duration>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The guest asked to reboot, shut down or crashed.
    GuestStopped(String),
    /// The guest executed `hlt`.
    Halted,
    /// The user pressed stop.
    Stopped,
    /// A probe ran out of time while the game was still running.
    TimedOut,
    /// The CPU or the machine could not continue (probe mode reports this instead of failing).
    Crashed(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::GuestStopped(why) => write!(f, "the game stopped itself ({why})"),
            Outcome::Halted => write!(f, "the game halted"),
            Outcome::Stopped => write!(f, "stopped by you"),
            Outcome::TimedOut => write!(f, "still running when the probe's time ran out"),
            Outcome::Crashed(why) => write!(f, "could not continue ({why})"),
        }
    }
}

/// (kernel functions implemented, kernel functions in the table).
pub mod selftest;

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
    // the console shows its memory again at 0x80000000 and 0xF0000000
    for alias in [0x8000_0000u64, 0xF000_0000] {
        mem.add_alias(alias, RAM_BYTES as u64, 0).map_err(|e| e.to_string())?;
    }
    // chips that are not emulated yet answer with zeros, and are listed in the report
    let mut placeholders: Vec<(&str, u64, ferrox_machine::PlaceholderLog)> = Vec::new();
    for (name, base, size) in PLACEHOLDERS {
        let (dev, log) = ferrox_machine::Placeholder::new();
        mem.add_mmio(base, size, Box::new(dev)).map_err(|e| e.to_string())?;
        placeholders.push((name, base, log));
    }
    let mut hle = KernelHle::with_table();
    let state: std::rc::Rc<std::cell::RefCell<KernelState>> = kernel::install(&mut hle, HEAP);
    let files = std::rc::Rc::new(std::cell::RefCell::new(io::Files::new(xbe.certificate.title_id)));
    if let Some(disc) = cfg.source.disc_volume() {
        files.borrow_mut().mount("\\Device\\CdRom0", disc);
    }
    io::install(&mut hle, files.clone());
    // the display signals each picture's end 60 times a second; the driver's handler hears it
    // on the GPU's interrupt line (bus level 3 -> vector 0x33)
    {
        let gpu = gpu_state.clone();
        let kernel_state = state.clone();
        let mut next = 0u64;
        hle.add_interrupt_source(0x33, move |now| {
            // the kernel setting aside the chip's instance memory: from then on the game's writes
            // to it are what the chip reads
            if let Some(base) = kernel_state.borrow().instance_memory {
                gpu.set_ramin_base(base);
            }
            if now >= next {
                gpu.vblank();
                next = now + 166_667;
            }
            gpu.irq_line()
        });
    }
    hle.set_lenient(cfg.probe.is_some());
    let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).map_err(|e| e.to_string())?;
    hle.start_scheduler(&mut mem, SCHED).map_err(|e| e.to_string())?;
    hle.sched.use_real_clock();

    trace("image loaded, creating the CPU backend");
    let mut cpu = make_backend(cfg.backend, &mut mem, loaded.entry)?;
    trace("CPU backend created");
    let mut r = cpu.regs();
    r.gpr[ESP] = loaded.stack_top;
    r.eip = loaded.entry;
    cpu.set_regs(r);
    cpu.set_fs_base(SCHED.kpcr);
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
    let started = std::time::Instant::now();
    // a game that hangs the host itself (not just the guest) would never reach the report: a
    // watchdog says what it can see from outside and ends the process
    let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if let Some(limit) = cfg.probe {
        let (done, gpu, say_to) = (finished.clone(), gpu_state.clone(), title.clone());
        std::thread::spawn(move || {
            std::thread::sleep(limit + std::time::Duration::from_secs(6));
            if done.load(Ordering::Relaxed) {
                return;
            }
            let calls = ferrox_hle::KERNEL_CALLS.load(Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_secs(2));
            let now = ferrox_hle::KERNEL_CALLS.load(Ordering::Relaxed);
            let act = gpu.activity();
            let recent: Vec<String> = gpu
                .recent_accesses()
                .iter()
                .rev()
                .take(24)
                .map(|&(w, o, v)| format!("{}{o:#x}={v:#x}", if w { "W" } else { "R" }))
                .collect();
            println!("[probe] HUNG: {say_to} did not come back to the probe loop {} s after its time was up", 8);
            println!("[probe] kernel calls in the last 2 s: {} (total {now})", now - calls);
            if act != 0 {
                println!(
                    "[probe] the GPU is stuck running a graphics method: subchannel {} method {:#x} value {:#x}",
                    (act >> 48) & 7,
                    (act >> 32) & 0xFFFF,
                    act as u32
                );
            } else {
                println!("[probe] the GPU is idle, so the loop is in the guest or the CPU backend");
            }
            println!("[probe] last GPU register accesses (newest first): {}", recent.join(" "));
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::exit(3);
        });
    }
    let outcome = loop {
        if stop.load(Ordering::Relaxed) {
            break Outcome::Stopped;
        }
        if cfg.probe.is_some_and(|limit| started.elapsed() >= limit) {
            break Outcome::TimedOut;
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
                if cfg.probe.is_none() {
                    return Err(e.to_string());
                }
                break Outcome::Crashed(e.to_string());
            }
        }
    };
    finished.store(true, Ordering::Relaxed);
    flush(&state);
    show(&mut mem, true);
    if cfg.probe.is_some() {
        probe_report(&say, &xbe, &file, &hle, &outcome, started.elapsed(), cpu.as_mut(), &mut mem);
        let opens = files.borrow().recent();
        if !opens.is_empty() {
            say(&format!("[probe] file operations (last {}; {} bytes read from files in total):\n", opens.len(), files.borrow().bytes_read));
            for o in opens {
                say(&format!("[probe]   {o}\n"));
            }
        }
    }
    for (name, base, log) in &placeholders {
        let t = log.touched();
        if !t.is_empty() {
            let list: Vec<String> = t
                .iter()
                .take(16)
                .map(|&(w, o, n)| format!("{} {o:#x} x{n}", if w { "write" } else { "read" }))
                .collect();
            say(&format!(
                "[ferrox] the {name} chip ({base:#x}) is not emulated yet; the game touched: {}{}\n",
                list.join(", "),
                if t.len() > 16 { ", ..." } else { "" }
            ));
        }
    }
    if cfg.probe.is_some() {
        let hot = gpu_state.hottest(10);
        if !hot.is_empty() {
            let list: Vec<String> = hot.iter().map(|&(w, o, n)| format!("{} {o:#x} x{n}", if w { "write" } else { "read" })).collect();
            say(&format!("[probe] GPU registers touched most: {}\n", list.join(", ")));
        }
        let recent = gpu_state.recent_accesses();
        if !recent.is_empty() {
            let list: Vec<String> = recent.iter().map(|&(w, o, v)| format!("{}{o:#x}={v:#x}", if w { "W" } else { "R" })).collect();
            say(&format!("[probe] last GPU register accesses (oldest first): {}\n", list.join(" ")));
        }
    }
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

/// Prints what a probe run learned: how the executable is built, which kernel functions it used,
/// which of those are missing, and where the CPU was when the run ended.
#[allow(clippy::too_many_arguments)]
fn probe_report(
    say: &dyn Fn(&str),
    xbe: &Xbe,
    file: &[u8],
    hle: &KernelHle,
    outcome: &Outcome,
    elapsed: std::time::Duration,
    cpu: &mut dyn CpuBackend,
    mem: &mut PhysMem,
) {
    say("[probe] ---- compatibility report ----\n");
    say(&format!(
        "[probe] title: {}, base {:#x}, image {:#x} bytes, entry {:#x}, {} sections\n",
        xbe.certificate.title_name, xbe.base_addr, xbe.size_of_image, xbe.entry_point, xbe.sections.len()
    ));
    if let Ok(ords) = xbe.kernel_imports(file) {
        let names: Vec<(u32, &str)> = ords.iter().map(|&o| (o, hle.name_of(o).unwrap_or("?"))).collect();
        let missing: Vec<String> = names.iter().filter(|(_, n)| !hle.is_implemented(n)).map(|(o, n)| format!("{n}(#{o})")).collect();
        say(&format!(
            "[probe] imports {} kernel functions/objects, {} of them not written yet\n",
            names.len(),
            missing.len()
        ));
        if !missing.is_empty() {
            say(&format!("[probe] imported but not written yet: {}\n", missing.join(", ")));
        }
    }
    let called = hle.missing_calls();
    if called.is_empty() {
        say("[probe] no calls to missing kernel functions\n");
    } else {
        let list: Vec<String> = called.iter().take(40).map(|(n, o, c)| format!("{n}(#{o}) x{c}")).collect();
        say(&format!("[probe] MISSING kernel functions the game actually called (each answered with 0): {}\n", list.join(", ")));
    }
    let used = hle.implemented_calls();
    let total: u64 = used.iter().map(|x| x.1).sum();
    let list: Vec<String> = used.iter().take(25).map(|(n, c)| format!("{n} x{c}")).collect();
    say(&format!("[probe] kernel functions that worked: {total} calls: {}\n", list.join(", ")));
    say(&format!("[probe] outcome after {:.1} s: {outcome}\n", elapsed.as_secs_f32()));
    let r = cpu.regs();
    say(&format!(
        "[probe] cpu: eip {:#010x} esp {:#010x} eax {:#x} ebx {:#x} ecx {:#x} edx {:#x} esi {:#x} edi {:#x} ebp {:#x}\n",
        r.eip, r.gpr[ESP], r.gpr[0], r.gpr[3], r.gpr[1], r.gpr[2], r.gpr[6], r.gpr[7], r.gpr[5]
    ));
    let mut code = [0u8; 16];
    for (i, b) in code.iter_mut().enumerate() {
        *b = mem.read(r.eip as u64 + i as u64, 1).unwrap_or(0) as u8;
    }
    say(&format!("[probe] bytes at eip: {}\n", code.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")));
    // the code around where the game is: enough to read a wait loop
    let from = r.eip.saturating_sub(64);
    let around: Vec<String> = (0..128u64)
        .map(|i| format!("{:02x}", mem.read(from as u64 + i, 1).unwrap_or(0)))
        .collect();
    say(&format!("[probe] code from {from:#x}: {}\n", around.join("")));
    say(&format!("[probe] interrupt handlers started: {}\n", hle.delivered));
    let mut stack = Vec::new();
    for i in 0..8u64 {
        stack.push(format!("{:#x}", mem.read(r.gpr[ESP] as u64 + 4 * i, 4).unwrap_or(0)));
    }
    say(&format!("[probe] top of stack: {}\n", stack.join(" ")));
    say(&format!("[probe] threads: {}\n", hle.sched.describe_threads()));
    let calls = hle.recent_calls();
    if !calls.is_empty() {
        say("[probe] last kernel calls (oldest first):\n");
        for line in calls {
            say(&format!("[probe]   {line}\n"));
        }
    }
    say("[probe] ---- end of report ----\n");
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
/// colour-blended triangles (one transformed by a vertex program, one read from a vertex array), a
/// depth-tested quad, a combiner-shaded textured quad and an alpha-blended quad over the gradient, and prints how many command bytes the GPU consumed.
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
    const TEXTURE: u32 = 0x03F2_0000;
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
    // a 4x4 texture (swizzled, A8R8G8B8: red grows with x, green with y) on a quad at the right
    let mut put = |ws: &[u32]| words.extend(ws.iter().map(|&w| Some(w)));
    put(&[
        hdr(0x184, 1), COLOR_DMA, hdr(0x1B00, 1), TEXTURE,
        hdr(0x1B04, 1), 1 | (2 << 4) | (0x06 << 8) | (1 << 16) | (2 << 20) | (2 << 24),
        hdr(0x1B08, 1), 0x0001_0101, hdr(0x1B0C, 1), 1 << 30, hdr(0x1B14, 1), (1 << 16) | (1 << 24), hdr(0x1E70, 1), 1,
    ]);
    // register combiners: one stage, r0 = texture * diffuse (colour and alpha), final output = r0
    put(&[
        hdr(0xAC0, 1), 0x0804_0000, hdr(0x1E40, 1), 0xC0,
        hdr(0x260, 1), 0x1814_0000, hdr(0xAA0, 1), 0xC0,
        hdr(0x288, 1), 0x0000_0C00, hdr(0x28C, 1), 0x0000_1C00,
        hdr(0x1E60, 1), 1,
    ]);
    put(&[hdr(0x17FC, 1), 8]);
    for (x, y, s_, t_) in [(1.5f32, 1.125f32, 0.0f32, 0.0f32), (2.1, 1.125, 1.0, 0.0), (2.1, -0.125, 1.0, 1.0), (1.5, -0.125, 0.0, 1.0)] {
        put(&[hdr(0x194C, 1), 0xFFFF_FFFF, hdr(0x18C8, 2), s_.to_bits(), t_.to_bits(), hdr(0x1A00, 4), x.to_bits(), y.to_bits(), 0, 1.0f32.to_bits()]);
    }
    put(&[hdr(0x17FC, 1), 0]);
    // a translucent black quad over the right half of the textured one: source-alpha blending,
    // no texture, combiners that pass the diffuse colour straight through
    put(&[
        hdr(0x1B0C, 1), 0, hdr(0x1E70, 1), 0, hdr(0x30C, 1), 0,
        hdr(0xAC0, 1), 0x0420_0000, hdr(0x260, 1), 0x1420_0000,
        hdr(0x304, 1), 1, hdr(0x344, 1), 0x302, hdr(0x348, 1), 0x303, hdr(0x350, 1), 0x8006,
        hdr(0x17FC, 1), 8,
    ]);
    for (x, y) in [(1.8f32, 1.125f32), (2.1, 1.125), (2.1, -0.125), (1.8, -0.125)] {
        put(&[hdr(0x194C, 1), 0x8000_0000, hdr(0x1A00, 4), x.to_bits(), y.to_bits(), 0, 1.0f32.to_bits()]);
    }
    put(&[hdr(0x17FC, 1), 0, hdr(0x304, 1), 0]);
    for y in 0..4u32 {
        for x in 0..4u32 {
            let index = ferrox_nv2a::texture::swizzle_index(x, y, 4, 4);
            st(c, TEXTURE + 4 * index, 0xFF00_0080 | ((x * 85) << 16) | ((y * 85) << 8));
        }
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
        let cfg = Config { source: Source::from_path(&path), backend: Backend::Reference, hdd: None, probe: None };
        let mut out = String::new();
        let mut frames = Vec::new();
        let rep = run(&cfg, &Arc::new(AtomicBool::new(false)), |s| out.push_str(s), |f| frames.push(f)).unwrap();
        let last = frames.last().expect("the demo displays a picture");
        let failed: Vec<String> = selftest::check_demo_output(&out)
            .into_iter()
            .chain(selftest::check_demo_frame(last))
            .filter(|c| c.status == selftest::Status::Fail)
            .map(|c| c.line())
            .collect();
        assert!(failed.is_empty(), "{failed:#?}\n{out}");
        assert!(out.contains("CPU: reference"), "{out}");
        assert_eq!((last.width, last.height), (640, 480));
        assert_eq!(rep.outcome, Outcome::GuestStopped("HalReturnToFirmware(0)".into()));
        assert!(rep.backend.starts_with("reference"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_builtin_selftest_passes_without_the_fast_cpu() {
        let dir = std::env::temp_dir().join(format!("ferrox-selftest-{}", std::process::id()));
        let mut log = String::new();
        let checks = selftest::run_all(&dir, selftest::Mode::NoHardware, &mut |s| log.push_str(s));
        let (pass, fail, _) = selftest::summary(&checks);
        assert_eq!(fail, 0, "{log}");
        assert!(pass >= 25, "{log}");
        assert!(dir.join("selftest-interpreter.png").exists());
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
    fn a_probe_survives_and_lists_missing_kernel_functions() {
        let dir = std::env::temp_dir().join(format!("ferrox-sys3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let name = "IoStartPacket";
        assert!(!{
            let mut h = KernelHle::with_table();
            kernel::install(&mut h, HEAP);
            h.is_implemented(name)
        }, "pick another unimplemented export for this test");
        let mut file = build(true, &[exports::by_name(name).unwrap().ordinal]);
        let slot = ferrox_formats::xbe::synth::BASE + 0x2000;
        let mut code = vec![0xFF, 0x15];
        code.extend(slot.to_le_bytes());
        code.extend([0xFF, 0x15]);
        code.extend(slot.to_le_bytes());
        code.push(0xF4); // hlt
        file[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let path = dir.join("missing.xbe");
        std::fs::write(&path, &file).unwrap();
        let mut cfg = Config { source: Source::Xbe(path), backend: Backend::Reference, hdd: None, probe: None };
        // without a probe the first missing call is an error naming the function
        let err = run(&cfg, &Arc::new(AtomicBool::new(false)), |_| {}, |_| {}).unwrap_err();
        assert!(err.contains(name), "{err}");
        cfg.probe = Some(std::time::Duration::from_secs(5));
        let mut out = String::new();
        let rep = run(&cfg, &Arc::new(AtomicBool::new(false)), |s| out.push_str(s), |_| {}).unwrap();
        assert_eq!(rep.outcome, Outcome::Halted, "{out}");
        assert!(out.contains(&format!("{name}(#")) && out.contains("x2"), "{out}");
        assert!(out.contains("MISSING kernel functions"), "{out}");
    }

    #[test]
    fn typed_game_paths_are_resolved_forgivingly() {
        let root = std::env::temp_dir().join(format!("ferrox-resolve-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["extracted", "boxed/inner", "empty"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("extracted/Default.xbe"), b"x").unwrap();
        std::fs::write(root.join("boxed/inner/game.xiso"), b"x").unwrap();
        std::fs::write(root.join("Named Game (USA).iso"), b"x").unwrap();
        let r = |p: &str| Source::resolve(&root.join(p));
        assert!(matches!(r("extracted"), Ok(Source::Folder(_))));
        assert!(matches!(r("boxed"), Ok(Source::Iso(p)) if p.ends_with("game.xiso")));
        assert!(matches!(r("Named Game (USA)"), Ok(Source::Iso(p)) if p.ends_with("Named Game (USA).iso")));
        assert!(matches!(r("Named Game (USA).iso"), Ok(Source::Iso(_))));
        let e = r("empty").unwrap_err();
        assert!(e.contains("no default.xbe"), "{e}");
        let e = r("Named Gam").unwrap_err();
        assert!(e.contains("not found") && e.contains("Named Game (USA).iso"), "{e}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn stop_flag_ends_a_runaway_game() {
        let dir = std::env::temp_dir().join(format!("ferrox-sys2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("loop.xbe");
        let mut file = build(true, &[exports::by_name("NtClose").unwrap().ordinal]);
        file[0x1000..0x1002].copy_from_slice(&[0xEB, 0xFE]); // jmp $
        std::fs::write(&path, file).unwrap();
        let cfg = Config { source: Source::Xbe(path), backend: Backend::Reference, hdd: None, probe: None };
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
            probe: None,
        };
        assert!(run(&cfg, &Arc::new(AtomicBool::new(false)), |_| {}, |_| {}).unwrap_err().contains("cannot read"));
    }
}
