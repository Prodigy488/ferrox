//! Logic behind the desktop front end, kept free of any GUI code so it can be
//! tested: settings storage and the game-library scanner.

use ferrox_formats::xbe::Xbe;
use ferrox_formats::xdvdfs::Xdvdfs;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

// ---------------------------------------------------------------- settings

/// How the emulated console starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum BootMode {
    /// Load the game directly and provide the kernel in software. Needs no
    /// BIOS or boot ROM. This is the only mode that exists so far.
    #[default]
    NoBios,
    /// Boot the console's own BIOS. Needs the user's own files. Not built yet.
    Bios,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Settings {
    pub library_folders: Vec<PathBuf>,
    pub boot_mode: BootMode,
    pub hdd_image: Option<PathBuf>,
    /// For the future BIOS boot mode. Stored, not used yet.
    pub bios_path: Option<PathBuf>,
    pub mcpx_path: Option<PathBuf>,
    /// Use the slow test interpreter instead of hardware virtualization.
    /// Only useful for development; it cannot run real games.
    pub use_reference_cpu: bool,
}

pub fn config_dir() -> PathBuf {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.unwrap_or_else(|| PathBuf::from(".")).join("ferrox")
}

impl Settings {
    pub fn load_from(path: &Path) -> Settings {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }
}

// ----------------------------------------------------------------- library

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    pub path: PathBuf,
    pub title: String,
    pub title_id: Option<u32>,
    pub is_disc: bool,
}

const DISC_EXTENSIONS: [&str; 2] = ["iso", "xiso"];

fn ext_is(p: &Path, list: &[&str]) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn parse_header(file: &[u8]) -> Option<Xbe> {
    Xbe::parse(file).ok()
}

/// Reads just the XBE header from a disc so scanning stays fast.
fn disc_game(path: &Path) -> Option<Game> {
    let f = File::open(path).ok()?;
    let mut disc = Xdvdfs::open(BufReader::new(f)).ok()?;
    let entry = disc.find("default.xbe").ok()??;
    let first = disc.read_file_range(&entry, 0, 0x1000).ok()?;
    let headers = u32::from_le_bytes(first.get(0x108..0x10C)?.try_into().ok()?) as usize;
    let data = if headers > first.len() && headers <= 1 << 20 {
        disc.read_file_range(&entry, 0, headers).ok()?
    } else {
        first
    };
    let xbe = parse_header(&data);
    let stem = path.file_stem()?.to_string_lossy().into_owned();
    Some(Game {
        path: path.to_path_buf(),
        title: xbe
            .as_ref()
            .map(|x| x.certificate.title_name.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or(stem),
        title_id: xbe.map(|x| x.certificate.title_id),
        is_disc: true,
    })
}

fn xbe_game(path: &Path) -> Option<Game> {
    let data = std::fs::read(path).ok()?;
    let xbe = parse_header(&data)?;
    let title = if xbe.certificate.title_name.is_empty() {
        path.parent()?.file_name()?.to_string_lossy().into_owned()
    } else {
        xbe.certificate.title_name.clone()
    };
    Some(Game { path: path.to_path_buf(), title, title_id: Some(xbe.certificate.title_id), is_disc: false })
}

/// Walks `folders`, sending each game found as soon as it is read, so the
/// library fills in while the scan runs. Stops early if `cancel` is set.
/// Finds `.iso`/`.xiso` discs and `default.xbe` files; does not follow symlinks.
pub fn scan(folders: &[PathBuf], tx: &Sender<Game>, cancel: &AtomicBool) {
    let mut stack: Vec<PathBuf> = folders.to_vec();
    while let Some(dir) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let game = if ext_is(&path, &DISC_EXTENSIONS) {
                    disc_game(&path)
                } else if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case("default.xbe")) {
                    xbe_game(&path)
                } else {
                    None
                };
                if let Some(g) = game {
                    if tx.send(g).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// Case-insensitive substring filter used by the search box.
pub fn matches(game: &Game, query: &str) -> bool {
    query.is_empty() || game.title.to_lowercase().contains(&query.to_lowercase())
}

// ---------------------------------------------------------------- helpers

/// Human-readable size for the UI.
pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrox_formats::xbe::synth;
    use std::sync::mpsc::channel;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ferrox-app-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn settings_round_trip_and_tolerate_garbage() {
        let d = tmp("settings");
        let p = d.join("sub/settings.json");
        let s = Settings {
            library_folders: vec!["/games".into()],
            hdd_image: Some("/hdd.img".into()),
            use_reference_cpu: true,
            ..Default::default()
        };
        s.save_to(&p).unwrap();
        let back = Settings::load_from(&p);
        assert_eq!(back.library_folders, s.library_folders);
        assert_eq!(back.hdd_image, s.hdd_image);
        assert_eq!(back.boot_mode, BootMode::NoBios);
        std::fs::write(&p, "{ not json").unwrap();
        assert!(Settings::load_from(&p).library_folders.is_empty());
        // a file from an older version with fewer fields still loads
        std::fs::write(&p, r#"{"library_folders":["/x"]}"#).unwrap();
        assert_eq!(Settings::load_from(&p).library_folders, vec![PathBuf::from("/x")]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn scan_finds_bare_xbe_folders_and_ignores_other_files() {
        let d = tmp("scan");
        std::fs::create_dir_all(d.join("Game One")).unwrap();
        std::fs::write(d.join("Game One/default.xbe"), synth::build(true, &[1])).unwrap();
        std::fs::write(d.join("notes.txt"), "hi").unwrap();
        std::fs::write(d.join("broken.iso"), vec![0u8; 100]).unwrap();
        std::fs::create_dir_all(d.join("Bad")).unwrap();
        std::fs::write(d.join("Bad/default.xbe"), b"nope").unwrap();
        let (tx, rx) = channel();
        scan(&[d.clone()], &tx, &AtomicBool::new(false));
        drop(tx);
        let found: Vec<Game> = rx.iter().collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(!found[0].is_disc);
        assert!(found[0].path.ends_with("Game One/default.xbe"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn scan_reads_the_title_from_inside_a_disc_image() {
        use ferrox_formats::xdvdfs::testutil::{build, Node};
        let d = tmp("iso");
        let iso = build(&[Node::File("default.xbe", synth::build(true, &[1]))], 0);
        std::fs::write(d.join("My Game.iso"), iso).unwrap();
        let (tx, rx) = channel();
        scan(&[d.clone()], &tx, &AtomicBool::new(false));
        drop(tx);
        let found: Vec<Game> = rx.iter().collect();
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].is_disc && found[0].title_id.is_some());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn scan_can_be_cancelled() {
        let d = tmp("cancel");
        std::fs::write(d.join("default.xbe"), synth::build(true, &[1])).unwrap();
        let (tx, rx) = channel();
        scan(&[d.clone()], &tx, &AtomicBool::new(true));
        drop(tx);
        assert_eq!(rx.iter().count(), 0);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn search_is_case_insensitive() {
        let g = Game { path: "x".into(), title: "Halo".into(), title_id: None, is_disc: true };
        assert!(matches(&g, "") && matches(&g, "hAl") && !matches(&g, "fable"));
    }
}
