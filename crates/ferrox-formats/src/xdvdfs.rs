//! XDVDFS, the Xbox disc file system, as found in XISO images.
//!
//! A volume descriptor sits at sector 32 of the game partition and points at
//! the root directory table. A directory table is a binary search tree: each
//! entry holds the offsets (in 4-byte units) of its left and right children,
//! the start sector and size of a file or sub-directory, attributes and the
//! name. Full disc dumps carry the partition at an offset; plain XISOs at 0.

use crate::{le16, le32, Error, Result};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};

pub const SECTOR_SIZE: u64 = 2048;
const MAGIC: &[u8; 20] = b"MICROSOFT*XBOX*MEDIA";
const VOLUME_SECTOR: u64 = 32;
const ATTR_DIR: u8 = 0x10;

/// Where the game partition can start: plain XISO, XGD3, XGD2, XGD1.
pub const PARTITION_OFFSETS: [u64; 4] = [0, 0x0208_0000, 0x0FD9_0000, 0x1830_0000];

/// Limits that keep a hostile image from making the reader run away.
const MAX_DIR_BYTES: u32 = 64 * 1024 * 1024;
const MAX_ENTRIES_PER_DIR: usize = 200_000;
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path from the disc root, `/` separated, no leading slash.
    pub path: String,
    pub sector: u32,
    pub size: u32,
    pub is_dir: bool,
}

impl Entry {
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

pub struct Xdvdfs<R: Read + Seek> {
    r: R,
    base: u64,
    root_sector: u32,
    root_size: u32,
    len: u64,
}

impl<R: Read + Seek> Xdvdfs<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        for &base in &PARTITION_OFFSETS {
            let at = base + VOLUME_SECTOR * SECTOR_SIZE;
            if at + SECTOR_SIZE > len {
                continue;
            }
            let mut vd = vec![0u8; SECTOR_SIZE as usize];
            r.seek(SeekFrom::Start(at))?;
            r.read_exact(&mut vd)?;
            if &vd[..20] != MAGIC {
                continue;
            }
            let root_sector = le32(&vd, 20).ok_or(Error::Corrupt("short volume descriptor"))?;
            let root_size = le32(&vd, 24).ok_or(Error::Corrupt("short volume descriptor"))?;
            if root_size > MAX_DIR_BYTES {
                return Err(Error::Corrupt("root directory is implausibly large"));
            }
            return Ok(Self { r, base, root_sector, root_size, len });
        }
        Err(Error::Format("not an Xbox disc image (no XDVDFS volume descriptor)"))
    }

    /// Byte offset of the game partition inside the image.
    pub fn partition_offset(&self) -> u64 {
        self.base
    }

    fn read_at(&mut self, sector: u32, offset: u64, len: usize) -> Result<Vec<u8>> {
        let at = self.base + sector as u64 * SECTOR_SIZE + offset;
        if at.checked_add(len as u64).map_or(true, |end| end > self.len) {
            return Err(Error::Corrupt("data lies beyond the end of the image"));
        }
        let mut buf = vec![0u8; len];
        self.r.seek(SeekFrom::Start(at))?;
        self.r.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// The entries of one directory table, in no particular order.
    fn read_table(&mut self, sector: u32, size: u32, parent: &str) -> Result<Vec<Entry>> {
        if size == 0 {
            return Ok(Vec::new());
        }
        if size > MAX_DIR_BYTES {
            return Err(Error::Corrupt("directory is implausibly large"));
        }
        let table = self.read_at(sector, 0, size as usize)?;
        let mut out = Vec::new();
        let mut stack = vec![0usize];
        let mut seen = HashSet::new();
        while let Some(pos) = stack.pop() {
            if !seen.insert(pos) {
                continue; // a loop in a damaged tree
            }
            let (Some(left), Some(right), Some(sec), Some(sz)) = (
                le16(&table, pos),
                le16(&table, pos + 2),
                le32(&table, pos + 4),
                le32(&table, pos + 8),
            ) else {
                return Err(Error::Corrupt("directory entry runs off the table"));
            };
            if left == 0xFFFF && right == 0xFFFF {
                continue; // padding (also what an empty directory holds)
            }
            let attr = *table.get(pos + 12).ok_or(Error::Corrupt("short entry"))?;
            let name_len = *table.get(pos + 13).ok_or(Error::Corrupt("short entry"))? as usize;
            let name_bytes = table
                .get(pos + 14..pos + 14 + name_len)
                .ok_or(Error::Corrupt("name runs off the table"))?;
            if name_len == 0 {
                return Err(Error::Corrupt("entry with an empty name"));
            }
            // Names are Latin-1 on disc; map bytes to chars so nothing is lost
            let name: String = name_bytes.iter().map(|&b| b as char).collect();
            let path = if parent.is_empty() { name } else { format!("{parent}/{name}") };
            out.push(Entry { path, sector: sec, size: sz, is_dir: attr & ATTR_DIR != 0 });
            if out.len() > MAX_ENTRIES_PER_DIR {
                return Err(Error::Corrupt("directory has too many entries"));
            }
            for child in [left, right] {
                if child != 0 && child != 0xFFFF {
                    stack.push(child as usize * 4);
                }
            }
        }
        out.sort_by(|a, b| cmp_names(&a.path, &b.path));
        Ok(out)
    }

    /// Every file and directory on the disc, depth first, names sorted.
    pub fn walk(&mut self) -> Result<Vec<Entry>> {
        let mut all = Vec::new();
        let mut visited = HashSet::new();
        self.walk_dir(self.root_sector, self.root_size, "", 0, &mut all, &mut visited)?;
        Ok(all)
    }

    fn walk_dir(
        &mut self,
        sector: u32,
        size: u32,
        parent: &str,
        depth: usize,
        all: &mut Vec<Entry>,
        visited: &mut HashSet<u32>,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::Corrupt("folders are nested too deeply"));
        }
        for e in self.read_table(sector, size, parent)? {
            all.push(e.clone());
            if e.is_dir && e.size > 0 && visited.insert(e.sector) {
                self.walk_dir(e.sector, e.size, &e.path, depth + 1, all, visited)?;
            }
        }
        Ok(())
    }

    /// Finds one file or folder by path (case-insensitive, `/` or `\` separated).
    pub fn find(&mut self, path: &str) -> Result<Option<Entry>> {
        let mut sector = self.root_sector;
        let mut size = self.root_size;
        let mut found: Option<Entry> = None;
        let mut prefix = String::new();
        for part in path.split(['/', '\\']).filter(|p| !p.is_empty()) {
            let entries = self.read_table(sector, size, &prefix)?;
            let hit = entries
                .into_iter()
                .find(|e| e.name().eq_ignore_ascii_case(part));
            match hit {
                None => return Ok(None),
                Some(e) => {
                    sector = e.sector;
                    size = e.size;
                    prefix = e.path.clone();
                    found = Some(e);
                }
            }
        }
        Ok(found)
    }

    pub fn read_file(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        if entry.is_dir {
            return Err(Error::Format("that is a folder, not a file"));
        }
        self.read_at(entry.sector, 0, entry.size as usize)
    }

    /// Reads `len` bytes starting `offset` bytes into a file.
    pub fn read_file_range(&mut self, entry: &Entry, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset + len as u64 > entry.size as u64 {
            return Err(Error::Corrupt("read past the end of the file"));
        }
        self.read_at(entry.sector, offset, len)
    }
}

/// Case-insensitive on ASCII letters only, as the console does.
pub fn cmp_names(a: &str, b: &str) -> std::cmp::Ordering {
    let up = |c: char| c.to_ascii_uppercase();
    a.chars().map(up).cmp(b.chars().map(up))
}

#[cfg(test)]
pub(crate) mod testutil {
    //! A tiny XISO writer for tests (balanced trees, files after tables).
    use super::*;

    pub enum Node {
        File(&'static str, Vec<u8>),
        Dir(&'static str, Vec<Node>),
    }

    fn name_of(n: &Node) -> &'static str {
        match n {
            Node::File(n, _) | Node::Dir(n, _) => n,
        }
    }

    struct Placed {
        sector: u32,
        size: u32,
    }

    /// Builds an image with the partition at `base`.
    pub fn build(root: &[Node], base: u64) -> Vec<u8> {
        let mut img = vec![0u8; base as usize + 33 * 2048];
        let mut next_sector = 33u32;
        let mut writes: Vec<(u32, Vec<u8>)> = Vec::new();
        let root_p = place(root, &mut next_sector, &mut writes);
        let mut vd = vec![0u8; 2048];
        vd[..20].copy_from_slice(MAGIC);
        vd[20..24].copy_from_slice(&root_p.sector.to_le_bytes());
        vd[24..28].copy_from_slice(&root_p.size.to_le_bytes());
        vd[0x7EC..0x7EC + 20].copy_from_slice(MAGIC);
        let at = base as usize + 32 * 2048;
        img[at..at + 2048].copy_from_slice(&vd);
        for (sector, data) in writes {
            let at = base as usize + sector as usize * 2048;
            if img.len() < at + data.len() {
                img.resize(at + data.len(), 0);
            }
            img[at..at + data.len()].copy_from_slice(&data);
        }
        let want = img.len().div_ceil(2048) * 2048;
        img.resize(want, 0);
        img
    }

    fn place(nodes: &[Node], next: &mut u32, writes: &mut Vec<(u32, Vec<u8>)>) -> Placed {
        // Children first need sectors; reserve this table's sector range after
        let mut sorted: Vec<&Node> = nodes.iter().collect();
        sorted.sort_by(|a, b| cmp_names(name_of(a), name_of(b)));
        let table_sector = *next;
        let table_sectors = 1u32.max(((sorted.len() * 40) as u32).div_ceil(2048));
        *next += table_sectors;
        let mut placed = Vec::new();
        for n in &sorted {
            match n {
                Node::File(_, data) => {
                    let s = *next;
                    *next += (data.len() as u32).div_ceil(2048);
                    if !data.is_empty() {
                        writes.push((s, data.clone()));
                    }
                    placed.push(Placed { sector: s, size: data.len() as u32 });
                }
                Node::Dir(_, kids) => placed.push(place(kids, next, writes)),
            }
        }
        // Balanced tree over the sorted names, entries 40 bytes apiece
        let mut table = vec![0xFFu8; table_sectors as usize * 2048];
        // Pre-order placement so the root lands at offset 0
        fn order(lo: isize, hi: isize, out: &mut Vec<(usize, Option<usize>, Option<usize>)>) -> Option<usize> {
            if lo > hi {
                return None;
            }
            let mid = ((lo + hi) / 2) as usize;
            let idx = out.len();
            out.push((mid, None, None));
            let l = order(lo, mid as isize - 1, out);
            let r = order(mid as isize + 1, hi, out);
            out[idx].1 = l;
            out[idx].2 = r;
            Some(idx)
        }
        let mut plan = Vec::new();
        order(0, sorted.len() as isize - 1, &mut plan);
        for (slot, &(item, l, r)) in plan.iter().enumerate() {
            let at = slot * 40;
            let n = sorted[item];
            let p = &placed[item];
            let lo = l.map_or(0u16, |s| (s * 40 / 4) as u16);
            let ro = r.map_or(0u16, |s| (s * 40 / 4) as u16);
            table[at..at + 2].copy_from_slice(&lo.to_le_bytes());
            table[at + 2..at + 4].copy_from_slice(&ro.to_le_bytes());
            table[at + 4..at + 8].copy_from_slice(&p.sector.to_le_bytes());
            table[at + 8..at + 12].copy_from_slice(&p.size.to_le_bytes());
            table[at + 12] = if matches!(n, Node::Dir(..)) { ATTR_DIR } else { 0x20 };
            let nm = name_of(n).as_bytes();
            table[at + 13] = nm.len() as u8;
            table[at + 14..at + 14 + nm.len()].copy_from_slice(nm);
        }
        writes.push((table_sector, table));
        Placed { sector: table_sector, size: table_sectors * 2048 }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use std::io::Cursor;

    fn sample() -> Vec<Node> {
        vec![
            Node::File("default.xbe", vec![0xAA; 5000]),
            Node::Dir(
                "Media",
                vec![
                    Node::File("a.bin", vec![1, 2, 3]),
                    Node::Dir("Sub", vec![Node::File("b.txt", b"hi".to_vec())]),
                ],
            ),
            Node::Dir("EmptyDir", vec![]),
            Node::File("Zeta.dat", vec![9; 2048]),
            Node::File("alpha.dat", vec![]),
        ]
    }

    #[test]
    fn walks_every_partition_layout() {
        for &base in &PARTITION_OFFSETS {
            let img = build(&sample(), base);
            let mut fs = Xdvdfs::open(Cursor::new(img)).unwrap();
            assert_eq!(fs.partition_offset(), base);
            let mut paths: Vec<String> = fs.walk().unwrap().into_iter().map(|e| e.path).collect();
            paths.sort();
            assert_eq!(
                paths,
                [
                    "EmptyDir", "Media", "Media/Sub", "Media/Sub/b.txt", "Media/a.bin",
                    "Zeta.dat", "alpha.dat", "default.xbe"
                ]
            );
        }
    }

    #[test]
    fn finds_and_reads_files_case_insensitively() {
        let mut fs = Xdvdfs::open(Cursor::new(build(&sample(), 0))).unwrap();
        let e = fs.find("MEDIA\\sub/B.TXT").unwrap().unwrap();
        assert_eq!(fs.read_file(&e).unwrap(), b"hi");
        let x = fs.find("default.xbe").unwrap().unwrap();
        assert_eq!(x.size, 5000);
        assert!(fs.read_file(&x).unwrap().iter().all(|&b| b == 0xAA));
        assert_eq!(fs.read_file_range(&x, 4990, 10).unwrap().len(), 10);
        assert!(fs.read_file_range(&x, 4995, 10).is_err());
        assert!(fs.find("nope/default.xbe").unwrap().is_none());
        let d = fs.find("EmptyDir").unwrap().unwrap();
        assert!(d.is_dir);
        assert!(fs.read_file(&d).is_err());
    }

    #[test]
    fn rejects_non_xbox_images() {
        let err = Xdvdfs::open(Cursor::new(vec![0u8; 0x20000]));
        assert!(matches!(err, Err(Error::Format(_))));
    }

    #[test]
    fn damaged_trees_error_instead_of_looping_or_panicking() {
        let mut img = build(&sample(), 0);
        // Point the root entry's left child back at itself, and truncate
        let root_sector = u32::from_le_bytes(img[32 * 2048 + 20..32 * 2048 + 24].try_into().unwrap());
        let at = root_sector as usize * 2048;
        img[at..at + 2].copy_from_slice(&0u16.to_le_bytes());
        img[at + 2..at + 4].copy_from_slice(&0u16.to_le_bytes());
        let mut fs = Xdvdfs::open(Cursor::new(img.clone())).unwrap();
        assert!(fs.walk().is_ok()); // a lone root entry is just a short listing
        img[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut fs = Xdvdfs::open(Cursor::new(img)).unwrap();
        let all = fs.walk();
        let _ = all; // beyond-the-end sector must not panic
        let mut fs = Xdvdfs::open(Cursor::new(build(&sample(), 0))).unwrap();
        let e = Entry { path: "x".into(), sector: u32::MAX, size: 10, is_dir: false };
        assert!(fs.read_file(&e).is_err());
    }
}
