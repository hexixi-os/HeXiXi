#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use user_lib::{close, exec, fork, open, waitpid, OpenFlags};

fn run(path: &str) {
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        let result = exec(path, &[path.as_ptr(), core::ptr::null()]);
        panic!("exec returned {}", result);
    }
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0, "child exit status for {}", path);
}

fn reject(path: &str) {
    reject_with(path, -8);
}

fn reject_with(path: &str, errno: isize) {
    assert_eq!(
        exec(path, &[path.as_ptr(), core::ptr::null()]),
        errno,
        "exec errno for {}",
        path
    );
}

fn reject_bad_argv() {
    const INVALID: usize = 1 << 38;
    let path = b"elf-startup\0";
    let with_bad_string = [INVALID, 0];
    for pointer in [INVALID, with_bad_string.as_ptr() as usize] {
        let result: isize;
        unsafe {
            asm!(
                "ecall",
                in("a7") 221,
                inlateout("a0") path.as_ptr() => result,
                in("a1") pointer,
                in("a2") 0usize,
            );
        }
        assert_eq!(result, -14);
    }
}

#[no_mangle]
fn main() -> i32 {
    run("elf-startup\0");
    let marker = open("startup-only\0", OpenFlags::RDONLY);
    if marker >= 0 {
        close(marker as usize);
        println!("elf-startup-test-ok");
        return 0;
    }

    // Both processes must see the original full file page before their own write.
    run("elf-boundary\0");
    run("elf-boundary\0");
    reject("elf-short-header\0");
    reject("elf-short-segment\0");
    reject("elf-long-interpreter\0");
    reject_with("elf-missing-pt-interp\0", -2);
    reject_with("elf-missing-musl-loader\0", -2);
    reject("elf-invalid-pt-interp\0");
    reject_with("elf-non-elf-pt-interp\0", -80);
    reject_with("elf-overlap-pt-interp\0", -80);
    reject_bad_argv();
    println!("elf-demand-test-ok");
    0
}
