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
    mut on_output: impl FnMut(&str),
    mut on_frame: impl FnMut(Frame),
) -> Result<Report, String> {
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
    let mut hle = KernelHle::with_table();
    let state: std::rc::Rc<std::cell::RefCell<KernelState>> = kernel::install(&mut hle, HEAP);
    let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).map_err(|e| e.to_string())?;

    let mut cpu = make_backend(cfg.backend, &mut mem, loaded.entry)?;
    let mut r = cpu.regs();
    r.gpr[ESP] = loaded.stack_top;
    r.eip = loaded.entry;
    cpu.set_regs(r);
    let backend = cpu.name();

    let mut flush = |state: &std::rc::Rc<std::cell::RefCell<KernelState>>| {
        let mut s = state.borrow_mut();
        if !s.debug.is_empty() {
            on_output(&std::mem::take(&mut s.debug));
        }
    };
    let mut last_frame = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let mut show = |mem: &mut PhysMem, force: bool| {
        let Some(fb) = state.borrow().framebuffer else { return };
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
    c.extend(push(BASE + 0x2040));
    c.extend(call(0)); // DbgPrint(greeting)
    c.extend([0x83, 0xC4, 0x04]);
    c.extend(push(0));
    c.extend(call(1)); // HalReturnToFirmware(0)
    c.push(0xF4);
    let mut file = build(true, &ords);
    file[0x1000..0x1000 + c.len()].copy_from_slice(&c);
    file[0x2040..0x2040 + GREETING.len()].copy_from_slice(GREETING);
    file
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
        assert_eq!(out, "hello, xbox! (test pattern drawn)\n");
        let last = frames.last().expect("the demo displays a picture");
        assert_eq!((last.width, last.height), (640, 480));
        assert_eq!(last.rgba.len(), 640 * 480 * 4);
        assert_eq!(&last.rgba[0..8], &[0, 0, 0, 255, 1, 2, 3, 255]);
        assert_ne!(&last.rgba[0..4], &last.rgba[last.rgba.len() - 4..], "the pattern varies across the screen");
        assert_eq!(rep.outcome, Outcome::GuestStopped("HalReturnToFirmware(0)".into()));
        assert!(rep.backend.starts_with("reference"));
        std::fs::remove_dir_all(&dir).unwrap();
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
