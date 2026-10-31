//! Blank hard-drive images for the original Xbox.
//!
//! The partition table is the console's fixed layout (xboxdevwiki, "Hard Drive").
//! Each data partition is formatted the way nxdk's `format.c` formats one: a
//! 4 KiB superblock, a cluster table, and an empty root directory cluster.
//! The 512 KiB configuration area is left zeroed; the console writes it itself.
//!
//! Images are sparse files, so an 8 GiB image costs almost no disk until used.
//! This has been checked against the nxdk formatter's logic, not yet by booting
//! it in another emulator.

use crate::{Error, Result};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

pub const SECTOR: u64 = 512;
const CLUSTER: u64 = 16 * 1024;
const SUPERBLOCK: u64 = 4096;
const SIGNATURE: &[u8; 4] = b"FATX";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub drive: char,
    pub start: u64,
    /// `None` means "to the end of the disk" (the E: data partition).
    pub size: Option<u64>,
}

/// X, Y, Z (game cache), C (system), E (data) in disk order.
pub const PARTITIONS: [Partition; 5] = [
    Partition { drive: 'X', start: 0x0008_0000, size: Some(0x2EE0_0000) },
    Partition { drive: 'Y', start: 0x2EE8_0000, size: Some(0x2EE0_0000) },
    Partition { drive: 'Z', start: 0x5DC8_0000, size: Some(0x2EE0_0000) },
    Partition { drive: 'C', start: 0x8CA8_0000, size: Some(0x1F40_0000) },
    Partition { drive: 'E', start: 0xABE8_0000, size: None },
];

/// Smallest image that holds the fixed partitions plus a 64 MiB E: partition.
pub const MIN_SIZE: u64 = 0xABE8_0000 + 64 * 1024 * 1024;

/// Bytes needed to format a partition of `len` bytes: superblock, cluster
/// table, root directory. `fatx16` says which entry width applies.
pub fn layout(len: u64) -> Result<(bool, u64, u64)> {
    let clusters = len / CLUSTER + 1;
    let fatx16 = clusters < 65520;
    let table = clusters * if fatx16 { 2 } else { 4 };
    let table = table.div_ceil(SUPERBLOCK) * SUPERBLOCK;
    if SUPERBLOCK + table + CLUSTER >= len {
        return Err(Error::Format("partition too small to format"));
    }
    Ok((fatx16, table, clusters))
}

/// Writes the formatted partition bytes `[0, SUPERBLOCK + table + CLUSTER)`.
fn format_partition(volume_id: u32, len: u64) -> Result<Vec<u8>> {
    let (fatx16, table, _) = layout(len)?;
    let mut out = vec![0u8; (SUPERBLOCK + table + CLUSTER) as usize];
    // superblock: 0xFF fill, then the fields (matching nxdk)
    out[..SUPERBLOCK as usize].fill(0xFF);
    out[0..4].copy_from_slice(SIGNATURE);
    out[4..8].copy_from_slice(&volume_id.to_le_bytes());
    out[8..12].copy_from_slice(&((CLUSTER / SECTOR) as u32).to_le_bytes());
    out[12..16].copy_from_slice(&1u32.to_le_bytes());
    out[16..18].copy_from_slice(&0u16.to_le_bytes());
    // cluster table: entry 0 = media byte + end marker, entry 1 = end of chain
    // (the root directory's single cluster); everything else free (zero)
    let t = SUPERBLOCK as usize;
    if fatx16 {
        out[t..t + 2].copy_from_slice(&0xFFF8u16.to_le_bytes());
        out[t + 2..t + 4].copy_from_slice(&0xFFFFu16.to_le_bytes());
    } else {
        out[t..t + 4].copy_from_slice(&0xFFFF_FFF8u32.to_le_bytes());
        out[t + 4..t + 8].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    }
    // empty root directory: every byte 0xFF
    let root = (SUPERBLOCK + table) as usize;
    out[root..].fill(0xFF);
    Ok(out)
}

/// Creates a sparse, formatted hard-drive image of `size` bytes at `path`.
/// Refuses to overwrite an existing file.
pub fn create_image(path: &Path, size: u64) -> Result<()> {
    if size < MIN_SIZE {
        return Err(Error::Format("image too small (minimum is about 2.8 GiB)"));
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| Error::Format("could not create the image file (does it already exist?)"))?;
    f.set_len(size).map_err(|_| Error::Format("could not size the image file"))?;
    for (i, p) in PARTITIONS.iter().enumerate() {
        let len = p.size.unwrap_or(size - p.start);
        // distinct, repeatable volume ids so two images do not look identical
        let id = 0x5846_5800u32 ^ (i as u32).wrapping_mul(0x9E37_79B1);
        let bytes = format_partition(id, len)?;
        f.seek(SeekFrom::Start(p.start)).and_then(|_| f.write_all(&bytes)).map_err(|_| Error::Format("write failed"))?;
    }
    f.flush().map_err(|_| Error::Format("write failed"))?;
    Ok(())
}

/// Describes an image: size and, per partition, whether it holds a FATX superblock.
pub fn inspect(path: &Path) -> Result<Vec<(char, bool)>> {
    use std::io::Read;
    let mut f = File::open(path).map_err(|_| Error::Format("could not open the image"))?;
    let mut out = Vec::new();
    for p in PARTITIONS {
        let mut sig = [0u8; 4];
        let ok = f.seek(SeekFrom::Start(p.start)).is_ok() && f.read_exact(&mut sig).is_ok() && &sig == SIGNATURE;
        out.push((p.drive, ok));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partitions_do_not_overlap_and_match_the_documented_table() {
        for w in PARTITIONS.windows(2) {
            // the documented map is in disk order except C, which sits between Z and E
            assert!(w[0].start + w[0].size.unwrap() <= w[1].start, "{:?} {:?}", w[0], w[1]);
        }
        assert_eq!(PARTITIONS[3].start, 0x8CA8_0000);
        assert_eq!(PARTITIONS[4].start, 0xABE8_0000);
    }

    #[test]
    fn entry_width_follows_the_cluster_count() {
        // 750 MiB (X/Y/Z) and 500 MiB (C) have fewer than 65520 clusters: 16-bit
        assert!(layout(0x2EE0_0000).unwrap().0);
        assert!(layout(0x1F40_0000).unwrap().0);
        // a 4 GiB data partition needs 32-bit entries
        assert!(!layout(4 * 1024 * 1024 * 1024).unwrap().0);
        assert!(layout(20_000).is_err());
    }

    #[test]
    fn formatted_partition_has_signature_table_and_empty_root() {
        let len = 0x2EE0_0000u64;
        let (_, table, _) = layout(len).unwrap();
        let b = format_partition(0x1234_5678, len).unwrap();
        assert_eq!(&b[0..4], b"FATX");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 0x1234_5678);
        assert_eq!(u32::from_le_bytes(b[8..12].try_into().unwrap()), 32); // sectors per cluster
        assert_eq!(&b[4096..4100], &[0xF8, 0xFF, 0xFF, 0xFF]); // 16-bit entries 0 and 1
        assert_eq!(b[4100], 0); // free clusters are zero
        let root = (SUPERBLOCK + table) as usize;
        assert_eq!(b.len(), root + CLUSTER as usize);
        assert!(b[root..].iter().all(|&x| x == 0xFF));
    }

    #[test]
    fn creates_a_sparse_image_and_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("ferrox-hdd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hdd.img");
        let _ = std::fs::remove_file(&path);
        create_image(&path, MIN_SIZE).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), MIN_SIZE);
        let parts = inspect(&path).unwrap();
        assert!(parts.iter().all(|&(_, ok)| ok), "{parts:?}");
        assert!(create_image(&path, MIN_SIZE).is_err());
        assert!(create_image(&dir.join("small.img"), 1024).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
