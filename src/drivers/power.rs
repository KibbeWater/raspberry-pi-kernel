// power.rs
//! BCM2835 power manager watchdog, used to reset or halt the board.
//!
//! Mirrors Linux's `bcm2835_wdt.c`.

use crate::board::PERIPHERAL_BASE;
use crate::drivers::mmio::{read, write};

const PM_BASE: usize = PERIPHERAL_BASE + 0x10_0000;
const PM_RSTC: usize = PM_BASE + 0x1C;
const PM_RSTS: usize = PM_BASE + 0x20;
const PM_WDOG: usize = PM_BASE + 0x24;

/// Writes to PM registers are ignored unless they carry this in the top byte.
const PM_PASSWORD: u32 = 0x5A00_0000;
const PM_RSTC_WRCFG_CLR: u32 = 0xFFFF_FFCF;
const PM_RSTC_WRCFG_FULL_RESET: u32 = 0x20;
const PM_RSTS_PARTITION_CLR: u32 = 0xFFFF_FAAA;

/// Watchdog timeout in ticks of ~15us: fire almost immediately.
const RESET_TICKS: u32 = 10;

/// What the firmware does after the reset.
#[derive(Clone, Copy)]
pub enum Partition {
    /// Boot normally.
    Boot = 0,
    /// Stay halted in low power until GPIO3 is pulled low (Linux `poweroff`).
    Halt = 63,
}

/// Triggers a full chip reset through the watchdog.
pub fn reset(partition: Partition) -> ! {
    // The firmware reads the partition from the even bits of RSTS.
    let partition = partition as u32;
    let rsts = (0..6).fold(0, |acc, bit| acc | ((partition >> bit) & 1) << (bit * 2));
    write(PM_RSTS, (read(PM_RSTS) & PM_RSTS_PARTITION_CLR) | PM_PASSWORD | rsts);

    write(PM_WDOG, PM_PASSWORD | RESET_TICKS);
    write(PM_RSTC, (read(PM_RSTC) & PM_RSTC_WRCFG_CLR) | PM_PASSWORD | PM_RSTC_WRCFG_FULL_RESET);

    loop {
        core::hint::spin_loop();
    }
}
