// block.rs
//! Block devices: storage read (and maybe written) in fixed 512-byte blocks, like an SD card.

use core::fmt;

pub const BLOCK_SIZE: usize = 512;

pub type Block = [u8; BLOCK_SIZE];

/// A block number (logical block address). A distinct type so block numbers can't be mixed
/// up with byte offsets, which some SD cards use on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lba(pub u64);

impl Lba {
    /// The block `blocks` further on.
    pub fn offset(self, blocks: u64) -> Lba {
        Lba(self.0 + blocks)
    }
}

impl fmt::Display for Lba {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "block {}", self.0)
    }
}

pub trait BlockDevice {
    type Error: fmt::Debug + fmt::Display;

    fn read_block(&mut self, lba: Lba, block: &mut Block) -> Result<(), Self::Error>;

    /// Reads consecutive blocks from `lba` on. Devices that can do it in one go (an SD card's
    /// multi-block read) override this; the default reads them one at a time.
    fn read_blocks(&mut self, lba: Lba, blocks: &mut [Block]) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter_mut().enumerate() {
            self.read_block(lba.offset(i as u64), block)?;
        }
        Ok(())
    }
}

/// A block device that can be written, too. Separate, so code that writes (like FAT's
/// file creation) can't be handed a read-only device.
pub trait WritableBlockDevice: BlockDevice {
    fn write_block(&mut self, lba: Lba, block: &Block) -> Result<(), Self::Error>;

    /// Writes consecutive blocks from `lba` on; like `read_blocks`, one at a time by default.
    fn write_blocks(&mut self, lba: Lba, blocks: &[Block]) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter().enumerate() {
            self.write_block(lba.offset(i as u64), block)?;
        }
        Ok(())
    }
}

impl<D: WritableBlockDevice + ?Sized> WritableBlockDevice for &mut D {
    fn write_block(&mut self, lba: Lba, block: &Block) -> Result<(), Self::Error> {
        (**self).write_block(lba, block)
    }

    fn write_blocks(&mut self, lba: Lba, blocks: &[Block]) -> Result<(), Self::Error> {
        (**self).write_blocks(lba, blocks)
    }
}

impl<D: BlockDevice + ?Sized> BlockDevice for &mut D {
    type Error = D::Error;

    fn read_block(&mut self, lba: Lba, block: &mut Block) -> Result<(), Self::Error> {
        (**self).read_block(lba, block)
    }

    fn read_blocks(&mut self, lba: Lba, blocks: &mut [Block]) -> Result<(), Self::Error> {
        (**self).read_blocks(lba, blocks)
    }
}

/// A sparse in-memory disk for tests: blocks never written read as zeros.
#[cfg(test)]
pub(crate) mod memory {
    use super::*;
    use alloc::collections::BTreeMap;

    #[derive(Default)]
    pub struct MemoryDisk {
        blocks: BTreeMap<u64, Block>,
        /// Blocks read.
        pub reads: usize,
        /// Read commands: a multi-block read is one.
        pub commands: usize,
        /// Blocks written.
        pub writes: usize,
    }

    impl MemoryDisk {
        pub fn write(&mut self, lba: u64, offset: usize, bytes: &[u8]) {
            let mut lba = lba + (offset / BLOCK_SIZE) as u64;
            let mut offset = offset % BLOCK_SIZE;
            let mut bytes = bytes;
            while !bytes.is_empty() {
                let block = self.blocks.entry(lba).or_insert([0; BLOCK_SIZE]);
                let n = bytes.len().min(BLOCK_SIZE - offset);
                block[offset..offset + n].copy_from_slice(&bytes[..n]);
                bytes = &bytes[n..];
                lba += 1;
                offset = 0;
            }
        }
    }

    impl BlockDevice for MemoryDisk {
        type Error = &'static str;

        fn read_block(&mut self, lba: Lba, block: &mut Block) -> Result<(), Self::Error> {
            self.read_blocks(lba, core::slice::from_mut(block))
        }

        fn read_blocks(&mut self, lba: Lba, blocks: &mut [Block]) -> Result<(), Self::Error> {
            self.commands += 1;
            for (i, block) in blocks.iter_mut().enumerate() {
                self.reads += 1;
                *block = self.blocks.get(&(lba.0 + i as u64)).copied().unwrap_or([0; BLOCK_SIZE]);
            }
            Ok(())
        }
    }

    impl WritableBlockDevice for MemoryDisk {
        fn write_block(&mut self, lba: Lba, block: &Block) -> Result<(), Self::Error> {
            self.writes += 1;
            self.blocks.insert(lba.0, *block);
            Ok(())
        }
    }
}
