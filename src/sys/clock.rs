// clock.rs
//! The wall clock: the date and time, once the network has set it by SNTP, in the time zone
//! chosen with `set_zone` (saved in `/timezone.txt`; UTC until then). Until the clock is set
//! nothing knows what day it is; the uptime counts on regardless.

use alloc::format;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use rustypi_core::time::{DateTime, Zone};
use crate::drivers::timer;
use crate::sys::fs::{self, FsError};

/// Where the time zone is kept between boots: its name, like `sv`.
const ZONE_FILE: &str = "/timezone.txt";

/// Unix time, in microseconds, when the uptime was 0; 0 while unset.
static EPOCH_US: AtomicU64 = AtomicU64::new(0);
/// The time zone, as an index into `Zone::ALL`.
static ZONE: AtomicUsize = AtomicUsize::new(0);

/// Sets the clock: it is `unix_us` microseconds after 1970 now.
pub fn set(unix_us: u64) {
    EPOCH_US.store(unix_us.saturating_sub(timer::now_us()).max(1), Ordering::Relaxed);
}

/// Seconds since 1970, if the clock is set.
pub fn unix() -> Option<u64> {
    let epoch = EPOCH_US.load(Ordering::Relaxed);
    (epoch != 0).then(|| (epoch + timer::now_us()) / 1_000_000)
}

/// The local date and time now, and what the time is called (like `CEST`), if the clock is
/// set.
pub fn now() -> Option<(DateTime, &'static str)> {
    unix().map(|unix| zone().local(unix))
}

pub fn zone() -> Zone {
    Zone::ALL[ZONE.load(Ordering::Relaxed)]
}

/// Switches the time zone, now and (saved in `ZONE_FILE`) from the next boot on.
pub fn set_zone(zone: Zone) -> Result<(), FsError> {
    use_zone(zone);
    fs::write_file(ZONE_FILE, format!("{}\n", zone.name()).as_bytes())
}

fn use_zone(zone: Zone) {
    let index = Zone::ALL.iter().position(|&z| z == zone).expect("every zone is in ALL");
    ZONE.store(index, Ordering::Relaxed);
}

/// Picks up the time zone saved by `set_zone`, if there is one. Call once the card is mounted.
pub fn load_zone() {
    let saved = fs::read_file(ZONE_FILE).ok().and_then(|bytes| {
        let name = core::str::from_utf8(&bytes).ok()?.trim().to_ascii_lowercase();
        Zone::from_name(&name)
    });
    if let Some(zone) = saved {
        use_zone(zone);
    }
}
