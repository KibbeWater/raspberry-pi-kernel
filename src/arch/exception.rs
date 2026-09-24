// exception.rs
//! Rust side of the EL1 exception vectors in `exception.s`.
//!
//! Synchronous exceptions (bad memory accesses, undefined instructions, ...) are fatal:
//! they are reported through the panic handler, which reboots. IRQs are handed to the
//! interrupt controller driver.

use core::arch::asm;
use crate::drivers::interrupt;

/// Registers saved by `exception.s`, in the order it pushes them.
#[repr(C)]
pub struct ExceptionContext {
    /// x0 to x29.
    pub gpr: [u64; 30],
    /// x30, the link register.
    pub lr: u64,
    /// Address of the faulting instruction, or where to resume after an IRQ.
    pub elr: u64,
    pub spsr: u64,
    /// Exception syndrome: what happened.
    pub esr: u64,
}

/// Names the exception class in ESR_EL1 bits [31:26].
fn class_name(esr: u64) -> &'static str {
    match esr >> 26 {
        0x00 => "unknown reason (undefined instruction?)",
        0x0E => "illegal execution state",
        0x15 => "svc",
        0x18 => "trapped system register access",
        0x21 => "instruction abort",
        0x22 => "pc alignment fault",
        0x25 => "data abort",
        0x26 => "sp alignment fault",
        0x2F => "serror",
        0x3C => "brk",
        _ => "unhandled exception class",
    }
}

fn far() -> u64 {
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack)) };
    far
}

#[no_mangle]
extern "C" fn exception_sync(ctx: &mut ExceptionContext, _kind: u64) {
    panic!(
        "{} at {:#x} (esr {:#x}, far {:#x})",
        class_name(ctx.esr),
        ctx.elr,
        ctx.esr,
        far(),
    );
}

#[no_mangle]
extern "C" fn exception_irq(_ctx: &mut ExceptionContext, _kind: u64) {
    interrupt::handle();
}

#[no_mangle]
extern "C" fn exception_unexpected(ctx: &mut ExceptionContext, kind: u64) {
    const KINDS: [&str; 4] = ["sync", "irq", "fiq", "serror"];
    const ORIGINS: [&str; 4] = ["el1 on sp_el0", "el1", "el0 aarch64", "el0 aarch32"];
    panic!(
        "unexpected {} from {} at {:#x} (esr {:#x})",
        KINDS[kind as usize % 4],
        ORIGINS[kind as usize / 4 % 4],
        ctx.elr,
        ctx.esr,
    );
}
