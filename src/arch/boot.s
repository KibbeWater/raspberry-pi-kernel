.section ".text.boot"

.global _boot
_boot:
    // The firmware starts all four cores here; park everything but core 0.
    mrs     x0, mpidr_el1
    and     x0, x0, #3
    cbnz    x0, .L_park

    // The firmware enters at EL2 (hypervisor). Drop to EL1, where kernels normally run.
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    .L_el1

    // EL1 runs AArch64 and takes its own interrupts (HCR_EL2.RW, IMO/FMO clear).
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    // Don't trap EL1 accesses to the generic timer or FP/SIMD registers.
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33FF
    msr     cptr_el2, x0
    // SCTLR_EL1 reserved-one bits only: MMU and caches off until mmu::enable.
    ldr     x0, =0x30D00800
    msr     sctlr_el1, x0
    // Enter EL1h (own stack pointer) with all exceptions masked.
    mov     x0, #0x3C5
    msr     spsr_el2, x0
    adr     x0, .L_el1
    msr     elr_el2, x0
    eret

.L_el1:
    // Don't trap FP/SIMD at EL1 either, but do at EL0 until a program claims the registers
    // (see arch/fp.rs).
    mov     x0, #(1 << 20)
    msr     cpacr_el1, x0

    // Stack grows down from the top of the reserved region.
    adrp    x0, LD_STACK_PTR
    add     x0, x0, #:lo12:LD_STACK_PTR
    mov     sp, x0

    adrp    x0, exception_vectors
    add     x0, x0, #:lo12:exception_vectors
    msr     vbar_el1, x0
    isb

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
