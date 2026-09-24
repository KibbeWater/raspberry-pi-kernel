// Exception vector table for EL1. Each entry saves the interrupted registers as an
// `ExceptionContext` on the stack and calls a Rust handler with it. The handler returns the
// context to resume: usually the same one, or another task's saved context to switch to it.
// `sched_finish_switch` runs on the resumed context's stack, below the context, which
// `exception_restore` then pops.

// Fills one 128-byte vector slot:
// \handler(ctx: *mut ExceptionContext, kind: u64) -> *mut ExceptionContext.
.macro VECTOR handler, kind
.balign 0x80
    sub     sp, sp, #16 * 18
    stp     x0, x1, [sp, #16 * 0]
    stp     x2, x3, [sp, #16 * 1]
    stp     x4, x5, [sp, #16 * 2]
    stp     x6, x7, [sp, #16 * 3]
    stp     x8, x9, [sp, #16 * 4]
    stp     x10, x11, [sp, #16 * 5]
    stp     x12, x13, [sp, #16 * 6]
    stp     x14, x15, [sp, #16 * 7]
    stp     x16, x17, [sp, #16 * 8]
    stp     x18, x19, [sp, #16 * 9]
    stp     x20, x21, [sp, #16 * 10]
    stp     x22, x23, [sp, #16 * 11]
    stp     x24, x25, [sp, #16 * 12]
    stp     x26, x27, [sp, #16 * 13]
    stp     x28, x29, [sp, #16 * 14]
    mrs     x1, elr_el1
    mrs     x2, spsr_el1
    mrs     x3, esr_el1
    stp     x30, x1, [sp, #16 * 15]
    stp     x2, x3, [sp, #16 * 16]
    mrs     x4, sp_el0
    str     x4, [sp, #16 * 17]
    mov     x0, sp
    mov     x1, #\kind
    bl      \handler
    mov     sp, x0
    // Off the stack of any task this core switched away from: other cores may take it now.
    bl      sched_finish_switch
    b       exception_restore
.endm

.section ".text.exception_vectors", "ax"
.balign 0x800
.global exception_vectors
exception_vectors:
    // Current EL with SP_EL0: never used, the kernel runs on SP_EL1.
    VECTOR exception_unexpected, 0
    VECTOR exception_unexpected, 1
    VECTOR exception_unexpected, 2
    VECTOR exception_unexpected, 3
    // Current EL with SP_EL1: the kernel itself.
    VECTOR exception_sync, 4
    VECTOR exception_irq, 5
    VECTOR exception_unexpected, 6
    VECTOR exception_unexpected, 7
    // Lower EL, AArch64: user programs. They arrive on the kernel stack of their task.
    VECTOR exception_user_sync, 8
    VECTOR exception_irq, 9
    VECTOR exception_unexpected, 10
    VECTOR exception_unexpected, 11
    // Lower EL, AArch32: never used.
    VECTOR exception_unexpected, 12
    VECTOR exception_unexpected, 13
    VECTOR exception_unexpected, 14
    VECTOR exception_unexpected, 15

exception_restore:
    ldr     x19, [sp, #16 * 17]
    msr     sp_el0, x19
    ldp     x19, x20, [sp, #16 * 16]
    msr     spsr_el1, x19
    ldp     x30, x20, [sp, #16 * 15]
    msr     elr_el1, x20
    ldp     x0, x1, [sp, #16 * 0]
    ldp     x2, x3, [sp, #16 * 1]
    ldp     x4, x5, [sp, #16 * 2]
    ldp     x6, x7, [sp, #16 * 3]
    ldp     x8, x9, [sp, #16 * 4]
    ldp     x10, x11, [sp, #16 * 5]
    ldp     x12, x13, [sp, #16 * 6]
    ldp     x14, x15, [sp, #16 * 7]
    ldp     x16, x17, [sp, #16 * 8]
    ldp     x18, x19, [sp, #16 * 9]
    ldp     x20, x21, [sp, #16 * 10]
    ldp     x22, x23, [sp, #16 * 11]
    ldp     x24, x25, [sp, #16 * 12]
    ldp     x26, x27, [sp, #16 * 13]
    ldp     x28, x29, [sp, #16 * 14]
    add     sp, sp, #16 * 18
    eret
