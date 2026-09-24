// Drops from EL2 (hypervisor), where the firmware starts every core, to EL1, where kernels
// normally run, continuing at \el1. Carries straight on at \el1 on a core already at EL1.
.macro DROP_TO_EL1 el1
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    \el1

    // EL1 runs AArch64 and takes its own interrupts (HCR_EL2.RW, IMO/FMO clear).
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    // Don't trap EL1 accesses to the generic timer or FP/SIMD registers.
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33FF
    msr     cptr_el2, x0
    // SCTLR_EL1 reserved-one bits only: MMU and caches off until mmu::enable/turn_on.
    ldr     x0, =0x30D00800
    msr     sctlr_el1, x0
    // Enter EL1h (own stack pointer) with all exceptions masked.
    mov     x0, #0x3C5
    msr     spsr_el2, x0
    adr     x0, \el1
    msr     elr_el2, x0
    eret
.endm

// At EL1: FP/SIMD free for EL1 but trapped at EL0 until a program claims the registers (see
// arch/fp.rs), and exceptions going to `exception_vectors`.
.macro EL1_SETUP
    mov     x0, #(1 << 20)
    msr     cpacr_el1, x0
    adrp    x0, exception_vectors
    add     x0, x0, #:lo12:exception_vectors
    msr     vbar_el1, x0
    isb
.endm

.section ".text.boot"

// The firmware starts core 0 here. Cores 1 to 3 wait in the firmware's spin loop until
// sys::cores sends them to `_secondary_start`.
.global _boot
_boot:
    // Only core 0 belongs here (in case a firmware starts them all here after all).
    mrs     x0, mpidr_el1
    and     x0, x0, #0xFF
    cbnz    x0, .L_park

    DROP_TO_EL1 .L_el1
.L_el1:
    EL1_SETUP

    // Stack grows down from the top of the reserved region.
    adrp    x0, LD_STACK_PTR
    add     x0, x0, #:lo12:LD_STACK_PTR
    mov     sp, x0

    // Zero .bss (bounds are page aligned, so 8-byte stores are fine).
    adrp    x0, __bss_start
    add     x0, x0, #:lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, #:lo12:__bss_end
.L_bss_loop:
    cmp     x0, x1
    b.hs    .L_bss_done
    str     xzr, [x0], #8
    b       .L_bss_loop
.L_bss_done:

    bl      kernel_main

.L_park:
    wfe
    b       .L_park

// Cores 1 to 3 start here, with the MMU and caches off, so they read their stack's top from
// memory: sys::cores cleans it there from its cache before sending them.
.global _secondary_start
_secondary_start:
    DROP_TO_EL1 .L_secondary_el1
.L_secondary_el1:
    EL1_SETUP

    mrs     x0, mpidr_el1
    and     x0, x0, #0xFF
    adrp    x1, SECONDARY_STACK_TOPS
    add     x1, x1, #:lo12:SECONDARY_STACK_TOPS
    ldr     x1, [x1, x0, lsl #3]
    mov     sp, x1

    bl      secondary_main
    b       .L_park
