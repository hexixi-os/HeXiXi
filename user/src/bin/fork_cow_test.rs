#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use user_lib::{close, exec, exit, fork, open, pipe, read, waitpid, write, yield_, OpenFlags};

const PAGE: usize = 4096;
const READ: usize = 1;
const WRITE: usize = 2;
const SHARED: usize = 1;
const PRIVATE: usize = 2;
const ANONYMOUS: usize = 0x20;
const WORKING_SET: usize = 40 * 1024 * 1024;

static mut DATA: u8 = 37;
static mut KERNEL_DESTINATION: u64 = 0x5a5a5a5a5a5a5a5a;

fn syscall(number: usize, args: [usize; 6]) -> isize {
    let result;
    unsafe {
        asm!(
            "ecall", in("a7") number, inlateout("a0") args[0] => result,
            in("a1") args[1], in("a2") args[2], in("a3") args[3],
            in("a4") args[4], in("a5") args[5],
        );
    }
    result
}

fn mmap(len: usize, flags: usize) -> usize {
    let addr = syscall(
        222,
        [0, len, READ | WRITE, flags | ANONYMOUS, usize::MAX, 0],
    );
    assert!(addr > 0);
    addr as usize
}

fn read_only(page: usize) {
    assert_eq!(syscall(226, [page, PAGE, READ, 0, 0, 0]), 0);
}

fn get(addr: usize) -> u8 {
    unsafe { (addr as *const u8).read_volatile() }
}

fn put(addr: usize, value: u8) {
    unsafe { (addr as *mut u8).write_volatile(value) };
}

fn child_status(pid: isize) -> i32 {
    assert!(pid > 0);
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    status
}

fn verify_isolation() {
    let private = mmap(3 * PAGE, PRIVATE);
    let protected = private + PAGE;
    let readonly_after_fork = private + 2 * PAGE;
    let shared = mmap(PAGE, SHARED);
    let heap = syscall(214, [0; 6]) as usize;
    assert!(heap != 0);
    assert_eq!(
        syscall(214, [heap + PAGE, 0, 0, 0, 0, 0]),
        (heap + PAGE) as isize
    );
    let data = unsafe { core::ptr::addr_of_mut!(DATA) } as usize;
    let destination = unsafe { core::ptr::addr_of_mut!(KERNEL_DESTINATION) } as usize;

    for addr in [private, protected, readonly_after_fork, shared, heap] {
        put(addr, 11);
    }
    put(data, 37);
    put(destination, 0x5a);
    read_only(protected);
    read_only(readonly_after_fork);

    let mut sync = [0; 2];
    assert_eq!(pipe(&mut sync), 0);
    let mut stack = [0x45u8; 1];
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        close(sync[1] as usize);
        assert_eq!(read(sync[0] as usize, &mut stack), 1);
        assert_eq!(stack[0], 0x99, "kernel copyout to a shared user stack");
        assert_eq!(get(private), 11, "the parent write must not leak");
        assert_eq!(get(heap), 11);
        assert_eq!(get(data), 37);
        assert_eq!(get(protected), 11);

        assert_eq!(syscall(59, [destination, 0, 0, 0, 0, 0]), 0);
        assert_ne!(
            get(destination),
            0x5a,
            "kernel copyout to a shared ELF page"
        );
        assert_eq!(syscall(226, [protected, PAGE, READ | WRITE, 0, 0, 0]), 0);
        put(private, 21);
        put(protected, 22);
        put(heap, 23);
        put(data, 24);
        put(shared, 25);
        exit(0);
    }

    close(sync[0] as usize);
    put(private, 31);
    put(heap, 32);
    put(data, 33);
    assert_eq!(write(sync[1] as usize, &[0x99]), 1);
    close(sync[1] as usize);
    assert_eq!(child_status(pid), 0);
    assert_eq!(get(stack.as_ptr() as usize), 0x45);
    assert_eq!(get(destination), 0x5a);
    assert_eq!(get(private), 31);
    assert_eq!(get(protected), 11);
    assert_eq!(get(heap), 32);
    assert_eq!(get(data), 33);
    assert_eq!(get(shared), 25, "MAP_SHARED remains shared");

    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        put(readonly_after_fork, 55);
        exit(99);
    }
    assert_eq!(child_status(pid), -11, "COW must respect PROT_READ");
    assert_eq!(get(readonly_after_fork), 11);
    assert_eq!(syscall(215, [private, 3 * PAGE, 0, 0, 0, 0]), 0);
    assert_eq!(syscall(215, [shared, PAGE, 0, 0, 0, 0]), 0);
    assert_eq!(syscall(214, [heap, 0, 0, 0, 0, 0]), heap as isize);
}

fn fork_exec() {
    let base = mmap(WORKING_SET, PRIVATE);
    for offset in (0..WORKING_SET).step_by(PAGE) {
        put(base + offset, (offset / PAGE) as u8);
    }
    let path = "fork-cow-leaf\0";
    let args = [path.as_ptr(), b"leaf\0".as_ptr(), core::ptr::null()];
    let pid = fork();
    assert!(pid >= 0, "fork of the 40 MiB resident working set");
    if pid == 0 {
        let result = exec(path, &args);
        panic!("exec of the leaf returned {}", result);
    }
    assert_eq!(child_status(pid), 0);

    for offset in (0..WORKING_SET).step_by(PAGE) {
        assert_eq!(get(base + offset), (offset / PAGE) as u8);
    }
    put(base, 61);
    assert_eq!(get(base), 61);
    assert_eq!(syscall(215, [base, WORKING_SET, 0, 0, 0, 0]), 0);
}

extern "C" fn signal_handler(_signal: usize) {
    exit(73);
}

fn verify_signal_context_isolation() {
    const SIGCTX: usize = 0x00ff_f000;
    const SIGUSR1: usize = 10;
    put(SIGCTX, 81);
    for protected in [false, true] {
        let pid = fork();
        assert!(pid >= 0);
        if pid == 0 {
            if protected {
                read_only(SIGCTX);
            }
            let action = [signal_handler as usize, 0, 0, 0];
            assert_eq!(
                syscall(134, [SIGUSR1, action.as_ptr() as usize, 0, 8, 0, 0]),
                0
            );
            assert_eq!(syscall(129, [0, SIGUSR1, 0, 0, 0, 0]), 0);
            exit(99);
        }
        // Saved registers belong to the task. Protecting the old user context
        // page must no longer affect signal delivery.
        assert_eq!(child_status(pid), 73);
        assert_eq!(
            get(SIGCTX),
            81,
            "child signal save must not modify the parent frame"
        );
    }
}

fn verify_wait_copyout() {
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        exit(31);
    }
    assert_eq!(syscall(260, [pid as usize, 1 << 38, 0, 0, 0, 0]), -14);
    assert_eq!(
        child_status(pid),
        31,
        "failed copyout must leave the child waitable"
    );
}

fn verify_fork_after_thread_exit() {
    let mut clear_tid = 1i32;
    let address = &mut clear_tid as *mut i32;
    let mut child_stack = [0u8; 1024];
    let stack_top = (child_stack.as_mut_ptr() as usize + child_stack.len()) & !15;
    let tid: isize;
    unsafe {
        // The pthread-style clone shares the process state and clears its TID on exit.
        asm!(
            "ecall", "bnez a0, 2f", "li a7, 93", "ecall", "unimp", "2:",
            inlateout("a0") 0x0021_0f00usize => tid,
            in("a1") stack_top, in("a2") 0usize, in("a3") 0usize, in("a4") address,
            inlateout("a7") 220usize => _,
        );
    }
    assert!(tid > 0);
    while unsafe { address.read_volatile() } != 0 {
        yield_();
    }
    let pid = fork();
    assert!(pid >= 0, "an empty thread slot must not prevent fork");
    if pid == 0 {
        exit(0);
    }
    assert_eq!(child_status(pid), 0);
}

fn verify_fork_oom() {
    const RESERVE: usize = 16 * PAGE;
    const PRESSURE: usize = 1024 * 1024 * 1024;
    const CHUNK: usize = 32 * PAGE;
    let reserve = mmap(RESERVE, PRIVATE);
    for offset in (0..RESERVE).step_by(PAGE) {
        put(reserve + offset, 71);
    }
    let zero = open("/dev/zero\0", OpenFlags::RDONLY);
    assert!(zero >= 0);
    let pressure = mmap(PRESSURE, PRIVATE);
    let mut read_result = 0;
    for offset in (0..PRESSURE).step_by(CHUNK) {
        let bytes =
            unsafe { core::slice::from_raw_parts_mut((pressure + offset) as *mut u8, CHUNK) };
        read_result = read(zero as usize, bytes);
        if read_result < 0 {
            break;
        }
    }

    // Release only a few data frames. Fork can start, but cannot finish all child page tables.
    let reclaimed = syscall(215, [reserve + PAGE, 12 * PAGE, 0, 0, 0, 0]);
    let first = fork();
    if first == 0 {
        exit(99);
    }
    let second = fork();
    if second == 0 {
        exit(99);
    }
    let released = syscall(215, [pressure, PRESSURE, 0, 0, 0, 0]);
    assert_eq!(read_result, -14, "copyout must report frame exhaustion");
    assert_eq!(reclaimed, 0);
    assert_eq!(first, -12);
    assert_eq!(second, -12);
    assert_eq!(released, 0);
    assert_eq!(get(reserve), 71);
    put(reserve, 72);
    assert_eq!(close(zero as usize), 0);

    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        assert_eq!(get(reserve), 72);
        exit(0);
    }
    assert_eq!(child_status(pid), 0);
    assert_eq!(syscall(215, [reserve, RESERVE, 0, 0, 0, 0]), 0);
}

#[no_mangle]
fn main(_argc: usize, args: &[&str]) -> i32 {
    if args.get(1) == Some(&"leaf") {
        return 0;
    }
    let marker = open("fork-cow-perf-only\0", OpenFlags::RDONLY);
    if marker >= 0 {
        close(marker as usize);
    } else {
        verify_isolation();
        verify_signal_context_isolation();
        verify_wait_copyout();
        verify_fork_after_thread_exit();
        println!("fork-cow-isolation-ok");
        verify_fork_oom();
        println!("fork-cow-oom-ok");
    }
    fork_exec();
    println!("fork-cow-test-ok");
    0
}
