#![allow(unused)]

use core::arch::asm;

const SBI_SET_TIMER: usize = 0;
const SBI_CONSOLE_PUTCHAR: usize = 1;
const SBI_CONSOLE_GETCHAR: usize = 2;
const SBI_CLEAR_IPI: usize = 3;
const SBI_SEND_IPI: usize = 4;
const SBI_REMOTE_FENCE_I: usize = 5;
const SBI_REMOTE_SFENCE_VMA: usize = 6;
const SBI_REMOTE_SFENCE_VMA_ASID: usize = 7;
const SBI_SHUTDOWN: usize = 8;

#[inline(always)]
fn sbi_call(which: usize, arg0: usize, arg1: usize, arg2: usize) -> usize {
    let mut ret;
    unsafe {
        asm!(
            "li x16, 0",
            "ecall",
            inlateout("x10") arg0 => ret,
            in("x11") arg1,
            in("x12") arg2,
            in("x17") which,
        );
    }
    ret
}

pub fn set_timer(timer: usize) {
    sbi_call(SBI_SET_TIMER, timer, 0, 0);
}

pub fn console_putchar(c: usize) {
    sbi_call(SBI_CONSOLE_PUTCHAR, c, 0, 0);
}

pub fn console_getchar() -> usize {
    sbi_call(SBI_CONSOLE_GETCHAR, 0, 0, 0)
}

pub fn shutdown() -> ! {
    sbi_call(SBI_SHUTDOWN, 0, 0, 0);
    panic!("It should shutdown!");
}

#[cfg(feature = "smp")]
fn call(extension: usize, function: usize, args: [usize; 4]) -> Result<(), isize> {
    let error: isize;
    unsafe {
        asm!(
            "ecall",
            inlateout("a0") args[0] => error,
            inlateout("a1") args[1] => _,
            in("a2") args[2],
            in("a3") args[3],
            in("a6") function,
            in("a7") extension,
        );
    }
    if error == 0 {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(feature = "smp")]
pub(super) fn hart_start(hart: usize, entry: usize, opaque: usize) -> Result<(), isize> {
    const HSM: usize = 0x0048_534d;
    call(HSM, 0, [hart, entry, opaque, 0])
}

#[cfg(feature = "smp")]
pub(super) fn remote_sfence_vma(hart_mask: usize) -> Result<(), isize> {
    const RFENCE: usize = 0x5246_4e43;
    call(RFENCE, 1, [hart_mask, 0, 0, usize::MAX])
}
