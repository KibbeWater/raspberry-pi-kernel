// mbr.rs
//! Finding the FAT volume on a disk: through an MBR partition table, or directly at block 0
//! for disks formatted without one ("superfloppy").

use crate::block::{Block, Lba};

/// The partition type byte from an MBR entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PartitionType(pub u8);

impl PartitionType {
    /// FAT16 and FAT32 types, with CHS or LBA addressing.
    pub fn is_fat(self) -> bool {
        matches!(self.0, 0x04 | 0x06 | 0x0B | 0x0C | 0x0E)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Partition {
    pub kind: PartitionType,
    pub start: Lba,
    pub blocks: u32,
}

/// Where the FAT volume starts, and the partition it is in, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Volume {
    Partition { index: usize, partition: Partition },
    /// No partition table; the whole disk is one volume.
    WholeDisk,
}

impl Volume {
    pub fn start(&self) -> Lba {
        match self {
            Volume::Partition { partition, .. } => partition.start,
            Volume::WholeDisk => Lba(0),
        }
    }
}

fn has_boot_signature(block: &Block) -> bool {
    block[510] == 0x55 && block[511] == 0xAA
}

/// Whether `block` looks like a FAT boot sector: a jump instruction, 512 bytes per sector,
/// a power-of-two cluster size and at least one FAT.
pub fn is_fat_boot_sector(block: &Block) -> bool {
    let jump = block[0] == 0xEB || block[0] == 0xE9;
    let bytes_per_sector = u16::from_le_bytes([block[11], block[12]]);
    let sectors_per_cluster = block[13];
    jump && has_boot_signature(block)
        && bytes_per_sector == 512
        && sectors_per_cluster.is_power_of_two()
        && block[16] > 0
}

/// The four primary partitions of an MBR, `None` for empty slots. Returns `None` if `block`
/// has no MBR signature.
pub fn partitions(block: &Block) -> Option<[Option<Partition>; 4]> {
    if !has_boot_signature(block) {
        return None;
    }
    Some(core::array::from_fn(|i| {
        let entry = &block[446 + i * 16..446 + (i + 1) * 16];
        let kind = PartitionType(entry[4]);
        let start = u32::from_le_bytes(entry[8..12].try_into().unwrap());
        let blocks = u32::from_le_bytes(entry[12..16].try_into().unwrap());
        (kind.0 != 0 && blocks != 0).then_some(Partition { kind, start: Lba(start as u64), blocks })
    }))
}

/// Finds the FAT volume given the disk's first block: block 0 itself if it is a FAT boot
/// sector, otherwise the first FAT partition in its MBR.
pub fn find_fat_volume(block0: &Block) -> Option<Volume> {
    if is_fat_boot_sector(block0) {
        return Some(Volume::WholeDisk);
    }
    partitions(block0)?
        .into_iter()
        .enumerate()
        .find_map(|(index, p)| p.filter(|p| p.kind.is_fat()).map(|partition| Volume::Partition { index, partition }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BLOCK_SIZE;

    fn mbr(entries: &[(usize, u8, u32, u32)]) -> Block {
        let mut block = [0; BLOCK_SIZE];
        for &(i, kind, start, blocks) in entries {
            let e = &mut block[446 + i * 16..446 + (i + 1) * 16];
            e[4] = kind;
            e[8..12].copy_from_slice(&start.to_le_bytes());
            e[12..16].copy_from_slice(&blocks.to_le_bytes());
        }
        block[510] = 0x55;
        block[511] = 0xAA;
        block
    }

    #[test]
    fn finds_the_first_fat_partition() {
        // A Linux partition first, then FAT32 LBA, like a Raspberry Pi OS card reversed.
        let block = mbr(&[(0, 0x83, 2048, 1000), (1, 0x0C, 8192, 500_000)]);
        let volume = find_fat_volume(&block).unwrap();
        assert_eq!(volume, Volume::Partition {
            index: 1,
            partition: Partition { kind: PartitionType(0x0C), start: Lba(8192), blocks: 500_000 },
        });
        assert_eq!(volume.start(), Lba(8192));
    }

    #[test]
    fn no_signature_or_no_fat_partition_is_none() {
        let mut block = mbr(&[(0, 0x0C, 2048, 1000)]);
        block[511] = 0;
        assert_eq!(find_fat_volume(&block), None);
        assert_eq!(find_fat_volume(&mbr(&[(0, 0x83, 2048, 1000)])), None);
    }

    #[test]
    fn a_fat_boot_sector_at_block_0_is_the_whole_disk() {
        let mut block = mbr(&[]);
        block[0] = 0xEB;
        block[11..13].copy_from_slice(&512u16.to_le_bytes());
        block[13] = 8;
        block[16] = 2;
        assert_eq!(find_fat_volume(&block), Some(Volume::WholeDisk));
    }
}
