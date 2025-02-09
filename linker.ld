ENTRY(_boot)

STACK_SIZE = 0x4000;

SECTIONS
{
    . = 0x8000;
    .text.boot :
    {
        *(.text.boot*)
    }
    . = ALIGN(4096);
    .text :
    {
        *(.text._start)
        *(.text*)
    }
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
    LD_STACK_PTR = .;
    . = . + STACK_SIZE;

    __end = .;
}