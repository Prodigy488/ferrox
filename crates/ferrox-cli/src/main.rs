//! `ferrox` command line: inspect discs and executables, dump pushbuffers.

use ferrox_formats::{xbe::Xbe, xdvdfs::Xdvdfs, Error};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::process::ExitCode;

const USAGE: &str = "ferrox - a work-in-progress original Xbox emulator

usage:
  ferrox info <file.xbe|file.iso>       describe an executable or disc image
  ferrox ls <file.iso>                  list the files on a disc image
  ferrox pushbuffer <file> [get] [put]  decode a raw NV2A pushbuffer dump
                                        (get/put are hex byte offsets)
  ferrox run <file.xbe|file.iso> [--hdd image] [--reference-cpu] [--save-frame out.png|out.ppm]
                                        run a game with no BIOS (needs KVM on Linux)
  ferrox hdd <new-image> [GiB]          create a formatted blank hard-drive image
  ferrox demo <folder>                  write a tiny demo game into <folder>/Demo
  ferrox selftest [--no-hardware|--hardware] [folder]
                                        run the built-in checks and write pictures into <folder>";

fn run_cmd(args: &[String]) -> Result<(), String> {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    let path = args.first().ok_or("run needs a file")?;
    let mut cfg = ferrox_system::Config {
        source: ferrox_system::Source::from_path(std::path::Path::new(path)),
        backend: ferrox_system::Backend::Auto,
        hdd: None,
    };
    let mut save_frame: Option<String> = None;
    let mut it = args[1..].iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--hdd" => cfg.hdd = Some(it.next().ok_or("--hdd needs a path")?.into()),
            "--reference-cpu" => cfg.backend = ferrox_system::Backend::Reference,
            "--save-frame" => save_frame = Some(it.next().ok_or("--save-frame needs a path")?.clone()),
            other => return Err(format!("unknown option {other}")),
        }
    }
    let mut last = None;
    let rep = ferrox_system::run(&cfg, &Arc::new(AtomicBool::new(false)), |s| print!("{s}"), |f| last = Some(f))?;
    if let Some(path) = save_frame {
        let f = last.ok_or("the game never displayed a picture")?;
        let out = if path.to_lowercase().ends_with(".png") {
            f.to_png()
        } else {
            let mut o = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
            for px in f.rgba.chunks_exact(4) {
                o.extend_from_slice(&px[..3]);
            }
            o
        };
        std::fs::write(&path, out).map_err(|e| e.to_string())?;
        eprintln!("saved {}x{} picture to {path}", f.width, f.height);
    }
    eprintln!("[{}] {:?} on {}", rep.title, rep.outcome, rep.backend);
    Ok(())
}

fn selftest_cmd(args: &[String]) -> Result<(), String> {
    use ferrox_system::selftest::{run_all, summary, Mode};
    use std::io::Write;
    let mut mode = Mode::All;
    let mut dir = std::path::PathBuf::from("selftest-output");
    for a in args {
        match a.as_str() {
            "--no-hardware" => mode = Mode::NoHardware,
            "--hardware" => mode = Mode::HardwareOnly,
            other if other.starts_with("--") => return Err(format!("unknown option {other}")),
            other => dir = other.into(),
        }
    }
    let checks = run_all(&dir, mode, &mut |s| {
        print!("{s}");
        let _ = std::io::stdout().flush();
    });
    let (pass, fail, skip) = summary(&checks);
    println!("selftest: {pass} passed, {fail} failed, {skip} skipped");
    if fail > 0 { Err(format!("{fail} check(s) failed")) } else { Ok(()) }
}

fn hdd_cmd(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("hdd needs an output path")?;
    let gib: u64 = match args.get(1) {
        Some(g) => g.parse().map_err(|_| "size must be a whole number of GiB".to_string())?,
        None => 8,
    };
    ferrox_formats::hdd::create_image(std::path::Path::new(path), gib << 30).map_err(|e| e.to_string())?;
    println!("created {path} ({gib} GiB, sparse)");
    Ok(())
}

fn demo_cmd(args: &[String]) -> Result<(), String> {
    let root = std::path::Path::new(args.first().ok_or("demo needs a folder")?).join("Demo");
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    std::fs::write(root.join("default.xbe"), ferrox_system::demo_xbe()).map_err(|e| e.to_string())?;
    println!("wrote {}", root.join("default.xbe").display());
    Ok(())
}

fn region_name(r: u32) -> String {
    match r & 7 {
        7 => "region free".into(),
        0 => "unknown".into(),
        bits => {
            let mut v = Vec::new();
            if bits & 1 != 0 { v.push("NTSC-U"); }
            if bits & 2 != 0 { v.push("NTSC-J"); }
            if bits & 4 != 0 { v.push("PAL"); }
            v.join(" / ")
        }
    }
}

fn print_xbe(x: &Xbe, data: &[u8]) {
    let c = &x.certificate;
    println!("title:       {}", c.title_name);
    println!("title id:    {:08X}", c.title_id);
    println!("region:      {}", region_name(c.region));
    println!("disc/version: {} / {}", c.disc_number, c.version);
    println!("build:       {}", if x.retail { "retail" } else { "debug" });
    println!("entry point: {:#010x}", x.entry_point);
    println!("image:       base {:#x}, {} bytes", x.base_addr, x.size_of_image);
    println!("sections:");
    for s in &x.sections {
        println!(
            "  {:<12} va {:#010x} size {:#8x}  file {:#8x} size {:#8x}",
            s.name, s.virtual_addr, s.virtual_size, s.raw_addr, s.raw_size
        );
    }
    match x.kernel_imports(data) {
        Ok(v) => println!("kernel imports: {} functions", v.len()),
        Err(e) => println!("kernel imports: unreadable ({e})"),
    }
}

fn info(path: &str) -> Result<(), Error> {
    let mut f = File::open(path)?;
    let mut magic = [0u8; 4];
    let n = f.read(&mut magic)?;
    f.seek(SeekFrom::Start(0))?;
    if n == 4 && &magic == b"XBEH" {
        let mut data = Vec::new();
        f.read_to_end(&mut data)?;
        print_xbe(&Xbe::parse(&data)?, &data);
        return Ok(());
    }
    let mut fs = Xdvdfs::open(f)?;
    println!("disc image, game partition at byte {:#x}", fs.partition_offset());
    let all = fs.walk()?;
    let files = all.iter().filter(|e| !e.is_dir).count();
    println!("{} files, {} folders", files, all.len() - files);
    match fs.find("default.xbe")? {
        None => println!("no default.xbe at the top level"),
        Some(e) => {
            let data = fs.read_file(&e)?;
            println!("\ndefault.xbe ({} bytes):", e.size);
            print_xbe(&Xbe::parse(&data)?, &data);
        }
    }
    Ok(())
}

fn ls(path: &str) -> Result<(), Error> {
    let mut fs = Xdvdfs::open(File::open(path)?)?;
    for e in fs.walk()? {
        if e.is_dir {
            println!("{:>12}  {}/", "", e.path);
        } else {
            println!("{:>12}  {}", e.size, e.path);
        }
    }
    Ok(())
}

fn pushbuffer(args: &[String]) -> Result<(), String> {
    let path = args.first().ok_or("pushbuffer needs a file")?;
    let buf = std::fs::read(path).map_err(|e| e.to_string())?;
    let hex = |i: usize, d: u32| -> Result<u32, String> {
        match args.get(i) {
            None => Ok(d),
            Some(s) => u32::from_str_radix(s.trim_start_matches("0x"), 16)
                .map_err(|_| format!("'{s}' is not a hex offset")),
        }
    };
    let get = hex(1, 0)?;
    let put = hex(2, buf.len() as u32)?;
    let d = ferrox_nv2a::decode(&buf, get, put, 1_000_000).map_err(|e| e.to_string())?;
    for c in &d.commands {
        use ferrox_nv2a::Command::*;
        match c {
            Jump(a) => println!("JUMP {a:#x}"),
            OldJump(a) => println!("OLD_JUMP {a:#x}"),
            Call(a) => println!("CALL {a:#x}"),
            Return => println!("RETURN"),
            Methods { .. } => {}
        }
    }
    for w in ferrox_nv2a::writes(&d.commands) {
        let name = ferrox_nv2a::method_name(w.method).unwrap_or("");
        println!("sc{} {:#06x} <= {:#010x}  {}", w.subchannel, w.method, w.value, name);
    }
    if d.truncated {
        println!("(last method block is cut short)");
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result: Result<(), String> = match args.first().map(String::as_str) {
        Some("info") => args.get(1).ok_or("info needs a file".to_string())
            .and_then(|p| info(p).map_err(|e| e.to_string())),
        Some("ls") => args.get(1).ok_or("ls needs a file".to_string())
            .and_then(|p| ls(p).map_err(|e| e.to_string())),
        Some("pushbuffer") => pushbuffer(&args[1..]),
        Some("run") => run_cmd(&args[1..]),
        Some("hdd") => hdd_cmd(&args[1..]),
        Some("demo") => demo_cmd(&args[1..]),
        Some("selftest") => selftest_cmd(&args[1..]),
        _ => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ferrox: {e}");
            ExitCode::FAILURE
        }
    }
}
