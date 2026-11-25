    .section .text.entry
    .globl _start
_start:
    csrci sstatus, 2
    li t0, {max_harts}
    bgeu a0, t0, .Lpark
    mv tp, a0
    la sp, boot_stacks
    addi t0, a0, 1
    slli t0, t0, 16
    add sp, sp, t0
    la t0, BOOT_DTB
    sd a1, 0(t0)
    tail rust_main

    .globl _start_secondary
    .align 2
_start_secondary:
    csrci sstatus, 2
    li t0, {max_harts}
    bgeu a0, t0, .Lpark
    mv tp, a0
    la sp, boot_stacks
    addi t0, a0, 1
    slli t0, t0, 16
    add sp, sp, t0
    mv a0, a1
    tail rust_secondary

.Lpark:
    wfi
    j .Lpark

    .section .bss.stack
    .balign 16
boot_stacks:
    .space {max_harts} * 65536

    .section .data
    .globl BOOT_DTB
    .align 3
BOOT_DTB:
    .space 8
