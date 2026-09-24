// timer.rs
//! BCM2835 system timer: a free-running 64-bit microsecond counter.

use core::ptr::read_volatile;
use crate::board::PERIPHERAL_BASE;

const TIMER_BASE: usize = PERIPHERAL_BASE + 0x3000;
const TIMER_CLO: usize = TIMER_BASE + 0x04;
const TIMER_CHI: usize = TIMER_BASE + 0x08;

#[inline(always)]
fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

/// Microseconds since the board was reset.
pub fn now_us() -> u64 {
    // The halves are read separately; retry if the low word wrapped in between.
    loop {
        let hi = read(TIMER_CHI);
        let lo = read(TIMER_CLO);
        if read(TIMER_CHI) == hi {
            return (hi as u64) << 32 | lo as u64;
        }
    }
}

/// Busy-waits for `us` microseconds.
pub fn delay_us(us: u64) {
    let start = now_us();
    while now_us() - start < us {
        core::hint::spin_loop();
    }
}
