// sys/mod.rs
//! Kernel-wide system API: time, power and board information.
//!
//! Code outside `drivers` should go through here rather than poking hardware.

pub mod print;
mod panic;

use core::time::Duration;
use crate::drivers::{mailbox, power, timer};
use crate::drivers::uart::Uart;

const TAG_BOARD_REVISION: u32 = 0x0001_0002;
const TAG_TEMPERATURE: u32 = 0x0003_0006;

/// Time since the board was reset.
pub fn uptime() -> Duration {
    Duration::from_micros(timer::now_us())
}

/// Busy-waits for `duration`.
pub fn sleep(duration: Duration) {
    timer::delay_us(duration.as_micros() as u64);
}

/// Resets the board. The firmware boots the kernel again from the SD card.
pub fn reboot() -> ! {
    Uart::flush();
    power::reset(power::Partition::Boot)
}

/// Halts the board in its low-power state. The Pi 3 cannot cut its own power;
/// pull GPIO3 (header pin 5) to ground to boot it again.
pub fn shutdown() -> ! {
    Uart::flush();
    power::reset(power::Partition::Halt)
}

/// Board revision code, see
/// <https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#raspberry-pi-revision-codes>.
pub fn board_revision() -> Option<u32> {
    mailbox::property(TAG_BOARD_REVISION, 0).map(|[revision, _]| revision)
}

/// SoC temperature in thousandths of a degree Celsius.
pub fn temperature() -> Option<u32> {
    mailbox::property(TAG_TEMPERATURE, 0).map(|[_, millidegrees]| millidegrees)
}
