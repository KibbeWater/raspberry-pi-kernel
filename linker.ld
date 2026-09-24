ENTRY(_boot)

STACK_SIZE = 0x10000; /* 64KB for the boot task, which becomes the shell */
HEAP_SIZE = 0x1000000; /* 16MB; NOLOAD, so it costs nothing in kernel8.img */

SECTIONS
{
    /* The firmware loads kernel8.img here when arm_64bit=1. */
    . = 0x80000;

    .text :
    {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    . = ALIGN(4096);
    .rodata :
    {
        *(.rodata .rodata.*)
        /* Built-in user programs (process/programs.s), copied to the user window at boot. */
        . = ALIGN(16);
        KEEP(*(.user_image))
    }

    . = ALIGN(4096);
    .data :
    {
        *(.data .data.*)
    }

    . = ALIGN(4096);
    __bss_start = .;
    .bss (NOLOAD) :
    {
        *(.bss .bss.*)
        *(COMMON)
    }
    . = ALIGN(4096);
    __bss_end = .;
    __bss_size = __bss_end - __bss_start;

    /* Stack grows down, so LD_STACK_PTR marks the top of the region. */
    .stack (NOLOAD) :
    {
        . = . + STACK_SIZE;
    }
    . = ALIGN(16);
    LD_STACK_PTR = .;

    . = ALIGN(4096);
    .heap (NOLOAD) :
    {
        __heap_start = .;
        . = . + HEAP_SIZE;
        __heap_end = .;
    }
    __heap_size = __heap_end - __heap_start;

    /* Backs the user window: mmu.rs maps it at 0x80000000 for user programs. One 2MB block. */
    . = ALIGN(0x200000);
    .user (NOLOAD) :
    {
        __user_phys = .;
        . = . + 0x200000;
    }

    . = ALIGN(4096);
    __end = .;

    /DISCARD/ : { *(.comment) *(.ARM.exidx*) }
}
