.section ".text.boot"

.global _boot
_boot:
    // The firmware starts all four cores here; park everything but core 0.
    mrs     x0, mpidr_el1
    and     x0, x0, #3
    cbnz    x0, .L_park

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
