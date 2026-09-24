//! The hardware-independent parts of the RustyPI kernel: protocols, data structures and
//! message encoding. Kept separate so they can be unit tested on the host (`./test.sh`);
//! the kernel supplies the hardware glue.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod block;
pub mod console;
pub mod elf;
pub mod fat;
pub mod font;
pub mod frames;
pub mod graphics;
pub mod heap;
pub mod link;
pub mod lock;
pub mod mailbox;
pub mod mbr;
pub mod paging;
pub mod path;
pub mod sched;
pub mod sd;
pub mod session;
pub mod usb;
