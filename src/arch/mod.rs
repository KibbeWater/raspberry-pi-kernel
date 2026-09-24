// arch/mod.rs
//! AArch64 CPU support: boot, exception levels, exceptions, MMU and IRQ masking.

pub mod exception;
pub mod mmu;

use core::arch::{asm, global_asm};

global_asm!(include_str!("boot.s"));
global_asm!(include_str!("exception.s"));

/// Current exception level (1 once `boot.s` has dropped out of EL2).
pub fn exception_level() -> u8 {
    let el: u64;
    unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    (el >> 2 & 3) as u8
}

/// Unmasks IRQs. Also a compiler barrier.
#[inline(always)]
pub fn irq_enable() {
    unsafe { asm!("msr daifclr, #2", options(nostack)) };
}

/// Masks IRQs. Also a compiler barrier, so memory accesses stay inside the masked region.
#[inline(always)]
pub fn irq_disable() {
    unsafe { asm!("msr daifset, #2", options(nostack)) };
}

/// Sleeps until the next interrupt, unless `ready()` already holds.
///
/// IRQs are masked while checking, so an interrupt that arrives between the check and
/// the `wfi` still wakes it; the handler then runs once IRQs are unmasked again.
pub fn wait_for_interrupt_unless(ready: impl FnOnce() -> bool) {
    irq_disable();
    if !ready() {
        unsafe { asm!("wfi", options(nostack)) };
    }
    irq_enable();
}
