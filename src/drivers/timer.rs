// timer.rs
//! BCM2835 system timer: a free-running 64-bit microsecond counter, plus a periodic
//! tick interrupt on compare channel 1.

use core::sync::atomic::{AtomicU32, Ordering};
use crate::board::PERIPHERAL_BASE;
use crate::drivers::mmio::{read, write};

const TIMER_BASE: usize = PERIPHERAL_BASE + 0x3000;
const TIMER_CS: usize = TIMER_BASE + 0x00;
const TIMER_CLO: usize = TIMER_BASE + 0x04;
const TIMER_CHI: usize = TIMER_BASE + 0x08;
const TIMER_C1: usize = TIMER_BASE + 0x10;

/// Compare 1 matched; write 1 to clear.
const TIMER_CS_M1: u32 = 1 << 1;

static TICK_US: AtomicU32 = AtomicU32::new(0);

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

/// Raises `Irq::SystemTimer1` every `interval_us` microseconds.
pub fn start_tick(interval_us: u32) {
    TICK_US.store(interval_us, Ordering::Relaxed);
    write(TIMER_C1, read(TIMER_CLO).wrapping_add(interval_us));
}

/// Acknowledges the tick and schedules the next one.
pub fn handle_interrupt() {
    write(TIMER_CS, TIMER_CS_M1);
    // Compare only matches the low 32 bits, so wrapping is fine.
    write(TIMER_C1, read(TIMER_CLO).wrapping_add(TICK_US.load(Ordering::Relaxed)));
}
