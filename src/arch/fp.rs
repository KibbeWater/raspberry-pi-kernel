// fp.rs
//! Floating point and SIMD registers for user programs.
//!
//! The kernel is softfloat and never touches the FP/SIMD registers, so each core's keep
//! whatever the last program to use them there left: that program owns them on that core.
//! Programs load their values lazily: EL0 traps on its first FP instruction after a switch
//! (CPACR_EL1.FPEN) unless the registers already hold its values, and the scheduler then
//! loads them. They are saved eagerly, whenever an owner is switched away from, so the saved
//! copy is always current and a program can move to another core. The scheduler does both
//! (`sched::take_fp_registers`); this is the mechanism.

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};
use rustypi_core::sched::TaskId;
use crate::board::CORES;

/// Everything a program can see of the FP/SIMD unit.
#[repr(C, align(16))]
pub struct FpState {
    v: [u128; 32],
    fpcr: u64,
    fpsr: u64,
}

impl FpState {
    /// What a program starts with: zeroes, and round to nearest with no exceptions trapping.
    pub const fn new() -> Self {
        FpState { v: [0; 32], fpcr: 0, fpsr: 0 }
    }
}

/// Per core, the owning task's id plus one; 0 for none.
static OWNER: [AtomicUsize; CORES] = [const { AtomicUsize::new(0) }; CORES];

/// CPACR_EL1.FPEN: EL0 traps, EL1 doesn't. And neither traps.
const FPEN_TRAP_EL0: u64 = 0b01 << 20;
const FPEN_NO_TRAPS: u64 = 0b11 << 20;

/// The task whose values are in `core`'s FP/SIMD registers.
pub fn owner(core: usize) -> Option<TaskId> {
    OWNER[core].load(Ordering::Relaxed).checked_sub(1).map(TaskId)
}

pub fn set_owner(core: usize, task: Option<TaskId>) {
    OWNER[core].store(task.map_or(0, |task| task.0 + 1), Ordering::Relaxed);
}

/// Whether EL0 on this core may use the FP/SIMD registers without trapping.
pub fn allow_el0(allowed: bool) {
    let fpen = if allowed { FPEN_NO_TRAPS } else { FPEN_TRAP_EL0 };
    unsafe { asm!("msr cpacr_el1, {}", "isb", in(reg) fpen, options(nostack)) };
}

/// Copies the FP/SIMD registers into `state`.
pub fn save(state: &mut FpState) {
    let fpcr: u64;
    let fpsr: u64;
    unsafe {
        // The kernel is built without FP, so the assembler needs telling these are fine.
        asm!(
            ".arch_extension fp",
            ".arch_extension simd",
            "stp q0, q1, [{v}, #0]",
            "stp q2, q3, [{v}, #32]",
            "stp q4, q5, [{v}, #64]",
            "stp q6, q7, [{v}, #96]",
            "stp q8, q9, [{v}, #128]",
            "stp q10, q11, [{v}, #160]",
            "stp q12, q13, [{v}, #192]",
            "stp q14, q15, [{v}, #224]",
            "stp q16, q17, [{v}, #256]",
            "stp q18, q19, [{v}, #288]",
            "stp q20, q21, [{v}, #320]",
            "stp q22, q23, [{v}, #352]",
            "stp q24, q25, [{v}, #384]",
            "stp q26, q27, [{v}, #416]",
            "stp q28, q29, [{v}, #448]",
            "stp q30, q31, [{v}, #480]",
            "mrs {fpcr}, fpcr",
            "mrs {fpsr}, fpsr",
            v = in(reg) state.v.as_mut_ptr(),
            fpcr = out(reg) fpcr,
            fpsr = out(reg) fpsr,
            options(nostack),
        );
    }
    state.fpcr = fpcr;
    state.fpsr = fpsr;
}

/// Puts `state` into the FP/SIMD registers.
pub fn load(state: &FpState) {
    unsafe {
        asm!(
            ".arch_extension fp",
            ".arch_extension simd",
            "ldp q0, q1, [{v}, #0]",
            "ldp q2, q3, [{v}, #32]",
            "ldp q4, q5, [{v}, #64]",
            "ldp q6, q7, [{v}, #96]",
            "ldp q8, q9, [{v}, #128]",
            "ldp q10, q11, [{v}, #160]",
            "ldp q12, q13, [{v}, #192]",
            "ldp q14, q15, [{v}, #224]",
            "ldp q16, q17, [{v}, #256]",
            "ldp q18, q19, [{v}, #288]",
            "ldp q20, q21, [{v}, #320]",
            "ldp q22, q23, [{v}, #352]",
            "ldp q24, q25, [{v}, #384]",
            "ldp q26, q27, [{v}, #416]",
            "ldp q28, q29, [{v}, #448]",
            "ldp q30, q31, [{v}, #480]",
            "msr fpcr, {fpcr}",
            "msr fpsr, {fpsr}",
            v = in(reg) state.v.as_ptr(),
            fpcr = in(reg) state.fpcr,
            fpsr = in(reg) state.fpsr,
            options(nostack),
        );
    }
}
