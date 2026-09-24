//! Random numbers from the Pi's hardware generator.

use core::ops::Range;
use rustypi_abi::{Syscall, MAX_RANDOM};
use crate::syscall;

/// Fills `buf` with random bytes.
pub fn fill(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(MAX_RANDOM) {
        // The kernel fills all of a chunk this size; it can only fail for a bad pointer.
        let call = Syscall::Random { ptr: chunk.as_mut_ptr() as u64, len: chunk.len() as u64 };
        syscall::call(call).expect("filling a buffer of ours with random bytes");
    }
}

pub fn u32() -> u32 {
    let mut bytes = [0; 4];
    fill(&mut bytes);
    u32::from_le_bytes(bytes)
}

pub fn u64() -> u64 {
    let mut bytes = [0; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}

/// A random number in `range`, every value equally likely. Panics if the range is empty.
pub fn range(range: Range<u64>) -> u64 {
    assert!(!range.is_empty(), "random::range of an empty range");
    let span = range.end - range.start;
    // Draws at or above the last whole multiple of `span` would favour the low results.
    let fair = u64::MAX / span * span;
    loop {
        let draw = u64();
        if draw < fair {
            return range.start + draw % span;
        }
    }
}
