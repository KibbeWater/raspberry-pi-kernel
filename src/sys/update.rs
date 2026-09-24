// update.rs
//! Installing a new kernel sent over the network (`rustypi_core::update`, driven by the
//! network task): only while armed by the `update` command, only a complete image whose
//! CRC-32 matches and that starts like a RustyPI kernel, and keeping the old one as
//! `/kernel8.bak`. Then the Pi reboots into it.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use crate::drivers::timer;
use crate::sys::fs::{self, FsError};

/// How long `arm` lets an update start, and each datagram of one keeps it going.
pub const ARMED_FOR: Duration = Duration::from_secs(60);

/// A RustyPI kernel's first instruction, `mrs x0, mpidr_el1` (see `arch/boot.s`): a check
/// that an image is a kernel for this board at all, not some other file.
const FIRST_INSTRUCTION: u32 = 0xD538_00A0;

/// Until when (microseconds of uptime) updates are taken; 0 when they aren't.
static ARMED_UNTIL: AtomicU64 = AtomicU64::new(0);

/// Lets an update start in the next `ARMED_FOR`.
pub fn arm() {
    ARMED_UNTIL.store(timer::now_us() + ARMED_FOR.as_micros() as u64, Ordering::Relaxed);
}

pub fn disarm() {
    ARMED_UNTIL.store(0, Ordering::Relaxed);
}

/// Whether an update may go on, keeping it armed if so.
pub fn still_armed() -> bool {
    let now = timer::now_us();
    if now >= ARMED_UNTIL.load(Ordering::Relaxed) {
        return false;
    }
    arm();
    true
}

#[derive(Debug)]
pub enum InstallError {
    NotAKernel,
    Fs(FsError),
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            InstallError::NotAKernel => write!(f, "not a RustyPI kernel image"),
            InstallError::Fs(error) => write!(f, "{error}"),
        }
    }
}

/// Checks `image` looks like a RustyPI kernel, then installs it as `/kernel8.img`, the old
/// one kept as `/kernel8.bak`.
pub fn install(image: &[u8]) -> Result<(), InstallError> {
    let first = image.get(..4).map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()));
    if first != Some(FIRST_INSTRUCTION) {
        return Err(InstallError::NotAKernel);
    }
    fs::install_kernel(image).map_err(InstallError::Fs)
}
