/* QEMU virt: RAM starts at 0x40000000, and QEMU puts the DTB at its base
 * when booting a bare-metal ELF with -kernel, so the image starts 2 MiB in. */
ENTRY(_start)

SECTIONS
{
    . = 0x40200000;
    __image_start = .;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(16) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(16) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    .stack (NOLOAD) : ALIGN(4096) {
        . += 4M;
        __stack_top = .;
    }

    /* The heap is the rest of the first 512 MiB of RAM; xtask probe runs
     * QEMU with -m 512M. It is not zeroed. */
    . = ALIGN(4096);
    __heap_start = .;
    __heap_end = 0x60000000;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}
