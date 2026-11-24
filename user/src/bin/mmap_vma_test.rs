#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use user_lib::{close, exit, fork, open, read, waitpid, write, yield_, OpenFlags};

const PAGE: usize = 4096;
const READ: usize = 1;
const WRITE: usize = 2;
const SHARED: usize = 1;
const PRIVATE: usize = 2;
const ANONYMOUS: usize = 0x20;
const FIXED: usize = 0x10;
const FIXED_NOREPLACE: usize = 0x100000;

fn syscall(number: usize, args: [usize; 6]) -> isize {
    let result;
    unsafe {
        asm!("ecall", in("a7") number, inlateout("a0") args[0] => result,
            in("a1") args[1], in("a2") args[2], in("a3") args[3],
            in("a4") args[4], in("a5") args[5]);
    }
    result
}

fn mmap(addr: usize, len: usize, prot: usize, flags: usize, fd: isize, offset: usize) -> isize {
    syscall(222, [addr, len, prot, flags, fd as usize, offset])
}

fn mprotect(addr: usize, len: usize, prot: usize) -> isize {
    syscall(226, [addr, len, prot, 0, 0, 0])
}

fn munmap(addr: usize, len: usize) -> isize {
    syscall(215, [addr, len, 0, 0, 0, 0])
}

fn load(addr: usize) -> u8 {
    unsafe { (addr as *const u8).read_volatile() }
}

fn store(addr: usize, value: u8) {
    unsafe { (addr as *mut u8).write_volatile(value) }
}

fn clone_with_clear_tid(addr: usize) -> isize {
    const CLONE_FLAGS: usize = 0x0021_0f00; // VM | FS | FILES | SIGHAND | THREAD | CHILD_CLEARTID
    let mut stack = [0u8; 1024];
    let stack_top = (stack.as_mut_ptr() as usize + stack.len()) & !15;
    let tid;
    unsafe {
        // The child exits from assembly without accessing its supplied stack.
        asm!(
            "ecall",
            "bnez a0, 2f",
            "li a7, 93",
            "ecall",
            "unimp",
            "2:",
            inlateout("a0") CLONE_FLAGS => tid,
            in("a1") stack_top, in("a2") 0usize, in("a3") 0usize, in("a4") addr,
            inlateout("a7") 220usize => _,
        );
    }
    tid
}

fn expect_signal(addr: usize, write_access: bool, signal: i32) {
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        if write_access {
            store(addr, b'!');
        } else {
            load(addr);
        }
        exit(123);
    }
    let mut status = 0;
    assert_eq!(
        syscall(
            260,
            [pid as usize, &mut status as *mut i32 as usize, 0, 0, 0, 0]
        ),
        pid
    );
    assert_eq!(status, signal);
}

#[no_mangle]
fn main() -> i32 {
    let dir = open("/\0", OpenFlags::RDONLY);
    assert!(dir >= 0);
    assert_eq!(mmap(0, PAGE, READ, PRIVATE, dir, 0), -19);
    assert_eq!(close(dir as usize), 0);
    let fd = open("mmap-fixture.bin\0", OpenFlags::RDONLY);
    assert_eq!(fd, dir);
    let base = mmap(0, 4 * PAGE, READ | WRITE, PRIVATE, fd, PAGE);
    let mirror = mmap(0, PAGE, READ, PRIVATE, fd, PAGE);
    let tail = mmap(0, 2 * PAGE, READ, PRIVATE, fd, 4 * PAGE);
    let kernel_copy = mmap(0, PAGE, READ | WRITE, PRIVATE, fd, PAGE);
    let cold_kernel_copy = mmap(0, PAGE, READ | WRITE, PRIVATE, fd, PAGE);
    let pipe_copy = mmap(0, PAGE, READ | WRITE, PRIVATE, fd, PAGE);
    let clear_tid_copy = mmap(0, PAGE, READ | WRITE, PRIVATE, fd, PAGE);
    assert!(base > 0 && mirror > 0 && tail > 0 && kernel_copy > 0 && cold_kernel_copy > 0);
    assert!(pipe_copy > 0 && clear_tid_copy > 0);
    let (base, mirror, tail) = (base as usize, mirror as usize, tail as usize);
    let (kernel_copy, cold_kernel_copy) = (kernel_copy as usize, cold_kernel_copy as usize);
    let (pipe_copy, clear_tid_copy) = (pipe_copy as usize, clear_tid_copy as usize);
    assert_eq!(load(base), b'B', "nonzero file offset");
    assert_eq!(
        mmap(base, PAGE, READ, PRIVATE | FIXED_NOREPLACE, fd, PAGE),
        -17
    );
    assert_eq!(mmap(0, PAGE, READ, PRIVATE, fd, 1), -22);
    assert_eq!(mmap(0, PAGE, READ, PRIVATE, -1, 0), -9);
    assert_eq!(mmap(0, PAGE, READ | WRITE, SHARED, fd, 0), -13);
    let shared_read = mmap(0, PAGE, READ, SHARED, fd, 0);
    assert!(shared_read > 0);
    assert_eq!(mprotect(shared_read as usize, PAGE, READ | WRITE), -13);
    assert_eq!(close(fd as usize), 0);
    assert_eq!(load(shared_read as usize), b'A');
    assert_eq!(load(kernel_copy), b'B');
    let child = fork();
    assert!(child >= 0);
    if child == 0 {
        store(kernel_copy, b'F');
        assert_eq!(load(kernel_copy), b'F');
        exit(0);
    }
    let mut status = 0;
    assert_eq!(waitpid(child as usize, &mut status), child);
    assert_eq!(status, 0);
    assert_eq!(load(kernel_copy), b'B');

    assert_eq!(mprotect(base + PAGE, PAGE, 0), 0);
    expect_signal(base + PAGE, false, 11);
    assert_eq!(load(base + 2 * PAGE), b'D');
    store(base, b'Z');
    assert_eq!(load(mirror), b'B');
    assert_eq!(mprotect(base + PAGE, PAGE, READ), 0);
    assert_eq!(load(base + PAGE), b'C');
    expect_signal(base + PAGE, true, 11);
    assert_eq!(munmap(base + PAGE, PAGE), 0);
    assert_eq!(mprotect(base, 3 * PAGE, READ), -12);
    expect_signal(base + PAGE, false, 11);
    store(base, b'Q');
    assert_eq!(load(base + 2 * PAGE), b'D');
    assert_eq!(load(tail + 10), b'E');
    assert_eq!(load(tail + 11), 0);
    expect_signal(tail + PAGE, false, 7);

    let fd = open("mmap-shared.bin\0", OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0);
    assert_eq!(write(fd as usize, b"abc"), 3);
    let shared = mmap(0, PAGE, READ | WRITE, SHARED, fd, 0);
    assert!(shared > 0);
    let shared = shared as usize;
    assert_eq!(load(shared + 1), b'b');
    let private_overlay = mmap(0, PAGE, READ, PRIVATE, fd, 0);
    assert!(private_overlay > 0);
    let private_overlay = private_overlay as usize;
    assert_eq!(load(private_overlay + 1), b'b');
    let child = fork();
    assert!(child >= 0);
    if child == 0 {
        store(shared + 1, b'X');
        exit(0);
    }
    let mut status = 0;
    assert_eq!(waitpid(child as usize, &mut status), child);
    assert_eq!(status, 0);
    assert_eq!(load(shared + 1), b'X');
    assert_eq!(
        load(private_overlay + 1),
        b'X',
        "private mapping shares the unmodified file page"
    );
    assert_eq!(mprotect(private_overlay, PAGE, READ | WRITE), 0);
    store(private_overlay + 1, b'P');
    assert_eq!(load(private_overlay + 1), b'P');
    assert_eq!(load(shared + 1), b'X');
    assert_eq!(munmap(private_overlay, PAGE), 0);
    let reader = open("mmap-shared.bin\0", OpenFlags::RDONLY);
    assert!(reader >= 0);
    assert_eq!(syscall(63, [reader as usize, mirror, 1, 0, 0, 0]), -14);
    let mut contents = [0; 3];
    assert_eq!(read(reader as usize, &mut contents), 3);
    assert_eq!(&contents, b"aXc");
    for destination in [kernel_copy, cold_kernel_copy] {
        assert_eq!(syscall(62, [reader as usize, 0, 0, 0, 0, 0]), 0);
        assert_eq!(syscall(63, [reader as usize, destination, 1, 0, 0, 0]), 1);
        assert_eq!(load(destination), b'a');
        assert_eq!(load(mirror), b'B');
    }
    let file_again = open("mmap-fixture.bin\0", OpenFlags::RDONLY);
    assert!(file_again >= 0);
    assert_eq!(
        syscall(62, [file_again as usize, PAGE, 0, 0, 0, 0]),
        PAGE as isize
    );
    assert_eq!(read(file_again as usize, &mut contents), 3);
    assert_eq!(&contents, b"BBB");
    assert_eq!(close(file_again as usize), 0);
    assert_eq!(load(mirror), b'B');

    assert_eq!(load(pipe_copy), b'B');
    assert_eq!(syscall(59, [pipe_copy, 0, 0, 0, 0, 0]), 0);
    for offset in [0, 4] {
        let fd = unsafe { ((pipe_copy + offset) as *const i32).read_volatile() };
        assert_eq!(close(fd as usize), 0, "pipe copyout returned a descriptor");
    }
    assert_eq!(load(mirror), b'B');

    store(shared + 3, b'Y');
    assert_eq!(syscall(64, [fd as usize, shared + 3, 1, 0, 0, 0]), 1);
    assert_eq!(load(shared + 3), b'Y');

    // The buffer aliases the file's backing frame. No overlapping Rust slices.
    assert_eq!(syscall(62, [reader as usize, 0, 0, 0, 0, 0]), 0);
    assert_eq!(syscall(63, [reader as usize, shared + 1, 2, 0, 0, 0]), 2);
    assert_eq!((load(shared + 1), load(shared + 2)), (b'a', b'X'));
    assert_eq!(syscall(62, [fd as usize, 0, 0, 0, 0, 0]), 0);
    assert_eq!(syscall(64, [fd as usize, shared + 1, 2, 0, 0, 0]), 2);
    assert_eq!((load(shared), load(shared + 1)), (b'a', b'X'));
    assert_eq!(syscall(46, [fd as usize, 1, 0, 0, 0, 0]), 0);
    assert_eq!((load(shared), load(shared + 1)), (b'a', 0));
    let truncated = open("mmap-shared.bin\0", OpenFlags::RDWR | OpenFlags::TRUNC);
    assert!(truncated >= 0);
    expect_signal(shared, false, 7);
    assert_eq!(close(truncated as usize), 0);
    assert_eq!(munmap(shared, PAGE), 0);

    let three = mmap(0, 3 * PAGE, READ | WRITE, PRIVATE | ANONYMOUS, -1, 0);
    assert!(three > 0);
    let three = three as usize;
    store(three, 1);
    store(three + PAGE, 2);
    store(three + 2 * PAGE, 3);
    assert_eq!(
        mmap(three + PAGE, PAGE, READ, PRIVATE | ANONYMOUS | FIXED, -1, 0),
        (three + PAGE) as isize
    );
    assert_eq!(
        (load(three), load(three + PAGE), load(three + 2 * PAGE)),
        (1, 0, 3)
    );

    let anon = mmap(0, PAGE, 0, PRIVATE | ANONYMOUS, -1, 0);
    assert!(anon > 0);
    expect_signal(anon as usize, false, 11);
    assert_eq!(mprotect(anon as usize, PAGE, READ | WRITE), 0);
    assert_eq!(load(anon as usize), 0);

    let anon_shared = mmap(0, 3 * PAGE, READ | WRITE, SHARED | ANONYMOUS, -1, 0);
    assert!(anon_shared > 0);
    let anon_shared = anon_shared as usize;
    assert_eq!(mprotect(anon_shared + PAGE, PAGE, READ), 0);
    let child = fork();
    assert!(child >= 0);
    if child == 0 {
        store(anon_shared + 2 * PAGE, b'R');
        exit(0);
    }
    let mut status = 0;
    assert_eq!(waitpid(child as usize, &mut status), child);
    assert_eq!(status, 0);
    assert_eq!(load(anon_shared), 0);
    assert_eq!(load(anon_shared + 2 * PAGE), b'R');

    assert_eq!(load(clear_tid_copy), b'B');
    assert!(clone_with_clear_tid(clear_tid_copy) > 0);
    yield_();
    assert_eq!(
        unsafe { (clear_tid_copy as *const i32).read_volatile() },
        0,
        "thread exit cleared TID through a private file mapping"
    );
    assert_eq!(load(mirror), b'B');
    println!("mmap-vma-test-ok");
    0
}
