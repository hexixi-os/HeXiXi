    .section .text.entry
    .globl _start
_start:
    # QEMU 8.2 enters at 0x9000_0000_0020_0000 while QEMU 10+ enters at
    # physical 0x0020_0000 in DA mode. Keep VSEG 0 mapped via DMW2 only
    # until PC jumps into the cached DMW1 window.
    li.d $t0, 0x8000000000000001
    csrwr $t0, 0x180
    li.d $t0, 0x9000000000000011
    csrwr $t0, 0x181
    li.d $t0, 0x0000000000000011
    csrwr $t0, 0x182
    li.d $t0, 0xb0
    csrwr $t0, 0x0
    la.pcrel $t0, 1f
    li.d $t1, 0x9000000000000000
    or $t0, $t0, $t1
    jirl $zero, $t0, 0
1:
    csrwr $zero, 0x182
    ibar 0
    li.d $t0, 1
    csrwr $t0, 0x2             # EUEN.FPE, for userspace FP context
    la.pcrel $sp, boot_stack_top
    bl rust_main

    .section .bss.stack,"aw",@nobits
    .balign 16
boot_stack:
    .space 4096 * 16
boot_stack_top:
