#![no_std]
#![feature(linkage)]
#![cfg_attr(target_arch = "riscv64", feature(panic_info_message))]
#![feature(alloc_error_handler)]

#[macro_use]
pub mod console;
mod lang_items;
mod syscall;

extern crate alloc;
#[macro_use]
extern crate bitflags;

use alloc::vec::Vec;
use buddy_system_allocator::LockedHeap;
use syscall::*;
pub use syscall::{sys_execve, sys_kill_raw, sys_mkdirat, sys_setpgid, sys_symlinkat};

const USER_HEAP_SIZE: usize = 32768;

static mut HEAP_SPACE: [u8; USER_HEAP_SIZE] = [0; USER_HEAP_SIZE];

#[global_allocator]
static HEAP: LockedHeap = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

#[cfg(target_arch = "riscv64")]
core::arch::global_asm!(
    ".section .text.entry, \"ax\"",
    ".globl _start",
    "_start:",
    "mv a0, sp",
    "tail __hexixi_user_start",
);

#[cfg(target_arch = "loongarch64")]
core::arch::global_asm!(
    ".section .text.entry, \"ax\"",
    ".globl _start",
    "_start:",
    "move $a0, $sp",
    "b __hexixi_user_start",
);

#[no_mangle]
extern "C" fn __hexixi_user_start(stack: *const usize) -> ! {
    let argc = unsafe { stack.read() };
    let argv = unsafe { stack.add(1) };
    unsafe {
        HEAP.lock()
            .init(core::ptr::addr_of!(HEAP_SPACE) as usize, USER_HEAP_SIZE);
    }
    let mut v: Vec<&'static str> = Vec::new();
    for i in 0..argc {
        let str_start = unsafe { argv.add(i).read() };
        let len = (0usize..)
            .find(|i| unsafe { ((str_start + *i) as *const u8).read_volatile() == 0 })
            .unwrap();
        v.push(
            core::str::from_utf8(unsafe {
                core::slice::from_raw_parts(str_start as *const u8, len)
            })
            .unwrap(),
        );
    }
    exit(main(argc, v.as_slice()));
}

#[linkage = "weak"]
#[no_mangle]
fn main(_argc: usize, _argv: &[&str]) -> i32 {
    panic!("Cannot find main!");
}

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 0o100;
        const TRUNC = 0o1000;
    }
}

pub fn dup(fd: usize) -> isize {
    sys_dup(fd)
}
pub fn chdir(path: &str) -> isize {
    sys_chdir(path)
}
pub fn open(path: &str, flags: OpenFlags) -> isize {
    sys_open(path, flags.bits)
}
pub fn close(fd: usize) -> isize {
    sys_close(fd)
}
pub fn pipe(pipe_fd: &mut [i32]) -> isize {
    sys_pipe(pipe_fd)
}
pub fn read(fd: usize, buf: &mut [u8]) -> isize {
    sys_read(fd, buf)
}
pub fn write(fd: usize, buf: &[u8]) -> isize {
    sys_write(fd, buf)
}
pub fn exit(exit_code: i32) -> ! {
    sys_exit(exit_code);
}
pub fn yield_() -> isize {
    sys_yield()
}
pub fn get_time() -> isize {
    let mut tv = [0usize; 2];
    if sys_get_time(tv.as_mut_ptr() as *mut u8) != 0 {
        return -1;
    }
    (tv[0] * 1000 + tv[1] / 1000) as isize
}
pub fn getpid() -> isize {
    sys_getpid()
}
pub fn fork() -> isize {
    sys_fork()
}
pub fn exec(path: &str, args: &[*const u8]) -> isize {
    sys_exec(path, args)
}
fn decode_wait_status(status: i32) -> i32 {
    let signal = status & 0x7f;
    if signal == 0 { status >> 8 } else { -signal }
}

pub fn wait(exit_code: &mut i32) -> isize {
    let mut raw: i32 = 0;
    loop {
        match sys_waitpid(-1, &mut raw as *mut i32) {
            -2 => {
                yield_();
            }
            // -1 or a real pid
            exit_pid => {
                // 内核返回 Linux 风格的 wait status，这里还原成原始退出码
                *exit_code = decode_wait_status(raw);
                return exit_pid;
            }
        }
    }
}

pub fn waitpid(pid: usize, exit_code: &mut i32) -> isize {
    let mut raw: i32 = 0;
    loop {
        match sys_waitpid(pid as isize, &mut raw as *mut i32) {
            -2 => {
                yield_();
            }
            // -1 or a real pid
            exit_pid => {
                *exit_code = decode_wait_status(raw);
                return exit_pid;
            }
        }
    }
}

pub fn waitpid_nb(pid: usize, exit_code: &mut i32) -> isize {
    let mut raw: i32 = 0;
    let ret = sys_waitpid_with_options(pid as isize, &mut raw as *mut i32, 1);
    if ret > 0 {
        *exit_code = decode_wait_status(raw);
    }
    ret
}

bitflags! {
    pub struct SignalFlags: i32 {
        const SIGINT    = 1 << 2;
        const SIGILL    = 1 << 4;
        const SIGABRT   = 1 << 6;
        const SIGFPE    = 1 << 8;
        const SIGSEGV   = 1 << 11;
    }
}

pub fn kill(pid: usize, signal: i32) -> isize {
    let bits = signal as u32;
    if bits == 0 {
        return sys_kill(pid, 0);
    }
    if bits.count_ones() != 1 {
        return -22;
    }
    sys_kill(pid, bits.trailing_zeros() as i32)
}

pub fn sleep(sleep_ms: usize) {
    sys_sleep(sleep_ms);
}

pub fn thread_create(entry: usize, arg: usize) -> isize {
    sys_thread_create(entry, arg)
}
pub fn gettid() -> isize {
    sys_gettid()
}
pub fn waittid(tid: usize) -> isize {
    loop {
        match sys_waittid(tid) {
            -2 => {
                yield_();
            }
            exit_code => return exit_code,
        }
    }
}

pub fn mutex_create() -> isize {
    sys_mutex_create(false)
}
pub fn mutex_blocking_create() -> isize {
    sys_mutex_create(true)
}
pub fn mutex_lock(mutex_id: usize) {
    sys_mutex_lock(mutex_id);
}
pub fn mutex_unlock(mutex_id: usize) {
    sys_mutex_unlock(mutex_id);
}
pub fn semaphore_create(res_count: usize) -> isize {
    sys_semaphore_create(res_count)
}
pub fn semaphore_up(sem_id: usize) {
    sys_semaphore_up(sem_id);
}
pub fn semaphore_down(sem_id: usize) {
    sys_semaphore_down(sem_id);
}
pub fn condvar_create() -> isize {
    sys_condvar_create(0)
}
pub fn condvar_signal(condvar_id: usize) {
    sys_condvar_signal(condvar_id);
}
pub fn condvar_wait(condvar_id: usize, mutex_id: usize) {
    sys_condvar_wait(condvar_id, mutex_id);
}
