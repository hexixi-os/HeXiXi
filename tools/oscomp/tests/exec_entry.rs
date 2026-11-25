#![no_std]
#![no_main]

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

#[cfg(target_arch = "riscv64")]
global_asm!(
    ".section .text.entry, \"ax\"",
    ".globl _start",
    "_start:",
    "mv a2, a1",
    "mv a1, a0",
    "mv a0, sp",
    "tail check_entry",
);

#[cfg(target_arch = "loongarch64")]
global_asm!(
    ".section .text.entry, \"ax\"",
    ".globl _start",
    "_start:",
    "move $a2, $a1",
    "move $a1, $a0",
    "move $a0, $sp",
    "b check_entry",
);

#[cfg(target_arch = "riscv64")]
global_asm!(
    ".text",
    ".globl check_clone_sp",
    "check_clone_sp:",
    "mv a2, a1", // Shared result word.
    "mv a1, a0", // Exact child stack pointer.
    "li a0, 0x10f00", // CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD.
    "li a7, 220",
    "ecall",
    "bltz a0, 3f",
    "beqz a0, 2f",
    "1:",
    "lw a0, (a2)",
    "bnez a0, 3f",
    "li a7, 124",
    "ecall",
    "j 1b",
    "2:",
    "xor t0, sp, a1",
    "seqz t0, t0",
    "addi t0, t0, 1",
    "fence rw, w",
    "sw t0, (a2)",
    "li a0, 0",
    "li a7, 93",
    "ecall",
    "unimp",
    "3:",
    "ret",
);

const PROGRAM: &[u8] = b"./exec_entry\0";
const ARG: &[u8] = b"entry-arg\0";

unsafe fn check_arg(stack: *const usize, index: usize, expected: &[u8]) {
    let arg = stack.add(1 + index).read() as *const u8;
    assert_eq!(core::slice::from_raw_parts(arg, expected.len()), expected);
}

unsafe fn syscall(number: usize, mut first: usize, second: usize, third: usize) -> usize {
    #[cfg(target_arch = "riscv64")]
    asm!("ecall", in("a7") number, inout("a0") first, in("a1") second, in("a2") third);
    #[cfg(target_arch = "loongarch64")]
    asm!("syscall 0", in("$a7") number, inout("$a0") first, in("$a1") second, in("$a2") third);
    first
}

fn exit(code: usize) -> ! {
    unsafe { syscall(94, code, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit(2)
}

#[no_mangle]
unsafe extern "C" fn check_entry(stack: *const usize, a0: usize, a1: usize) -> ! {
    assert_eq!((a0, a1), (0, 0));
    assert_eq!(stack as usize % 16, 0);
    let argc = stack.read();
    if argc == 3 {
        // The embedded runner started this probe in place of BusyBox.
        check_arg(stack, 0, b"busybox\0");
        check_arg(stack, 1, b"sh\0");
        check_arg(stack, 2, b"basic_testcode.sh\0");
        let argv = [PROGRAM.as_ptr(), ARG.as_ptr(), core::ptr::null()];
        syscall(221, PROGRAM.as_ptr() as usize, argv.as_ptr() as usize, 0);
        exit(3);
    }
    assert_eq!(argc, 2);
    check_arg(stack, 0, PROGRAM);
    check_arg(stack, 1, ARG);
    assert_eq!(stack.add(3).read(), 0);
    assert_eq!(stack.add(4).read(), 0);

    #[cfg(target_arch = "riscv64")]
    {
        #[repr(C, align(16))]
        struct Stack([u8; 64]);
        extern "C" {
            fn check_clone_sp(stack: *const u8, result: *mut u32) -> isize;
        }
        let stack = Stack([0; 64]);
        let mut result = 0;
        assert_eq!(check_clone_sp(stack.0.as_ptr().add(56), &mut result), 2);
    }

    let output = b"exec-entry-test-ok\n";
    syscall(64, 1, output.as_ptr() as usize, output.len());
    exit(0)
}
