ENTRY(_boot)

STACK_SIZE = 0x4000;
HEAP_SIZE = 0x100000;

SECTIONS
{

    .text.boot :
    {
        . = 0x8000;
        *(.text.boot*)
    }
    . = ALIGN(4096);
    .text :
    {
        *(.text._start)
        *(.text*)
    }

    . = ALIGN(4096);
    LD_STACK_PTR = .;
    . = . + STACK_SIZE;

    . = ALIGN(4096);
    .rodata :
    {
        *(.rodata)
    }
    . = ALIGN(4096);
    .data :
    {
        *(.data)
    }
    . = ALIGN(4096);
    __bss_start = .;
    .bss :
    {
        bss = .;
        *(.bss)
    }
    . = ALIGN(4096);
    __bss_end = .;
    __bss_size = __bss_end - __bss_start;

    . = ALIGN(4096);

    .heap (NOLOAD) :
    {
        __heap_start = .;
        . = . + HEAP_SIZE;
        __heap_end = .;
        __heap_size = __heap_end - __heap_start;
    }

    . = ALIGN(4096);

    __end = .;
}