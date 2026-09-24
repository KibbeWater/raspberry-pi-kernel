// clock.rs
//! The wall clock: the date and time (UTC), once the network has set it by SNTP. Until then
//! nothing knows what day it is; the uptime counts on regardless.

use core::sync::atomic::{AtomicU64, Ordering};
use rustypi_core::time::DateTime;
use crate::drivers::timer;

/// Unix time, in microseconds, when the uptime was 0; 0 while unset.
static EPOCH_US: AtomicU64 = AtomicU64::new(0);

/// Sets the clock: it is `unix_us` microseconds after 1970 now.
pub fn set(unix_us: u64) {
    EPOCH_US.store(unix_us.saturating_sub(timer::now_us()).max(1), Ordering::Relaxed);
}

/// Seconds since 1970, if the clock is set.
pub fn unix() -> Option<u64> {
    let epoch = EPOCH_US.load(Ordering::Relaxed);
    (epoch != 0).then(|| (epoch + timer::now_us()) / 1_000_000)
}

/// The date and time now (UTC), if the clock is set.
pub fn now() -> Option<DateTime> {
    unix().map(DateTime::from_unix)
}
