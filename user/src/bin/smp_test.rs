#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicU32, Ordering};
use user_lib::{close, exit, fork, open, pipe, read, waitpid, write, OpenFlags};

const PAGE: usize = 4096;
const MAP_SHARED: usize = 1;
const MAP_PRIVATE_ANONYMOUS: usize = 0x22;
const NO_CPU: u32 = u32::MAX;
const ROUNDS: usize = 8;

const PARENT_WROTE: u32 = 1;
const CHILD_WARM: u32 = 2;
const PARENT_TRUNCATED: u32 = 3;
const CHILD_PROBING: u32 = 4;

struct Handshake {
    child_cpu: AtomicU32,
    stage: AtomicU32,
}

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

fn cpu_id(node: *mut u32) -> u32 {
    let mut cpu = NO_CPU;
    assert_eq!(
        syscall(
            168,
            [&mut cpu as *mut u32 as usize, node as usize, 0, 0, 0, 0]
        ),
        0,
        "getcpu failed"
    );
    assert_ne!(cpu, NO_CPU, "getcpu did not write the CPU ID");
    let mut mask = 0usize;
    assert_eq!(
        syscall(
            123,
            [
                0,
                core::mem::size_of_val(&mask),
                &mut mask as *mut usize as usize,
                0,
                0,
                0
            ]
        ),
        core::mem::size_of_val(&mask) as isize,
        "sched_getaffinity failed"
    );
    assert_eq!(mask, 1usize << cpu, "affinity must match the current hart");
    cpu
}

fn map(fd: usize, len: usize, flags: usize) -> usize {
    let address = syscall(222, [0, len, 3, flags, fd, 0]);
    assert!(address > 0, "mmap returned {}", address);
    address as usize
}

fn unmap(address: usize, len: usize) {
    assert_eq!(syscall(215, [address, len, 0, 0, 0, 0]), 0);
}

fn resize(fd: usize, len: usize) {
    assert_eq!(syscall(46, [fd, len, 0, 0, 0, 0]), 0, "ftruncate");
}

fn byte(address: usize) -> u8 {
    unsafe { (address as *const u8).read_volatile() }
}

fn store(address: usize, value: u8) {
    unsafe { (address as *mut u8).write_volatile(value) }
}

fn wait_for_change(word: &AtomicU32, initial: u32, operation: &str) -> u32 {
    for _ in 0..100_000_000 {
        let value = word.load(Ordering::Acquire);
        if value != initial {
            return value;
        }
        spin_loop();
    }
    panic!("timed out waiting for {}", operation);
}

fn wait_for_child(pid: isize) -> i32 {
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    status
}

fn cross_hart_round(fd: usize, parent_cpu: u32) -> Option<u32> {
    resize(fd, 2 * PAGE);
    let shared = map(fd, 2 * PAGE, MAP_SHARED);
    let private = map(usize::MAX, PAGE, MAP_PRIVATE_ANONYMOUS);
    let kept = shared + PAGE - 1;
    let removed = shared + PAGE;
    let sync = unsafe {
        let sync = shared as *mut Handshake;
        sync.write(Handshake {
            child_cpu: AtomicU32::new(NO_CPU),
            stage: AtomicU32::new(0),
        });
        &*sync
    };
    store(kept, b'k');
    store(removed, b'r');
    store(private, b'i');

    let pid = fork();
    assert!(pid >= 0, "fork returned {}", pid);
    if pid == 0 {
        let child_cpu = cpu_id(ptr::null_mut());
        sync.child_cpu.store(child_cpu, Ordering::Release);
        if child_cpu == parent_cpu {
            exit(0);
        }

        assert_eq!(byte(private), b'i', "child's initial private mapping");
        store(private, b'c');
        assert_eq!(
            wait_for_change(&sync.stage, 0, "parent COW write"),
            PARENT_WROTE
        );
        assert_eq!(
            byte(private),
            b'c',
            "parent write reached the child's COW page"
        );

        // Warm the translation, then stay in userspace until the next load.
        assert_eq!(
            byte(removed),
            b'r',
            "child must touch the page before truncation"
        );
        sync.stage.store(CHILD_WARM, Ordering::Release);
        assert_eq!(
            wait_for_change(&sync.stage, CHILD_WARM, "parent ftruncate"),
            PARENT_TRUNCATED
        );
        assert_eq!(
            byte(kept),
            b'k',
            "the first shared page should remain mapped"
        );
        sync.stage.store(CHILD_PROBING, Ordering::Release);
        let value = byte(removed);
        panic!("truncated shared page remained readable: {:#x}", value);
    }

    let child_cpu = wait_for_change(&sync.child_cpu, NO_CPU, "child CPU ID");
    if child_cpu != parent_cpu {
        assert_eq!(byte(private), b'i', "parent's initial private mapping");
        store(private, b'p');
        sync.stage.store(PARENT_WROTE, Ordering::Release);
        assert_eq!(
            wait_for_change(&sync.stage, PARENT_WROTE, "child COW write and page load"),
            CHILD_WARM
        );
        assert_eq!(
            byte(private),
            b'p',
            "child write reached the parent's COW page"
        );

        resize(fd, PAGE);
        sync.stage.store(PARENT_TRUNCATED, Ordering::Release);
        let status = wait_for_child(pid);
        assert_eq!(sync.stage.load(Ordering::Acquire), CHILD_PROBING);
        assert_eq!(status, -7, "truncated page access should raise SIGBUS");
    } else {
        assert_eq!(wait_for_child(pid), 0);
    }
    unmap(private, PAGE);
    unmap(shared, 2 * PAGE);
    if child_cpu != parent_cpu {
        Some(child_cpu)
    } else {
        None
    }
}

fn pipe_byte(offset: usize) -> u8 {
    (offset * 17 + (offset >> 8)) as u8
}

fn check_cross_hart_pipe(parent_cpu: u32) {
    // Exceed the ring capacity and compare bytes independently of stdout ordering.
    const LEN: usize = 5 * PAGE + 13;
    for _ in 0..2 {
        let mut ends = [0; 2];
        assert_eq!(pipe(&mut ends), 0);
        let [read_fd, write_fd] = ends.map(|fd| fd as usize);
        let pid = fork();
        assert!(pid >= 0);
        if pid == 0 {
            let same_hart = cpu_id(ptr::null_mut()) == parent_cpu;
            assert_eq!(close(write_fd), 0);
            let mut received = 0;
            let mut buffer = [0; 1024];
            loop {
                let count = read(read_fd, &mut buffer);
                assert!(count >= 0);
                if count == 0 {
                    break;
                }
                for &byte in &buffer[..count as usize] {
                    assert_eq!(byte, pipe_byte(received));
                    received += 1;
                }
            }
            assert_eq!(received, LEN);
            exit(i32::from(same_hart));
        }

        assert_eq!(close(read_fd), 0);
        let mut payload = [0; LEN];
        for (offset, byte) in payload.iter_mut().enumerate() {
            *byte = pipe_byte(offset);
        }
        assert_eq!(write(write_fd, &payload), LEN as isize);
        assert_eq!(close(write_fd), 0);
        let status = wait_for_child(pid);
        if status == 0 {
            println!("smp-pipe-test-ok");
            return;
        }
        assert_eq!(status, 1, "pipe reader failed");
    }
    panic!("pipe reader did not run on a different hart");
}

#[no_mangle]
fn main() -> i32 {
    let mut node = NO_CPU;
    let parent_cpu = cpu_id(&mut node);
    assert_eq!(node, 0, "getcpu NUMA node on QEMU virt");
    let fd = open("smp-mapping\0", OpenFlags::CREATE | OpenFlags::RDWR);
    assert!(fd >= 0, "open returned {}", fd);
    let fd = fd as usize;

    let mut completed = 0;
    // A round-robin fork can also land on the parent's hart.
    for _ in 0..ROUNDS * 2 {
        if let Some(child_cpu) = cross_hart_round(fd, parent_cpu) {
            completed += 1;
            println!(
                "SMP round {}: parent CPU {}, child CPU {}",
                completed, parent_cpu, child_cpu
            );
            if completed == ROUNDS {
                break;
            }
        }
    }
    assert_eq!(completed, ROUNDS, "insufficient forks on a second hart");
    assert_eq!(close(fd), 0);
    check_cross_hart_pipe(parent_cpu);
    println!("smp-test-ok");
    0
}
