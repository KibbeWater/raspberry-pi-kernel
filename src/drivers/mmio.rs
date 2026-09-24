// mmio.rs
//! Reading and writing 32-bit peripheral registers, for drivers that address them by
//! `PERIPHERAL_BASE + offset` constants.

use core::ptr::{read_volatile, write_volatile};

/// Reads the register at `addr`, which must be one of a driver's register constants.
#[inline(always)]
pub fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Writes the register at `addr`, which must be one of a driver's register constants.
#[inline(always)]
pub fn write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}
