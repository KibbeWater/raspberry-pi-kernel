// buffer.rs
//! File bytes the kernel holds on its heap for programs: files they opened, new files they
//! are writing, programs they are starting. Each is capped at `MAX_FILE`, but a handful of
//! programs with a handful of handles could still use up the 16MB kernel heap, which would
//! take the kernel down. So they share a budget: past it, programs get `NoMemory`.

use alloc::vec::Vec;
use core::ops::Deref;
use core::sync::atomic::{AtomicUsize, Ordering};
use rustypi_abi::{Errno, MAX_FILE};

/// Bytes all `FileBuffer`s hold together, leaving the rest of the heap to everything else.
const BUDGET: usize = 8 * 1024 * 1024;

/// Bytes the `FileBuffer`s that exist hold (their capacity, which is what they allocated).
static HELD: AtomicUsize = AtomicUsize::new(0);

fn take(bytes: usize) -> Result<(), Errno> {
    HELD.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
        held.checked_add(bytes).filter(|&total| total <= BUDGET)
    })
    .map(|_| ())
    .map_err(|_| Errno::NoMemory)
}

fn give_back(bytes: usize) {
    HELD.fetch_sub(bytes, Ordering::Relaxed);
}

/// File bytes on the kernel heap, counted against the budget for as long as they exist.
pub(super) struct FileBuffer(Vec<u8>);

impl FileBuffer {
    pub(super) fn new() -> Self {
        FileBuffer(Vec::new())
    }

    /// Takes over bytes already read, like a whole file.
    pub(super) fn adopt(bytes: Vec<u8>) -> Result<Self, Errno> {
        take(bytes.capacity())?;
        Ok(FileBuffer(bytes))
    }

    /// Appends `bytes`: `NoSpace` past `MAX_FILE`, `NoMemory` past the budget.
    pub(super) fn extend(&mut self, bytes: &[u8]) -> Result<(), Errno> {
        let len = self.0.len() + bytes.len();
        if len > MAX_FILE {
            return Err(Errno::NoSpace);
        }
        let capacity = self.0.capacity();
        if len > capacity {
            // Doubling, like `Vec` would, so writing a file a bit at a time stays cheap.
            let grown = len.max(capacity * 2).min(MAX_FILE);
            take(grown - capacity)?;
            self.0.reserve_exact(grown - self.0.len());
            // In case the allocator handed out more than asked for.
            HELD.fetch_add(self.0.capacity() - grown, Ordering::Relaxed);
        }
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

impl Deref for FileBuffer {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for FileBuffer {
    fn drop(&mut self) {
        give_back(self.0.capacity());
    }
}
