// mailbox.rs
//! VideoCore mailbox, used to query and configure the GPU firmware.

use core::arch::asm;
use core::ptr::{read_volatile, write_volatile};
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

/// Property tags channel (ARM -> VC).
pub const CHANNEL_PROPERTY: u8 = 8;

const REQUEST: u32 = 0;
const RESPONSE_OK: u32 = 0x8000_0000;
const TAG_RESPONSE: u32 = 1 << 31;
const TAG_END: u32 = 0;

/// A message buffer. The firmware needs 16-byte alignment, since the low four bits
/// of the address carry the channel.
#[repr(C, align(16))]
pub struct Message<const N: usize>(pub [u32; N]);

#[inline(always)]
fn read(addr: usize) -> u32 {
    unsafe { read_volatile(addr as *const u32) }
}

#[inline(always)]
fn write(addr: usize, value: u32) {
    unsafe { write_volatile(addr as *mut u32, value) }
}

/// Barrier that also tells the compiler the firmware may read or write `ptr`.
#[inline(always)]
fn sync_buffer(ptr: *mut u32) {
    unsafe { asm!("dsb sy", in("x0") ptr, options(nostack, preserves_flags)) };
}

/// Sends `msg` on `channel` and waits for the firmware to answer in place.
///
/// Returns `false` if the firmware reported an error.
pub fn call<const N: usize>(channel: u8, msg: &mut Message<N>) -> bool {
    let ptr = msg.0.as_mut_ptr();
    // Kernel memory lives in the low 4GB, so the address fits in 32 bits.
    let value = (ptr as usize as u32 & !0xF) | (channel as u32 & 0xF);

    sync_buffer(ptr);
    while read(MBOX1_STATUS) & STATUS_FULL != 0 {}
    write(MBOX1_WRITE, value);

    loop {
        while read(MBOX0_STATUS) & STATUS_EMPTY != 0 {}
        if read(MBOX0_READ) == value {
            break;
        }
    }
    sync_buffer(ptr);

    msg.0[1] == RESPONSE_OK
}

/// Runs a single property tag with one argument and returns the first two words of
/// its response value.
pub fn property(tag: u32, arg: u32) -> Option<[u32; 2]> {
    let mut msg = Message([
        8 * 4,   // buffer size in bytes
        REQUEST,
        tag,
        8,       // value buffer size in bytes
        REQUEST,
        arg,
        0,
        TAG_END,
    ]);
    if call(CHANNEL_PROPERTY, &mut msg) && msg.0[4] & TAG_RESPONSE != 0 {
        Some([msg.0[5], msg.0[6]])
    } else {
        None
    }
}
