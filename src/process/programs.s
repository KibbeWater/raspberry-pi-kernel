// Built-in user programs. They run at EL0 from a copy at the start of the user window, so
// they may only address memory PC-relatively, and reach the kernel only through svc.
// Names in braces are system call numbers and error codes from rustypi-abi.

// Makes a system call: arguments in x0 to x5, result in x0.
.macro USER_SYSCALL number
    mov     x8, #\number
    svc     #0
.endm

.pushsection .user_image, "a"
.balign 16
.global user_image_start
user_image_start:

// Prints a line and exits 0.
.balign 4
.global user_hello
user_hello:
    adr     x0, .Lhello_text
    adr     x1, .Lhello_end
    sub     x1, x1, x0
    USER_SYSCALL {WRITE}
    mov     x0, #0
    USER_SYSCALL {EXIT}
.Lhello_text:
    .ascii  "hello from EL0\n"
.Lhello_end:

// Ticks three times, sleeping in between, with state kept in callee-saved registers, the
// stack pointer and on the stack. Exits 0 if all of it survived the task switches.
.balign 4
.global user_ticker
user_ticker:
    mov     x19, #3                 // ticks left
    movz    x20, #0x1234, lsl #16   // a pattern that must survive
    movk    x20, #0x5678
    mov     x21, sp
    sub     sp, sp, #16
    str     x20, [sp]
.Lticker_loop:
    adr     x0, .Lticker_text
    mov     x1, #5
    USER_SYSCALL {WRITE}
    mov     x0, #50000              // 50ms
    USER_SYSCALL {SLEEP}
    subs    x19, x19, #1
    b.ne    .Lticker_loop
    ldr     x9, [sp]
    add     sp, sp, #16
    mov     x0, #1
    cmp     x9, x20
    b.ne    .Lticker_exit
    cmp     sp, x21
    b.ne    .Lticker_exit
    movz    x10, #0x1234, lsl #16
    movk    x10, #0x5678
    cmp     x20, x10
    b.ne    .Lticker_exit
    mov     x0, #0
.Lticker_exit:
    USER_SYSCALL {EXIT}
.Lticker_text:
    .ascii  "tick\n"

// Checks that the kernel refuses bad system calls. Exits 0, or the number of the first
// check that failed.
.balign 4
.global user_abi
user_abi:
    // 1: writing from kernel memory is a fault.
    mov     x19, #1
    mov     x0, #0x80000
    mov     x1, #4
    USER_SYSCALL {WRITE}
    cmn     x0, #{EFAULT}
    b.ne    .Labi_fail
    // 2: so is a buffer running off the end of the user window (2MB from its start).
    mov     x19, #2
    adr     x0, user_image_start
    add     x0, x0, #0x200, lsl #12
    sub     x0, x0, #2
    mov     x1, #4
    USER_SYSCALL {WRITE}
    cmn     x0, #{EFAULT}
    b.ne    .Labi_fail
    // 3: an unknown system call number is NoSys.
    mov     x19, #3
    USER_SYSCALL 999
    cmn     x0, #{ENOSYS}
    b.ne    .Labi_fail
    // 4: an exit code that isn't an i32 is Invalid, and doesn't exit.
    mov     x19, #4
    mov     x0, #1
    lsl     x0, x0, #32
    USER_SYSCALL {EXIT}
    cmn     x0, #{EINVAL}
    b.ne    .Labi_fail
    // 5: an empty write writes nothing.
    mov     x19, #5
    adr     x0, .Lhello_text
    mov     x1, #0
    USER_SYSCALL {WRITE}
    cbnz    x0, .Labi_fail
    // 6: uptime moves forward across a 1ms sleep, which returns 0.
    mov     x19, #6
    USER_SYSCALL {UPTIME}
    mov     x20, x0
    mov     x0, #1000
    USER_SYSCALL {SLEEP}
    cbnz    x0, .Labi_fail
    USER_SYSCALL {UPTIME}
    sub     x0, x0, x20
    cmp     x0, #1000
    b.lo    .Labi_fail
    mov     x0, #0
    USER_SYSCALL {EXIT}
.Labi_fail:
    mov     x0, x19
    USER_SYSCALL {EXIT}

// Busy-loops for a while without system calls, so only the timer can take the CPU from it.
.balign 4
.global user_spin
user_spin:
    mov     x0, #0x4000000
.Lspin_loop:
    subs    x0, x0, #1
    b.ne    .Lspin_loop
    USER_SYSCALL {EXIT}

// Reads address 0, which is kernel memory: killed by a data abort.
.balign 4
.global user_fault
user_fault:
    mov     x0, #0
    ldr     x0, [x0]
    USER_SYSCALL {EXIT}

// Reads SCTLR_EL1, which EL0 may not: killed as an undefined instruction.
.balign 4
.global user_privileged
user_privileged:
    mrs     x0, sctlr_el1
    USER_SYSCALL {EXIT}

// Uses a floating point register, which traps at EL0: killed.
.balign 4
.global user_float
user_float:
    .inst   0x1e6e1000              // fmov d0, #1.0, which the softfloat assembler refuses
    mov     x0, #0
    USER_SYSCALL {EXIT}

.balign 4
.global user_image_end
user_image_end:
.popsection
