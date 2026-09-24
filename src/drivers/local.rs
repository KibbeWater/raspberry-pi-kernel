// local.rs
//! The ARM local peripherals of the BCM2836 family (QA7): per-core interrupt routing for the
//! cores' generic timers, and the interrupt sources each core sees. GPU interrupts, like the
//! UART's, reach core 0 only.

use crate::board::LOCAL_PERIPHERAL_BASE;
use crate::drivers::mmio::{read, write};

/// Per core, 4 bytes apart: which of its generic timers raise an IRQ.
const CORE_TIMER_IRQ_CONTROL: usize = LOCAL_PERIPHERAL_BASE + 0x40;
/// Per core, 4 bytes apart: what is interrupting it.
const CORE_IRQ_SOURCE: usize = LOCAL_PERIPHERAL_BASE + 0x60;

/// The non-secure EL1 physical timer (CNTP), in both registers.
const PHYSICAL_TIMER: u32 = 1 << 1;
/// Any GPU interrupt, in the source register.
const GPU: u32 = 1 << 8;

/// Routes `core`'s EL1 physical timer to it as an IRQ.
pub fn route_timer(core: usize) {
    write(CORE_TIMER_IRQ_CONTROL + 4 * core, PHYSICAL_TIMER);
}

/// What is interrupting `core`.
#[derive(Clone, Copy, Debug)]
pub struct Sources {
    pub timer: bool,
    pub gpu: bool,
}

pub fn pending(core: usize) -> Sources {
    let source = read(CORE_IRQ_SOURCE + 4 * core);
    Sources { timer: source & PHYSICAL_TIMER != 0, gpu: source & GPU != 0 }
}
