// test_image.rs
//! Builds FAT images for tests, and a small sample volume.

use super::*;
use crate::block::memory::MemoryDisk;
use alloc::string::ToString;
use alloc::vec;

/// Builds FAT images for tests: a minimal formatter with directories, long names and
/// optionally scattered clusters.
pub(super) struct Image {
    pub(super) disk: MemoryDisk,
    pub(super) fat_type: FatType,
    pub(super) start: u64,
    pub(super) reserved: u64,
    pub(super) fat_size: u64,
    pub(super) root_sectors: u64,
    pub(super) next_cluster: u32,
    /// Leave a gap after each cluster, so chains aren't contiguous.
    pub(super) scatter: bool,
    pub(super) fat: Vec<u32>,
    /// Directory contents by first cluster (0 = the FAT16 root).
    pub(super) dirs: Vec<(u32, Vec<u8>)>,
}

impl Image {
    pub(super) fn new(fat_type: FatType, start: u64) -> Self {
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

    pub(super) fn root(&self) -> u32 {
        self.dirs[0].0
    }

    pub(super) fn end_marker(&self) -> u32 {
        if self.fat_type == FatType::Fat16 { 0xFFFF } else { 0x0FFF_FFFF }
    }

    /// Allocates `count` clusters as one chain and returns the first.
    pub(super) fn alloc_chain(&mut self, count: usize) -> u32 {
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

    pub(super) fn cluster_lba(&self, cluster: u32) -> u64 {
        self.start + self.reserved + 2 * self.fat_size + self.root_sectors + (cluster as u64 - 2)
    }

    pub(super) fn write_chain(&mut self, first: u32, data: &[u8]) {
        let mut cluster = first;
        for chunk in data.chunks(512) {
            self.disk.write(self.cluster_lba(cluster), 0, chunk);
            cluster = self.fat[cluster as usize];
        }
    }

    /// Adds a directory entry (with long name entries when needed) to directory `dir`.
    pub(super) fn add_entry(&mut self, dir: u32, name: &str, attr: u8, cluster: u32, size: u32) {
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
                for (unit, at) in chars.iter().zip(LFN_OFFSETS) {
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

    pub(super) fn add_file(&mut self, dir: u32, name: &str, data: &[u8]) -> u32 {
        let cluster = if data.is_empty() { 0 } else { self.alloc_chain(data.len().div_ceil(512)) };
        if !data.is_empty() {
            self.write_chain(cluster, data);
        }
        self.add_entry(dir, name, 0x20, cluster, data.len() as u32);
        cluster
    }

    pub(super) fn add_dir(&mut self, parent: u32, name: &str) -> u32 {
        let cluster = self.alloc_chain(4);
        self.add_entry(parent, name, ATTR_DIRECTORY, cluster, 0);
        self.dirs.push((cluster, Vec::new()));
        let parent_ref = if parent == self.root() && self.fat_type == FatType::Fat16 { 0 } else { parent };
        self.add_entry(cluster, ".", ATTR_DIRECTORY, cluster, 0);
        self.add_entry(cluster, "..", ATTR_DIRECTORY, parent_ref, 0);
        cluster
    }

    /// Writes the FATs and directories and returns the disk.
    pub(super) fn finish(mut self) -> MemoryDisk {
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

pub(super) fn names(entries: &[DirEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.name.as_str()).collect()
}

/// A small tree on either FAT type: files of several sizes, a long name, a subdirectory.
pub(super) fn sample(fat_type: FatType, scatter: bool) -> (Fat<MemoryDisk>, Vec<u8>) {
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
