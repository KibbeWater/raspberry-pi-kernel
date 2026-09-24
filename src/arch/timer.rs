// timer.rs
//! Each core's ARM generic timer, as the scheduler's tick: its EL1 physical timer, counting at
//! the rate the firmware put in CNTFRQ_EL0 and raising an interrupt when its countdown runs out.
//! `boot.s` lets EL1 use it (CNTHCTL_EL2).

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;

/// Timer counts per tick, the same on every core.
static TICK_COUNTS: AtomicU64 = AtomicU64::new(0);

/// CNTP_CTL_EL0: enabled, interrupt not masked.
const CTL_ENABLE: u64 = 1;

/// The Pi 3's crystal, which the counter runs from, in case the firmware left CNTFRQ_EL0 unset.
const CRYSTAL_HZ: u64 = 19_200_000;

fn frequency() -> u64 {
    let hz: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) hz, options(nomem, nostack)) };
    if hz == 0 { CRYSTAL_HZ } else { hz }
}

/// Sets how often `start` and `rearm` make this and every other core's timer fire.
pub fn set_tick(interval: Duration) {
    TICK_COUNTS.store(frequency() * interval.as_micros() as u64 / 1_000_000, Ordering::Relaxed);
}

/// Starts this core's timer: it fires once a tick from now.
pub fn start() {
    rearm();
    unsafe { asm!("msr cntp_ctl_el0, {}", "isb", in(reg) CTL_ENABLE, options(nostack)) };
}

/// Makes this core's timer fire again a tick from now, which also clears its interrupt.
pub fn rearm() {
    let counts = TICK_COUNTS.load(Ordering::Relaxed);
    unsafe { asm!("msr cntp_tval_el0, {}", "isb", in(reg) counts, options(nostack)) };
}
