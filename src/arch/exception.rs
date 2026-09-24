// exception.rs
//! Rust side of the EL1 exception vectors in `exception.s`.
//!
//! Synchronous exceptions in the kernel (bad memory accesses, undefined instructions, ...)
//! are fatal: they are reported through the panic handler, which reboots. The exceptions are
//! `svc #0`, which asks the scheduler to switch tasks, and IRQs; both go to the scheduler,
//! which returns the context to resume. Synchronous exceptions from user programs (system
//! calls and faults) go to `process`, and never bring the kernel down.

use core::arch::asm;
use crate::sched;

/// Registers saved by `exception.s`, in the order it pushes them. A task that isn't running
/// is exactly one of these on its stack.
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
    /// The user stack pointer. Kernel tasks don't use it.
    pub sp_el0: u64,
    /// Keeps the size a multiple of 16, as the stack pointer requires.
    pub _reserved: u64,
}

impl ExceptionContext {
    pub fn class(&self) -> u64 {
        class(self.esr)
    }
}

/// The exception class in ESR_EL1 bits [31:26].
pub fn class(esr: u64) -> u64 {
    esr >> 26
}

/// Whether FAR_EL1 says something for this ESR_EL1 value: the address of a memory abort or a
/// misaligned PC. For other exceptions it holds whatever it last did.
pub fn has_fault_address(esr: u64) -> bool {
    matches!(class(esr), 0x20 | 0x21 | 0x22 | 0x24 | 0x25)
}

/// Names the exception class of an ESR_EL1 value.
pub fn class_name(esr: u64) -> &'static str {
    match class(esr) {
        0x00 => "unknown reason (undefined instruction?)",
        0x07 => "floating point or SIMD instruction",
        0x0E => "illegal execution state",
        0x15 => "svc",
        0x18 => "trapped system register access",
        0x20 | 0x21 => "instruction abort",
        0x22 => "pc alignment fault",
        0x24 | 0x25 => "data abort",
        0x26 => "sp alignment fault",
        0x2F => "serror",
        0x3C => "brk",
        _ => "unhandled exception class",
    }
}

/// The address a memory fault was about (FAR_EL1).
pub fn far() -> u64 {
    let far: u64;
    unsafe { asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack)) };
    far
}

/// ESR_EL1 exception class of `svc` from AArch64.
pub const CLASS_SVC: u64 = 0x15;
/// ESR_EL1 exception class of an FP/SIMD instruction trapped by CPACR_EL1.
pub const CLASS_FP: u64 = 0x07;

#[no_mangle]
extern "C" fn exception_sync(ctx: *mut ExceptionContext, _kind: u64) -> *mut ExceptionContext {
    let esr = unsafe { (*ctx).esr };
    if class(esr) == CLASS_SVC && esr & 0xFFFF == sched::SVC_YIELD as u64 {
        return sched::on_yield(ctx);
    }
    let ctx = unsafe { &*ctx };
    panic!(
        "{} at {:#x} (esr {:#x}, far {:#x})",
        class_name(ctx.esr),
        ctx.elr,
        ctx.esr,
        far(),
    );
}

#[no_mangle]
extern "C" fn exception_user_sync(ctx: *mut ExceptionContext, _kind: u64) -> *mut ExceptionContext {
    crate::process::on_user_sync(ctx)
}

#[no_mangle]
extern "C" fn exception_irq(ctx: *mut ExceptionContext, _kind: u64) -> *mut ExceptionContext {
    sched::on_irq(ctx)
}

#[no_mangle]
extern "C" fn exception_unexpected(ctx: *mut ExceptionContext, kind: u64) -> *mut ExceptionContext {
    let ctx = unsafe { &*ctx };
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
