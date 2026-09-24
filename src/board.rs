// board.rs
//! Constants for the board this kernel runs on (Raspberry Pi 3B+, BCM2837).

/// Cortex-A53 cores on the BCM2837.
pub const CORES: usize = 4;

/// Start of the peripheral MMIO window as seen from the ARM cores.
pub const PERIPHERAL_BASE: usize = 0x3F00_0000;

/// GPIO pin of the external status LED, blinked on panic.
pub const STATUS_LED: u8 = 21;

/// ARM local peripherals: core timers, core mailboxes and interrupt routing.
pub const LOCAL_PERIPHERAL_BASE: usize = 0x4000_0000;
