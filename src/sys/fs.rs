// fs.rs
//! The SD card's FAT volume, mounted at boot.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use rustypi_core::block::{BlockDevice, Lba, BLOCK_SIZE};
use rustypi_core::fat::{Fat, FatError, FatType};
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
            FsError::Card(error) => write!(f, "sd card: {}", error),
            FsError::NoFatVolume => write!(f, "no FAT volume on the card"),
            FsError::Fat(error) => write!(f, "{}", error),
            FsError::NotMounted => write!(f, "no filesystem mounted"),
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
    let (high_capacity, card_blocks) = (card.high_capacity(), card.blocks());
    let fat = Fat::mount(card, volume.start())?;
    let info = MountInfo {
        fat_type: fat.fat_type(),
        label: fat.label().into(),
        volume,
        cluster_size: fat.cluster_size(),
        high_capacity,
        card_blocks,
    };
    let mounted = Mounted { fat, info: info.clone() };
    FS.lock(|fs| *fs = Some(mounted));
    Ok(info)
}

fn with_fs<R>(f: impl FnOnce(&mut Mounted) -> Result<R, FsError>) -> Result<R, FsError> {
    FS.lock(|fs| fs.as_mut().map_or(Err(FsError::NotMounted), f))
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

/// Reads a whole file.
pub fn read_file(path: &str) -> Result<Vec<u8>, FsError> {
    with_fs(|fs| Ok(fs.fat.read_file(path)?))
}
