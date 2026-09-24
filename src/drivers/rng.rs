// rng.rs
//! BCM2835 hardware random number generator: a noise source feeding a FIFO of 32-bit words.
//!
//! Undocumented in the datasheet; mirrors Linux's `bcm2835-rng.c`.

use core::ptr::{read_volatile, write_volatile};
use crate::board::PERIPHERAL_BASE;

const RNG_BASE: usize = PERIPHERAL_BASE + 0x10_4000;
const RNG_CTRL: usize = RNG_BASE + 0x00;
const RNG_STATUS: usize = RNG_BASE + 0x04;
const RNG_DATA: usize = RNG_BASE + 0x08;
const RNG_INT_MASK: usize = RNG_BASE + 0x10;

/// CTRL: generate.
const RNG_ENABLE: u32 = 1;
/// STATUS, low bits: how many numbers to throw away after enabling, while the noise source
/// settles.
const RNG_WARMUP_COUNT: u32 = 0x4_0000;
/// INT_MASK: no interrupt; `next_u32` polls.
const RNG_INT_OFF: u32 = 1;

fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

fn write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

pub fn init() {
    write(RNG_STATUS, RNG_WARMUP_COUNT);
    write(RNG_INT_MASK, read(RNG_INT_MASK) | RNG_INT_OFF);
    write(RNG_CTRL, read(RNG_CTRL) | RNG_ENABLE);
}

/// The next random word, waiting for one if the FIFO is empty (it is at first, while the
/// generator warms up). Callers must not interleave: another reader could empty the FIFO
/// between the check and the read.
pub fn next_u32() -> u32 {
    // STATUS bits 31:24 count the words waiting.
    while read(RNG_STATUS) >> 24 == 0 {
        core::hint::spin_loop();
    }
    read(RNG_DATA)
}
