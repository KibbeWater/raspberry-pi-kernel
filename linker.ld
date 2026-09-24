ENTRY(_boot)

STACK_SIZE = 0x4000;
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

    . = ALIGN(4096);
    __end = .;

    /DISCARD/ : { *(.comment) *(.ARM.exidx*) }
}
