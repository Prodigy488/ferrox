//! A battery of end-to-end checks that runs in one process and prints a
//! pass/fail line for each, so a single run on a new machine answers many
//! questions at once. The expected picture and output of the built-in demo
//! live here, shared with the unit tests.

use crate::{demo_xbe, run, Backend, Config, Frame, Source};
use ferrox_formats::xdvdfs::testutil::{build, Node};
use ferrox_formats::{hdd, xbe::Xbe};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    fn new(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> Check {
        Check { name: name.into(), status: if ok { Status::Pass } else { Status::Fail }, detail: detail.into() }
    }
    fn skip(name: impl Into<String>, why: impl Into<String>) -> Check {
        Check { name: name.into(), status: Status::Skip, detail: why.into() }
    }
    pub fn line(&self) -> String {
        let tag = match self.status {
            Status::Pass => "PASS",
            Status::Fail => "FAIL",
            Status::Skip => "SKIP",
        };
        if self.detail.is_empty() { format!("{tag}  {}", self.name) } else { format!("{tag}  {} ({})", self.name, self.detail) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Everything, including the fast CPU when the machine has one.
    All,
    /// Skip the fast CPU (use when it might crash the process).
    NoHardware,
    /// Only the fast CPU (plus the reference run it is compared against).
    HardwareOnly,
}

/// What the demo game prints, with the harness's own `[ferrox]` lines removed.
pub const DEMO_GAME_OUTPUT: &str = "gpu id: 02a000a3\ngpu took 940 bytes of commands\nhello, xbox! (test pattern drawn)\n";

/// Checks the text a demo run produced.
pub fn check_demo_output(out: &str) -> Vec<Check> {
    let game: String = out.lines().filter(|l| !l.starts_with("[ferrox]")).map(|l| format!("{l}\n")).collect();
    let gaps: Vec<&str> = out.lines().filter(|l| l.contains("not emulated yet")).collect();
    vec![
        Check::new("guest output is exactly what the demo prints", game == DEMO_GAME_OUTPUT, if game == DEMO_GAME_OUTPUT { String::new() } else { format!("got {game:?}") }),
        Check::new("display mode was set to 640x480", out.contains("display set: 640x480"), ""),
        Check::new(
            "GPU ran every command (159 commands, 3 clears, 6 triangles)",
            out.contains("GPU ran 159 commands (3 surface clears, 6 triangles)"),
            out.lines().find(|l| l.contains("GPU ran")).unwrap_or("no GPU summary line").to_string(),
        ),
        Check::new(
            "the only unemulated feature reported is the register combiners",
            gaps.len() == 1 && gaps[0].contains("register combiners") && !gaps[0].contains("; "),
            format!("{gaps:?}"),
        ),
        Check::new("no unemulated GPU registers touched", !out.contains("registers touched but not emulated"), ""),
        Check::new("no GPU command errors", !out.contains("GPU: "), ""),
    ]
}

/// Checks the picture the demo draws: the gradient, two clears, a vertex-program
/// triangle, a vertex-array triangle, a depth-hidden quad and a textured quad.
pub fn check_demo_frame(f: &Frame) -> Vec<Check> {
    if (f.width, f.height) != (640, 480) || f.rgba.len() != 640 * 480 * 4 {
        return vec![Check::new("picture is 640x480", false, format!("{}x{}", f.width, f.height))];
    }
    let px = |x: usize, y: usize| -> [u8; 4] { f.rgba[(y * 640 + x) * 4..][..4].try_into().unwrap() };
    let probe = |name: &str, got: [u8; 4], want: [u8; 4]| Check::new(name, got == want, if got == want { String::new() } else { format!("got {got:?}, want {want:?}") });
    let range = |name: &str, got: [u8; 4], ok: bool| Check::new(name, ok, format!("{got:?}"));
    let tri_top = px(320, 170);
    let tri_mid = px(320, 280);
    let arr = px(140, 270);
    let hidden = px(350, 232);
    vec![
        Check::new("picture is 640x480", true, ""),
        Check::new(
            "gradient visible outside the shapes",
            f.rgba[0..8] == [0, 0, 0, 255, 1, 2, 3, 255] && px(99, 100) != px(98, 100),
            "",
        ),
        probe("clear: orange rectangle", px(150, 100), [0xE0, 0x80, 0x20, 0xFF]),
        probe("clear: dark-blue rectangle", px(200, 240), [0x10, 0x20, 0x60, 0xFF]),
        range("vertex program + triangle: red at the apex", tri_top, tri_top[0] > 0xC0 && tri_top[1] < 0x40 && tri_top[2] < 0x40),
        range("triangle colours blend across it", tri_mid, tri_mid[0] > 0x10 && tri_mid[1] > 0x20 && tri_mid[2] > 0x20),
        probe("triangle edge: pixel just outside is still the rectangle", px(200, 160), [0x10, 0x20, 0x60, 0xFF]),
        range("vertex array triangle drawn", arr, arr[0] > 0x80 && arr[1] > 0x80 && arr != [0xE0, 0x80, 0x20, 0xFF]),
        probe("vertex array triangle: outside is still orange", px(120, 200), [0xE0, 0x80, 0x20, 0xFF]),
        probe("depth: far quad visible where nothing is in front", px(420, 232), [0xFF; 4]),
        range("depth: far quad hidden behind the triangle", hidden, hidden != [0xFF; 4] && hidden[0] > 0x10),
        probe("texture: texel (1, 2)", px(492, 212), [85, 170, 0x80, 0xFF]),
        probe("texture: texel (3, 3)", px(522, 238), [255, 255, 0x80, 0xFF]),
    ]
}

struct Outcome {
    out: String,
    frame: Option<Frame>,
    backend: &'static str,
    elapsed: Duration,
}

fn run_once(source: Source, backend: Backend, hdd: Option<std::path::PathBuf>) -> Result<Outcome, String> {
    let cfg = Config { source, backend, hdd };
    let mut out = String::new();
    let mut frame = None;
    let t = Instant::now();
    let rep = run(&cfg, &Arc::new(AtomicBool::new(false)), |s| out.push_str(s), |f| frame = Some(f))?;
    Ok(Outcome { out, frame, backend: rep.backend, elapsed: t.elapsed() })
}

fn ms(d: Duration) -> String {
    format!("{} ms", d.as_millis())
}

/// Runs the checks. Progress goes to `say` as it happens, so a crash part-way
/// still leaves everything before it on record. Pictures are written into `dir`.
pub fn run_all(dir: &Path, mode: Mode, say: &mut dyn FnMut(&str)) -> Vec<Check> {
    let mut all: Vec<Check> = Vec::new();
    let add = |c: Vec<Check>, all: &mut Vec<Check>, say: &mut dyn FnMut(&str)| {
        for c in c {
            say(&format!("{}\n", c.line()));
            all.push(c);
        }
    };
    let _ = std::fs::create_dir_all(dir);

    let (done, total) = crate::kernel_coverage();
    say(&format!(
        "system: {} {}, {} logical processors, hardware CPU backend {}, kernel functions {done}/{total}\n",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0),
        if crate::hardware_virtualization() { "available" } else { "not available" },
    ));

    let demo = demo_xbe();
    let parsed = Xbe::parse(&demo);
    add(
        vec![Check::new(
            "demo executable parses",
            parsed.as_ref().is_ok_and(|x| x.certificate.title_name == "Test Game"),
            parsed.as_ref().map(|x| x.certificate.title_name.clone()).unwrap_or_else(|e| e.to_string()),
        )],
        &mut all,
        say,
    );
    let xbe_path = dir.join("selftest.xbe");
    if let Err(e) = std::fs::write(&xbe_path, &demo) {
        add(vec![Check::new("write the demo executable", false, e.to_string())], &mut all, say);
        return all;
    }

    // the reference run everything else is compared with
    let reference = match run_once(Source::Xbe(xbe_path.clone()), Backend::Reference, None) {
        Ok(o) => o,
        Err(e) => {
            add(vec![Check::new("reference interpreter runs the demo", false, e)], &mut all, say);
            return all;
        }
    };
    if mode != Mode::HardwareOnly {
        add(vec![Check::new("reference interpreter runs the demo", true, ms(reference.elapsed))], &mut all, say);
        add(check_demo_output(&reference.out), &mut all, say);
        if let Some(f) = &reference.frame {
            add(check_demo_frame(f), &mut all, say);
            let _ = std::fs::write(dir.join("selftest-interpreter.png"), f.to_png());
        } else {
            add(vec![Check::new("the demo displayed a picture", false, "")], &mut all, say);
        }

        // determinism
        match run_once(Source::Xbe(xbe_path.clone()), Backend::Reference, None) {
            Ok(again) => add(
                vec![Check::new("a second run gives the same output and picture", again.out == reference.out && again.frame == reference.frame, "")],
                &mut all,
                say,
            ),
            Err(e) => add(vec![Check::new("a second run gives the same output and picture", false, e)], &mut all, say),
        }

        // a disc image
        let iso = build(&[Node::File("default.xbe", demo.clone()), Node::Dir("media", vec![Node::File("readme.txt", b"hello".to_vec())])], 0);
        let iso_path = dir.join("selftest.iso");
        let res = std::fs::write(&iso_path, &iso).map_err(|e| e.to_string()).and_then(|_| run_once(Source::Iso(iso_path.clone()), Backend::Reference, None));
        match res {
            Ok(o) => add(
                vec![Check::new("runs from a disc image", o.out == reference.out && o.frame == reference.frame, format!("{} byte image, {}", iso.len(), ms(o.elapsed)))],
                &mut all,
                say,
            ),
            Err(e) => add(vec![Check::new("runs from a disc image", false, e)], &mut all, say),
        }
        let _ = std::fs::remove_file(&iso_path);

        // a hard-drive image
        let hdd_path = dir.join("selftest-hdd.img");
        let _ = std::fs::remove_file(&hdd_path);
        let made = hdd::create_image(&hdd_path, hdd::MIN_SIZE).map_err(|e| e.to_string());
        match made {
            Err(e) => add(vec![Check::new("creates a blank hard-drive image", false, e)], &mut all, say),
            Ok(()) => {
                let parts = hdd::inspect(&hdd_path).map_err(|e| e.to_string());
                let ok = parts.as_ref().is_ok_and(|p| p.len() == 5 && p.iter().all(|&(_, ok)| ok));
                add(vec![Check::new("creates a formatted hard-drive image (5 FATX partitions)", ok, format!("{parts:?}"))], &mut all, say);
                match run_once(Source::Xbe(xbe_path.clone()), Backend::Reference, Some(hdd_path.clone())) {
                    Ok(o) => add(vec![Check::new("runs with the hard-drive image attached", o.out == reference.out, "")], &mut all, say),
                    Err(e) => add(vec![Check::new("runs with the hard-drive image attached", false, e)], &mut all, say),
                }
            }
        }
        let _ = std::fs::remove_file(&hdd_path);

        // an unformatted image must be refused with a clear message
        let blank = dir.join("selftest-blank.img");
        let _ = std::fs::write(&blank, vec![0u8; 4096]);
        let refused = run_once(Source::Xbe(xbe_path.clone()), Backend::Reference, Some(blank.clone()));
        add(
            vec![Check::new("an unformatted drive image is refused clearly", refused.as_ref().err().is_some_and(|e| e.contains("not formatted")), refused.err().unwrap_or_default())],
            &mut all,
            say,
        );
        let _ = std::fs::remove_file(&blank);

        // a broken executable must not crash the emulator
        let bad = dir.join("selftest-bad.xbe");
        let _ = std::fs::write(&bad, b"this is not an executable");
        let r = run_once(Source::Xbe(bad.clone()), Backend::Reference, None);
        add(vec![Check::new("a corrupt executable is refused with an error, not a crash", r.is_err(), r.err().unwrap_or_default())], &mut all, say);
        let _ = std::fs::remove_file(&bad);
    }

    if mode != Mode::NoHardware {
        if !crate::hardware_virtualization() {
            add(vec![Check::skip("fast CPU backend", crate::virtualization_help())], &mut all, say);
        } else {
            match run_once(Source::Xbe(xbe_path.clone()), Backend::Auto, None) {
                Ok(o) => {
                    add(vec![Check::new("fast CPU backend runs the demo", true, format!("{}, {}", o.backend, ms(o.elapsed)))], &mut all, say);
                    add(check_demo_output(&o.out), &mut all, say);
                    if let Some(f) = &o.frame {
                        add(check_demo_frame(f), &mut all, say);
                        let _ = std::fs::write(dir.join("selftest-hardware.png"), f.to_png());
                    }
                    add(
                        vec![Check::new(
                            "fast CPU picture matches the reference interpreter exactly",
                            o.frame == reference.frame && o.frame.is_some(),
                            "",
                        )],
                        &mut all,
                        say,
                    );
                    say(&format!(
                        "speed: interpreter {} vs fast CPU {} for the demo (this includes startup and drawing)\n",
                        ms(reference.elapsed),
                        ms(o.elapsed)
                    ));
                }
                Err(e) => add(vec![Check::new("fast CPU backend runs the demo", false, e)], &mut all, say),
            }
        }
    }
    let _ = std::fs::remove_file(&xbe_path);
    all
}

pub fn summary(checks: &[Check]) -> (usize, usize, usize) {
    let n = |s| checks.iter().filter(|c| c.status == s).count();
    (n(Status::Pass), n(Status::Fail), n(Status::Skip))
}
