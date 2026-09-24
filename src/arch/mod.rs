// arch/mod.rs
//! AArch64 CPU support: boot, exception levels, exceptions, MMU and IRQ masking.

pub mod exception;
pub mod fp;
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

/// The core this code runs on, `0..board::CORES`. Stable while IRQs are masked or preemption
/// is off; otherwise the task may move to another core right after.
#[inline(always)]
pub fn core_id() -> usize {
    let mpidr: u64;
    unsafe { asm!("mrs {}, mpidr_el1", out(reg) mpidr, options(nomem, nostack, preserves_flags)) };
    (mpidr & 0xFF) as usize
}

/// Unmasks IRQs. Also a compiler barrier.
#[inline(always)]
pub fn irq_enable() {
    unsafe { asm!("msr daifclr, #2", options(nostack)) };
}

/// Masks IRQs. Also a compiler barrier.
#[inline(always)]
pub fn irq_disable() {
    unsafe { asm!("msr daifset, #2", options(nostack)) };
}

/// Whether IRQs are unmasked, i.e. this is task code rather than an exception handler or a
/// masked section.
pub fn irqs_enabled() -> bool {
    let daif: u64;
    unsafe { asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) == 0
}

/// Masks IRQs and returns the previous mask state, for `irq_restore`.
#[inline(always)]
pub fn irq_save() -> u64 {
    let daif: u64;
    unsafe { asm!("mrs {}, daif", "msr daifset, #2", out(reg) daif, options(nostack)) };
    daif
}

/// Restores the IRQ mask state returned by `irq_save`.
#[inline(always)]
pub fn irq_restore(daif: u64) {
    unsafe { asm!("msr daif, {}", in(reg) daif, options(nostack)) };
}
