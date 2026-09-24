// mailbox/mod.rs
//! VideoCore mailbox hardware, used to query and configure the GPU firmware.
//!
//! The message encoding lives in `rustypi_core::mailbox`; this is the `Transport` that
//! delivers its messages:
//!
//! ```ignore
//! let replies = batch.send(&Mailbox)?;
//! ```

// The typed interface, re-exported so drivers only need this module.
pub use rustypi_core::mailbox::{query, tags, Batch, MailboxError};

use rustypi_core::mailbox::{Message, Transport};

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use crate::board::PERIPHERAL_BASE;
use crate::synchronization::TryLock;

const MBOX_BASE: usize = PERIPHERAL_BASE + 0xB880;
/// Mailbox 0 carries firmware -> ARM replies.
const MBOX0_READ: usize = MBOX_BASE + 0x00;
const MBOX0_STATUS: usize = MBOX_BASE + 0x18;
/// Mailbox 1 carries ARM -> firmware requests.
const MBOX1_WRITE: usize = MBOX_BASE + 0x20;
const MBOX1_STATUS: usize = MBOX_BASE + 0x38;

const STATUS_FULL: u32 = 1 << 31;
const STATUS_EMPTY: u32 = 1 << 30;

const CACHE_LINE: usize = 64;

/// Property tags channel, ARM -> VideoCore.
const CHANNEL_PROPERTY: u32 = 8;

/// Held while a call is in flight. It keeps other tasks from preempting a call; finding it
/// held means an interrupt handler started a call during another, which is a bug.
static IN_FLIGHT: TryLock<()> = TryLock::new(());

#[inline(always)]
fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline(always)]
fn write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// Cleans and invalidates the message's cache lines so the firmware, which doesn't see the
/// ARM caches, reads what we wrote and we read what it wrote. Also tells the compiler the
/// firmware may have changed the buffer.
#[inline(always)]
fn sync_message(msg: &mut Message) {
    let ptr = msg.as_mut_ptr();
    let start = ptr as usize;
    for line in (start..start + size_of::<Message>()).step_by(CACHE_LINE) {
        unsafe { asm!("dc civac, {}", in(reg) line, options(nostack, preserves_flags)) };
    }
    unsafe { asm!("dsb sy", in("x0") ptr, options(nostack, preserves_flags)) };
}

/// The hardware mailbox's property channel.
pub struct Mailbox;

impl Transport for Mailbox {
    fn call(&self, msg: &mut Message) {
        IN_FLIGHT
            .try_lock(|_| exchange(msg))
            .expect("mailbox: call started while another was in flight (from an interrupt handler?)");
    }
}

/// Hands `msg` to the firmware and waits for it to answer in place.
fn exchange(msg: &mut Message) {
    // Kernel memory lives in the low 1GB, so the address fits in 32 bits, and `Message` is
    // 16-byte aligned, leaving the low four bits for the channel.
    let value = msg.as_mut_ptr() as usize as u32 | CHANNEL_PROPERTY;

    sync_message(msg);
    while read(MBOX1_STATUS) & STATUS_FULL != 0 {}
    write(MBOX1_WRITE, value);

    loop {
        while read(MBOX0_STATUS) & STATUS_EMPTY != 0 {}
        if read(MBOX0_READ) == value {
            break;
        }
    }
    sync_message(msg);
}
