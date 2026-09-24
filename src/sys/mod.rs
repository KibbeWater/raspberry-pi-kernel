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
use crate::drivers::mailbox::{self, tags, Batch};
use crate::drivers::{power, timer};
use crate::drivers::uart::Uart;

/// Git commit the kernel was built from, with `-dirty` for uncommitted changes.
pub const VERSION: &str = env!("GIT_VERSION");

/// Period of the timer tick. It wakes `idle` so the main loop runs at least this often.
const TICK: Duration = Duration::from_millis(100);

pub use heap::Stats as HeapStats;
pub use mailbox::MailboxError;

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

pub struct BoardInfo {
    /// Board revision code, see
    /// <https://www.raspberrypi.com/documentation/computers/raspberry-pi.html#raspberry-pi-revision-codes>.
    pub revision: u32,
    /// Firmware build time, as a Unix timestamp.
    pub firmware: u32,
    /// Bytes of RAM left to the ARM cores; the GPU has the rest.
    pub arm_memory: u32,
    /// SoC temperature in thousandths of a degree Celsius.
    pub millidegrees: u32,
}

/// Asks the firmware about the board, in a single mailbox message.
pub fn board_info() -> Result<BoardInfo, MailboxError> {
    let mut batch = Batch::new();
    let revision = batch.add::<tags::GetBoardRevision>(());
    let firmware = batch.add::<tags::GetFirmwareRevision>(());
    let memory = batch.add::<tags::GetArmMemory>(());
    let temperature = batch.add::<tags::GetTemperature>(tags::SensorId::SOC);
    let replies = batch.send()?;
    Ok(BoardInfo {
        revision: replies.get(revision)?,
        firmware: replies.get(firmware)?,
        arm_memory: replies.get(memory)?.size,
        millidegrees: replies.get(temperature)?.millidegrees,
    })
}
