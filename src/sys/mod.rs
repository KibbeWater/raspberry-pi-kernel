// sys/mod.rs
//! Kernel-wide system API: startup, time, power, interrupts and board information.
//!
//! Code outside `drivers` should go through here rather than poking hardware.

pub mod print;
mod heap;
mod panic;

use core::time::Duration;
use crate::arch;
use crate::drivers::interrupt::{self, Irq};
use crate::drivers::{mailbox, power, timer};
use crate::drivers::uart::Uart;

/// Git commit the kernel was built from, with `-dirty` for uncommitted changes.
pub const VERSION: &str = env!("GIT_VERSION");

/// Period of the timer tick. It wakes `idle` so the main loop runs at least this often.
const TICK: Duration = Duration::from_millis(100);

const TAG_BOARD_REVISION: u32 = 0x0001_0002;
const TAG_TEMPERATURE: u32 = 0x0003_0006;

pub use heap::Stats as HeapStats;

/// Turns on the MMU and caches and sets up the heap. Must be the first thing
/// `kernel_main` does.
pub fn init() {
    arch::mmu::enable();
    heap::init();
}

/// Heap usage, in bytes.
pub fn heap_stats() -> HeapStats {
    heap::stats()
}

/// Starts interrupt-driven UART receive and the timer tick, then unmasks IRQs.
/// Call once the UART is initialized.
pub fn enable_interrupts() {
    Uart::enable_rx_interrupt();
    interrupt::enable(Irq::Uart0);
    timer::start_tick(TICK.as_micros() as u32);
    interrupt::enable(Irq::SystemTimer1);
    arch::irq_enable();
}

/// Sleeps until an interrupt, unless UART input is already waiting. Wakes at least
/// every `TICK`.
pub fn idle() {
    arch::wait_for_interrupt_unless(Uart::has_input);
}

/// Current exception level; 1 in normal operation.
pub fn exception_level() -> u8 {
    arch::exception_level()
}

/// Whether the MMU and caches are on.
pub fn mmu_enabled() -> bool {
    arch::mmu::is_enabled()
}

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
