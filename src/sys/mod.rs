// sys/mod.rs
//! Kernel-wide system API: startup, time, power, interrupts and board information.
//!
//! Code outside `drivers` should go through here rather than poking hardware.

pub mod console;
pub mod cores;
pub mod fs;
pub mod memory;
pub mod net;
pub mod print;
pub mod random;
pub mod usb;
mod heap;
mod panic;

pub use panic::stop_if_another_core_panicked;

use core::time::Duration;
use crate::arch;
use crate::drivers::interrupt::{self, Irq};
use crate::drivers::mailbox::{self, tags, Batch, Mailbox};
use crate::drivers::{local, power, timer};
use crate::drivers::uart::Uart;

/// Git commit the kernel was built from, with `-dirty` for uncommitted changes.
pub const VERSION: &str = env!("GIT_VERSION");

/// Period of the timer tick: the scheduler's time slice, and the resolution of
/// `sched::sleep`.
pub const TICK: Duration = Duration::from_millis(10);

// `tasks` reports recent CPU use as "over the last second".
const _: () = assert!(TICK.as_millis() * rustypi_core::sched::CPU_WINDOW as u128 == 1000);

pub use rustypi_core::heap::Stats as HeapStats;
pub use mailbox::MailboxError;

/// Turns on the MMU and caches, sets up the heap and starts the random number generator.
/// Must be the first thing `kernel_main` does.
pub fn init() {
    arch::mmu::enable();
    heap::init();
    crate::drivers::rng::init();
}

/// Heap usage, in bytes.
pub fn heap_stats() -> HeapStats {
    heap::stats()
}

/// Starts interrupt-driven UART receive and core 0's timer tick, then unmasks IRQs.
/// Call once the UART is initialized.
pub fn enable_interrupts() {
    Uart::enable_rx_interrupt();
    interrupt::enable(Irq::Uart0);
    arch::timer::set_tick(TICK);
    start_tick();
}

/// Starts this core's timer tick and its wake-up interrupt, and unmasks its IRQs.
fn start_tick() {
    let core = arch::core_id();
    local::route_timer(core);
    local::enable_ipi(core);
    arch::timer::start();
    arch::irq_enable();
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

/// Busy-waits for `duration` without letting other tasks run. For early boot and the panic
/// handler; tasks should use `sched::sleep`.
pub fn delay(duration: Duration) {
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
    let replies = batch.send(&Mailbox)?;
    Ok(BoardInfo {
        revision: replies.get(revision)?,
        firmware: replies.get(firmware)?,
        arm_memory: replies.get(memory)?.size,
        millidegrees: replies.get(temperature)?.millidegrees,
    })
}
