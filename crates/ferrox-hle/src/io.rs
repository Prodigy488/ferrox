//! Files: the disc (`D:`), the hard-drive partitions and the symbolic-link namespace that maps
//! drive letters to devices.
//!
//! A game opens `\??\D:\levels\a.bin`; the link `\??\D:` points at `\Device\CdRom0`, which is
//! a [`Volume`] holding the files. All calls complete at once (the console would finish them
//! later and signal an event; signalling happens immediately here).

use crate::kernel::STATUS_SUCCESS;
use crate::{Call, KernelHle};
use ferrox_formats::xdvdfs::{Entry, Xdvdfs};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek};
use std::path::PathBuf;
use std::rc::Rc;

pub const STATUS_INVALID_INFO_CLASS: u32 = 0xC000_0003;
pub const STATUS_INVALID_DEVICE_REQUEST: u32 = 0xC000_0010;
pub const STATUS_NO_SUCH_DEVICE: u32 = 0xC000_000E;
pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;
pub const STATUS_NAME_NOT_FOUND: u32 = 0xC000_0034;
pub const STATUS_NAME_COLLISION: u32 = 0xC000_0035;
pub const STATUS_PATH_NOT_FOUND: u32 = 0xC000_003A;
pub const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
pub const STATUS_INVALID_HANDLE: u32 = 0xC000_0008;
pub const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
pub const STATUS_IS_A_DIRECTORY: u32 = 0xC000_00BA;
pub const STATUS_WRITE_PROTECTED: u32 = 0xC000_00A2;
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
pub const STATUS_BUFFER_TOO_SMALL: u32 = 0xC000_0023;
pub const STATUS_NOT_IMPLEMENTED: u32 = 0xC000_0002;
/// 100 ns ticks since 1601 for the timestamps files report (a date in 2003).
const FILE_TIME: u64 = 0x01C3_5B7A_0000_0000;
const CLUSTER: u64 = 16 * 1024;
const RECENT: usize = 40;

pub type IoResult<T> = Result<T, u32>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stat {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Something files can live on. Paths are relative to the volume root, `\`-separated, with no
/// leading separator; the empty path is the root folder. Names compare without regard to case.
pub trait Volume {
    fn stat(&mut self, path: &str) -> Option<Stat>;
    fn read(&mut self, path: &str, offset: u64, len: usize) -> IoResult<Vec<u8>>;
    fn list(&mut self, path: &str) -> Option<Vec<Stat>>;
    fn writable(&self) -> bool {
        false
    }
    fn create(&mut self, _path: &str, _dir: bool) -> IoResult<()> {
        Err(STATUS_WRITE_PROTECTED)
    }
    fn write(&mut self, _path: &str, _offset: u64, _data: &[u8]) -> IoResult<()> {
        Err(STATUS_WRITE_PROTECTED)
    }
    fn set_len(&mut self, _path: &str, _len: u64) -> IoResult<()> {
        Err(STATUS_WRITE_PROTECTED)
    }
    fn delete(&mut self, _path: &str) -> IoResult<()> {
        Err(STATUS_WRITE_PROTECTED)
    }
}

fn clamp_read(size: u64, offset: u64, len: usize) -> IoResult<usize> {
    if len > 0 && offset >= size {
        return Err(STATUS_END_OF_FILE);
    }
    Ok((len as u64).min(size.saturating_sub(offset)) as usize)
}

// ---------------------------------------------------------------- a disc image ----

/// The files of an Xbox disc image.
pub struct DiscVolume<R: Read + Seek> {
    disc: Xdvdfs<R>,
    index: Option<(HashMap<String, Entry>, HashMap<String, Vec<Entry>>)>,
}

impl<R: Read + Seek> DiscVolume<R> {
    pub fn new(disc: Xdvdfs<R>) -> Self {
        DiscVolume { disc, index: None }
    }

    fn index(&mut self) -> &(HashMap<String, Entry>, HashMap<String, Vec<Entry>>) {
        if self.index.is_none() {
            let mut files = HashMap::new();
            let mut kids: HashMap<String, Vec<Entry>> = HashMap::new();
            for e in self.disc.walk().unwrap_or_default() {
                let key = e.path.to_ascii_lowercase();
                let parent = key.rsplit_once('/').map_or(String::new(), |p| p.0.to_string());
                kids.entry(parent).or_default().push(e.clone());
                files.insert(key, e);
            }
            self.index = Some((files, kids));
        }
        self.index.as_ref().unwrap()
    }
}

fn key_of(path: &str) -> String {
    path.replace('\\', "/").to_ascii_lowercase()
}

fn stat_of(e: &Entry) -> Stat {
    Stat { name: e.name().to_string(), is_dir: e.is_dir, size: e.size as u64 }
}

impl<R: Read + Seek> Volume for DiscVolume<R> {
    fn stat(&mut self, path: &str) -> Option<Stat> {
        if path.is_empty() {
            return Some(Stat { name: String::new(), is_dir: true, size: 0 });
        }
        self.index().0.get(&key_of(path)).map(stat_of)
    }

    fn read(&mut self, path: &str, offset: u64, len: usize) -> IoResult<Vec<u8>> {
        let e = self.index().0.get(&key_of(path)).cloned().ok_or(STATUS_NAME_NOT_FOUND)?;
        if e.is_dir {
            return Err(STATUS_IS_A_DIRECTORY);
        }
        let n = clamp_read(e.size as u64, offset, len)?;
        if n == 0 {
            return Ok(Vec::new());
        }
        self.disc.read_file_range(&e, offset, n).map_err(|_| STATUS_END_OF_FILE)
    }

    fn list(&mut self, path: &str) -> Option<Vec<Stat>> {
        let key = key_of(path);
        if !key.is_empty() && !self.index().0.get(&key)?.is_dir {
            return None;
        }
        Some(self.index().1.get(&key).map(|v| v.iter().map(stat_of).collect()).unwrap_or_default())
    }
}

// ---------------------------------------------------------------- a host folder ----

/// A folder on the host, read-only, matched without regard to case.
pub struct HostVolume {
    root: PathBuf,
}

impl HostVolume {
    pub fn new(root: PathBuf) -> Self {
        HostVolume { root }
    }

    fn find(&self, path: &str) -> Option<PathBuf> {
        let mut at = self.root.clone();
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            let hit = std::fs::read_dir(&at)
                .ok()?
                .filter_map(|e| e.ok())
                .find(|e| e.file_name().to_string_lossy().eq_ignore_ascii_case(part))?;
            at = hit.path();
        }
        Some(at)
    }
}

impl Volume for HostVolume {
    fn stat(&mut self, path: &str) -> Option<Stat> {
        let p = self.find(path)?;
        let m = std::fs::metadata(&p).ok()?;
        Some(Stat { name: p.file_name().map_or(String::new(), |n| n.to_string_lossy().into()), is_dir: m.is_dir(), size: m.len() })
    }

    fn read(&mut self, path: &str, offset: u64, len: usize) -> IoResult<Vec<u8>> {
        use std::io::SeekFrom;
        let p = self.find(path).ok_or(STATUS_NAME_NOT_FOUND)?;
        let mut f = std::fs::File::open(p).map_err(|_| STATUS_ACCESS_DENIED)?;
        let size = f.metadata().map_err(|_| STATUS_ACCESS_DENIED)?.len();
        let n = clamp_read(size, offset, len)?;
        let mut buf = vec![0u8; n];
        f.seek(SeekFrom::Start(offset)).map_err(|_| STATUS_END_OF_FILE)?;
        f.read_exact(&mut buf).map_err(|_| STATUS_END_OF_FILE)?;
        Ok(buf)
    }

    fn list(&mut self, path: &str) -> Option<Vec<Stat>> {
        let p = self.find(path)?;
        let mut v: Vec<Stat> = std::fs::read_dir(p)
            .ok()?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let m = e.metadata().ok()?;
                Some(Stat { name: e.file_name().to_string_lossy().into(), is_dir: m.is_dir(), size: m.len() })
            })
            .collect();
        v.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
        Some(v)
    }
}

// ---------------------------------------------------------------- the raw disk ----

/// The whole hard disk as one big file of sectors: unwritten sectors read as zeros, writes are
/// kept for the run. Games read the disk's configuration sectors through it.
#[derive(Default)]
pub struct RawDisk {
    sectors: HashMap<u64, [u8; 512]>,
}

/// 8 GiB: the size reported for the raw disk.
const RAW_DISK_BYTES: u64 = 8 << 30;

impl Volume for RawDisk {
    fn stat(&mut self, path: &str) -> Option<Stat> {
        path.is_empty().then(|| Stat { name: String::new(), is_dir: false, size: RAW_DISK_BYTES })
    }

    fn read(&mut self, path: &str, offset: u64, len: usize) -> IoResult<Vec<u8>> {
        if !path.is_empty() {
            return Err(STATUS_NAME_NOT_FOUND);
        }
        let n = clamp_read(RAW_DISK_BYTES, offset, len)?;
        let mut out = vec![0u8; n];
        for (i, b) in out.iter_mut().enumerate() {
            let at = offset + i as u64;
            if let Some(s) = self.sectors.get(&(at / 512)) {
                *b = s[(at % 512) as usize];
            }
        }
        Ok(out)
    }

    fn list(&mut self, _path: &str) -> Option<Vec<Stat>> {
        None
    }

    fn writable(&self) -> bool {
        true
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> IoResult<()> {
        if !path.is_empty() {
            return Err(STATUS_NAME_NOT_FOUND);
        }
        for (i, &b) in data.iter().enumerate() {
            let at = offset + i as u64;
            self.sectors.entry(at / 512).or_insert([0; 512])[(at % 512) as usize] = b;
        }
        Ok(())
    }

    fn set_len(&mut self, _path: &str, _len: u64) -> IoResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------- memory only ----

struct Node {
    name: String,
    dir: bool,
    data: Vec<u8>,
}

/// A writable volume kept in memory: stands in for a hard-drive partition until real FATX
/// access exists. Contents last for the run.
#[derive(Default)]
pub struct MemVolume {
    nodes: BTreeMap<String, Node>,
}

impl MemVolume {
    pub fn new() -> Self {
        Self::default()
    }

    fn parent(key: &str) -> &str {
        key.rsplit_once('\\').map_or("", |p| p.0)
    }

    /// Creates a folder and any missing parents.
    pub fn make_dirs(&mut self, path: &str) {
        let mut at = String::new();
        for part in path.split('\\').filter(|p| !p.is_empty()) {
            if !at.is_empty() {
                at.push('\\');
            }
            at.push_str(part);
            self.nodes.entry(at.to_ascii_lowercase()).or_insert(Node { name: part.to_string(), dir: true, data: Vec::new() });
        }
    }
}

impl Volume for MemVolume {
    fn stat(&mut self, path: &str) -> Option<Stat> {
        if path.is_empty() {
            return Some(Stat { name: String::new(), is_dir: true, size: 0 });
        }
        self.nodes.get(&path.to_ascii_lowercase()).map(|n| Stat { name: n.name.clone(), is_dir: n.dir, size: n.data.len() as u64 })
    }

    fn read(&mut self, path: &str, offset: u64, len: usize) -> IoResult<Vec<u8>> {
        let n = self.nodes.get(&path.to_ascii_lowercase()).ok_or(STATUS_NAME_NOT_FOUND)?;
        if n.dir {
            return Err(STATUS_IS_A_DIRECTORY);
        }
        let k = clamp_read(n.data.len() as u64, offset, len)?;
        Ok(n.data[offset as usize..offset as usize + k].to_vec())
    }

    fn list(&mut self, path: &str) -> Option<Vec<Stat>> {
        let key = path.to_ascii_lowercase();
        if !key.is_empty() && !self.nodes.get(&key)?.dir {
            return None;
        }
        Some(
            self.nodes
                .iter()
                .filter(|(k, _)| Self::parent(k) == key && !k.is_empty())
                .map(|(_, n)| Stat { name: n.name.clone(), is_dir: n.dir, size: n.data.len() as u64 })
                .collect(),
        )
    }

    fn writable(&self) -> bool {
        true
    }

    fn create(&mut self, path: &str, dir: bool) -> IoResult<()> {
        let key = path.to_ascii_lowercase();
        let parent = Self::parent(&key);
        if !parent.is_empty() && !self.nodes.get(parent).is_some_and(|n| n.dir) {
            return Err(STATUS_PATH_NOT_FOUND);
        }
        let name = path.rsplit('\\').next().unwrap_or(path).to_string();
        self.nodes.insert(key, Node { name, dir, data: Vec::new() });
        Ok(())
    }

    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> IoResult<()> {
        let n = self.nodes.get_mut(&path.to_ascii_lowercase()).ok_or(STATUS_NAME_NOT_FOUND)?;
        if n.dir {
            return Err(STATUS_IS_A_DIRECTORY);
        }
        let end = offset as usize + data.len();
        if n.data.len() < end {
            n.data.resize(end, 0);
        }
        n.data[offset as usize..end].copy_from_slice(data);
        Ok(())
    }

    fn set_len(&mut self, path: &str, len: u64) -> IoResult<()> {
        let n = self.nodes.get_mut(&path.to_ascii_lowercase()).ok_or(STATUS_NAME_NOT_FOUND)?;
        n.data.resize(len as usize, 0);
        Ok(())
    }

    fn delete(&mut self, path: &str) -> IoResult<()> {
        let key = path.to_ascii_lowercase();
        let has_kids = self.nodes.keys().any(|k| Self::parent(k) == key);
        if has_kids {
            return Err(STATUS_ACCESS_DENIED);
        }
        self.nodes.remove(&key).map(|_| ()).ok_or(STATUS_NAME_NOT_FOUND)
    }
}

// ---------------------------------------------------------------- the namespace ----

struct OpenFile {
    dev: usize,
    path: String,
    pos: u64,
    is_dir: bool,
    delete_on_close: bool,
    listing: Option<Vec<Stat>>,
    cursor: usize,
}

/// Devices, drive-letter links and open files.
pub struct Files {
    links: HashMap<String, String>,
    devices: Vec<(String, Box<dyn Volume>)>,
    open: HashMap<u32, OpenFile>,
    symlink_handles: HashMap<u32, String>,
    /// The most recent opens and their outcomes, for the compatibility report.
    recent: std::collections::VecDeque<String>,
    pub bytes_read: u64,
}

impl Default for Files {
    fn default() -> Self {
        Files::new(0)
    }
}

impl Files {
    /// The standard drive letters and the hard-drive partitions as empty in-memory volumes.
    /// `title_id` names the save folders created on `E:`.
    pub fn new(title_id: u32) -> Files {
        let mut f = Files {
            links: HashMap::new(),
            devices: Vec::new(),
            open: HashMap::new(),
            symlink_handles: HashMap::new(),
            recent: Default::default(),
            bytes_read: 0,
        };
        for (letter, part) in [('c', 2), ('e', 1), ('x', 3), ('y', 4), ('z', 5), ('f', 6)] {
            let dev = format!("\\device\\harddisk0\\partition{part}");
            let mut v = MemVolume::new();
            if letter == 'e' {
                v.make_dirs(&format!("TDATA\\{title_id:08X}"));
                v.make_dirs("UDATA");
            }
            f.devices.push((dev.clone(), Box::new(v)));
            f.links.insert(format!("\\??\\{letter}:"), dev);
        }
        // the whole disk as one device: opened for geometry and raw access
        f.devices.push(("\\device\\harddisk0\\partition0".into(), Box::new(RawDisk::default())));
        f.links.insert("\\??\\d:".into(), "\\device\\cdrom0".into());
        f.links.insert("\\??\\cdrom0:".into(), "\\device\\cdrom0".into());
        f
    }

    /// Puts `volume` at the device path `device` (e.g. `\Device\CdRom0`).
    pub fn mount(&mut self, device: &str, volume: Box<dyn Volume>) {
        let d = device.to_ascii_lowercase();
        self.devices.retain(|(n, _)| *n != d);
        self.devices.push((d, volume));
    }

    /// Opens, with their outcomes, the most recent file operations (oldest first).
    pub fn recent(&self) -> Vec<String> {
        self.recent.iter().cloned().collect()
    }

    fn note(&mut self, line: String) {
        if self.recent.len() >= RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(line);
    }

    /// Follows links and picks the device: (device index, path inside it).
    fn resolve(&self, name: &str, root: Option<(usize, &str)>) -> IoResult<(usize, String)> {
        let mut n = name.replace('/', "\\");
        if let Some((dev, base)) = root {
            return Ok((dev, normalise(&format!("{base}\\{n}"))));
        }
        if !n.starts_with('\\') {
            n = format!("\\??\\{n}");
        }
        for _ in 0..8 {
            let lower = n.to_ascii_lowercase();
            let hit = self.links.iter().filter(|(k, _)| prefix_at_boundary(&lower, k)).max_by_key(|(k, _)| k.len());
            match hit {
                Some((k, target)) => n = format!("{target}{}", &n[k.len()..]),
                None => break,
            }
        }
        let lower = n.to_ascii_lowercase();
        let (i, (dev, _)) = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, (d, _))| prefix_at_boundary(&lower, d))
            .max_by_key(|(_, (d, _))| d.len())
            .ok_or(if lower.starts_with("\\device\\") || lower.starts_with("\\??\\") { STATUS_NO_SUCH_DEVICE } else { STATUS_PATH_NOT_FOUND })?;
        Ok((i, normalise(&n[dev.len()..])))
    }
}

fn prefix_at_boundary(s: &str, prefix: &str) -> bool {
    s.starts_with(prefix) && (s.len() == prefix.len() || s[prefix.len()..].starts_with('\\'))
}

fn normalise(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('\\') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            x => out.push(x),
        }
    }
    out.join("\\")
}

fn wildcard(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.to_ascii_lowercase().chars().collect(), name.to_ascii_lowercase().chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some(('?', rest)) => !n.is_empty() && go(rest, &n[1..]),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    go(&p, &n)
}

// ---------------------------------------------------------------- guest-side helpers ----

fn arg(c: &mut Call, i: u32) -> u32 {
    c.arg(i).unwrap_or(0)
}

fn ansi(c: &mut Call, p: u32) -> String {
    if p == 0 {
        return String::new();
    }
    let len = c.bus.read(p as u64, 2).unwrap_or(0) as usize;
    let buf = c.read_u32(p + 4).unwrap_or(0);
    let mut bytes = vec![0u8; len.min(0x1000)];
    let _ = c.bus.read_bytes(buf as u64, &mut bytes);
    bytes.iter().map(|&b| b as char).collect()
}

fn status_block(c: &mut Call, iosb: u32, status: u32, info: u32) {
    if iosb != 0 {
        let _ = c.write_u32(iosb, status);
        let _ = c.write_u32(iosb + 4, info);
    }
}

fn put64(c: &mut Call, at: u32, v: u64) {
    let _ = c.write_u32(at, v as u32);
    let _ = c.write_u32(at + 4, (v >> 32) as u32);
}

/// OBJECT_ATTRIBUTES { RootDirectory, ObjectName (ANSI_STRING*), Attributes }.
fn object_name(c: &mut Call, oa: u32) -> (u32, String) {
    let root = c.read_u32(oa).unwrap_or(0);
    let name = c.read_u32(oa + 4).unwrap_or(0);
    (root, ansi(c, name))
}

fn signal(c: &mut Call, event: u32) {
    if event != 0 {
        if let Some(o) = c.sched.object_of(event) {
            c.sched.set_signal(c.bus, o, 1);
        }
    }
}

const DISPOSITION_NAMES: [&str; 6] = ["supersede", "open", "create", "open-if", "overwrite", "overwrite-if"];

fn open_file(c: &mut Call, files: &Rc<RefCell<Files>>, out: u32, oa: u32, iosb: u32, disp: u32, options: u32) -> u32 {
    let (root, name) = object_name(c, oa);
    let mut f = files.borrow_mut();
    let rootinfo = if root != 0 {
        match f.open.get(&root) {
            Some(o) => Some((o.dev, o.path.clone())),
            None => {
                f.note(format!("open {name:?} relative to handle {root:#x}, which is not a folder opened here -> invalid handle"));
                return STATUS_INVALID_HANDLE;
            }
        }
    } else {
        None
    };
    let result = (|| -> IoResult<(u32, usize, String, bool, u32)> {
        let (dev, rel) = f.resolve(&name, rootinfo.as_ref().map(|(d, p)| (*d, p.as_str())))?;
        let vol = &mut f.devices[dev].1;
        let existing = vol.stat(&rel);
        let want_dir = options & 1 != 0;
        let info;
        match (disp, &existing) {
            (1, None) | (4, None) => return Err(STATUS_NAME_NOT_FOUND),
            (2, Some(_)) => return Err(STATUS_NAME_COLLISION),
            (1, Some(_)) | (3, Some(_)) => info = 1,
            (4, Some(s)) | (0, Some(s)) | (5, Some(s)) => {
                if !s.is_dir {
                    vol.set_len(&rel, 0)?;
                }
                info = if disp == 0 { 0 } else { 3 };
            }
            (0, None) | (2, None) | (3, None) | (5, None) => {
                vol.create(&rel, want_dir)?;
                info = 2;
            }
            _ => return Err(STATUS_NOT_IMPLEMENTED),
        }
        let is_dir = existing.as_ref().map_or(want_dir, |s| s.is_dir);
        if is_dir && options & 0x40 != 0 {
            return Err(STATUS_IS_A_DIRECTORY);
        }
        if !is_dir && want_dir {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        Ok((info, dev, rel, is_dir, options & 0x1000))
    })();
    let label = DISPOSITION_NAMES.get(disp as usize).copied().unwrap_or("?");
    match result {
        Ok((info, dev, rel, is_dir, del)) => {
            let h = c.sched.new_handle(0);
            f.open.insert(h, OpenFile { dev, path: rel, pos: 0, is_dir, delete_on_close: del != 0, listing: None, cursor: 0 });
            f.note(format!("{label} {name:?} -> ok"));
            drop(f);
            let _ = c.write_u32(out, h);
            status_block(c, iosb, STATUS_SUCCESS, info);
            STATUS_SUCCESS
        }
        Err(status) => {
            f.note(format!("{label} {name:?} -> {status:#x}"));
            drop(f);
            status_block(c, iosb, status, 0);
            status
        }
    }
}

fn attributes(s: &Stat) -> u32 {
    if s.is_dir { 0x10 } else { 0x80 }
}

fn alloc_size(size: u64) -> u64 {
    size.div_ceil(CLUSTER) * CLUSTER
}

/// FILE_NETWORK_OPEN_INFORMATION: four times, allocation size, end of file, attributes.
fn network_info(c: &mut Call, at: u32, s: &Stat) {
    for i in 0..4 {
        put64(c, at + 8 * i, FILE_TIME);
    }
    put64(c, at + 32, alloc_size(s.size));
    put64(c, at + 40, s.size);
    let _ = c.write_u32(at + 48, attributes(s));
}

pub fn install(hle: &mut KernelHle, files: Rc<RefCell<Files>>) {
    let f = files.clone();
    hle.handle("NtCreateFile", move |c| {
        // (Handle*, Access, ObjectAttributes*, IoStatusBlock*, AllocationSize*, Attributes,
        //  ShareAccess, CreateDisposition, CreateOptions)
        let (out, oa, iosb, disp, options) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 7), arg(c, 8));
        open_file(c, &f, out, oa, iosb, disp, options)
    });
    let f = files.clone();
    hle.handle("NtOpenFile", move |c| {
        // (Handle*, Access, ObjectAttributes*, IoStatusBlock*, ShareAccess, OpenOptions)
        let (out, oa, iosb, options) = (arg(c, 0), arg(c, 2), arg(c, 3), arg(c, 5));
        open_file(c, &f, out, oa, iosb, 1, options)
    });

    let f = files.clone();
    hle.handle("NtReadFile", move |c| {
        // (Handle, Event, ApcRoutine, ApcContext, IoStatusBlock*, Buffer, Length, ByteOffset*)
        let (h, event, iosb, buf, len, off_p) = (arg(c, 0), arg(c, 1), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
        let off = if off_p != 0 {
            let lo = c.read_u32(off_p).unwrap_or(0) as u64;
            let hi = c.read_u32(off_p + 4).unwrap_or(0) as u64;
            Some(hi << 32 | lo).filter(|&v| v != u64::MAX && hi != 0xFFFF_FFFF)
        } else {
            None
        };
        let mut files = f.borrow_mut();
        let Some(file) = files.open.get_mut(&h) else { return STATUS_INVALID_HANDLE };
        let at = off.unwrap_or(file.pos);
        let (dev, path) = (file.dev, file.path.clone());
        let data = files.devices[dev].1.read(&path, at, len as usize);
        match data {
            Ok(d) => {
                let n = d.len() as u32;
                files.bytes_read += n as u64;
                files.open.get_mut(&h).unwrap().pos = at + n as u64;
                drop(files);
                if c.bus.write_bytes(buf as u64, &d).is_err() {
                    status_block(c, iosb, 0xC000_0005, 0);
                    return 0xC000_0005;
                }
                status_block(c, iosb, STATUS_SUCCESS, n);
                signal(c, event);
                STATUS_SUCCESS
            }
            Err(status) => {
                files.note(format!("read {path:?} at {at} len {len} -> {status:#x}"));
                drop(files);
                status_block(c, iosb, status, 0);
                status
            }
        }
    });
    let f = files.clone();
    hle.handle("NtWriteFile", move |c| {
        let (h, event, iosb, buf, len, off_p) = (arg(c, 0), arg(c, 1), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 7));
        let off = (off_p != 0).then(|| {
            let lo = c.read_u32(off_p).unwrap_or(0) as u64;
            let hi = c.read_u32(off_p + 4).unwrap_or(0) as u64;
            hi << 32 | lo
        }).filter(|&v| v >> 32 != 0xFFFF_FFFF);
        let mut data = vec![0u8; len as usize];
        if c.bus.read_bytes(buf as u64, &mut data).is_err() {
            return 0xC000_0005;
        }
        let mut files = f.borrow_mut();
        let Some(file) = files.open.get_mut(&h) else { return STATUS_INVALID_HANDLE };
        let at = off.unwrap_or(file.pos);
        let (dev, path) = (file.dev, file.path.clone());
        let r = files.devices[dev].1.write(&path, at, &data);
        let status = match r {
            Ok(()) => {
                files.open.get_mut(&h).unwrap().pos = at + len as u64;
                STATUS_SUCCESS
            }
            Err(s) => {
                files.note(format!("write {path:?} -> {s:#x}"));
                s
            }
        };
        drop(files);
        status_block(c, iosb, status, if status == 0 { len } else { 0 });
        if status == 0 {
            signal(c, event);
        }
        status
    });

    let f = files.clone();
    hle.handle("NtQueryInformationFile", move |c| {
        // (Handle, IoStatusBlock*, Information*, Length, Class)
        let (h, iosb, info, len, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let mut files = f.borrow_mut();
        let Some(file) = files.open.get(&h) else { return STATUS_INVALID_HANDLE };
        let (dev, path, pos) = (file.dev, file.path.clone(), file.pos);
        let Some(s) = files.devices[dev].1.stat(&path) else { return STATUS_NAME_NOT_FOUND };
        drop(files);
        let need = match class {
            4 => 36,
            5 => 24,
            14 => 8,
            34 => 52,
            _ => return STATUS_INVALID_INFO_CLASS,
        };
        if len < need {
            return STATUS_BUFFER_TOO_SMALL;
        }
        match class {
            4 => {
                for i in 0..4 {
                    put64(c, info + 8 * i, FILE_TIME);
                }
                let _ = c.write_u32(info + 32, attributes(&s));
            }
            5 => {
                put64(c, info, alloc_size(s.size));
                put64(c, info + 8, s.size);
                let _ = c.write_u32(info + 16, 1);
                let _ = c.bus.write((info + 20) as u64, 1, 0);
                let _ = c.bus.write((info + 21) as u64, 1, s.is_dir as u64);
            }
            14 => put64(c, info, pos),
            _ => network_info(c, info, &s),
        }
        status_block(c, iosb, STATUS_SUCCESS, need);
        STATUS_SUCCESS
    });
    let f = files.clone();
    hle.handle("NtSetInformationFile", move |c| {
        // (Handle, IoStatusBlock*, Information*, Length, Class)
        let (h, iosb, info, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 4));
        let mut files = f.borrow_mut();
        let Some(file) = files.open.get_mut(&h) else { return STATUS_INVALID_HANDLE };
        let (dev, path) = (file.dev, file.path.clone());
        let status = match class {
            14 => {
                let lo = c.read_u32(info).unwrap_or(0) as u64;
                let hi = c.read_u32(info + 4).unwrap_or(0) as u64;
                files.open.get_mut(&h).unwrap().pos = hi << 32 | lo;
                STATUS_SUCCESS
            }
            20 => {
                let lo = c.read_u32(info).unwrap_or(0) as u64;
                let hi = c.read_u32(info + 4).unwrap_or(0) as u64;
                match files.devices[dev].1.set_len(&path, hi << 32 | lo) {
                    Ok(()) => STATUS_SUCCESS,
                    Err(s) => s,
                }
            }
            13 => {
                let del = c.bus.read(info as u64, 1).unwrap_or(0) != 0;
                files.open.get_mut(&h).unwrap().delete_on_close = del;
                STATUS_SUCCESS
            }
            4 | 19 => STATUS_SUCCESS, // times and allocation: nothing to keep
            _ => STATUS_INVALID_INFO_CLASS,
        };
        drop(files);
        status_block(c, iosb, status, 0);
        status
    });
    let f = files.clone();
    hle.handle("NtQueryVolumeInformationFile", move |c| {
        // (Handle, IoStatusBlock*, Information*, Length, Class)
        let (h, iosb, info, len, class) = (arg(c, 0), arg(c, 1), arg(c, 2), arg(c, 3), arg(c, 4));
        let files = f.borrow_mut();
        let Some(file) = files.open.get(&h) else { return STATUS_INVALID_HANDLE };
        let dev = file.dev;
        let writable = files.devices[dev].1.writable();
        let is_disc = files.devices[dev].0.contains("cdrom");
        drop(files);
        let units: u64 = 0x80000; // 8 GiB of 16 KiB clusters
        let free = if writable { units - 0x1000 } else { 0 };
        let need = match class {
            3 => 24,
            4 => 8,
            5 => 12 + 8,
            7 => 32,
            _ => return STATUS_INVALID_INFO_CLASS,
        };
        if len < need {
            return STATUS_BUFFER_TOO_SMALL;
        }
        match class {
            3 | 7 => {
                put64(c, info, units);
                put64(c, info + 8, free);
                if class == 7 {
                    put64(c, info + 16, free);
                }
                let at = info + if class == 7 { 24 } else { 16 };
                let _ = c.write_u32(at, 32);
                let _ = c.write_u32(at + 4, 512);
            }
            4 => {
                // device type (CD-ROM file system / disk file system) and characteristics
                let _ = c.write_u32(info, if is_disc { 3 } else { 8 });
                let _ = c.write_u32(info + 4, if is_disc { 3 } else { 0 });
            }
            _ => {
                let name: &[u8] = if is_disc { b"UDFS" } else { b"FATX" };
                let _ = c.write_u32(info, if writable { 0 } else { 0x2 });
                let _ = c.write_u32(info + 4, 42);
                let _ = c.write_u32(info + 8, name.len() as u32);
                let _ = c.bus.write_bytes((info + 12) as u64, name);
            }
        }
        status_block(c, iosb, STATUS_SUCCESS, need);
        STATUS_SUCCESS
    });

    let f = files.clone();
    hle.handle("NtQueryDirectoryFile", move |c| {
        // (Handle, Event, ApcRoutine, ApcContext, IoStatusBlock*, Information*, Length, Class,
        //  FileName*, RestartScan)
        let (h, iosb, info, len, mask_p, restart) = (arg(c, 0), arg(c, 4), arg(c, 5), arg(c, 6), arg(c, 8), arg(c, 9) & 0xFF != 0);
        let mask = if mask_p != 0 { ansi(c, mask_p) } else { "*".into() };
        let mut files = f.borrow_mut();
        let Some(file) = files.open.get(&h) else { return STATUS_INVALID_HANDLE };
        if !file.is_dir {
            return STATUS_NOT_A_DIRECTORY;
        }
        let (dev, path, fresh) = (file.dev, file.path.clone(), file.listing.is_none() || restart);
        if fresh {
            let all = files.devices[dev].1.list(&path).unwrap_or_default();
            let pat = if mask.is_empty() { "*" } else { mask.as_str() };
            let listing: Vec<Stat> = all.into_iter().filter(|s| wildcard(pat, &s.name)).collect();
            let file = files.open.get_mut(&h).unwrap();
            file.listing = Some(listing);
            file.cursor = 0;
        }
        let file = files.open.get_mut(&h).unwrap();
        let entry = file.listing.as_ref().and_then(|l| l.get(file.cursor)).cloned();
        let Some(e) = entry else {
            drop(files);
            status_block(c, iosb, STATUS_NO_MORE_FILES, 0);
            return STATUS_NO_MORE_FILES;
        };
        let need = 64 + e.name.len() as u32;
        if len < need {
            drop(files);
            return STATUS_BUFFER_TOO_SMALL;
        }
        file.cursor += 1;
        drop(files);
        // FILE_DIRECTORY_INFORMATION
        let _ = c.write_u32(info, 0);
        let _ = c.write_u32(info + 4, 0);
        for i in 0..4 {
            put64(c, info + 8 + 8 * i, FILE_TIME);
        }
        put64(c, info + 40, e.size);
        put64(c, info + 48, alloc_size(e.size));
        let _ = c.write_u32(info + 56, attributes(&e));
        let _ = c.write_u32(info + 60, e.name.len() as u32);
        let _ = c.bus.write_bytes((info + 64) as u64, e.name.as_bytes());
        status_block(c, iosb, STATUS_SUCCESS, need);
        STATUS_SUCCESS
    });
    let f = files.clone();
    hle.handle("NtQueryFullAttributesFile", move |c| {
        // (ObjectAttributes*, FILE_NETWORK_OPEN_INFORMATION*)
        let (oa, out) = (arg(c, 0), arg(c, 1));
        let (_, name) = object_name(c, oa);
        let mut files = f.borrow_mut();
        let r = files.resolve(&name, None).and_then(|(d, p)| files.devices[d].1.stat(&p).ok_or(STATUS_NAME_NOT_FOUND));
        files.note(format!("attributes {name:?} -> {}", r.as_ref().map_or_else(|e| format!("{e:#x}"), |_| "ok".into())));
        drop(files);
        match r {
            Ok(s) => {
                network_info(c, out, &s);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    });
    hle.handle("NtFlushBuffersFile", |c| {
        let iosb = arg(c, 1);
        status_block(c, iosb, STATUS_SUCCESS, 0);
        STATUS_SUCCESS
    });
    let f = files.clone();
    hle.handle("NtDeleteFile", move |c| {
        let oa = arg(c, 0);
        let (_, name) = object_name(c, oa);
        let mut files = f.borrow_mut();
        let r = files.resolve(&name, None).and_then(|(d, p)| files.devices[d].1.delete(&p));
        files.note(format!("delete {name:?} -> {}", r.as_ref().map_or_else(|e| format!("{e:#x}"), |_| "ok".into())));
        r.err().unwrap_or(STATUS_SUCCESS)
    });
    for name in ["NtFsControlFile", "NtDeviceIoControlFile"] {
        let f = files.clone();
        hle.handle(name, move |c| {
            // (Handle, Event, ApcRoutine, ApcContext, IoStatusBlock*, IoControlCode, ...)
            let (iosb, code) = (arg(c, 4), arg(c, 5));
            f.borrow_mut().note(format!("{name} control code {code:#x} -> not supported"));
            status_block(c, iosb, STATUS_INVALID_DEVICE_REQUEST, 0);
            STATUS_INVALID_DEVICE_REQUEST
        });
    }

    // ---- drive-letter links ----
    let f = files.clone();
    hle.handle("IoCreateSymbolicLink", move |c| {
        let (link, target) = (arg(c, 0), arg(c, 1));
        let (l, t) = (ansi(c, link), ansi(c, target));
        f.borrow_mut().links.insert(l.to_ascii_lowercase(), t.to_ascii_lowercase());
        f.borrow_mut().note(format!("link {l:?} -> {t:?}"));
        STATUS_SUCCESS
    });
    let f = files.clone();
    hle.handle("IoDeleteSymbolicLink", move |c| {
        let link = arg(c, 0);
        let l = ansi(c, link).to_ascii_lowercase();
        if f.borrow_mut().links.remove(&l).is_some() { STATUS_SUCCESS } else { STATUS_NAME_NOT_FOUND }
    });
    let f = files.clone();
    hle.handle("NtOpenSymbolicLinkObject", move |c| {
        let (out, oa) = (arg(c, 0), arg(c, 1));
        let (_, name) = object_name(c, oa);
        open_link(c, &f, out, &name)
    });
    let f = files.clone();
    hle.handle("ObOpenObjectByName", move |c| {
        // (ObjectAttributes*, ObjectType, ParseContext, Handle*)
        let (oa, out) = (arg(c, 0), arg(c, 3));
        let (_, name) = object_name(c, oa);
        open_link(c, &f, out, &name)
    });
    let f = files.clone();
    hle.handle("NtQuerySymbolicLinkObject", move |c| {
        // (Handle, LinkTarget (ANSI_STRING*), ReturnedLength*)
        let (h, target, ret) = (arg(c, 0), arg(c, 1), arg(c, 2));
        let files = f.borrow();
        let Some(t) = files.symlink_handles.get(&h).and_then(|n| files.links.get(n)).cloned() else { return STATUS_INVALID_HANDLE };
        drop(files);
        let max = c.bus.read((target + 2) as u64, 2).unwrap_or(0) as usize;
        let buf = c.read_u32(target + 4).unwrap_or(0);
        if t.len() > max {
            return STATUS_BUFFER_TOO_SMALL;
        }
        let _ = c.bus.write_bytes(buf as u64, t.as_bytes());
        let _ = c.bus.write(target as u64, 2, t.len() as u64);
        if ret != 0 {
            let _ = c.write_u32(ret, t.len() as u32);
        }
        STATUS_SUCCESS
    });

    // ---- closing ----
    let f = files.clone();
    hle.handle("NtClose", move |c| {
        let h = arg(c, 0);
        let mut files = f.borrow_mut();
        files.symlink_handles.remove(&h);
        if let Some(o) = files.open.remove(&h) {
            if o.delete_on_close {
                let _ = files.devices[o.dev].1.delete(&o.path);
            }
        }
        drop(files);
        c.sched.close_handle(h);
        STATUS_SUCCESS
    });
}

fn open_link(c: &mut Call, f: &Rc<RefCell<Files>>, out: u32, name: &str) -> u32 {
    let key = name.to_ascii_lowercase();
    let mut files = f.borrow_mut();
    if !files.links.contains_key(&key) {
        return STATUS_NAME_NOT_FOUND;
    }
    let h = c.sched.new_handle(0);
    files.symlink_handles.insert(h, key);
    drop(files);
    let _ = c.write_u32(out, h);
    STATUS_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_raw_disk_reads_zeros_and_keeps_writes() {
        let mut d = RawDisk::default();
        assert_eq!(d.read("", 4096, 512).unwrap(), vec![0; 512]);
        d.write("", 510, &[1, 2, 3, 4]).unwrap();
        assert_eq!(d.read("", 508, 8).unwrap(), vec![0, 0, 1, 2, 3, 4, 0, 0]);
        assert_eq!(d.stat("").unwrap().size, 8 << 30);
        assert!(d.read("", 8 << 30, 512).is_err());
    }

    fn files() -> Files {
        let mut f = Files::new(0x4D53_0004);
        let mut disc = MemVolume::new();
        disc.make_dirs("levels");
        disc.create("levels\\one.bin", false).unwrap();
        disc.write("levels\\one.bin", 0, b"hello").unwrap();
        f.mount("\\Device\\CdRom0", Box::new(disc));
        f
    }

    #[test]
    fn drive_letters_resolve_through_links_to_devices() {
        let f = files();
        let (d, p) = f.resolve("\\??\\D:\\levels\\One.bin", None).unwrap();
        assert_eq!(f.devices[d].0, "\\device\\cdrom0");
        assert_eq!(p, "levels\\One.bin");
        let (d, p) = f.resolve("E:\\TDATA", None).unwrap();
        assert_eq!((f.devices[d].0.as_str(), p.as_str()), ("\\device\\harddisk0\\partition1", "TDATA"));
        let (d, p) = f.resolve("\\Device\\CdRom0", None).unwrap();
        assert_eq!((f.devices[d].0.as_str(), p.as_str()), ("\\device\\cdrom0", ""));
        assert_eq!(f.resolve("\\Device\\Nothing\\x", None).unwrap_err(), STATUS_NO_SUCH_DEVICE);
    }

    #[test]
    fn links_made_by_the_game_can_point_into_a_folder() {
        let mut f = files();
        f.links.insert("\\??\\t:".into(), "\\device\\harddisk0\\partition1\\tdata\\4d530004".into());
        let (d, p) = f.resolve("T:\\save.dat", None).unwrap();
        assert_eq!(f.devices[d].0, "\\device\\harddisk0\\partition1");
        assert_eq!(p, "tdata\\4d530004\\save.dat");
        let (_, p) = f.resolve("\\??\\T:\\", None).unwrap();
        assert_eq!(p, "tdata\\4d530004");
        assert!(f.devices[d].1.stat("TDATA\\4D530004").unwrap().is_dir, "the save folder exists");
    }

    #[test]
    fn dot_dot_cannot_climb_out_of_a_volume() {
        assert_eq!(normalise("a\\..\\..\\b\\.\\c"), "b\\c");
    }

    #[test]
    fn memory_volume_creates_writes_lists_and_deletes() {
        let mut v = MemVolume::new();
        assert_eq!(v.create("a\\b", false), Err(STATUS_PATH_NOT_FOUND));
        v.create("a", true).unwrap();
        v.create("a\\b.txt", false).unwrap();
        v.write("a\\b.txt", 3, b"xy").unwrap();
        assert_eq!(v.stat("A\\B.TXT").unwrap().size, 5);
        assert_eq!(v.read("a\\b.txt", 3, 10).unwrap(), b"xy");
        assert_eq!(v.read("a\\b.txt", 5, 1), Err(STATUS_END_OF_FILE));
        assert_eq!(v.list("a").unwrap().len(), 1);
        assert_eq!(v.delete("a"), Err(STATUS_ACCESS_DENIED), "not empty");
        v.delete("a\\b.txt").unwrap();
        v.delete("a").unwrap();
        assert!(v.list("").unwrap().is_empty());
    }

    #[test]
    fn wildcards_match_like_the_console() {
        assert!(wildcard("*", "anything.xbe"));
        assert!(wildcard("*.XBE", "default.xbe"));
        assert!(wildcard("d?fault.*", "Default.xbe"));
        assert!(!wildcard("*.xbe", "default.xbf"));
    }

    #[test]
    fn the_disc_volume_reads_files_from_an_image() {
        use ferrox_formats::xdvdfs::testutil::{build, Node};
        let image = build(
            &[Node::File("default.xbe", vec![7; 5000]), Node::Dir("levels", vec![Node::File("a.bin", (0..100).collect())])],
            0,
        );
        let disc = Xdvdfs::open(std::io::Cursor::new(image)).unwrap();
        let mut v = DiscVolume::new(disc);
        assert_eq!(v.stat("LEVELS\\A.BIN").unwrap().size, 100);
        assert_eq!(v.read("levels\\a.bin", 10, 3).unwrap(), [10, 11, 12]);
        assert_eq!(v.read("levels\\a.bin", 100, 1), Err(STATUS_END_OF_FILE));
        let names: Vec<String> = v.list("").unwrap().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"levels".to_string()) && names.contains(&"default.xbe".to_string()));
        assert!(v.stat("levels").unwrap().is_dir);
    }

    #[test]
    fn a_guest_opens_and_reads_a_file_through_the_kernel_calls() {
        use crate::{exports, kernel, Layout, SchedAreas};
        use ferrox_cpu::{CpuBackend, Exit, RefCpu, ESP};
        use ferrox_formats::xbe::synth::{build, BASE};
        use ferrox_formats::xbe::Xbe;
        use ferrox_machine::PhysMem;
        const LAYOUT: Layout = Layout { stub_base: 0x30_0000, data_base: 0x31_0000, stack_top: 0x40_0000 - 16 };
        const AREAS: SchedAreas = SchedAreas { kpcr: 0x32_0000, stubs: 0x32_1000, objects: 0x33_0000..0x37_0000, stack_top: 0x40_0000 - 16, stack_size: 0x1_0000 };
        let (h, iosb, oa, name, text, buf, cwd) = (0x2_0000u32, 0x2_0010u32, 0x2_0020u32, 0x2_0040u32, 0x2_0060u32, 0x2_0100u32, 0x2_0200u32);
        let ords: Vec<u32> = ["NtOpenFile", "NtReadFile"].iter().map(|n| exports::by_name(n).unwrap().ordinal).collect();
        let mut code = Vec::new();
        let call = |code: &mut Vec<u8>, slot: u32, args: &[u32]| {
            for &a in args.iter().rev() {
                code.push(0x68);
                code.extend_from_slice(&a.to_le_bytes());
            }
            code.extend_from_slice(&[0xFF, 0x15]);
            code.extend_from_slice(&(BASE + 0x2000 + 4 * slot).to_le_bytes());
        };
        call(&mut code, 0, &[h, 0x100001, oa, iosb, 0, 0]);
        code.push(0xA3); // the status of the open
        code.extend_from_slice(&cwd.to_le_bytes());
        // NtReadFile(handle, 0, 0, 0, &iosb, buf, 5, NULL): the handle is only known at run time,
        // so load it into eax and push it last
        code.extend_from_slice(&[0xA1]);
        code.extend_from_slice(&h.to_le_bytes());
        code.extend_from_slice(&[0x68]); // push 0 (ByteOffset)
        code.extend_from_slice(&0u32.to_le_bytes());
        code.extend_from_slice(&[0x68]);
        code.extend_from_slice(&5u32.to_le_bytes());
        code.extend_from_slice(&[0x68]);
        code.extend_from_slice(&buf.to_le_bytes());
        code.extend_from_slice(&[0x68]);
        code.extend_from_slice(&iosb.to_le_bytes());
        for _ in 0..3 {
            code.extend_from_slice(&[0x68, 0, 0, 0, 0]);
        }
        code.push(0x50); // push eax (the handle)
        code.extend_from_slice(&[0xFF, 0x15]);
        code.extend_from_slice(&(BASE + 0x2004).to_le_bytes());
        code.push(0xA3);
        code.extend_from_slice(&(cwd + 4).to_le_bytes());
        code.push(0xF4);
        let mut file = build(true, &ords);
        file[0x1000..0x1000 + code.len()].copy_from_slice(&code);
        let xbe = Xbe::parse(&file).unwrap();
        let mut hle = KernelHle::with_table();
        kernel::install(&mut hle, 0x38_0000..0x3F_0000);
        hle_install(&mut hle);
        let mut mem = PhysMem::new();
        mem.add_ram(0, 4 * 1024 * 1024).unwrap();
        // "\\??\\D:\\levels\\one.bin" as an ANSI_STRING inside OBJECT_ATTRIBUTES
        let path = b"\\??\\D:\\levels\\one.bin";
        mem.load(text as u64, path).unwrap();
        mem.write(name as u64, 2, path.len() as u64).unwrap();
        mem.write(name as u64 + 2, 2, path.len() as u64).unwrap();
        mem.write(name as u64 + 4, 4, text as u64).unwrap();
        mem.write(oa as u64 + 4, 4, name as u64).unwrap();
        let loaded = hle.load(&mut mem, &xbe, &file, LAYOUT).unwrap();
        hle.start_scheduler(&mut mem, AREAS).unwrap();
        let mut cpu = RefCpu::new(loaded.entry);
        let mut r = cpu.regs();
        r.gpr[ESP] = loaded.stack_top;
        cpu.set_regs(r);
        assert_eq!(hle.run(&mut cpu, &mut mem, 100_000).unwrap(), Exit::Halt);
        use ferrox_core::Bus;
        assert_eq!(mem.read(cwd as u64, 4).unwrap(), 0, "open succeeded");
        assert_eq!(mem.read(cwd as u64 + 4, 4).unwrap(), 0, "read succeeded");
        assert_eq!(mem.read(iosb as u64 + 4, 4).unwrap(), 5, "five bytes came back");
        let mut got = [0u8; 5];
        mem.read_bytes(buf as u64, &mut got).unwrap();
        assert_eq!(&got, b"hello");
    }

    fn hle_install(hle: &mut KernelHle) {
        let mut f = files();
        f.mount("\\Device\\CdRom0", {
            let mut disc = MemVolume::new();
            disc.make_dirs("levels");
            disc.create("levels\\one.bin", false).unwrap();
            disc.write("levels\\one.bin", 0, b"hello").unwrap();
            Box::new(disc)
        });
        install(hle, Rc::new(RefCell::new(f)));
    }
}
