// write.rs
//! Writing FAT volumes: creating, replacing and removing files, and making and removing
//! directories.
//!
//! Each change is ordered so that stopping part way (a power cut) leaves at worst clusters
//! marked used that no file owns, which `fsck` gives back, and never a file pointing at
//! clusters that aren't its own:
//!
//! 1. new data goes into free clusters,
//! 2. the FAT links them into a chain, in every copy of the FAT,
//! 3. a directory entry points at the chain,
//! 4. only then are old clusters freed.
//!
//! Removing marks the entry deleted first, then frees its clusters. Entries get the time
//! `set_time` last gave (a fixed 2026-01-01 until then, for want of a clock). On FAT32, the free clusters are counted before the first change,
//! and the FSInfo sector's count and next-free hint are kept up after every one.

use alloc::vec;
use alloc::vec::Vec;
use super::*;
use crate::block::WritableBlockDevice;

/// The date and time entries get until `set_time`: 2026-01-01 00:00.
pub(super) const DEFAULT_STAMP: (u16, u16) = ((2026 - 1980) << 9 | 1 << 5 | 1, 0);
const ATTR_ARCHIVE: u8 = 0x20;
/// Longest name, in UTF-16 units.
const MAX_NAME: usize = 255;
/// Characters FAT names may not contain, besides control characters.
const FORBIDDEN: &str = "\"*/:<>?\\|";
/// Characters an 8.3 name may contain, besides letters and digits.
const SHORT_EXTRA: &str = "!#$%&'()-@^_`{}~";

const FSINFO_LEAD: u32 = 0x4161_5252;
const FSINFO_STRUCT: u32 = 0x6141_7272;


/// A whole directory in memory, with the sectors it lives in, to change and write back.
struct DirImage {
    sectors: Vec<u64>,
    bytes: Vec<u8>,
}

impl DirImage {
    fn entry(&mut self, index: usize) -> &mut [u8] {
        &mut self.bytes[index * DIR_ENTRY_SIZE..(index + 1) * DIR_ENTRY_SIZE]
    }

    fn slots(&self) -> usize {
        self.bytes.len() / DIR_ENTRY_SIZE
    }

    /// The 8.3 names in use, which a new alias mustn't repeat.
    fn short_names(&self) -> Vec<[u8; 11]> {
        self.bytes
            .chunks_exact(DIR_ENTRY_SIZE)
            .take_while(|entry| entry[0] != ENTRY_END)
            .filter(|entry| entry[0] != ENTRY_DELETED && entry[11] & 0x3F != ATTR_LONG_NAME)
            .map(|entry| entry[..11].try_into().unwrap())
            .collect()
    }

    /// The first run of `count` free slots: deleted ones, or any after the end marker.
    fn free_run(&self, count: usize) -> Option<usize> {
        let mut run = 0;
        for (index, entry) in self.bytes.chunks_exact(DIR_ENTRY_SIZE).enumerate() {
            if entry[0] == ENTRY_END {
                // Everything from here on is free.
                return (index - run + count <= self.slots()).then_some(index - run);
            }
            run = if entry[0] == ENTRY_DELETED { run + 1 } else { 0 };
            if run == count {
                return Some(index + 1 - count);
            }
        }
        None
    }
}

/// Checks `name` is one FAT can store.
pub fn check_name<E>(name: &str) -> Result<(), E> {
    let bad = name.is_empty()
        || name.encode_utf16().count() > MAX_NAME
        || name == "."
        || name == ".."
        || name.ends_with(' ')
        || name.ends_with('.')
        || name.chars().any(|c| (c as u32) < 0x20 || FORBIDDEN.contains(c));
    if bad { Err(FatError::InvalidName) } else { Ok(()) }
}

/// Splits a path into its parent directory and final name.
fn split_path(path: &str) -> (&str, &str) {
    let path = path.trim_end_matches('/');
    path.rsplit_once('/').unwrap_or(("", path))
}

fn is_short_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || SHORT_EXTRA.contains(c)
}

/// `name` as an 8.3 name with its case flags, if it is one: up to 8 characters, maybe a dot
/// and up to 3 more, from the characters 8.3 names allow, each part in a single case.
fn exact_short_name(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = name.split_once('.').unwrap_or((name, ""));
    if base.is_empty() || base.len() > 8 || ext.len() > 3 || ext.contains('.') {
        return None;
    }
    let part_case = |part: &str| {
        let lower = part.chars().any(|c| c.is_ascii_lowercase());
        let upper = part.chars().any(|c| c.is_ascii_uppercase());
        (part.chars().all(is_short_char) && !(lower && upper)).then_some(lower)
    };
    let (base_lower, ext_lower) = (part_case(base)?, part_case(ext)?);
    let mut short = [b' '; 11];
    short[..base.len()].copy_from_slice(base.to_ascii_uppercase().as_bytes());
    short[8..8 + ext.len()].copy_from_slice(ext.to_ascii_uppercase().as_bytes());
    let flags = if base_lower { CASE_LOWER_BASE } else { 0 } | if ext_lower { CASE_LOWER_EXT } else { 0 };
    Some((short, flags))
}

/// A unique 8.3 alias for a long name, like `LONGFI~1TXT`: the start of the name and its
/// extension, uppercased with characters 8.3 names can't have left out or replaced.
fn alias(name: &str, taken: &[[u8; 11]]) -> [u8; 11] {
    let clean = |part: &str, max: usize| -> Vec<u8> {
        part.chars()
            .filter(|&c| c != ' ' && c != '.')
            .map(|c| if is_short_char(c) { c.to_ascii_uppercase() as u8 } else { b'_' })
            .take(max)
            .collect()
    };
    let trimmed = name.trim_start_matches('.');
    let (base, ext) = match trimmed.rsplit_once('.') {
        Some((base, ext)) => (base, ext),
        None => (trimmed, ""),
    };
    let mut basis = clean(base, 6);
    if basis.is_empty() {
        basis.push(b'_');
    }
    let ext = clean(ext, 3);
    let mut short = [b' '; 11];
    short[8..8 + ext.len()].copy_from_slice(&ext);
    for n in 1u32.. {
        let tail = alloc::format!("~{n}");
        let keep = basis.len().min(8 - tail.len());
        short[..8].fill(b' ');
        short[..keep].copy_from_slice(&basis[..keep]);
        short[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        if !taken.contains(&short) {
            break;
        }
    }
    short
}

/// The 8.3 entry for a file or directory, created and modified at `stamp` (FAT date, time).
fn short_entry(short: &[u8; 11], case: u8, attr: u8, cluster: u32, size: u32, stamp: (u16, u16)) -> [u8; DIR_ENTRY_SIZE] {
    let (date, time) = stamp;
    let mut entry = [0; DIR_ENTRY_SIZE];
    entry[..11].copy_from_slice(short);
    entry[11] = attr;
    entry[12] = case;
    entry[14..16].copy_from_slice(&time.to_le_bytes());
    entry[16..18].copy_from_slice(&date.to_le_bytes());
    entry[18..20].copy_from_slice(&date.to_le_bytes());
    set_modified(&mut entry, stamp);
    set_cluster_and_size(&mut entry, cluster, size);
    entry
}

fn set_modified(entry: &mut [u8], (date, time): (u16, u16)) {
    entry[22..24].copy_from_slice(&time.to_le_bytes());
    entry[24..26].copy_from_slice(&date.to_le_bytes());
}

fn set_cluster_and_size(entry: &mut [u8], cluster: u32, size: u32) {
    entry[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    entry[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
}

/// The long name entries for `name`, in the order they are stored (last piece first).
fn long_entries(name: &str, short: &[u8; 11]) -> Vec<[u8; DIR_ENTRY_SIZE]> {
    let checksum = short_name_checksum(short);
    let mut units: Vec<u16> = name.encode_utf16().collect();
    let pieces = units.len().div_ceil(LFN_CHARS);
    // A terminator if there's room, then padding.
    if units.len() % LFN_CHARS != 0 {
        units.push(0);
    }
    units.resize(pieces * LFN_CHARS, 0xFFFF);
    (1..=pieces)
        .rev()
        .map(|seq| {
            let mut entry = [0; DIR_ENTRY_SIZE];
            entry[0] = seq as u8 | if seq == pieces { LFN_LAST } else { 0 };
            entry[11] = ATTR_LONG_NAME;
            entry[13] = checksum;
            let chars = &units[(seq - 1) * LFN_CHARS..seq * LFN_CHARS];
            for (unit, at) in chars.iter().zip(LFN_OFFSETS) {
                entry[at..at + 2].copy_from_slice(&unit.to_le_bytes());
            }
            entry
        })
        .collect()
}

impl<D: WritableBlockDevice> Fat<D> {
    /// The time entries created or changed from now on get.
    pub fn set_time(&mut self, now: crate::time::DateTime) {
        self.stamp = now.fat();
    }

    /// Creates the file at `path` with `data` in it, or replaces what an existing file holds.
    /// The parent directory must exist.
    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), D::Error> {
        let result = self.write_file_inner(path, data);
        self.finish_change(result)
    }

    fn write_file_inner(&mut self, path: &str, data: &[u8]) -> Result<(), D::Error> {
        let size = u32::try_from(data.len()).map_err(|_| FatError::TooBig)?;
        let (parent, name) = split_path(path);
        check_name(name)?;
        let dir = self.find_dir(parent)?;
        let mut image = self.load_dir(dir)?;
        let existing = parse_located(&image.bytes, self.fat_type)
            .into_iter()
            .find(|located| names_match(&located.entry.name, name));
        if existing.as_ref().is_some_and(|located| located.entry.kind == EntryKind::Directory) {
            return Err(FatError::NotAFile);
        }

        self.begin_change()?;
        let first = self.write_chain(data)?;
        match existing {
            Some(located) => {
                // Point the entry at the new chain, then let the old one go.
                let slot = located.slots.end - 1;
                let entry = image.entry(slot);
                set_cluster_and_size(entry, first, size);
                set_modified(entry, self.stamp);
                self.store_dir(&image, slot..slot + 1)?;
                self.free_chain(located.entry.first_cluster.0)?;
            }
            None => {
                if let Err(error) = self.add_entry(dir, &mut image, name, ATTR_ARCHIVE, first, size) {
                    // Nothing points at the new chain; give it back.
                    self.free_chain(first)?;
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Makes an empty directory at `path`. The parent must exist.
    pub fn create_dir(&mut self, path: &str) -> Result<(), D::Error> {
        let result = self.create_dir_inner(path);
        self.finish_change(result)
    }

    fn create_dir_inner(&mut self, path: &str) -> Result<(), D::Error> {
        let (parent, name) = split_path(path);
        check_name(name)?;
        let dir = self.find_dir(parent)?;
        let mut image = self.load_dir(dir)?;
        if parse_located(&image.bytes, self.fat_type).iter().any(|located| names_match(&located.entry.name, name)) {
            return Err(FatError::AlreadyExists);
        }

        self.begin_change()?;
        let cluster = self.allocate(1)?[0];
        let mut contents = vec![0; self.cluster_size()];
        // "." is the directory itself; ".." its parent, which is 0 for the root.
        let parent_cluster = match dir {
            Dir::Root => 0,
            Dir::Chain(cluster) => cluster.0,
        };
        contents[..DIR_ENTRY_SIZE].copy_from_slice(&short_entry(b".          ", 0, ATTR_DIRECTORY, cluster, 0, self.stamp));
        contents[DIR_ENTRY_SIZE..2 * DIR_ENTRY_SIZE]
            .copy_from_slice(&short_entry(b"..         ", 0, ATTR_DIRECTORY, parent_cluster, 0, self.stamp));
        self.write_clusters(&[cluster], &contents)?;
        self.link(&[cluster])?;
        if let Err(error) = self.add_entry(dir, &mut image, name, ATTR_DIRECTORY, cluster, 0) {
            self.free_chain(cluster)?;
            return Err(error);
        }
        Ok(())
    }

    /// Removes the file, or empty directory, at `path`.
    pub fn remove(&mut self, path: &str) -> Result<(), D::Error> {
        let result = self.remove_inner(path);
        self.finish_change(result)
    }

    fn remove_inner(&mut self, path: &str) -> Result<(), D::Error> {
        let (parent, name) = split_path(path);
        let dir = self.find_dir(parent)?;
        let mut image = self.load_dir(dir)?;
        let located = parse_located(&image.bytes, self.fat_type)
            .into_iter()
            .find(|located| names_match(&located.entry.name, name))
            .ok_or(FatError::NotFound)?;
        let first = located.entry.first_cluster.0;
        if located.entry.kind == EntryKind::Directory {
            let contents = self.load_dir(Dir::Chain(Cluster(first)))?;
            if !parse_located(&contents.bytes, self.fat_type).is_empty() {
                return Err(FatError::NotEmpty);
            }
        }

        self.begin_change()?;
        for slot in located.slots.clone() {
            image.entry(slot)[0] = ENTRY_DELETED;
        }
        self.store_dir(&image, located.slots)?;
        self.free_chain(first)
    }

    /// Clusters no chain uses. Reads the whole FAT.
    pub fn free_clusters(&mut self) -> Result<u32, D::Error> {
        let mut free = 0;
        for cluster in 2..self.cluster_count + 2 {
            free += (self.fat_entry(cluster)? == 0) as u32;
        }
        Ok(free)
    }

    /// Before the first change on FAT32: counts the free clusters, so FSInfo can be kept up.
    fn begin_change(&mut self) -> Result<(), D::Error> {
        if self.fsinfo.is_some() && self.free_count.is_none() {
            self.free_count = Some(self.free_clusters()?);
        }
        Ok(())
    }

    /// After a change, successful or not: brings FSInfo up to date.
    fn finish_change(&mut self, result: Result<(), D::Error>) -> Result<(), D::Error> {
        if let (Some(sector), Some(free)) = (self.fsinfo, self.free_count) {
            let mut block = [0; BLOCK_SIZE];
            self.device.read_block(self.start.offset(sector), &mut block)?;
            if u32_at(&block, 0) == FSINFO_LEAD && u32_at(&block, 484) == FSINFO_STRUCT {
                block[488..492].copy_from_slice(&free.to_le_bytes());
                block[492..496].copy_from_slice(&self.next_free.to_le_bytes());
                self.device.write_block(self.start.offset(sector), &block)?;
            }
        }
        result
    }

    /// Puts `data` in new clusters, chained in the FAT, and returns the first (0 for none).
    fn write_chain(&mut self, data: &[u8]) -> Result<u32, D::Error> {
        if data.is_empty() {
            return Ok(0);
        }
        let clusters = self.allocate(data.len().div_ceil(self.cluster_size()))?;
        self.write_clusters(&clusters, data)?;
        self.link(&clusters)?;
        Ok(clusters[0])
    }

    /// Finds `count` free clusters. Nothing is marked yet: they stay free until `link`.
    fn allocate(&mut self, count: usize) -> Result<Vec<u32>, D::Error> {
        let mut clusters = Vec::with_capacity(count);
        let total = self.cluster_count;
        let start = self.next_free.clamp(2, total + 1);
        for i in 0..total {
            if clusters.len() == count {
                break;
            }
            let cluster = 2 + (start - 2 + i) % total;
            if self.fat_entry(cluster)? == 0 {
                clusters.push(cluster);
            }
        }
        if clusters.len() < count {
            return Err(FatError::NoSpace);
        }
        self.next_free = clusters.last().map_or(2, |&last| last + 1);
        Ok(clusters)
    }

    /// Writes `data` (zero-padded to whole clusters) into `clusters`, a run of consecutive
    /// ones at a time.
    fn write_clusters(&mut self, clusters: &[u32], data: &[u8]) -> Result<(), D::Error> {
        let cluster_size = self.cluster_size();
        let mut padded = data.to_vec();
        padded.resize(clusters.len() * cluster_size, 0);
        let (blocks, _) = padded.as_chunks::<BLOCK_SIZE>();
        let per_cluster = self.sectors_per_cluster as usize;
        let max_run = (MAX_RUN_SECTORS as usize / per_cluster).max(1);
        let mut i = 0;
        while i < clusters.len() {
            let mut run = 1;
            while i + run < clusters.len() && clusters[i + run] == clusters[i] + run as u32 && run < max_run {
                run += 1;
            }
            let sector = self.first_sector(Cluster(clusters[i]));
            self.device.write_blocks(self.start.offset(sector), &blocks[i * per_cluster..(i + run) * per_cluster])?;
            i += run;
        }
        Ok(())
    }

    /// Chains `clusters` in the FAT, in order, ending the chain after the last.
    fn link(&mut self, clusters: &[u32]) -> Result<(), D::Error> {
        let end = self.fat_type.entry_mask();
        for (i, &cluster) in clusters.iter().enumerate() {
            let next = clusters.get(i + 1).copied().unwrap_or(end);
            self.set_fat_entry(cluster, next)?;
        }
        self.flush_fat()?;
        if let Some(free) = &mut self.free_count {
            *free -= clusters.len() as u32;
        }
        Ok(())
    }

    /// Frees the chain starting at `first` (nothing for 0).
    fn free_chain(&mut self, first: u32) -> Result<(), D::Error> {
        // Cluster 0 is an empty file's: no chain.
        let chain = if first == 0 { Vec::new() } else { self.chain(first)? };
        for &cluster in &chain {
            self.set_fat_entry(cluster, 0)?;
        }
        self.flush_fat()?;
        if let Some(free) = &mut self.free_count {
            *free += chain.len() as u32;
        }
        if let Some(&lowest) = chain.iter().min() {
            self.next_free = self.next_free.min(lowest);
        }
        Ok(())
    }

    /// Changes FAT entry `cluster` in the pending sector, writing out another pending one
    /// first. FAT32's reserved top bits are kept.
    fn set_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), D::Error> {
        let (sector, at) = self.fat_position(cluster);
        if self.fat_dirty.as_ref().is_some_and(|(dirty, _)| *dirty != sector) {
            self.flush_fat()?;
        }
        if self.fat_dirty.is_none() {
            let block = match self.fat_cache.take() {
                Some((cached, block)) if cached == sector => block,
                _ => {
                    let mut block = [0; BLOCK_SIZE];
                    self.device.read_block(self.start.offset(sector), &mut block)?;
                    block
                }
            };
            self.fat_dirty = Some((sector, block));
        }
        let (_, block) = self.fat_dirty.as_mut().expect("loaded above");
        match self.fat_type {
            FatType::Fat16 => block[at..at + 2].copy_from_slice(&(value as u16).to_le_bytes()),
            FatType::Fat32 => {
                let kept = u32_at(block, at) & !self.fat_type.entry_mask();
                block[at..at + 4].copy_from_slice(&(kept | value).to_le_bytes());
            }
        }
        Ok(())
    }

    /// Writes the pending FAT sector to every copy of the FAT.
    fn flush_fat(&mut self) -> Result<(), D::Error> {
        let Some((sector, block)) = self.fat_dirty.take() else { return Ok(()) };
        for copy in 0..self.fat_count {
            self.device.write_block(self.start.offset(sector + copy * self.fat_size), &block)?;
        }
        self.fat_cache = Some((sector, block));
        Ok(())
    }

    /// Reads a whole directory, noting the sectors it is in.
    fn load_dir(&mut self, dir: Dir) -> Result<DirImage, D::Error> {
        let sectors: Vec<u64> = match (dir, self.root) {
            (Dir::Root, RootDir::Fixed { start, sectors }) => (start..start + sectors).collect(),
            (Dir::Root, RootDir::Chain(first)) | (Dir::Chain(first), _) => {
                let mut sectors = Vec::new();
                for cluster in self.chain(first.0)? {
                    let first_sector = self.first_sector(Cluster(cluster));
                    sectors.extend(first_sector..first_sector + self.sectors_per_cluster);
                }
                sectors
            }
        };
        let mut bytes = vec![0; sectors.len() * BLOCK_SIZE];
        for (block, &sector) in bytes.as_chunks_mut::<BLOCK_SIZE>().0.iter_mut().zip(&sectors) {
            self.device.read_block(self.start.offset(sector), block)?;
        }
        Ok(DirImage { sectors, bytes })
    }

    /// The clusters of the chain starting at `first`, checked.
    fn chain(&mut self, first: u32) -> Result<Vec<u32>, D::Error> {
        let mut chain = Vec::new();
        let mut cluster = Some(Cluster(first));
        while let Some(current) = cluster {
            self.check(current.0)?;
            if chain.len() as u32 >= self.cluster_count {
                return Err(FatError::ChainLoop);
            }
            chain.push(current.0);
            cluster = self.next(current)?;
        }
        Ok(chain)
    }

    /// Writes back the sectors holding slots `slots` of a directory.
    fn store_dir(&mut self, image: &DirImage, slots: Range<usize>) -> Result<(), D::Error> {
        let per_sector = BLOCK_SIZE / DIR_ENTRY_SIZE;
        for index in slots.start / per_sector..slots.end.div_ceil(per_sector) {
            let block: &Block = image.bytes[index * BLOCK_SIZE..(index + 1) * BLOCK_SIZE].try_into().unwrap();
            self.device.write_block(self.start.offset(image.sectors[index]), block)?;
        }
        Ok(())
    }

    /// Adds entries for `name` to a directory, growing it by a cluster if it is full (or
    /// failing, for FAT16's fixed root).
    fn add_entry(&mut self, dir: Dir, image: &mut DirImage, name: &str, attr: u8, cluster: u32, size: u32) -> Result<(), D::Error> {
        let mut entries = Vec::new();
        match exact_short_name(name) {
            Some((short, case)) => entries.push(short_entry(&short, case, attr, cluster, size, self.stamp)),
            None => {
                let short = alias(name, &image.short_names());
                entries.extend(long_entries(name, &short));
                entries.push(short_entry(&short, 0, attr, cluster, size, self.stamp));
            }
        }
        let at = match image.free_run(entries.len()) {
            Some(at) => at,
            None => self.grow_dir(dir, image, entries.len())?,
        };
        for (i, entry) in entries.iter().enumerate() {
            image.entry(at + i).copy_from_slice(entry);
        }
        self.store_dir(image, at..at + entries.len())
    }

    /// Adds zeroed clusters to a directory's chain until it has `needed` free slots at its
    /// end, and returns where they start.
    fn grow_dir(&mut self, dir: Dir, image: &mut DirImage, needed: usize) -> Result<usize, D::Error> {
        let first = match (dir, self.root) {
            (Dir::Root, RootDir::Fixed { .. }) => return Err(FatError::DirectoryFull),
            (Dir::Root, RootDir::Chain(first)) | (Dir::Chain(first), _) => first.0,
        };
        let chain = self.chain(first)?;
        // Free slots at the end already count towards what's needed.
        let end = image.bytes.chunks_exact(DIR_ENTRY_SIZE).position(|entry| entry[0] == ENTRY_END).unwrap_or(image.slots());
        let free_at_end = image.slots() - end;
        let per_cluster = self.cluster_size() / DIR_ENTRY_SIZE;
        let clusters = (needed - free_at_end).div_ceil(per_cluster);
        let new = self.allocate(clusters)?;
        let zeroes = vec![0; clusters * self.cluster_size()];
        self.write_clusters(&new, &zeroes)?;
        // Chain the new clusters, then hook them onto the directory's last one.
        self.link(&new)?;
        self.set_fat_entry(*chain.last().expect("a chain has clusters"), new[0])?;
        self.flush_fat()?;
        for &cluster in &new {
            let first_sector = self.first_sector(Cluster(cluster));
            image.sectors.extend(first_sector..first_sector + self.sectors_per_cluster);
        }
        image.bytes.resize(image.sectors.len() * BLOCK_SIZE, 0);
        Ok(end)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_image::sample;
    use super::*;
    use crate::block::memory::MemoryDisk;
    use alloc::format;

    fn volumes() -> [Fat<MemoryDisk>; 2] {
        [FatType::Fat16, FatType::Fat32].map(|fat_type| sample(fat_type, false).0)
    }

    fn bytes(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)).collect()
    }

    /// How many slots the entry for `name` in `dir` takes.
    fn slots(fat: &mut Fat<MemoryDisk>, dir: &str, name: &str) -> usize {
        let dir = fat.find_dir(dir).unwrap();
        let image = fat.load_dir(dir).unwrap();
        let located = parse_located(&image.bytes, fat.fat_type);
        located.into_iter().find(|l| l.entry.name == name).unwrap().slots.len()
    }

    /// The (created date, modified date, modified time) of `name`'s entry in `dir`.
    fn stamps(fat: &mut Fat<MemoryDisk>, dir: &str, name: &str) -> (u16, u16, u16) {
        let dir = fat.find_dir(dir).unwrap();
        let image = fat.load_dir(dir).unwrap();
        let located = parse_located(&image.bytes, fat.fat_type);
        let slot = located.into_iter().find(|l| l.entry.name == name).unwrap().slots.end - 1;
        let entry = &image.bytes[slot * DIR_ENTRY_SIZE..(slot + 1) * DIR_ENTRY_SIZE];
        let word = |at: usize| u16::from_le_bytes([entry[at], entry[at + 1]]);
        (word(16), word(24), word(22))
    }

    #[test]
    fn entries_get_the_time_set_and_replacing_keeps_the_created_date() {
        for mut fat in volumes() {
            fat.write_file("/stamped", b"one").unwrap();
            assert_eq!(stamps(&mut fat, "/", "stamped").0, DEFAULT_STAMP.0);
            let later = crate::time::DateTime::from_unix(1_790_000_000); // 2026-09-21 14:13:20
            fat.set_time(later);
            fat.write_file("/stamped", b"two").unwrap();
            let (date, time) = later.fat();
            assert_eq!(stamps(&mut fat, "/", "stamped"), (DEFAULT_STAMP.0, date, time));
            fat.create_dir("/newdir").unwrap();
            assert_eq!(stamps(&mut fat, "/", "newdir"), (date, date, time));
            // And what listings show.
            let entry = fat.metadata("/stamped").unwrap();
            assert_eq!(format!("{}", entry.modified), "2026-09-21 14:13");
        }
    }

    #[test]
    fn created_files_read_back() {
        for mut fat in volumes() {
            let free = fat.free_clusters().unwrap();
            let data = bytes(3000, 1);
            fat.write_file("/new file.txt", &data).unwrap();
            assert_eq!(fat.read_file("/NEW FILE.TXT").unwrap(), data);
            let entry = fat.metadata("/new file.txt").unwrap();
            assert_eq!((entry.name.as_str(), entry.size), ("new file.txt", 3000));
            assert_eq!(free - fat.free_clusters().unwrap(), 3000u32.div_ceil(512));
            fat.write_file("/overlays/inner", b"x").unwrap();
            assert_eq!(fat.read_file("/overlays/inner").unwrap(), b"x");
            fat.write_file("/empty2", b"").unwrap();
            assert_eq!(fat.read_file("/empty2").unwrap(), b"");
        }
    }

    #[test]
    fn replacing_a_file_frees_its_old_clusters() {
        for mut fat in volumes() {
            fat.write_file("/big", &bytes(5000, 2)).unwrap();
            let free = fat.free_clusters().unwrap();
            fat.write_file("/big", b"small now").unwrap();
            assert_eq!(fat.read_file("/big").unwrap(), b"small now");
            assert_eq!(fat.free_clusters().unwrap(), free + 10 - 1);
            // An existing file matched regardless of case keeps its entry.
            fat.write_file("/config.txt", b"arm_64bit=0\n").unwrap();
            assert_eq!(fat.read_file("/CONFIG.TXT").unwrap(), b"arm_64bit=0\n");
            assert_eq!(fat.read_dir("/").unwrap().iter().filter(|e| names_match(&e.name, "config.txt")).count(), 1);
        }
    }

    #[test]
    fn short_names_need_no_long_entries() {
        for mut fat in volumes() {
            fat.write_file("/notes.txt", b"a").unwrap();
            fat.write_file("/README", b"b").unwrap();
            fat.write_file("/Mixed.txt", b"c").unwrap();
            assert_eq!(slots(&mut fat, "/", "notes.txt"), 1);
            assert_eq!(slots(&mut fat, "/", "README"), 1);
            assert_eq!(slots(&mut fat, "/", "Mixed.txt"), 2);
        }
    }

    #[test]
    fn aliases_of_long_names_are_unique() {
        for mut fat in volumes() {
            fat.write_file("/long file name one.txt", b"1").unwrap();
            fat.write_file("/long file name two.txt", b"2").unwrap();
            assert_eq!(fat.read_file("/long file name one.txt").unwrap(), b"1");
            assert_eq!(fat.read_file("/long file name two.txt").unwrap(), b"2");
            let image = fat.load_dir(Dir::Root).unwrap();
            let shorts = image.short_names();
            assert!(shorts.contains(b"LONGFI~1TXT") && shorts.contains(b"LONGFI~2TXT"));
        }
    }

    #[test]
    fn directories_can_be_made_filled_and_removed() {
        for mut fat in volumes() {
            let free = fat.free_clusters().unwrap();
            fat.create_dir("/docs").unwrap();
            assert!(matches!(fat.create_dir("/docs"), Err(FatError::AlreadyExists)));
            fat.write_file("/docs/a.txt", b"inside").unwrap();
            fat.create_dir("/docs/sub").unwrap();
            fat.write_file("/docs/sub/deep", b"deeper").unwrap();
            let names: Vec<_> = fat.read_dir("/docs").unwrap().into_iter().map(|e| e.name).collect();
            assert_eq!(names, ["a.txt", "sub"]);
            assert_eq!(fat.read_file("/docs/sub/deep").unwrap(), b"deeper");

            assert!(matches!(fat.remove("/docs"), Err(FatError::NotEmpty)));
            fat.remove("/docs/sub/deep").unwrap();
            fat.remove("/docs/sub").unwrap();
            fat.remove("/docs/a.txt").unwrap();
            fat.remove("/docs").unwrap();
            assert!(matches!(fat.metadata("/docs"), Err(FatError::NotFound)));
            assert_eq!(fat.free_clusters().unwrap(), free);
        }
    }

    #[test]
    fn dot_entries_point_at_the_directory_and_its_parent() {
        let (mut fat, _) = sample(FatType::Fat32, false);
        fat.create_dir("/outer").unwrap();
        fat.create_dir("/outer/inner").unwrap();
        let outer = fat.metadata("/outer").unwrap().first_cluster;
        let inner = fat.metadata("/outer/inner").unwrap().first_cluster;
        let image = fat.load_dir(Dir::Chain(inner)).unwrap();
        let cluster_of = |entry: &[u8]| (u16_at(entry, 20) as u32) << 16 | u16_at(entry, 26) as u32;
        assert_eq!(&image.bytes[..11], b".          ");
        assert_eq!(cluster_of(&image.bytes[..32]), inner.0);
        assert_eq!(&image.bytes[32..43], b"..         ");
        assert_eq!(cluster_of(&image.bytes[32..64]), outer.0);
        // The root's children point ".." at cluster 0.
        let image = fat.load_dir(Dir::Chain(outer)).unwrap();
        assert_eq!(cluster_of(&image.bytes[32..64]), 0);
    }

    #[test]
    fn directories_grow_past_a_cluster() {
        for mut fat in volumes() {
            fat.create_dir("/many").unwrap();
            for i in 0..40 {
                fat.write_file(&format!("/many/file number {i} with a long name.txt"), &bytes(i * 10, i as u8)).unwrap();
            }
            let entries = fat.read_dir("/many").unwrap();
            assert_eq!(entries.len(), 40);
            for i in 0..40 {
                let path = format!("/many/file number {i} with a long name.txt");
                assert_eq!(fat.read_file(&path).unwrap(), bytes(i * 10, i as u8));
            }
        }
    }

    #[test]
    fn deleted_slots_are_reused() {
        for mut fat in volumes() {
            fat.write_file("/a long name to delete.txt", b"x").unwrap();
            let before = fat.load_dir(Dir::Root).unwrap().bytes.len();
            fat.remove("/a long name to delete.txt").unwrap();
            fat.write_file("/another long name here.txt", b"y").unwrap();
            assert_eq!(fat.load_dir(Dir::Root).unwrap().bytes.len(), before);
            assert_eq!(fat.read_file("/another long name here.txt").unwrap(), b"y");
        }
    }

    #[test]
    fn the_fat16_root_fills_up_without_leaking() {
        let (mut fat, _) = sample(FatType::Fat16, false);
        let mut i = 0;
        let error = loop {
            let free = fat.free_clusters().unwrap();
            match fat.write_file(&format!("/a rather long file name number {i}.txt"), b"data") {
                Ok(()) => i += 1,
                Err(error) => {
                    assert_eq!(fat.free_clusters().unwrap(), free);
                    break error;
                }
            }
        };
        assert!(matches!(error, FatError::DirectoryFull));
        assert!(i > 100);
    }

    #[test]
    fn a_full_disk_is_no_space_and_leaks_nothing() {
        let (mut fat, _) = sample(FatType::Fat16, false);
        let free = fat.free_clusters().unwrap();
        let too_big = bytes((free as usize + 1) * 512, 3);
        assert!(matches!(fat.write_file("/huge", &too_big), Err(FatError::NoSpace)));
        assert_eq!(fat.free_clusters().unwrap(), free);
        // Exactly what's free fits.
        fat.write_file("/huge", &too_big[..free as usize * 512]).unwrap();
        assert_eq!(fat.free_clusters().unwrap(), 0);
    }

    #[test]
    fn bad_names_and_paths_are_refused() {
        let (mut fat, _) = sample(FatType::Fat32, false);
        for name in ["/", "/a?b", "/..", "/trailing.", "/co:lon", "/tab\there"] {
            assert!(matches!(fat.write_file(name, b"x"), Err(FatError::InvalidName)), "{}", name);
        }
        assert!(matches!(fat.write_file("/missing/x", b"x"), Err(FatError::NotFound)));
        assert!(matches!(fat.write_file("/overlays", b"x"), Err(FatError::NotAFile)));
        assert!(matches!(fat.write_file("/config.txt/x", b"x"), Err(FatError::NotADirectory)));
        assert!(matches!(fat.remove("/missing"), Err(FatError::NotFound)));
    }

    #[test]
    fn fsinfo_tracks_the_free_clusters() {
        let (mut fat, _) = sample(FatType::Fat32, false);
        let mut fsinfo = [0u8; BLOCK_SIZE];
        let at = fat.start;
        // The test image has no FSInfo sector; give it one at sector 1.
        fsinfo[0..4].copy_from_slice(&FSINFO_LEAD.to_le_bytes());
        fsinfo[484..488].copy_from_slice(&FSINFO_STRUCT.to_le_bytes());
        fat.device.write_block(at.offset(1), &fsinfo).unwrap();
        fat.fsinfo = Some(1);
        fat.write_file("/one", &bytes(2000, 5)).unwrap();
        fat.create_dir("/two").unwrap();
        fat.remove("/empty").unwrap();
        fat.device.read_block(at.offset(1), &mut fsinfo).unwrap();
        assert_eq!(u32_at(&fsinfo, 488), fat.free_clusters().unwrap());
        assert_eq!(u32_at(&fsinfo, 492), fat.next_free);
    }

    #[test]
    fn every_fat_copy_gets_the_changes() {
        for mut fat in volumes() {
            fat.write_file("/copied", &bytes(4000, 4)).unwrap();
            fat.create_dir("/dir").unwrap();
            fat.remove("/empty").unwrap();
            let (start, size) = (fat.start, fat.fat_size);
            let mut read = |copy: u64| {
                let mut blocks = vec![[0; BLOCK_SIZE]; size as usize];
                fat.device.read_blocks(start.offset(fat.fat_start + copy * size), &mut blocks).unwrap();
                blocks
            };
            assert!(read(0) == read(1));
        }
    }
}
