.global _boot
.extern LD_STACK_PTR

.section ".text.boot"

_boot:
    ldr     r0, =LD_STACK_PTR
    mov     sp, r0
    bl      _start
