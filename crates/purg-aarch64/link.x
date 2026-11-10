/* The image layout shared by purg-kernel and purg-probe (see
 * purg_aarch64::boot): loaded at physical 0x40200000 (QEMU virt RAM starts
 * at 0x40000000, and QEMU puts the DTB there for a bare-metal ELF), linked
 * at KERNEL_VBASE + physical in the TTBR1 high half. The boot code runs at
 * its physical address until the MMU is on, so it uses only PC-relative
 * addressing; the ELF entry point is the physical address of _start.
 * KERNEL_VBASE must equal purg_aarch64::mmu::KERNEL_VBASE. */
KERNEL_VBASE = 0xffff000000000000;
KERNEL_PHYS = 0x40200000;

ENTRY(_start_phys)

SECTIONS
{
    . = KERNEL_VBASE + KERNEL_PHYS;
    __image_start = .;

    .text : AT(ADDR(.text) - KERNEL_VBASE) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    . = ALIGN(4096);
    __text_end = .;

    .rodata : AT(ADDR(.rodata) - KERNEL_VBASE) {
        *(.rodata .rodata.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.got .got.*)
    }
    . = ALIGN(4096);
    __rodata_end = .;

    .data : AT(ADDR(.data) - KERNEL_VBASE) {
        *(.data .data.*)
    }

    .bss (NOLOAD) : ALIGN(16) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }
    . = ALIGN(4096);
    __data_end = .;

    /* One unmapped guard page, then the boot stack. */
    . += 4096;
    .stack (NOLOAD) : ALIGN(4096) {
        __stack_bottom = .;
        . += 64K;
        __stack_top = .;
    }
    . = ALIGN(4096);
    __image_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
        *(.eh_frame .eh_frame_hdr)
    }
}

_start_phys = _head - KERNEL_VBASE;
__image_size = __image_end - __image_start;
