ENTRY(_start)

SECTIONS {
    . = 0x9000000000200000;
    .text : ALIGN(4K) {
        *(.text.entry)
        *(.text.trap)
        *(.text .text.*)
    }
    .rodata : ALIGN(4K) {
        *(.rodata .rodata.* .srodata .srodata.*)
    }
    .data : ALIGN(4K) {
        *(.data .data.* .sdata .sdata.*)
    }
    .bss (NOLOAD) : ALIGN(4K) {
        *(.bss.stack)
        sbss = .;
        *(.bss .bss.* .sbss .sbss.*)
        ebss = .;
    }
    ekernel = ALIGN(4K);
    /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
