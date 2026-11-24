    .section .text.entry
    .globl _start
_start:
    la sp, boot_stack_top
    la t0, BOOT_DTB
    sd a1, 0(t0)
    call rust_main

    .section .bss.stack
    .globl boot_stack
boot_stack:
    .space 4096 * 16
    .globl boot_stack_top
boot_stack_top:

    .section .data
    .globl BOOT_DTB
    .align 3
BOOT_DTB:
    .space 8
