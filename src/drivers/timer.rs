// timer.rs
//! BCM2835 system timer: a free-running 64-bit microsecond counter, the kernel's clock. (The
//! scheduler's tick comes from each core's own timer, `arch::timer`.)

use crate::board::PERIPHERAL_BASE;
use crate::drivers::mmio::read;

const TIMER_BASE: usize = PERIPHERAL_BASE + 0x3000;
const TIMER_CLO: usize = TIMER_BASE + 0x04;
const TIMER_CHI: usize = TIMER_BASE + 0x08;

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
