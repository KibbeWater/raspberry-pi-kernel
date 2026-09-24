//! Time.

use core::time::Duration;
use rustypi_abi::Syscall;
use crate::syscall;

/// Time since the board was reset.
pub fn uptime() -> Duration {
    Duration::from_micros(syscall::call(Syscall::Uptime).unwrap_or(0))
}

/// Sleeps for at least `duration`, letting other tasks run.
pub fn sleep(duration: Duration) {
    let micros = duration.as_micros().min(u64::MAX as u128) as u64;
    let _ = syscall::call(Syscall::Sleep { micros });
}
