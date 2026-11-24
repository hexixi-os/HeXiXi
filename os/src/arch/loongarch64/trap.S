    .section .text.trap, "ax"
    .balign 4096
    .globl __loongarch_kernel_trap
__loongarch_kernel_trap:
    bl trap_from_kernel

    .balign 4096
__loongarch_user_trap:
    # KS0 holds the kernel direct-map pointer to this thread's TrapContext.
    csrwr $sp, 0x30
    .irp n,1,2,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        st.d $r\n, $sp, \n * 8
    .endr
    csrrd $t0, 0x30
    st.d $t0, $sp, 3 * 8
    csrrd $t0, 0x1
    st.d $t0, $sp, 32 * 8
    csrrd $t0, 0x6
    st.d $t0, $sp, 33 * 8

    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fst.d $f\n, $sp, (37 + \n) * 8
    .endr
    movfcsr2gr $t0, $fcsr0
    st.d $t0, $sp, 69 * 8
    move $t0, $zero
    .irp n,0,1,2,3,4,5,6,7
        movcf2gr $t1, $fcc\n
        slli.d $t1, $t1, \n
        or $t0, $t0, $t1
    .endr
    st.d $t0, $sp, 70 * 8

    la.pcrel $t0, __loongarch_kernel_trap
    csrwr $t0, 0xc
    ld.d $t0, $sp, 36 * 8
    ld.d $sp, $sp, 35 * 8
    jr $t0

    .globl __loongarch_restore
__loongarch_restore:
    csrwr $a1, 0x19             # PGDL; PGDH still maps kernel stacks.
    dbar 0
    invtlb 0, $zero, $zero
    ibar 0
    move $sp, $a0
    csrwr $a0, 0x30
    la.pcrel $t0, __loongarch_user_trap
    csrwr $t0, 0xc

    ld.d $t0, $sp, 32 * 8
    csrwr $t0, 0x1
    ld.d $t0, $sp, 33 * 8
    csrwr $t0, 0x6
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld.d $f\n, $sp, (37 + \n) * 8
    .endr
    ld.d $t0, $sp, 69 * 8
    movgr2fcsr $fcsr0, $t0
    ld.d $t0, $sp, 70 * 8
    .irp n,0,1,2,3,4,5,6,7
        srli.d $t1, $t0, \n
        movgr2cf $fcc\n, $t1
    .endr
    .irp n,1,2,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        ld.d $r\n, $sp, \n * 8
    .endr
    ld.d $sp, $sp, 3 * 8
    ertn

    .balign 4096
    .globl __loongarch_refill
__loongarch_refill:
    csrwr $t0, 0x8b             # TLBRSAVE
    # The software tables cover canonical 39-bit virtual addresses.
    # Reject any other address before looking up its lower index bits.
    csrrd $t0, 0x89            # TLBRBADV
    srai.d $t0, $t0, 38
    addi.d $t0, $t0, 1
    sltui $t0, $t0, 2          # Address is valid if the shifted value was -1 or 0.
    beqz $t0, .Lmissing
    csrrd $t0, 0x1b            # PGD selects PGDL or PGDH for the fault VA.
    lddir $t0, $t0, 2
    beqz $t0, .Lmissing
    lddir $t0, $t0, 1
    beqz $t0, .Lmissing
    ldpte $t0, 0
    ldpte $t0, 1
    b .Lfill
.Lmissing:
    csrwr $zero, 0x8c          # TLBRELO0
    csrwr $zero, 0x8d          # TLBRELO1
.Lfill:
    csrrd $t0, 0x8e            # TLBREHI: retain the fault's VPPN.
    bstrins.d $t0, $zero, 5, 0
    ori $t0, $t0, 12
    csrwr $t0, 0x8e
    tlbfill
    csrrd $t0, 0x8b
    ertn
