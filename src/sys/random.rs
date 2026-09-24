// random.rs
//! Random numbers from the hardware generator.

use crate::drivers::rng;
use crate::synchronization::{interface::Mutex, IrqLock};

/// Keeps readers from emptying the FIFO under each other.
static RNG: IrqLock<()> = IrqLock::new(());

/// Fills `buf` with random bytes.
pub fn fill(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(4) {
        let word = RNG.lock(|_| rng::next_u32());
        chunk.copy_from_slice(&word.to_le_bytes()[..chunk.len()]);
    }
}

pub fn u64() -> u64 {
    let mut bytes = [0; 8];
    fill(&mut bytes);
    u64::from_le_bytes(bytes)
}
