//! ferrox desktop front end.

use eframe::egui::{self, Color32, RichText};
use ferrox_app::{config_dir, gib, matches, scan, BootMode, Game, Settings};
use ferrox_system::{Backend, Config, Frame, Report, Source};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;

const ACCENT: Color32 = Color32::from_rgb(0x6C, 0xC0, 0x4A);
const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA3, 0xAD);
const WARN: Color32 = Color32::from_rgb(0xE0, 0xA8, 0x3C);
const BAD: Color32 = Color32::from_rgb(0xE0, 0x5A, 0x5A);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Library,
    Play,
    Settings,
    About,
}

enum Msg {
    Out(String),
    Frame(Frame),
    Done(Result<Report, String>),
}

struct Running {
    title: String,
    stop: Arc<AtomicBool>,
    rx: Receiver<Msg>,
}

struct App {
    settings: Settings,
    settings_path: PathBuf,
    tab: Tab,
    games: Vec<Game>,
    query: String,
    scan_rx: Option<Receiver<Game>>,
    scan_cancel: Arc<AtomicBool>,
    run: Option<Running>,
    console: String,
    picture: Option<egui::TextureHandle>,
    status: (String, Color32),
    kvm: bool,
    hdd_gib: u32,
    hdd_note: Option<(String, Color32)>,
    /// Development aid: launch the first game found (FERROX_AUTOLAUNCH=1).
    autolaunch: bool,
    last_frame: Option<Frame>,
    smoke: Option<Smoke>,
}

/// `ferrox-gui --smoke <folder>`: open the window, play the built-in demo, write
/// what happened to <folder>/gui-smoke.txt and close. For checking a new machine.
struct Smoke {
    dir: PathBuf,
    started: std::time::Instant,
    launched: bool,
    opened_noted: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut v = egui::Visuals::dark();
        v.panel_fill = Color32::from_rgb(0x14, 0x17, 0x1B);
        v.window_fill = Color32::from_rgb(0x1B, 0x1F, 0x24);
        v.selection.bg_fill = Color32::from_rgb(0x2F, 0x5A, 0x22);
        v.hyperlink_color = ACCENT;
        cc.egui_ctx.set_visuals(v);
        cc.egui_ctx.style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(10.0, 8.0);
            s.spacing.button_padding = egui::vec2(12.0, 6.0);
        });
        let smoke_dir = std::env::args().skip_while(|a| a != "--smoke").nth(1).map(PathBuf::from);
        let settings_path = match &smoke_dir {
            Some(d) => d.join("settings.json"), // never touch the user's real settings
            None => config_dir().join("settings.json"),
        };
        let mut settings = Settings::load_from(&settings_path);
        if let Some(d) = &smoke_dir {
            let games = d.join("games").join("Demo");
            let _ = std::fs::create_dir_all(&games);
            let _ = std::fs::write(games.join("default.xbe"), ferrox_system::demo_xbe());
            settings.library_folders = vec![d.join("games")];
            settings.hdd_image = None;
            settings.use_reference_cpu = !ferrox_system::hardware_virtualization(); // use the fast CPU wherever there is one
        }
        let mut app = Self {
            settings,
            settings_path,
            tab: Tab::Library,
            games: Vec::new(),
            query: String::new(),
            scan_rx: None,
            scan_cancel: Arc::new(AtomicBool::new(false)),
            run: None,
            console: String::new(),
            picture: None,
            status: ("Ready.".into(), MUTED),
            kvm: ferrox_system::hardware_virtualization(),
            hdd_gib: 8,
            hdd_note: None,
            autolaunch: std::env::var_os("FERROX_AUTOLAUNCH").is_some() || smoke_dir.is_some(),
            last_frame: None,
            smoke: smoke_dir.map(|dir| Smoke { dir, started: std::time::Instant::now(), launched: false, opened_noted: false }),
        };
        app.tab = match std::env::var("FERROX_TAB").as_deref() {
            Ok("settings") => Tab::Settings,
            Ok("about") => Tab::About,
            Ok("play") => Tab::Play,
            _ => Tab::Library,
        };
        app.rescan();
        app
    }

    fn save(&self) {
        let _ = self.settings.save_to(&self.settings_path);
    }

    fn rescan(&mut self) {
        self.scan_cancel.store(true, Ordering::Relaxed); // stop any scan in flight
        self.games.clear();
        if self.settings.library_folders.is_empty() {
            self.scan_rx = None;
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = cancel.clone();
        let (tx, rx) = channel();
        self.scan_rx = Some(rx);
        let folders = self.settings.library_folders.clone();
        std::thread::spawn(move || scan(&folders, &tx, &cancel));
    }

    fn launch(&mut self, ctx: &egui::Context, game: &Game) {
        if self.run.is_some() {
            return;
        }
        let cfg = Config {
            source: Source::from_path(&game.path),
            backend: if self.settings.use_reference_cpu { Backend::Reference } else { Backend::Auto },
            hdd: self.settings.hdd_image.clone(),
            probe: None,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (stop2, ctx2) = (stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            let out = tx.clone();
            let (out2, ctx3) = (tx.clone(), ctx2.clone());
            let res = ferrox_system::run(
                &cfg,
                &stop2,
                |s| {
                    let _ = out.send(Msg::Out(s.to_string()));
                    ctx2.request_repaint();
                },
                |f| {
                    let _ = out2.send(Msg::Frame(f));
                    ctx3.request_repaint();
                },
            );
            let _ = tx.send(Msg::Done(res));
            ctx2.request_repaint();
        });
        self.console.clear();
        self.picture = None;
        self.status = (format!("Running {}…", game.title), ACCENT);
        self.run = Some(Running { title: game.title.clone(), stop, rx });
        self.tab = Tab::Play;
    }

    fn smoke_step(&mut self, ctx: &egui::Context) {
        let Some(sm) = &mut self.smoke else { return };
        if !sm.opened_noted {
            sm.opened_noted = true;
            let _ = std::fs::write(
                sm.dir.join("gui-smoke.txt"),
                format!("ferrox gui smoke test, build {}\nwindow opened: yes\n(still running; this file is rewritten when the demo finishes)\n", env!("FERROX_BUILD")),
            );
        }
        let finished = sm.launched && self.run.is_none();
        let timed_out = sm.started.elapsed() > std::time::Duration::from_secs(30);
        if !finished && !timed_out {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        use ferrox_system::selftest::{check_demo_frame, check_demo_output, Status};
        let mut text = format!("ferrox gui smoke test, build {}\nwindow opened: yes\n", env!("FERROX_BUILD"));
        text.push_str(&format!("demo finished: {}\n", if finished { "yes" } else { "NO (timed out after 30 s)" }));
        text.push_str(&format!("games found in the library: {}\n", self.games.len()));
        text.push_str(&format!("status line: {}\n", self.status.0));
        text.push_str(&format!("seconds from start: {:.1}\n", sm.started.elapsed().as_secs_f32()));
        let mut bad = !finished;
        match &self.last_frame {
            Some(f) => {
                text.push_str(&format!("picture shown: {}x{}\n", f.width, f.height));
                let _ = std::fs::write(sm.dir.join("gui-frame.png"), f.to_png());
                for c in check_demo_frame(f).into_iter().chain(check_demo_output(&self.console)) {
                    if c.status == Status::Fail {
                        bad = true;
                        text.push_str(&format!("{}\n", c.line()));
                    }
                }
            }
            None => {
                bad = true;
                text.push_str("picture shown: NONE\n");
            }
        }
        text.push_str(&format!("result: {}\n\n--- console ---\n{}\n", if bad { "PROBLEM (see above)" } else { "OK (picture and output are exactly as expected)" }, self.console));
        let _ = std::fs::write(sm.dir.join("gui-smoke.txt"), text);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.scan_rx {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(g) => self.games.push(g),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(_) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                self.scan_rx = None;
                self.games.sort_by_key(|g| g.title.to_lowercase());
            }
        }
        if let Some(run) = &self.run {
            let mut finished = None;
            let mut newest: Option<Frame> = None;
            while let Ok(m) = run.rx.try_recv() {
                match m {
                    Msg::Out(s) => self.console.push_str(&s),
                    Msg::Frame(f) => newest = Some(f),
                    Msg::Done(r) => finished = Some(r),
                }
            }
            if let Some(f) = newest {
                self.last_frame = Some(f.clone());
                let img = egui::ColorImage::from_rgba_unmultiplied([f.width as usize, f.height as usize], &f.rgba);
                match &mut self.picture {
                    Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                    None => self.picture = Some(ctx.load_texture("screen", img, egui::TextureOptions::LINEAR)),
                }
            }
            if let Some(r) = finished {
                let title = run.title.clone();
                self.run = None;
                self.status = match r {
                    Ok(rep) => (format!("{title}: {} — CPU: {}", rep.outcome, rep.backend), MUTED),
                    Err(e) => {
                        self.console.push_str(&format!("\n[stopped] {e}\n"));
                        (format!("{title} could not continue: {e}"), BAD)
                    }
                };
            }
        }
    }

    fn library(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Add folder…").clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("Add a games folder").pick_folder() {
                    if !self.settings.library_folders.contains(&p) {
                        self.settings.library_folders.push(p);
                        self.save();
                        self.rescan();
                    }
                }
            }
            if ui.button("Rescan").clicked() {
                self.rescan();
            }
            if ui.button("Clear library").on_hover_text("Forget all folders and games. Files are not touched.").clicked() {
                self.settings.library_folders.clear();
                self.save();
                self.rescan();
            }
            ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Search…").desired_width(220.0));
            if self.scan_rx.is_some() {
                ui.spinner();
                ui.label(RichText::new(format!("scanning… {} found", self.games.len())).color(MUTED));
            }
        });
        let mut remove = None;
        ui.horizontal_wrapped(|ui| {
            for (i, f) in self.settings.library_folders.iter().enumerate() {
                if ui.small_button(format!("×  {}", f.display())).on_hover_text("Remove this folder").clicked() {
                    remove = Some(i);
                }
            }
        });
        if let Some(i) = remove {
            self.settings.library_folders.remove(i);
            self.save();
            self.rescan();
        }
        ui.separator();
        if self.games.is_empty() && self.scan_rx.is_none() {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No games yet").size(22.0));
                ui.label(RichText::new("Add a folder that holds .iso / .xiso files or game folders with a default.xbe.").color(MUTED));
                ui.add_space(12.0);
                if ui.button("Create the demo game (test pattern)").clicked() {
                    let dir = config_dir().join("demo-games");
                    let game = dir.join("Demo");
                    if std::fs::create_dir_all(&game).is_ok()
                        && std::fs::write(game.join("default.xbe"), ferrox_system::demo_xbe()).is_ok()
                    {
                        self.settings.library_folders.push(dir);
                        self.save();
                        self.rescan();
                    }
                }
            });
            return;
        }
        let mut to_launch = None;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            for g in self.games.iter().filter(|g| matches(g, &self.query)) {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&g.title).size(17.0).strong());
                            let id = g.title_id.map(|t| format!("  ·  title {t:08X}")).unwrap_or_default();
                            ui.label(
                                RichText::new(format!("{}{id}", if g.is_disc { "disc image" } else { "game folder" }))
                                    .color(MUTED),
                            );
                            ui.label(RichText::new(g.path.display().to_string()).small().color(MUTED));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let can = self.run.is_none();
                            if ui.add_enabled(can, egui::Button::new(RichText::new("▶  Launch").color(ACCENT))).clicked() {
                                to_launch = Some(g.clone());
                            }
                        });
                    });
                });
            }
        });
        if let Some(g) = to_launch {
            let ctx = ui.ctx().clone();
            self.launch(&ctx, &g);
        }
    }

    fn play(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&self.status.0).color(self.status.1));
            if let Some(r) = &self.run {
                if ui.button("■  Stop").clicked() {
                    r.stop.store(true, Ordering::Relaxed);
                }
            }
        });
        let w = ui.available_width().min(760.0);
        let aspect = self.picture.as_ref().map(|t| t.aspect_ratio()).unwrap_or(4.0 / 3.0);
        // Xbox pictures are 4:3 on screen even when the stored width is 720 or 640.
        let shown = if self.picture.is_some() { 4.0 / 3.0 } else { aspect };
        let (rect, _) = ui.allocate_exact_size(egui::vec2(w, w / shown), egui::Sense::hover());
        ui.painter().rect_filled(rect, 6.0, Color32::BLACK);
        match &self.picture {
            Some(t) => {
                ui.painter().image(t.id(), rect, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    "No picture yet\nIf the game is running: it has not asked for a display,\nor it draws with the GPU, which is not emulated.\nThe console below shows what happened.",
                    egui::FontId::proportional(15.0),
                    MUTED,
                );
            }
        }
        ui.label(RichText::new("Console (what the game prints)").color(MUTED));
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false; 2]).show(ui, |ui| {
            ui.add(egui::Label::new(RichText::new(&self.console).monospace()).wrap());
        });
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let mut dirty = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Boot mode");
            dirty |= ui
                .radio_value(&mut self.settings.boot_mode, BootMode::NoBios, "No BIOS — load the game directly (recommended)")
                .changed();
            ui.add_enabled_ui(false, |ui| {
                ui.radio_value(&mut self.settings.boot_mode, BootMode::Bios, "BIOS — boot the console's own firmware (not built yet)");
            });
            ui.label(RichText::new("No-BIOS mode needs no copyrighted files. ferrox provides the console's kernel itself.").color(MUTED));
            ui.add_space(10.0);

            ui.heading("Processor");
            if self.kvm {
                ui.label(RichText::new("Hardware virtualization available. Programs run on the real CPU at full speed.").color(ACCENT));
            } else {
                ui.label(RichText::new("Hardware virtualization is not available on this computer.").color(BAD));
                ui.label(RichText::new(ferrox_system::virtualization_help()).color(MUTED));
            }
            dirty |= ui
                .checkbox(&mut self.settings.use_reference_cpu, "Use the slow test interpreter (development only; cannot run real games)")
                .changed();
            ui.add_space(10.0);

            ui.heading("Hard drive");
            match &self.settings.hdd_image {
                Some(p) => ui.label(p.display().to_string()),
                None => ui.label(RichText::new("No hard-drive image attached.").color(MUTED)),
            };
            ui.horizontal(|ui| {
                if ui.button("Choose image…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Hard-drive image").pick_file() {
                        self.hdd_note = Some(match ferrox_formats::hdd::inspect(&p) {
                            Ok(parts) if parts.iter().any(|&(_, ok)| ok) => {
                                let list: String = parts.iter().map(|&(d, ok)| format!("{d}{} ", if ok { "ok" } else { "empty" })).collect();
                                self.settings.hdd_image = Some(p);
                                dirty = true;
                                (format!("Attached. Partitions: {}", list.trim_end()), ACCENT)
                            }
                            Ok(_) => ("That file has no formatted partitions. (qcow2 images from other emulators are not read yet; convert to raw first.)".into(), WARN),
                            Err(e) => (e.to_string(), BAD),
                        });
                    }
                }
                if self.settings.hdd_image.is_some() && ui.button("Detach").clicked() {
                    self.settings.hdd_image = None;
                    dirty = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("New blank image size:");
                ui.add(egui::DragValue::new(&mut self.hdd_gib).range(3..=2048).suffix(" GiB"));
                if ui.button("Create…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().set_title("Save new hard-drive image").set_file_name("xbox_hdd.img").save_file() {
                        let size = self.hdd_gib as u64 * (1 << 30);
                        self.hdd_note = Some(match ferrox_formats::hdd::create_image(&p, size) {
                            Ok(()) => {
                                self.settings.hdd_image = Some(p);
                                dirty = true;
                                (format!("Created a formatted {} image (sparse: uses little disk until filled) and attached it.", gib(size)), ACCENT)
                            }
                            Err(e) => (e.to_string(), BAD),
                        });
                    }
                }
            });
            if let Some((t, c)) = &self.hdd_note {
                ui.label(RichText::new(t).color(*c));
            }
            ui.label(RichText::new("Games cannot read or write the drive yet; the kernel's file functions come next.").color(MUTED));
            ui.add_space(10.0);

            ui.heading("BIOS files (for the future BIOS mode)");
            for (label, slot) in [("Flash BIOS", &mut self.settings.bios_path), ("MCPX boot ROM", &mut self.settings.mcpx_path)] {
                ui.horizontal(|ui| {
                    ui.label(format!("{label}:"));
                    ui.label(slot.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "not set".into()));
                    if ui.button("Browse…").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_file() {
                            *slot = Some(p);
                            dirty = true;
                        }
                    }
                });
            }
            ui.label(RichText::new("Stored only. ferrox never includes these files; you must supply your own.").color(MUTED));
        });
        if dirty {
            self.save();
        }
    }

    fn about(&self, ui: &mut egui::Ui) {
        ui.heading(format!("ferrox {} (build {})", env!("CARGO_PKG_VERSION"), env!("FERROX_BUILD")));
        ui.label("An original-Xbox emulator in Rust. GPL-2.0-or-later.");
        ui.add_space(8.0);
        ui.label(RichText::new("What works today").strong());
        ui.label("• Opens disc images and executables, builds formatted hard-drive images");
        ui.label("• Shows a program's framebuffer (software drawing); 3D drawing needs the GPU, which is not emulated");
        ui.label("• Runs a program's own code on the real CPU (Linux KVM, Windows Hypervisor Platform) with the kernel provided in software");
        let (done, total) = ferrox_system::kernel_coverage();
        ui.label(format!("• The built-in kernel implements {done} of {total} functions (memory, debug text, time, shutdown)"));
        ui.add_space(8.0);
        ui.label(RichText::new("Not built yet").strong().color(WARN));
        ui.label("• Graphics (NV2A 3D), audio, USB controllers, networking, file access — so commercial games cannot run");
        ui.label("• BIOS boot mode, macOS CPU backend");
        ui.label("• The KVM and Windows hypervisor backends compile but have not been run on real machines yet");
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        self.poll(ctx);
        if self.autolaunch && self.scan_rx.is_none() && !self.games.is_empty() && self.run.is_none() {
            self.autolaunch = false;
            let g = self.games[0].clone();
            self.launch(ctx, &g);
            if let Some(sm) = &mut self.smoke {
                sm.launched = true;
            }
        }
        self.smoke_step(ctx);
        if self.scan_rx.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        egui::SidePanel::left("nav").exact_width(150.0).show(ctx, |ui| {
            ui.add_space(8.0);
            ui.label(RichText::new("ferrox").size(26.0).strong().color(ACCENT));
            ui.label(RichText::new(concat!("v", env!("CARGO_PKG_VERSION"), "  ", env!("FERROX_BUILD"))).color(MUTED));
            ui.add_space(14.0);
            for (tab, label) in [(Tab::Library, "Library"), (Tab::Play, "Play"), (Tab::Settings, "Settings"), (Tab::About, "About")] {
                if ui.selectable_label(self.tab == tab, RichText::new(label).size(17.0)).clicked() {
                    self.tab = tab;
                }
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Library => self.library(ui),
            Tab::Play => self.play(ui),
            Tab::Settings => self.settings_tab(ui),
            Tab::About => self.about(ui),
        });
    }
}

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 680.0]).with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        concat!("ferrox v", env!("CARGO_PKG_VERSION"), " (build ", env!("FERROX_BUILD"), ")"),
        opts,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
