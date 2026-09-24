// local.rs
//! The ARM local peripherals of the BCM2836 family (QA7): per-core interrupt routing for the
//! cores' generic timers, mailboxes one core can interrupt another with, and the interrupt
//! sources each core sees. GPU interrupts, like the UART's, reach core 0 only.

use crate::board::LOCAL_PERIPHERAL_BASE;
use crate::drivers::mmio::{read, write};

/// Per core, 4 bytes apart: which of its generic timers raise an IRQ.
const CORE_TIMER_IRQ_CONTROL: usize = LOCAL_PERIPHERAL_BASE + 0x40;
/// Per core, 4 bytes apart: which of its mailboxes raise an IRQ.
const CORE_MAILBOX_IRQ_CONTROL: usize = LOCAL_PERIPHERAL_BASE + 0x50;
/// Per core, 4 bytes apart: what is interrupting it.
const CORE_IRQ_SOURCE: usize = LOCAL_PERIPHERAL_BASE + 0x60;
/// Per core, 16 bytes apart: writing sets bits of its mailbox 0, and a set bit interrupts it.
const CORE_MAILBOX0_SET: usize = LOCAL_PERIPHERAL_BASE + 0x80;
/// Per core, 16 bytes apart: reading gives its mailbox 0, writing clears bits of it.
const CORE_MAILBOX0_CLEAR: usize = LOCAL_PERIPHERAL_BASE + 0xC0;

/// The non-secure EL1 physical timer (CNTP), in both registers.
const PHYSICAL_TIMER: u32 = 1 << 1;
/// Mailbox 0, in the control register and the source register.
const MAILBOX0_CONTROL: u32 = 1 << 0;
const MAILBOX0_SOURCE: u32 = 1 << 4;
/// Any GPU interrupt, in the source register.
const GPU: u32 = 1 << 8;

/// Routes `core`'s EL1 physical timer to it as an IRQ.
pub fn route_timer(core: usize) {
    write(CORE_TIMER_IRQ_CONTROL + 4 * core, PHYSICAL_TIMER);
}

/// Lets other cores interrupt `core` through its mailbox 0.
pub fn enable_ipi(core: usize) {
    write(CORE_MAILBOX_IRQ_CONTROL + 4 * core, MAILBOX0_CONTROL);
}

/// Interrupts `core`, as a nudge to look for something to do.
pub fn send_ipi(core: usize) {
    write(CORE_MAILBOX0_SET + 16 * core, 1);
}

/// Takes `core`'s nudges back, once it has been interrupted by them.
pub fn clear_ipi(core: usize) {
    write(CORE_MAILBOX0_CLEAR + 16 * core, u32::MAX);
}

/// What is interrupting `core`.
#[derive(Clone, Copy, Debug)]
pub struct Sources {
    pub timer: bool,
    pub ipi: bool,
    pub gpu: bool,
}

pub fn pending(core: usize) -> Sources {
    let source = read(CORE_IRQ_SOURCE + 4 * core);
    Sources {
        timer: source & PHYSICAL_TIMER != 0,
        ipi: source & MAILBOX0_SOURCE != 0,
        gpu: source & GPU != 0,
    }
}
