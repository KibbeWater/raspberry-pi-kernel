// fs.rs
//! The SD card's FAT volume, mounted at boot.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use rustypi_abi::MAX_FILE;
use rustypi_core::block::{BlockDevice, Lba, WritableBlockDevice, BLOCK_SIZE};
use rustypi_core::fat::{self, Fat, FatError, FatType};
use rustypi_core::mbr::{self, Volume};
use crate::drivers::sdcard::{SdCard, SdError};
use crate::synchronization::{interface::Mutex as _, Mutex};

pub use rustypi_core::fat::{DirEntry, EntryKind};

#[derive(Debug)]
pub enum FsError {
    Card(SdError),
    /// Neither block 0 nor any MBR partition holds a FAT volume.
    NoFatVolume,
    Fat(FatError<SdError>),
    NotMounted,
    /// No blocks outside every partition to test writing on.
    NoSpareBlocks,
    /// A file the Pi needs to boot, which is left alone.
    Protected,
    /// Bigger than `MAX_FILE`, too big to read whole into the kernel heap.
    TooBig,
}

impl From<SdError> for FsError {
    fn from(error: SdError) -> Self {
        FsError::Card(error)
    }
}

impl From<FatError<SdError>> for FsError {
    fn from(error: FatError<SdError>) -> Self {
        FsError::Fat(error)
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FsError::Card(error) => write!(f, "sd card: {error}"),
            FsError::NoFatVolume => write!(f, "no FAT volume on the card"),
            FsError::Fat(error) => write!(f, "{error}"),
            FsError::NotMounted => write!(f, "no filesystem mounted"),
            FsError::NoSpareBlocks => write!(f, "no blocks before the first partition to test on"),
            FsError::Protected => write!(f, "the Pi needs it to boot, so it can't be changed"),
            FsError::TooBig => write!(f, "bigger than {} MB, too big to read", MAX_FILE >> 20),
        }
    }
}

#[derive(Clone, Debug)]
pub struct MountInfo {
    pub fat_type: FatType,
    pub label: String,
    pub volume: Volume,
    pub cluster_size: usize,
    /// SDHC/SDXC rather than a standard capacity card.
    pub high_capacity: bool,
    pub card_blocks: Option<u64>,
    /// Data lines in use, and the SD clock.
    pub bus: (u8, u32),
}

struct Mounted {
    fat: Fat<SdCard>,
    info: MountInfo,
}

/// A sleeping lock: reading a big file takes a while, and other tasks keep running meanwhile.
static FS: Mutex<Option<Mounted>> = Mutex::new(None);

/// Initialises the SD card and mounts its FAT volume.
pub fn mount() -> Result<MountInfo, FsError> {
    let mut card = SdCard::init()?;
    let mut block0 = [0; BLOCK_SIZE];
    card.read_block(Lba(0), &mut block0)?;
    let volume = mbr::find_fat_volume(&block0).ok_or(FsError::NoFatVolume)?;
    let (high_capacity, card_blocks, bus) = (card.high_capacity(), card.blocks(), card.bus());
    let fat = Fat::mount(card, volume.start())?;
    let info = MountInfo {
        fat_type: fat.fat_type(),
        label: fat.label().into(),
        volume,
        cluster_size: fat.cluster_size(),
        high_capacity,
        card_blocks,
        bus,
    };
    let mounted = Mounted { fat, info: info.clone() };
    FS.lock(|fs| *fs = Some(mounted));
    Ok(info)
}

fn with_fs<R>(f: impl FnOnce(&mut Mounted) -> Result<R, FsError>) -> Result<R, FsError> {
    FS.lock(|fs| fs.as_mut().map_or(Err(FsError::NotMounted), f))
}

/// Like `with_fs`, for changing the volume: entries get the time now, if the clock knows it.
fn with_fs_writing<R>(f: impl FnOnce(&mut Mounted) -> Result<R, FsError>) -> Result<R, FsError> {
    with_fs(|fs| {
        if let Some(now) = super::clock::now() {
            fs.fat.set_time(now);
        }
        f(fs)
    })
}

pub fn info() -> Result<MountInfo, FsError> {
    with_fs(|fs| Ok(fs.info.clone()))
}

/// Lists a directory, like `/` or `/overlays`.
pub fn read_dir(path: &str) -> Result<Vec<DirEntry>, FsError> {
    with_fs(|fs| Ok(fs.fat.read_dir(path)?))
}

/// Looks up a file or directory.
pub fn metadata(path: &str) -> Result<DirEntry, FsError> {
    with_fs(|fs| Ok(fs.fat.metadata(path)?))
}

/// Checks `path` (absolute) is a directory, the root included.
pub fn check_dir(path: &str) -> Result<(), FsError> {
    if path.trim_matches('/').is_empty() {
        return Ok(());
    }
    match metadata(path)?.kind {
        EntryKind::Directory => Ok(()),
        EntryKind::File => Err(FsError::Fat(FatError::NotADirectory)),
    }
}

/// Whether `path` is something the Pi needs to boot: the firmware, its configuration, the
/// kernel, device trees, overlays. Writing, replacing and removing leave these alone, so a
/// bug (or a slip) can't stop the Pi booting.
fn is_protected(path: &str) -> bool {
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let (Some(first), rest) = (parts.next(), parts.next()) else { return true };
    let first = first.to_ascii_lowercase();
    if first == "overlays" {
        return true;
    }
    rest.is_none()
        && (first == "bootcode.bin"
            || first == "config.txt"
            || first == "cmdline.txt"
            || first.starts_with("start") && first.ends_with(".elf")
            || first.starts_with("fixup") && first.ends_with(".dat")
            || first.starts_with("kernel") && first.ends_with(".img")
            || first.ends_with(".dtb"))
}

/// Checks a file could be written at `path`: not protected, a name FAT can store, its
/// directory exists, and it isn't a directory itself. For finding out early, before collecting what to write.
pub fn check_writable(path: &str) -> Result<(), FsError> {
    if is_protected(path) {
        return Err(FsError::Protected);
    }
    let (parent, name) = path.trim_end_matches('/').rsplit_once('/').unwrap_or(("", path));
    fat::check_name(name)?;
    with_fs(|fs| {
        if !parent.trim_matches('/').is_empty() && fs.fat.metadata(parent)?.kind != EntryKind::Directory {
            return Err(FsError::Fat(FatError::NotADirectory));
        }
        match fs.fat.metadata(path) {
            Ok(entry) if entry.kind == EntryKind::Directory => Err(FsError::Fat(FatError::NotAFile)),
            Ok(_) | Err(FatError::NotFound) => Ok(()),
            Err(error) => Err(error.into()),
        }
    })
}

/// Creates or replaces a file with `data`, unless the Pi needs it to boot.
pub fn write_file(path: &str, data: &[u8]) -> Result<(), FsError> {
    if is_protected(path) {
        return Err(FsError::Protected);
    }
    with_fs_writing(|fs| Ok(fs.fat.write_file(path, data)?))
}

/// Installs `image` as the kernel the Pi boots, `/kernel8.img`, copying the one there now to
/// `/kernel8.bak` first. The only way to replace a boot file: for network updates, which
/// check the image first (`sys::update`). Each file is written crash-safely, so a failure
/// part way leaves a bootable kernel.
pub fn install_kernel(image: &[u8]) -> Result<(), FsError> {
    with_fs_writing(|fs| {
        match fs.fat.read_file("/kernel8.img") {
            Ok(old) => fs.fat.write_file("/kernel8.bak", &old)?,
            Err(FatError::NotFound) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(fs.fat.write_file("/kernel8.img", image)?)
    })
}

/// Makes a directory.
pub fn create_dir(path: &str) -> Result<(), FsError> {
    if is_protected(path) {
        return Err(FsError::Protected);
    }
    with_fs_writing(|fs| Ok(fs.fat.create_dir(path)?))
}

/// Removes a file or an empty directory, unless the Pi needs it to boot.
pub fn remove(path: &str) -> Result<(), FsError> {
    if is_protected(path) {
        return Err(FsError::Protected);
    }
    with_fs_writing(|fs| Ok(fs.fat.remove(path)?))
}

/// Free space on the volume, in bytes. Reads the whole FAT.
pub fn free_space() -> Result<u64, FsError> {
    with_fs(|fs| Ok(fs.fat.free_clusters()? as u64 * fs.info.cluster_size as u64))
}

/// Blocks the write test uses.
const WRITE_TEST_BLOCKS: usize = 9;

/// Tests writing to the card, outside the filesystem: on blocks halfway between the MBR and
/// the first partition, which belong to nothing. Writes one block, then eight in one go,
/// reads them back, and puts back what was there. Returns whether the patterns and the
/// restored blocks read back right.
pub fn write_test() -> Result<(bool, bool), FsError> {
    with_fs(|fs| {
        let card = fs.fat.device();
        let mut block0 = [0; BLOCK_SIZE];
        card.read_block(Lba(0), &mut block0)?;
        let first = mbr::first_partition_start(&block0).ok_or(FsError::NoSpareBlocks)?;
        if first.0 < 2 * WRITE_TEST_BLOCKS as u64 + 2 {
            return Err(FsError::NoSpareBlocks);
        }
        let at = Lba(first.0 / 2);
        let mut saved = vec![[0; BLOCK_SIZE]; WRITE_TEST_BLOCKS];
        card.read_blocks(at, &mut saved)?;
        let pattern: Vec<_> = (0..WRITE_TEST_BLOCKS)
            .map(|i| core::array::from_fn(|j| (i * 31 + j * 7) as u8 ^ 0xA5))
            .collect();

        let mut back = vec![[0; BLOCK_SIZE]; WRITE_TEST_BLOCKS];
        let written = card
            .write_block(at, &pattern[0])
            .and_then(|()| card.write_blocks(at.offset(1), &pattern[1..]))
            .and_then(|()| card.read_blocks(at, &mut back));
        // Put the old contents back whatever happened.
        card.write_blocks(at, &saved)?;
        written?;
        let patterns_ok = back == pattern;
        card.read_blocks(at, &mut back)?;
        Ok((patterns_ok, back == saved))
    })
}

/// Reads a whole file, of at most `MAX_FILE` bytes so a huge one can't use up the kernel heap.
pub fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    with_fs(|fs| {
        let entry = fs.fat.metadata(path)?;
        if entry.kind == EntryKind::File && entry.size as usize > MAX_FILE {
            return Err(FsError::TooBig);
        }
        Ok(fs.fat.read_file(path)?)
    })
}
