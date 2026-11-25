#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::sync::atomic::{AtomicBool, Ordering};
use user_lib::{
    close, exec, exit, fork, get_time, kill, open, pipe, read, sleep, thread_create, waitpid,
    write, OpenFlags,
};

const CAPACITY: usize = 16384;
const EINTR: isize = -4;
const EPIPE: isize = -32;
static ALARM_HANDLED: AtomicBool = AtomicBool::new(false);
static THREAD_STARTED: AtomicBool = AtomicBool::new(false);

fn syscall(number: usize, [a0, a1, a2, a3]: [usize; 4]) -> isize {
    let result;
    unsafe {
        #[cfg(target_arch = "riscv64")]
        asm!("ecall", in("a7") number, inlateout("a0") a0 => result,
            in("a1") a1, in("a2") a2, in("a3") a3);
        #[cfg(target_arch = "loongarch64")]
        asm!("syscall 0", in("$a7") number, inlateout("$a0") a0 => result,
            in("$a1") a1, in("$a2") a2, in("$a3") a3);
    }
    result
}

fn set_handler(signal: usize, handler: usize) {
    let action = [handler, 0, 0];
    assert_eq!(syscall(134, [signal, action.as_ptr() as usize, 0, 8]), 0);
}

fn endpoints() -> [usize; 2] {
    let mut fds = [0; 2];
    assert_eq!(pipe(&mut fds), 0);
    fds.map(|fd| fd as usize)
}

fn child_pid() -> isize {
    let pid = fork();
    assert!(pid >= 0);
    pid
}

fn reap(pid: isize, expected: i32) {
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, expected);
}

fn delayed_bytes(rounds: u8, delay: fn()) {
    let [reader, writer] = endpoints();
    let pid = child_pid();
    if pid == 0 {
        assert_eq!(close(reader), 0);
        for byte in 0..rounds {
            delay();
            assert_eq!(write(writer, &[byte]), 1);
        }
        exit(0);
    }
    assert_eq!(close(writer), 0);
    let mut buffer = [0; 8];
    for byte in 0..rounds {
        assert_eq!(read(reader, &mut buffer), 1);
        assert_eq!(buffer[0], byte);
    }
    assert_eq!(read(reader, &mut buffer), 0);
    assert_eq!(close(reader), 0);
    reap(pid, 0);
}

fn busy_delay() {
    let deadline = get_time() + 15;
    while get_time() < deadline {
        core::hint::spin_loop();
    }
}

fn multiple_readers() {
    let [reader, writer] = endpoints();
    let [eof_reader, eof_writer] = endpoints();
    let mut children = [0; 4];
    for pid in &mut children {
        *pid = child_pid();
        if *pid == 0 {
            assert_eq!(close(writer), 0);
            assert_eq!(close(eof_writer), 0);
            let mut byte = [0];
            assert_eq!(read(reader, &mut byte), 1);
            assert_eq!(byte[0], b'x');
            assert_eq!(read(eof_reader, &mut byte), 0);
            exit(0);
        }
    }
    assert_eq!(close(reader), 0);
    assert_eq!(close(eof_reader), 0);
    sleep(30);
    assert_eq!(write(writer, b"xxxx"), 4);
    sleep(30);
    assert_eq!(close(writer), 0);
    assert_eq!(close(eof_writer), 0);
    for pid in children {
        reap(pid, 0);
    }
}

fn writer_wakes_on_space() {
    let [reader, writer] = endpoints();
    let [ack_reader, ack_writer] = endpoints();
    assert_eq!(write(writer, &[b'a'; CAPACITY]), CAPACITY as isize);
    let pid = child_pid();
    if pid == 0 {
        assert_eq!(close(writer), 0);
        assert_eq!(close(ack_writer), 0);
        sleep(30);
        let mut byte = [0];
        assert_eq!(read(reader, &mut byte), 1);
        assert_eq!(byte, [b'a']);
        // Keep the read end open until the writer confirms it made progress.
        assert_eq!(read(ack_reader, &mut byte), 1);
        let mut data = [0; CAPACITY];
        assert_eq!(read(reader, &mut data), CAPACITY as isize);
        assert!(data[..CAPACITY - 1].iter().all(|&byte| byte == b'a'));
        assert_eq!(data[CAPACITY - 1], b'b');
        exit(0);
    }
    assert_eq!(close(reader), 0);
    assert_eq!(close(ack_reader), 0);
    assert_eq!(write(writer, b"b"), 1);
    assert_eq!(write(ack_writer, b"x"), 1);
    assert_eq!(close(writer), 0);
    assert_eq!(close(ack_writer), 0);
    reap(pid, 0);
}

fn closed_reader() {
    set_handler(13, 1); // SIGPIPE, SIG_IGN
    let [reader, writer] = endpoints();
    assert_eq!(write(writer, &[0; CAPACITY]), CAPACITY as isize);
    let pid = child_pid();
    if pid == 0 {
        assert_eq!(close(writer), 0);
        sleep(30);
        assert_eq!(close(reader), 0);
        exit(0);
    }
    assert_eq!(close(reader), 0);
    assert_eq!(write(writer, b""), 0);
    assert_eq!(write(writer, b"x"), EPIPE);
    assert_eq!(close(writer), 0);
    reap(pid, 0);
}

fn killed_reader() {
    let [reader, writer] = endpoints();
    let pid = child_pid();
    if pid == 0 {
        assert_eq!(close(writer), 0);
        assert_eq!(read(reader, &mut []), 0);
        read(reader, &mut [0]);
        panic!("SIGKILL returned to the pipe reader");
    }
    sleep(30);
    assert_eq!(kill(pid as usize, 1 << 9), 0);
    reap(pid, -9);
    assert_eq!(write(writer, b"y"), 1);
    assert_eq!(read(reader, &mut [0]), 1);
    assert_eq!(close(reader), 0);
    assert_eq!(close(writer), 0);
}

fn blocked_thread(reader: usize) -> ! {
    THREAD_STARTED.store(true, Ordering::Release);
    read(reader, &mut [0]);
    panic!("discarded thread resumed");
}

fn group_exit_cancels_reader() {
    let [reader, writer] = endpoints();
    let pid = child_pid();
    if pid == 0 {
        assert_eq!(close(writer), 0);
        assert!(thread_create(blocked_thread as *const () as usize, reader) > 0);
        sleep(30);
        assert!(THREAD_STARTED.load(Ordering::Acquire));
        exit(0);
    }
    reap(pid, 0);
    assert_eq!(write(writer, b"y"), 1);
    assert_eq!(read(reader, &mut [0]), 1);
    assert_eq!(close(reader), 0);
    assert_eq!(close(writer), 0);
}

extern "C" fn alarm_handler(_: usize) {
    ALARM_HANDLED.store(true, Ordering::Relaxed);
}

fn alarm_interrupts_reader() {
    set_handler(14, alarm_handler as *const () as usize);
    let [reader, writer] = endpoints();
    let timer = [0usize, 0, 0, 30_000];
    assert_eq!(syscall(103, [0, timer.as_ptr() as usize, 0, 0]), 0);
    assert_eq!(read(reader, &mut [0]), EINTR);
    assert!(ALARM_HANDLED.load(Ordering::Relaxed));
    assert_eq!(write(writer, b"z"), 1);
    assert_eq!(read(reader, &mut [0]), 1);
    assert_eq!(close(reader), 0);
    assert_eq!(close(writer), 0);
}

#[no_mangle]
fn main(argc: usize, _argv: &[&str]) -> i32 {
    let benchmark = open("pipe_wait_bench\0", OpenFlags::RDONLY);
    if benchmark >= 0 {
        assert_eq!(close(benchmark as usize), 0);
        delayed_bytes(12, busy_delay);
        println!("pipe-wait-bench-ok");
    } else {
        if argc == 0 {
            // The exec path installs the signal return trampoline for the alarm test.
            let args = [b"initproc\0".as_ptr(), core::ptr::null()];
            panic!("exec(initproc) returned {}", exec("initproc\0", &args));
        }
        println!("pipe-wait: delayed writer");
        delayed_bytes(2, || sleep(15));
        println!("pipe-wait: multiple readers");
        multiple_readers();
        println!("pipe-wait: freeing space for a writer");
        writer_wakes_on_space();
        println!("pipe-wait: closing the reader of a full pipe");
        closed_reader();
        killed_reader();
        group_exit_cancels_reader();
        alarm_interrupts_reader();
        println!("pipe-wait-test-ok");
    }
    0
}
