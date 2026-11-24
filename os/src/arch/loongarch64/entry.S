    .section .text.entry
    .globl _start
_start:
    # DA mode masks the upper physical address bits. Enable cached and
    # uncached PLV0 windows before taking the first translated fetch.
    li.d $t0, 0x8000000000000001
    csrwr $t0, 0x180
    li.d $t0, 0x9000000000000011
    csrwr $t0, 0x181
    li.d $t0, 0xb0
    csrwr $t0, 0x0
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
