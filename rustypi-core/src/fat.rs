// fat.rs
//! Read-only FAT16 and FAT32 filesystem.
//!
//! A FAT volume is: a boot sector describing the layout, the file allocation table (a linked
//! list of clusters per file, one entry per cluster), the root directory (a fixed region on
//! FAT16, a cluster chain on FAT32), and the data area of equally sized clusters. Directories
//! are arrays of 32-byte entries; long names are stored in extra entries before the 8.3 one.
//!
//! Every chain is checked as it is followed, so a corrupt volume gives an error rather than a
//! hang or garbage.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use crate::block::{Block, BlockDevice, Lba, BLOCK_SIZE};

const DIR_ENTRY_SIZE: usize = 32;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LONG_NAME: u8 = 0x0F;
const ENTRY_END: u8 = 0x00;
const ENTRY_DELETED: u8 = 0xE5;
/// An 8.3 name really starting with 0xE5 is stored as 0x05.
const ENTRY_KANJI_E5: u8 = 0x05;
const LFN_LAST: u8 = 0x40;
const LFN_CHARS: usize = 13;
/// Case flags in the reserved byte of 8.3 entries (set by Windows NT and later).
const CASE_LOWER_BASE: u8 = 0x08;
const CASE_LOWER_EXT: u8 = 0x10;

/// A cluster number. Data clusters start at 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cluster(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FatType {
    Fat16,
    Fat32,
}

#[derive(Debug)]
pub enum FatError<E> {
    Device(E),
    /// The boot sector doesn't describe a FAT volume.
    NotFat(&'static str),
    UnsupportedSectorSize(u16),
    /// Too few clusters for FAT16; FAT12 isn't supported.
    Fat12,
    /// A chain points at a cluster outside the volume, or marked bad.
    BadCluster(u32),
    /// A chain goes on for longer than the volume has clusters.
    ChainLoop,
    /// A chain ends before the file's recorded size.
    Truncated,
    NotFound,
    NotADirectory,
    NotAFile,
}

impl<E> From<E> for FatError<E> {
    fn from(error: E) -> Self {
        FatError::Device(error)
    }
}

impl<E: fmt::Display> fmt::Display for FatError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FatError::Device(error) => write!(f, "device: {}", error),
            FatError::NotFat(reason) => write!(f, "not a FAT volume: {}", reason),
            FatError::UnsupportedSectorSize(size) => write!(f, "{}-byte sectors not supported", size),
            FatError::Fat12 => write!(f, "FAT12 not supported"),
            FatError::BadCluster(cluster) => write!(f, "bad cluster {}", cluster),
            FatError::ChainLoop => write!(f, "cluster chain loops"),
            FatError::Truncated => write!(f, "file shorter than its size"),
            FatError::NotFound => write!(f, "not found"),
            FatError::NotADirectory => write!(f, "not a directory"),
            FatError::NotAFile => write!(f, "not a file"),
        }
    }
}

type Result<T, E> = core::result::Result<T, FatError<E>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    /// Bytes; always 0 for directories.
    pub size: u32,
    first_cluster: Cluster,
}

impl DirEntry {
    pub fn first_cluster(&self) -> Cluster {
        self.first_cluster
    }
}

#[derive(Clone, Copy, Debug)]
enum RootDir {
    /// FAT16: a fixed run of sectors after the FATs.
    Fixed { start: u64, sectors: u64 },
    /// FAT32: an ordinary cluster chain.
    Chain(Cluster),
}

#[derive(Clone, Copy, Debug)]
enum Dir {
    Root,
    Chain(Cluster),
}

pub struct Fat<D: BlockDevice> {
    device: D,
    /// First block of the volume; all sector numbers below are relative to it.
    start: Lba,
    fat_type: FatType,
    sectors_per_cluster: u64,
    fat_start: u64,
    root: RootDir,
    data_start: u64,
    cluster_count: u32,
    label: String,
    /// The last FAT sector read, since chains mostly stay within one.
    fat_cache: Option<(u64, Block)>,
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

impl<D: BlockDevice> Fat<D> {
    /// Opens the volume whose boot sector is at `start`.
    pub fn mount(mut device: D, start: Lba) -> Result<Self, D::Error> {
        let mut boot = [0; BLOCK_SIZE];
        device.read_block(start, &mut boot)?;
        if boot[510] != 0x55 || boot[511] != 0xAA {
            return Err(FatError::NotFat("no boot signature"));
        }
        let bytes_per_sector = u16_at(&boot, 11);
        if bytes_per_sector != BLOCK_SIZE as u16 {
            return Err(FatError::UnsupportedSectorSize(bytes_per_sector));
        }
        let sectors_per_cluster = boot[13] as u64;
        if !(sectors_per_cluster as u8).is_power_of_two() {
            return Err(FatError::NotFat("cluster size is not a power of two"));
        }
        let reserved = u16_at(&boot, 14) as u64;
        let fat_count = boot[16] as u64;
        let root_entries = u16_at(&boot, 17) as u64;
        let total = match u16_at(&boot, 19) {
            0 => u32_at(&boot, 32) as u64,
            n => n as u64,
        };
        let fat_size = match u16_at(&boot, 22) {
            0 => u32_at(&boot, 36) as u64,
            n => n as u64,
        };
        if reserved == 0 || fat_count == 0 || fat_size == 0 {
            return Err(FatError::NotFat("empty reserved area or FAT"));
        }

        let root_sectors = (root_entries * DIR_ENTRY_SIZE as u64).div_ceil(BLOCK_SIZE as u64);
        let root_start = reserved + fat_count * fat_size;
        let data_start = root_start + root_sectors;
        if data_start >= total {
            return Err(FatError::NotFat("no room for data"));
        }
        let cluster_count = ((total - data_start) / sectors_per_cluster).min(0x0FFF_FFF5) as u32;

        // The cluster count alone decides the FAT type.
        let (fat_type, root, label_at) = if cluster_count < 4085 {
            return Err(FatError::Fat12);
        } else if cluster_count < 65525 {
            (FatType::Fat16, RootDir::Fixed { start: root_start, sectors: root_sectors }, 43)
        } else {
            (FatType::Fat32, RootDir::Chain(Cluster(u32_at(&boot, 44))), 71)
        };
        let label = String::from_utf8_lossy(&boot[label_at..label_at + 11]).trim_end().into();

        Ok(Fat {
            device,
            start,
            fat_type,
            sectors_per_cluster,
            fat_start: reserved,
            root,
            data_start,
            cluster_count,
            label,
            fat_cache: None,
        })
    }

    pub fn fat_type(&self) -> FatType {
        self.fat_type
    }

    /// The volume label from the boot sector (may be `NO NAME`).
    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn cluster_size(&self) -> usize {
        self.sectors_per_cluster as usize * BLOCK_SIZE
    }

    pub fn cluster_count(&self) -> u32 {
        self.cluster_count
    }

    pub fn into_device(self) -> D {
        self.device
    }

    /// Lists a directory. `path` is absolute, `/`-separated and matched case-insensitively,
    /// like `/overlays`. `.` and `..` are left out.
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, D::Error> {
        let dir = self.find_dir(path)?;
        self.entries(dir)
    }

    /// Reads a whole file.
    pub fn read_file(&mut self, path: &str) -> Result<Vec<u8>, D::Error> {
        let entry = self.metadata(path)?;
        if entry.kind != EntryKind::File {
            return Err(FatError::NotAFile);
        }
        self.read_chain(entry.first_cluster, Some(entry.size as usize))
    }

    /// Looks up a file or directory. The root itself has no entry, so `/` is `NotFound`.
    pub fn metadata(&mut self, path: &str) -> Result<DirEntry, D::Error> {
        let (parent, name) = match path.trim_end_matches('/').rsplit_once('/') {
            Some((parent, name)) if !name.is_empty() => (parent, name),
            _ if !path.trim_matches('/').is_empty() => ("", path.trim_matches('/')),
            _ => return Err(FatError::NotFound),
        };
        let dir = self.find_dir(parent)?;
        self.find_in(dir, name)
    }

    fn find_dir(&mut self, path: &str) -> Result<Dir, D::Error> {
        let mut dir = Dir::Root;
        for name in path.split('/').filter(|part| !part.is_empty()) {
            let entry = self.find_in(dir, name)?;
            if entry.kind != EntryKind::Directory {
                return Err(FatError::NotADirectory);
            }
            // A ".." entry pointing at the root uses cluster 0.
            dir = if entry.first_cluster.0 == 0 { Dir::Root } else { Dir::Chain(entry.first_cluster) };
        }
        Ok(dir)
    }

    fn find_in(&mut self, dir: Dir, name: &str) -> Result<DirEntry, D::Error> {
        self.entries(dir)?
            .into_iter()
            .find(|entry| names_match(&entry.name, name))
            .ok_or(FatError::NotFound)
    }

    fn entries(&mut self, dir: Dir) -> Result<Vec<DirEntry>, D::Error> {
        let bytes = match (dir, self.root) {
            (Dir::Root, RootDir::Fixed { start, sectors }) => self.read_sectors(start, sectors)?,
            (Dir::Root, RootDir::Chain(cluster)) | (Dir::Chain(cluster), _) => {
                self.read_chain(cluster, None)?
            }
        };
        Ok(parse_entries(&bytes, self.fat_type))
    }

    fn read_sectors(&mut self, start: u64, count: u64) -> Result<Vec<u8>, D::Error> {
        let mut bytes = Vec::new();
        self.read_sectors_into(start, count, &mut bytes)?;
        Ok(bytes)
    }

    /// Appends `count` sectors from `start` to `bytes`, in one read.
    fn read_sectors_into(&mut self, start: u64, count: u64, bytes: &mut Vec<u8>) -> Result<(), D::Error> {
        let at = bytes.len();
        bytes.resize(at + count as usize * BLOCK_SIZE, 0);
        let (blocks, _) = bytes[at..].as_chunks_mut::<BLOCK_SIZE>();
        self.device.read_blocks(self.start.offset(start), blocks)?;
        Ok(())
    }

    /// Reads the chain starting at `first`: `limit` bytes of it, or all of it. Runs of
    /// consecutive clusters (the usual layout of a file) are read in one go.
    fn read_chain(&mut self, first: Cluster, limit: Option<usize>) -> Result<Vec<u8>, D::Error> {
        let mut bytes = Vec::new();
        if limit == Some(0) {
            return Ok(bytes);
        }
        let cluster_bytes = self.cluster_size();
        let max_run = (MAX_RUN_SECTORS / self.sectors_per_cluster).max(1);
        let covered = |bytes: usize| limit.is_some_and(|limit| bytes >= limit);
        let mut cluster = Some(first);
        let mut steps = 0u32;
        while let Some(start) = cluster {
            if covered(bytes.len()) {
                break;
            }
            // Extend the run while the chain goes on to the very next cluster.
            let mut run = 0u64;
            let mut current = start;
            loop {
                self.check(current.0)?;
                steps += 1;
                if steps > self.cluster_count {
                    return Err(FatError::ChainLoop);
                }
                run += 1;
                cluster = self.next(current)?;
                match cluster {
                    Some(next)
                        if next.0 == current.0 + 1
                            && run < max_run
                            && !covered(bytes.len() + run as usize * cluster_bytes) =>
                    {
                        current = next;
                    }
                    _ => break,
                }
            }
            let sector = self.data_start + (start.0 as u64 - 2) * self.sectors_per_cluster;
            self.read_sectors_into(sector, run * self.sectors_per_cluster, &mut bytes)?;
        }
        if let Some(limit) = limit {
            if bytes.len() < limit {
                return Err(FatError::Truncated);
            }
            bytes.truncate(limit);
        }
        Ok(bytes)
    }

    fn check(&self, cluster: u32) -> Result<(), D::Error> {
        if cluster < 2 || cluster >= self.cluster_count + 2 {
            return Err(FatError::BadCluster(cluster));
        }
        Ok(())
    }

    /// The cluster after `cluster` in its chain, or `None` at the end.
    fn next(&mut self, cluster: Cluster) -> Result<Option<Cluster>, D::Error> {
        let (width, end, bad, mask) = match self.fat_type {
            FatType::Fat16 => (2, 0xFFF8, 0xFFF7, 0xFFFF),
            FatType::Fat32 => (4, 0x0FFF_FFF8, 0x0FFF_FFF7, 0x0FFF_FFFF),
        };
        let offset = cluster.0 as usize * width;
        let sector = self.fat_start + (offset / BLOCK_SIZE) as u64;
        let at = offset % BLOCK_SIZE;
        let block = match self.fat_cache {
            Some((cached, ref block)) if cached == sector => block,
            _ => {
                let mut block = [0; BLOCK_SIZE];
                self.device.read_block(self.start.offset(sector), &mut block)?;
                &self.fat_cache.insert((sector, block)).1
            }
        };
        let value = if width == 2 { u16_at(block, at) as u32 } else { u32_at(block, at) } & mask;
        match value {
            v if v >= end => Ok(None),
            v if v == bad => Err(FatError::BadCluster(cluster.0)),
            v => Ok(Some(Cluster(v))),
        }
    }
}

/// Most sectors read in one go: a run of clusters longer than this is read in pieces.
const MAX_RUN_SECTORS: u64 = 256;

fn names_match(a: &str, b: &str) -> bool {
    a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
}

/// Checksum of an 8.3 name, stored in its long name entries to tie them together.
fn short_name_checksum(name: &[u8]) -> u8 {
    name.iter().fold(0u8, |sum, &b| sum.rotate_right(1).wrapping_add(b))
}

fn short_name(entry: &[u8]) -> String {
    let case = entry[12];
    let part = |bytes: &[u8], lower: bool| {
        let text: String = bytes.iter().map(|&b| b as char).collect::<String>().trim_end().into();
        if lower { text.to_lowercase() } else { text }
    };
    let mut base = part(&entry[0..8], case & CASE_LOWER_BASE != 0);
    if entry[0] == ENTRY_KANJI_E5 {
        base.replace_range(0..1, "\u{E5}");
    }
    let ext = part(&entry[8..11], case & CASE_LOWER_EXT != 0);
    if ext.is_empty() { base } else { base + "." + &ext }
}

/// Collects the pieces of a long name, which come before their 8.3 entry, last piece first.
struct LongName {
    units: Vec<u16>,
    checksum: u8,
    /// The next sequence number expected (counting down to 1), or 0 if none is in progress.
    expected: u8,
}

impl LongName {
    fn reset(&mut self) {
        self.units.clear();
        self.expected = 0;
    }

    fn add(&mut self, entry: &[u8]) {
        let seq = entry[0] & 0x1F;
        if entry[0] & LFN_LAST != 0 {
            self.units = alloc::vec![0xFFFF; seq as usize * LFN_CHARS];
            self.checksum = entry[13];
            self.expected = seq;
        }
        if seq == 0 || seq != self.expected || entry[13] != self.checksum {
            self.reset();
            return;
        }
        let chars = (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2));
        let base = (seq as usize - 1) * LFN_CHARS;
        for (i, at) in chars.enumerate() {
            self.units[base + i] = u16_at(entry, at);
        }
        self.expected -= 1;
    }

    /// The finished name, if a complete one matching `short` was collected.
    fn take(&mut self, short: &[u8]) -> Option<String> {
        let complete = self.expected == 0 && !self.units.is_empty();
        let name = (complete && self.checksum == short_name_checksum(short)).then(|| {
            let len = self.units.iter().position(|&u| u == 0 || u == 0xFFFF).unwrap_or(self.units.len());
            char::decode_utf16(self.units[..len].iter().copied())
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        });
        self.reset();
        name
    }
}

fn parse_entries(bytes: &[u8], fat_type: FatType) -> Vec<DirEntry> {
    let mut entries = Vec::new();
    let mut long = LongName { units: Vec::new(), checksum: 0, expected: 0 };
    for entry in bytes.chunks_exact(DIR_ENTRY_SIZE) {
        let attr = entry[11];
        match entry[0] {
            ENTRY_END => break,
            ENTRY_DELETED => long.reset(),
            _ if attr & 0x3F == ATTR_LONG_NAME => long.add(entry),
            _ if attr & ATTR_VOLUME_ID != 0 => long.reset(),
            _ => {
                let name = long.take(&entry[0..11]).unwrap_or_else(|| short_name(entry));
                if name == "." || name == ".." {
                    continue;
                }
                let high = if fat_type == FatType::Fat32 { u16_at(entry, 20) as u32 } else { 0 };
                let directory = attr & ATTR_DIRECTORY != 0;
                entries.push(DirEntry {
                    name,
                    kind: if directory { EntryKind::Directory } else { EntryKind::File },
                    size: if directory { 0 } else { u32_at(entry, 28) },
                    first_cluster: Cluster(high << 16 | u16_at(entry, 26) as u32),
                });
            }
        }
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::memory::MemoryDisk;
    use alloc::string::ToString;
    use alloc::vec;

    /// Builds FAT images for tests: a minimal formatter with directories, long names and
    /// optionally scattered clusters.
    struct Image {
        disk: MemoryDisk,
        fat_type: FatType,
        start: u64,
        reserved: u64,
        fat_size: u64,
        root_sectors: u64,
        next_cluster: u32,
        /// Leave a gap after each cluster, so chains aren't contiguous.
        scatter: bool,
        fat: Vec<u32>,
        /// Directory contents by first cluster (0 = the FAT16 root).
        dirs: Vec<(u32, Vec<u8>)>,
    }

    impl Image {
        fn new(fat_type: FatType, start: u64) -> Self {
            let (clusters, reserved, root_entries): (u64, u64, u64) = match fat_type {
                FatType::Fat16 => (5_000, 4, 512),
                FatType::Fat32 => (70_000, 32, 0),
            };
            let width = if fat_type == FatType::Fat16 { 2 } else { 4 };
            let fat_size = ((clusters + 2) * width).div_ceil(512);
            let root_sectors = root_entries * 32 / 512;
            let mut image = Image {
                disk: MemoryDisk::default(),
                fat_type,
                start,
                reserved,
                fat_size,
                root_sectors,
                next_cluster: 2,
                scatter: false,
                fat: vec![0; clusters as usize + 2],
                dirs: Vec::new(),
            };
            let total = reserved + 2 * fat_size + root_sectors + clusters;
            let mut boot = [0u8; 512];
            boot[0] = 0xEB;
            boot[11..13].copy_from_slice(&512u16.to_le_bytes());
            boot[13] = 1;
            boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
            boot[16] = 2;
            boot[17..19].copy_from_slice(&(root_entries as u16).to_le_bytes());
            boot[32..36].copy_from_slice(&(total as u32).to_le_bytes());
            match fat_type {
                FatType::Fat16 => {
                    boot[22..24].copy_from_slice(&(fat_size as u16).to_le_bytes());
                    boot[43..54].copy_from_slice(b"TESTVOL16  ");
                    image.dirs.push((0, Vec::new()));
                }
                FatType::Fat32 => {
                    boot[36..40].copy_from_slice(&(fat_size as u32).to_le_bytes());
                    boot[71..82].copy_from_slice(b"TESTVOL32  ");
                    let root = image.alloc_chain(1);
                    boot[44..48].copy_from_slice(&root.to_le_bytes());
                    image.dirs.push((root, Vec::new()));
                }
            }
            boot[510] = 0x55;
            boot[511] = 0xAA;
            image.disk.write(start, 0, &boot);
            image
        }

        fn root(&self) -> u32 {
            self.dirs[0].0
        }

        fn end_marker(&self) -> u32 {
            if self.fat_type == FatType::Fat16 { 0xFFFF } else { 0x0FFF_FFFF }
        }

        /// Allocates `count` clusters as one chain and returns the first.
        fn alloc_chain(&mut self, count: usize) -> u32 {
            let clusters: Vec<u32> = (0..count)
                .map(|_| {
                    let c = self.next_cluster;
                    self.next_cluster += if self.scatter { 2 } else { 1 };
                    c
                })
                .collect();
            for pair in clusters.windows(2) {
                self.fat[pair[0] as usize] = pair[1];
            }
            let end = self.end_marker();
            self.fat[*clusters.last().unwrap() as usize] = end;
            clusters[0]
        }

        fn cluster_lba(&self, cluster: u32) -> u64 {
            self.start + self.reserved + 2 * self.fat_size + self.root_sectors + (cluster as u64 - 2)
        }

        fn write_chain(&mut self, first: u32, data: &[u8]) {
            let mut cluster = first;
            for chunk in data.chunks(512) {
                self.disk.write(self.cluster_lba(cluster), 0, chunk);
                cluster = self.fat[cluster as usize];
            }
        }

        /// Adds a directory entry (with long name entries when needed) to directory `dir`.
        fn add_entry(&mut self, dir: u32, name: &str, attr: u8, cluster: u32, size: u32) {
            let is_short = name.len() <= 12
                && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '.')
                && name.split('.').count() <= 2;
            let mut short = [b' '; 11];
            if name == "." || name == ".." {
                short[..name.len()].copy_from_slice(name.as_bytes());
            } else if is_short {
                let (base, ext) = name.split_once('.').unwrap_or((name, ""));
                short[..base.len()].copy_from_slice(base.as_bytes());
                short[8..8 + ext.len()].copy_from_slice(ext.as_bytes());
            } else {
                let alias = b"LONGNA~1";
                short[..8].copy_from_slice(alias);
                short[8..11].copy_from_slice(&(self.dirs.len() as u32 + 100).to_string().as_bytes()[..3]);
            }
            let mut bytes = Vec::new();
            if !is_short {
                let checksum = short_name_checksum(&short);
                let mut units: Vec<u16> = name.encode_utf16().collect();
                let pieces = units.len().div_ceil(LFN_CHARS);
                if units.len() % LFN_CHARS != 0 {
                    units.push(0);
                }
                units.resize(pieces * LFN_CHARS, 0xFFFF);
                for seq in (1..=pieces).rev() {
                    let mut e = [0u8; 32];
                    e[0] = seq as u8 | if seq == pieces { LFN_LAST } else { 0 };
                    e[11] = ATTR_LONG_NAME;
                    e[13] = checksum;
                    let chars = &units[(seq - 1) * LFN_CHARS..seq * LFN_CHARS];
                    let slots = (1..11).step_by(2).chain((14..26).step_by(2)).chain((28..32).step_by(2));
                    for (unit, at) in chars.iter().zip(slots) {
                        e[at..at + 2].copy_from_slice(&unit.to_le_bytes());
                    }
                    bytes.extend_from_slice(&e);
                }
            }
            let mut e = [0u8; 32];
            e[..11].copy_from_slice(&short);
            e[11] = attr;
            e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
            e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
            e[28..32].copy_from_slice(&size.to_le_bytes());
            bytes.extend_from_slice(&e);
            self.dirs.iter_mut().find(|(c, _)| *c == dir).unwrap().1.extend(bytes);
        }

        fn add_file(&mut self, dir: u32, name: &str, data: &[u8]) -> u32 {
            let cluster = if data.is_empty() { 0 } else { self.alloc_chain(data.len().div_ceil(512)) };
            if !data.is_empty() {
                self.write_chain(cluster, data);
            }
            self.add_entry(dir, name, 0x20, cluster, data.len() as u32);
            cluster
        }

        fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
            let cluster = self.alloc_chain(4);
            self.add_entry(parent, name, ATTR_DIRECTORY, cluster, 0);
            self.dirs.push((cluster, Vec::new()));
            let parent_ref = if parent == self.root() && self.fat_type == FatType::Fat16 { 0 } else { parent };
            self.add_entry(cluster, ".", ATTR_DIRECTORY, cluster, 0);
            self.add_entry(cluster, "..", ATTR_DIRECTORY, parent_ref, 0);
            cluster
        }

        /// Writes the FATs and directories and returns the disk.
        fn finish(mut self) -> MemoryDisk {
            let width = if self.fat_type == FatType::Fat16 { 2 } else { 4 };
            let mut fat_bytes = Vec::new();
            for &entry in &self.fat {
                fat_bytes.extend_from_slice(&entry.to_le_bytes()[..width]);
            }
            for copy in 0..2 {
                let lba = self.start + self.reserved + copy * self.fat_size;
                self.disk.write(lba, 0, &fat_bytes);
            }
            for (cluster, bytes) in core::mem::take(&mut self.dirs) {
                if cluster == 0 {
                    let lba = self.start + self.reserved + 2 * self.fat_size;
                    self.disk.write(lba, 0, &bytes);
                } else {
                    self.write_chain(cluster, &bytes);
                }
            }
            self.disk
        }
    }

    fn names(entries: &[DirEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// A small tree on either FAT type: files of several sizes, a long name, a subdirectory.
    fn sample(fat_type: FatType, scatter: bool) -> (Fat<MemoryDisk>, Vec<u8>) {
        let mut image = Image::new(fat_type, 2048);
        image.scatter = scatter;
        let root = image.root();
        let big: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        image.add_file(root, "CONFIG.TXT", b"arm_64bit=1\n");
        image.add_file(root, "kernel8 image with a long name.img", &big);
        image.add_file(root, "EMPTY", b"");
        let sub = image.add_dir(root, "OVERLAYS");
        image.add_file(sub, "README", b"overlays go here\n");
        let fat = Fat::mount(image.finish(), Lba(2048)).unwrap();
        (fat, big)
    }

    #[test]
    fn mounts_and_reports_the_layout() {
        let (fat16, _) = sample(FatType::Fat16, false);
        assert_eq!((fat16.fat_type(), fat16.label()), (FatType::Fat16, "TESTVOL16"));
        let (fat32, _) = sample(FatType::Fat32, false);
        assert_eq!((fat32.fat_type(), fat32.label()), (FatType::Fat32, "TESTVOL32"));
        assert_eq!(fat32.cluster_size(), 512);
    }

    #[test]
    fn lists_directories_with_long_names() {
        for fat_type in [FatType::Fat16, FatType::Fat32] {
            let (mut fat, _) = sample(fat_type, false);
            let root = fat.read_dir("/").unwrap();
            assert_eq!(names(&root), ["CONFIG.TXT", "kernel8 image with a long name.img", "EMPTY", "OVERLAYS"]);
            assert_eq!(root[3].kind, EntryKind::Directory);
            assert_eq!(root[1].size, 5000);
            // "." and ".." are hidden; trailing slashes and case don't matter.
            assert_eq!(names(&fat.read_dir("/overlays/").unwrap()), ["README"]);
        }
    }

    #[test]
    fn reads_files_across_scattered_clusters() {
        for fat_type in [FatType::Fat16, FatType::Fat32] {
            for scatter in [false, true] {
                let (mut fat, big) = sample(fat_type, scatter);
                assert_eq!(fat.read_file("/config.txt").unwrap(), b"arm_64bit=1\n");
                assert_eq!(fat.read_file("/KERNEL8 IMAGE WITH A LONG NAME.IMG").unwrap(), big);
                assert_eq!(fat.read_file("/empty").unwrap(), b"");
                assert_eq!(fat.read_file("overlays/readme").unwrap(), b"overlays go here\n");
            }
        }
    }

    #[test]
    fn contiguous_files_are_read_in_one_command() {
        for (scatter, commands) in [(false, 1), (true, 10)] {
            let (mut fat, big) = sample(FatType::Fat32, scatter);
            let entry = fat.metadata("/kernel8 image with a long name.img").unwrap();
            let before = fat.device.commands;
            assert_eq!(fat.read_chain(entry.first_cluster(), Some(entry.size as usize)).unwrap(), big);
            assert_eq!(fat.device.commands - before, commands, "scatter {}", scatter);
        }
    }

    #[test]
    fn caches_fat_sectors() {
        let (mut fat, _) = sample(FatType::Fat32, false);
        fat.read_file("/kernel8 image with a long name.img").unwrap();
        let before = fat.device.reads;
        fat.read_file("/kernel8 image with a long name.img").unwrap();
        // 10 data sectors, plus the root directory's cluster, and no extra FAT reads.
        assert_eq!(fat.device.reads - before, 10 + 1);
    }

    #[test]
    fn path_errors() {
        let (mut fat, _) = sample(FatType::Fat16, false);
        assert!(matches!(fat.read_file("/missing"), Err(FatError::NotFound)));
        assert!(matches!(fat.read_file("/overlays"), Err(FatError::NotAFile)));
        assert!(matches!(fat.read_dir("/config.txt"), Err(FatError::NotADirectory)));
        assert!(matches!(fat.read_dir("/config.txt/x"), Err(FatError::NotADirectory)));
        assert!(matches!(fat.metadata("/"), Err(FatError::NotFound)));
        assert_eq!(fat.metadata("/overlays/readme").unwrap().size, 17);
    }

    #[test]
    fn corrupt_chains_are_errors() {
        let mut image = Image::new(FatType::Fat32, 0);
        let root = image.root();
        // Directories have no size to stop at, so a loop there would read forever.
        let dir = image.add_dir(root, "LOOP");
        let second = image.add_file(root, "BAD", &[2; 1024]);
        let third = image.add_file(root, "SHORT", &[3; 1024]);
        image.fat[dir as usize + 3] = dir; // last cluster points back to the first
        image.fat[second as usize] = 0x0FFF_FFF7; // marked bad
        image.fat[third as usize] = 0x0FFF_FFFF; // ends after one of its two clusters
        let mut fat = Fat::mount(image.finish(), Lba(0)).unwrap();
        assert!(matches!(fat.read_dir("/loop"), Err(FatError::ChainLoop)));
        assert!(matches!(fat.read_file("/bad"), Err(FatError::BadCluster(_))));
        assert!(matches!(fat.read_file("/short"), Err(FatError::Truncated)));
    }

    #[test]
    fn rejects_non_fat_boot_sectors() {
        let mut disk = MemoryDisk::default();
        assert!(matches!(Fat::mount(&mut disk, Lba(0)), Err(FatError::NotFat(_))));
        let mut boot = [0u8; 512];
        boot[11..13].copy_from_slice(&4096u16.to_le_bytes());
        boot[510] = 0x55;
        boot[511] = 0xAA;
        disk.write(0, 0, &boot);
        assert!(matches!(Fat::mount(&mut disk, Lba(0)), Err(FatError::UnsupportedSectorSize(4096))));
    }

    #[test]
    fn short_names_honour_case_flags_and_the_e5_escape() {
        let mut entry = [b' '; 32];
        entry[..11].copy_from_slice(b"README  TXT");
        entry[12] = CASE_LOWER_BASE;
        assert_eq!(short_name(&entry), "readme.TXT");
        entry[0] = ENTRY_KANJI_E5;
        entry[12] = 0;
        assert_eq!(short_name(&entry), "\u{E5}EADME.TXT");
    }

    #[test]
    fn orphaned_long_name_pieces_fall_back_to_the_short_name() {
        let mut image = Image::new(FatType::Fat16, 0);
        let root = image.root();
        image.add_file(root, "a long file name.txt", b"x");
        let mut disk = image.finish();
        // Corrupt the long name's checksum, as if the 8.3 entry had been renamed by an
        // old system that doesn't know about long names.
        let root_lba = 4 + 2 * 20;
        let mut block = [0; BLOCK_SIZE];
        disk.read_block(Lba(root_lba), &mut block).unwrap();
        block[13] ^= 0xFF;
        block[32 + 13] ^= 0xFF;
        disk.write(root_lba, 0, &block);
        let mut fat = Fat::mount(disk, Lba(0)).unwrap();
        assert_eq!(names(&fat.read_dir("/").unwrap()), ["LONGNA~1.101"]);
    }
}
