// interrupt.rs
//! Interrupts: each core's generic timer through the local interrupt controller, and the
//! BCM2835 controller's GPU interrupts, which reach core 0 only.

use core::ptr::{read_volatile, write_volatile};
use crate::board::PERIPHERAL_BASE;
use crate::arch;
use crate::drivers::local;
use crate::drivers::uart::Uart;

const IRQ_BASE: usize = PERIPHERAL_BASE + 0xB200;
const IRQ_PENDING_1: usize = IRQ_BASE + 0x04;
const IRQ_PENDING_2: usize = IRQ_BASE + 0x08;
const IRQ_ENABLE_1: usize = IRQ_BASE + 0x10;
const IRQ_ENABLE_2: usize = IRQ_BASE + 0x14;

/// GPU interrupt numbers (0-63) this kernel uses.
#[derive(Clone, Copy)]
pub enum Irq {
    Uart0 = 57,
}

impl Irq {
    fn register_and_bit(self, low: usize, high: usize) -> (usize, u32) {
        let n = self as u32;
        (if n < 32 { low } else { high }, 1 << (n % 32))
    }
}

pub fn enable(irq: Irq) {
    let (register, bit) = irq.register_and_bit(IRQ_ENABLE_1, IRQ_ENABLE_2);
    unsafe { write_volatile(register as *mut u32, bit) };
}

fn is_pending(irq: Irq) -> bool {
    let (register, bit) = irq.register_and_bit(IRQ_PENDING_1, IRQ_PENDING_2);
    unsafe { read_volatile(register as *const u32) & bit != 0 }
}

/// Which interrupts `handle` serviced.
#[derive(Clone, Copy, Debug, Default)]
pub struct Serviced {
    pub timer: bool,
    pub uart: bool,
}

/// Runs the handler of every interrupt pending on this core. Called from the IRQ vector.
pub fn handle() -> Serviced {
    let sources = local::pending(arch::core_id());
    let serviced = Serviced { timer: sources.timer, uart: sources.gpu && is_pending(Irq::Uart0) };
    if serviced.timer {
        arch::timer::rearm();
    }
    if serviced.uart {
        Uart::handle_interrupt();
    }
    serviced
}
