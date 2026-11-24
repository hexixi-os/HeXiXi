    .section .text
    .globl __switch
__switch:
    st.d $ra, $a0, 0
    st.d $sp, $a0, 8
    # LoongArch callee-saved registers are r22 (fp) through r31 (s8).
    .irp n,22,23,24,25,26,27,28,29,30,31
        st.d $r\n, $a0, (\n - 20) * 8
    .endr

    ld.d $ra, $a1, 0
    .irp n,22,23,24,25,26,27,28,29,30,31
        ld.d $r\n, $a1, (\n - 20) * 8
    .endr
    ld.d $sp, $a1, 8
    ret
