// mailbox/mod.rs
//! VideoCore mailbox, used to query and configure the GPU firmware.
//!
//! Only the typed property interface is public (`query`, `Batch`). The raw message buffer
//! and hardware access stay private to this module, so every message sent is one that
//! `Batch` built.

mod property;
pub mod tags;

// The driver's whole public API, whether or not the kernel uses every part yet.
#[allow(unused_imports)]
pub use property::{query, Batch, BusAddress, Handle, MailboxError, Replies, Tag, Words};

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, Ordering};
use crate::board::PERIPHERAL_BASE;

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

/// Size of a message buffer in 32-bit words.
const MESSAGE_WORDS: usize = 256;

#[derive(Clone, Copy)]
enum Channel {
    /// Property tags, ARM -> VideoCore.
    Property = 8,
}

/// A message buffer. The firmware needs 16-byte alignment, since the low four bits of the
/// address carry the channel.
#[repr(C, align(16))]
struct Message([u32; MESSAGE_WORDS]);

/// Set while a call is in flight. The kernel runs on one core, so a call can only overlap
/// another if an interrupt handler makes one; that is a bug, and it is caught here rather
/// than by masking IRQs for the whole (possibly slow) firmware round trip.
static IN_USE: AtomicBool = AtomicBool::new(false);

struct InUseGuard;

impl InUseGuard {
    fn acquire() -> Self {
        if IN_USE.load(Ordering::Acquire) {
            panic!("mailbox: call started while another was in flight (from an interrupt handler?)");
        }
        IN_USE.store(true, Ordering::Release);
        InUseGuard
    }
}

impl Drop for InUseGuard {
    fn drop(&mut self) {
        IN_USE.store(false, Ordering::Release);
    }
}

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
    let ptr = msg.0.as_mut_ptr();
    let start = ptr as usize;
    for line in (start..start + size_of::<Message>()).step_by(CACHE_LINE) {
        unsafe { asm!("dc civac, {}", in(reg) line, options(nostack, preserves_flags)) };
    }
    unsafe { asm!("dsb sy", in("x0") ptr, options(nostack, preserves_flags)) };
}

/// Sends `msg` on `channel` and waits for the firmware to answer in place.
fn call(channel: Channel, msg: &mut Message) {
    let _guard = InUseGuard::acquire();
    // Kernel memory lives in the low 1GB, so the address fits in 32 bits, and `Message`
    // is 16-byte aligned, leaving the low four bits for the channel.
    let value = msg.0.as_ptr() as usize as u32 | channel as u32;

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
