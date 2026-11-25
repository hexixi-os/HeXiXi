#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::convert::TryInto;
use core::sync::atomic::{AtomicU32, Ordering};
use user_lib::{close, exit, fork, read, sleep, waitpid, write};

const CWD: usize = (-100isize) as usize;
const PAGE: usize = 4096;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_RDWR: usize = 2;
const O_DIRECTORY: usize = 0o200000;
const O_NOFOLLOW: usize = 0o400000;
const O_PATH: usize = 0o10000000;
const PROT_READ: usize = 1;
const PROT_WRITE: usize = 2;
const MAP_SHARED: usize = 1;
const EXT4_MAGIC: u64 = 0xef53;
const TMPFS_MAGIC: u64 = 0x0102_1994;
const PROC_MAGIC: u64 = 0x9fa0;

fn syscall6(id: usize, [a0, a1, a2, a3, a4, a5]: [usize; 6]) -> isize {
    let result;
    unsafe {
        #[cfg(target_arch = "riscv64")]
        asm!(
            "ecall",
            in("a7") id,
            inlateout("a0") a0 => result,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
            in("a4") a4,
            in("a5") a5,
        );
        #[cfg(target_arch = "loongarch64")]
        asm!(
            "syscall 0",
            in("$a7") id,
            inlateout("$a0") a0 => result,
            in("$a1") a1,
            in("$a2") a2,
            in("$a3") a3,
            in("$a4") a4,
            in("$a5") a5,
        );
    }
    result
}

fn syscall(id: usize, [a, b, c, d]: [usize; 4]) -> isize {
    syscall6(id, [a, b, c, d, 0, 0])
}

fn open_at(dir: usize, name: &str, flags: usize) -> isize {
    syscall(56, [dir, name.as_ptr() as usize, flags, 0o600])
}

fn read_link(dir: usize, name: &str, data: &mut [u8]) -> isize {
    syscall(
        78,
        [
            dir,
            name.as_ptr() as usize,
            data.as_mut_ptr() as usize,
            data.len(),
        ],
    )
}

fn link_at(target: &str, dir: usize, name: &str) -> isize {
    syscall(
        36,
        [target.as_ptr() as usize, dir, name.as_ptr() as usize, 0],
    )
}

fn unlink(dir: usize, name: &str) -> isize {
    syscall(35, [dir, name.as_ptr() as usize, 0, 0])
}

fn statfs_type(path: &str) -> u64 {
    let mut buf = [0u8; 120];
    assert_eq!(
        syscall(43, [path.as_ptr() as usize, buf.as_mut_ptr() as usize, 0, 0]),
        0,
        "statfs {} failed",
        path
    );
    u64::from_le_bytes(buf[0..8].try_into().unwrap())
}

fn fstatfs_type(fd: usize) -> u64 {
    let mut buf = [0u8; 120];
    assert_eq!(syscall(44, [fd, buf.as_mut_ptr() as usize, 0, 0]), 0);
    u64::from_le_bytes(buf[0..8].try_into().unwrap())
}

fn stat_dev(path: &str) -> u64 {
    let mut st = [0u8; 128];
    assert_eq!(
        syscall(79, [CWD, path.as_ptr() as usize, st.as_mut_ptr() as usize, 0]),
        0
    );
    u64::from_le_bytes(st[0..8].try_into().unwrap())
}

fn expect_contents(dir: usize, name: &str, expected: &[u8]) {
    let fd = open_at(dir, name, 0);
    assert!(fd >= 0, "open {} returned {}", name, fd);
    let mut data = [0; 160];
    assert_eq!(read(fd as usize, &mut data), expected.len() as isize);
    assert_eq!(&data[..expected.len()], expected);
    assert_eq!(close(fd as usize), 0);
}

fn test_mounts_and_tmpfs() {
    assert_eq!(statfs_type("/fixture\0"), EXT4_MAGIC);
    assert_eq!(statfs_type("/proc\0"), PROC_MAGIC);
    for path in ["/tmp\0", "/var/tmp\0", "/dev/shm\0"] {
        assert_eq!(statfs_type(path), TMPFS_MAGIC);
    }
    let tmp_dev = stat_dev("/tmp\0");
    let vartmp_dev = stat_dev("/var/tmp\0");
    let shm_dev = stat_dev("/dev/shm\0");
    assert_ne!(tmp_dev, vartmp_dev);
    assert_ne!(tmp_dev, shm_dev);
    assert_ne!(vartmp_dev, shm_dev);

    let tmp_file = open_at(CWD, "/tmp/cross\0", O_CREAT | O_EXCL | O_RDWR);
    assert!(tmp_file >= 0);
    assert_eq!(fstatfs_type(tmp_file as usize), TMPFS_MAGIC);
    assert_eq!(close(tmp_file as usize), 0);
    assert_eq!(
        syscall(
            38,
            [
                CWD,
                b"/tmp/cross\0".as_ptr() as usize,
                CWD,
                b"/var/tmp/cross\0".as_ptr() as usize
            ]
        ),
        -18
    );
    assert_eq!(unlink(CWD, "/tmp/cross\0"), 0);

    // Unknown fstype -> ENODEV; unmounted dir -> EINVAL; system mount -> EBUSY.
    assert_eq!(
        syscall6(
            40,
            [
                b"none\0".as_ptr() as usize,
                b"/fixture/data\0".as_ptr() as usize,
                b"nosuchfs\0".as_ptr() as usize,
                0,
                0,
                0,
            ]
        ),
        -19
    );
    assert_eq!(syscall(39, [b"/fixture/data\0".as_ptr() as usize, 0, 0, 0]), -22);
    assert_eq!(syscall(39, [b"/proc\0".as_ptr() as usize, 0, 0, 0]), -16);

    // Mount tmpfs over /fixture/data: shadows lower files until unmounted.
    assert_eq!(
        syscall6(
            40,
            [
                b"tmpfs\0".as_ptr() as usize,
                b"/fixture/data\0".as_ptr() as usize,
                b"tmpfs\0".as_ptr() as usize,
                0,
                0,
                0,
            ]
        ),
        0
    );
    assert_eq!(statfs_type("/fixture/data\0"), TMPFS_MAGIC);
    assert_eq!(open_at(CWD, "/fixture/data/value\0", 0), -2);
    let mounts_fd = open_at(CWD, "/proc/mounts\0", 0);
    assert!(mounts_fd >= 0);
    let mut mounts_buf = [0u8; 512];
    let mounts_len = read(mounts_fd as usize, &mut mounts_buf) as usize;
    assert!(mounts_buf[..mounts_len]
        .windows(19)
        .any(|w| w == b"/fixture/data tmpfs"));
    assert_eq!(close(mounts_fd as usize), 0);

    let upper_fd = open_at(CWD, "/fixture/data/upper\0", O_CREAT | O_EXCL | O_RDWR);
    assert!(upper_fd >= 0);
    assert_eq!(write(upper_fd as usize, b"mounted"), 7);
    // Busy while upper_fd is open.
    assert_eq!(syscall(39, [b"/fixture/data\0".as_ptr() as usize, 0, 0, 0]), -16);
    assert_eq!(close(upper_fd as usize), 0);
    assert_eq!(syscall(39, [b"/fixture/data\0".as_ptr() as usize, 0, 0, 0]), 0);
    assert_eq!(open_at(CWD, "/fixture/data/upper\0", 0), -2);
    expect_contents(CWD, "/fixture/data/value\0", b"payload");

    // Lazy unmount (MNT_DETACH = 2) detaches namespace while open handle stays valid.
    assert_eq!(
        syscall6(
            40,
            [
                b"tmpfs\0".as_ptr() as usize,
                b"/fixture/data\0".as_ptr() as usize,
                b"tmpfs\0".as_ptr() as usize,
                0,
                0,
                0,
            ]
        ),
        0
    );
    let lazy_fd = open_at(CWD, "/fixture/data/lazy\0", O_CREAT | O_EXCL | O_RDWR);
    assert!(lazy_fd >= 0);
    assert_eq!(write(lazy_fd as usize, b"ok"), 2);
    assert_eq!(syscall(39, [b"/fixture/data\0".as_ptr() as usize, 2, 0, 0]), 0);
    expect_contents(CWD, "/fixture/data/value\0", b"payload");
    assert_eq!(syscall(62, [lazy_fd as usize, 0, 0, 0]), 0);
    let mut lazy_bytes = [0u8; 2];
    assert_eq!(read(lazy_fd as usize, &mut lazy_bytes), 2);
    assert_eq!(&lazy_bytes, b"ok");
    assert_eq!(close(lazy_fd as usize), 0);
}

fn test_devices_and_lib_links() {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    for dev in ["/dev/random\0", "/dev/urandom\0"] {
        let fd = open_at(CWD, dev, O_RDWR);
        assert!(fd >= 0, "open {} returned {}", dev, fd);
        assert_eq!(read(fd as usize, &mut a), 32);
        assert_eq!(read(fd as usize, &mut b), 32);
        assert_ne!(a, b);
        assert_eq!(write(fd as usize, b"seed"), 4);
        assert_eq!(close(fd as usize), 0);
    }
    assert_eq!(syscall(278, [a.as_mut_ptr() as usize, a.len(), 0, 0]), 32);
    assert_eq!(syscall(278, [b.as_mut_ptr() as usize, b.len(), 0, 0]), 32);
    assert_ne!(a, b);
    assert_eq!(syscall(278, [a.as_mut_ptr() as usize, a.len(), 0x100, 0]), -22);
    assert_eq!(syscall(278, [usize::MAX, 16, 0, 0]), -14);

    // /dev/zero shared mmap across fork.
    let zero = open_at(CWD, "/dev/zero\0", O_RDWR);
    assert!(zero >= 0);
    let shared_zero = syscall6(
        222,
        [0, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, zero as usize, 0],
    );
    assert!(shared_zero > 0);
    assert_eq!(close(zero as usize), 0);
    let zero_word = unsafe { &*(shared_zero as *const AtomicU32) };
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        zero_word.store(0x55aa_1234, Ordering::Release);
        exit(0);
    }
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0);
    assert_eq!(zero_word.load(Ordering::Acquire), 0x55aa_1234);
    assert_eq!(syscall(215, [shared_zero as usize, PAGE, 0, 0]), 0);

    // Boot-time /lib64 -> /lib compatibility link when /lib is present.
    let mut link_buf = [0u8; 16];
    assert_eq!(read_link(CWD, "/lib64\0", &mut link_buf), 4);
    assert_eq!(&link_buf[..4], b"/lib");
    expect_contents(CWD, "/lib64/runtime.marker\0", b"lib-ok");
}

fn test_shared_futex() {
    let shm = open_at(CWD, "/dev/shm/futex.bin\0", O_CREAT | O_EXCL | O_RDWR);
    assert!(shm >= 0);
    let shm = shm as usize;
    assert_eq!(syscall(46, [shm, PAGE, 0, 0]), 0);
    let parent_map = syscall6(222, [0, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0]);
    assert!(parent_map > 0);
    let parent_addr = parent_map as usize;
    let state = unsafe { &*(parent_addr as *const AtomicU32) };
    let exit_tid = unsafe { &*((parent_addr + 4) as *const AtomicU32) };
    state.store(0, Ordering::SeqCst);
    exit_tid.store(u32::MAX, Ordering::SeqCst);

    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        // Remap the same /dev/shm page at a different virtual address in the child.
        let extra = syscall6(222, [0, PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0]);
        assert!(extra > 0 && extra as usize != parent_addr);
        assert_eq!(syscall(215, [parent_addr, PAGE, 0, 0]), 0);
        let child_addr = extra as usize;
        let child_state = unsafe { &*(child_addr as *const AtomicU32) };
        let my_tid = syscall(96, [child_addr + 4, 0, 0, 0]); // set_tid_address on shared word
        assert!(my_tid > 0);
        unsafe { *((child_addr + 4) as *mut u32) = my_tid as u32 };
        sleep(20);
        child_state.store(1, Ordering::Release);
        assert!(syscall6(98, [child_addr, 1, 1, 0, 0, 0]) >= 0); // FUTEX_WAKE (shared)
        sleep(20);
        exit(0);
    }

    while state.load(Ordering::Acquire) == 0 {
        let rc = syscall6(98, [parent_addr, 0, 0, 0, 0, 0]); // FUTEX_WAIT (shared)
        assert!(rc == 0 || rc == -11, "unexpected futex wait rc={}", rc);
    }
    assert_eq!(state.load(Ordering::Acquire), 1);

    // Wait for child exit to clear exit_tid to 0 and FUTEX_WAKE the shared word.
    loop {
        let cur = exit_tid.load(Ordering::Acquire);
        if cur == 0 {
            break;
        }
        let rc = syscall6(98, [parent_addr + 4, 0, cur as usize, 0, 0, 0]);
        assert!(rc == 0 || rc == -11, "unexpected clear_child_tid wait rc={}", rc);
    }
    let mut status = 0;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0);
    assert_eq!(syscall(215, [parent_addr, PAGE, 0, 0]), 0);
    assert_eq!(close(shm), 0);
    assert_eq!(unlink(CWD, "/dev/shm/futex.bin\0"), 0);
}

#[no_mangle]
fn main() -> i32 {
    let dir = open_at(CWD, "/fixture\0", O_DIRECTORY);
    assert!(dir >= 0);
    let dir = dir as usize;
    let mut data = [b'!'; 160];
    assert_eq!(read_link(dir, "short\0", &mut data[..4]), 4);
    assert_eq!(&data[..5], b"data!");
    assert_eq!(read_link(dir, "edge\0", &mut data), 60);
    assert!(read_link(dir, "long\0", &mut data) > 60);
    assert_eq!(read_link(dir, "data/value\0", &mut data), -22);
    assert_eq!(read_link(dir, "missing\0", &mut data), -2);
    assert_eq!(read_link(dir, "short/\0", &mut data), -20);
    assert_eq!(read_link(dir, "short\0", &mut []), -22);
    assert_eq!(open_at(dir, "short\0", O_NOFOLLOW), -40);
    let link_fd = open_at(dir, "short\0", O_PATH | O_NOFOLLOW);
    assert!(link_fd >= 0);
    assert_eq!(read(link_fd as usize, &mut data), -9);
    assert_eq!(
        syscall(25, [link_fd as usize, 3, 0, 0]),
        (O_PATH | O_NOFOLLOW) as isize
    );
    assert_eq!(syscall(46, [link_fd as usize, 0, 0, 0]), -9);
    assert_eq!(read_link(link_fd as usize, "\0", &mut data), 10);
    assert_eq!(close(link_fd as usize), 0);
    expect_contents(dir, "short\0", b"payload");
    expect_contents(dir, "long\0", b"payload");
    expect_contents(dir, "dir-link/../short\0", b"payload");
    expect_contents(dir, "dir-link/value\0", b"payload");
    expect_contents(dir, "unicode\0", b"UTF-8");
    expect_contents(dir, "data/多字节\0", b"UTF-8");

    let lower = open_at(dir, "edge\0", O_PATH | O_NOFOLLOW);
    assert!(lower >= 0);
    let lower = lower as usize;
    assert_eq!(
        syscall(
            38,
            [
                dir,
                b"edge\0".as_ptr() as usize,
                dir,
                b"moved-edge\0".as_ptr() as usize
            ]
        ),
        0
    );
    let moved = open_at(dir, "moved-edge\0", O_PATH | O_NOFOLLOW);
    assert!(moved >= 0);
    let mut before = [0; 128];
    let mut after = [0; 128];
    assert_eq!(syscall(80, [lower, before.as_mut_ptr() as usize, 0, 0]), 0);
    assert_eq!(
        syscall(80, [moved as usize, after.as_mut_ptr() as usize, 0, 0]),
        0
    );
    assert_eq!(&before[..16], &after[..16]);
    assert_eq!(unlink(dir, "moved-edge\0"), 0);
    assert_eq!(syscall(80, [lower, after.as_mut_ptr() as usize, 0, 0]), 0);
    assert_eq!(&after[20..24], &[0; 4]);
    assert_eq!(read_link(lower, "\0", &mut data), 60);
    assert_eq!(close(lower), 0);
    assert_eq!(close(moved as usize), 0);

    let mut stat = [0; 128];
    for (flags, kind) in [(0, 0o100000), (0x100, 0o120000)] {
        assert_eq!(
            syscall(
                79,
                [
                    dir,
                    b"short\0".as_ptr() as usize,
                    stat.as_mut_ptr() as usize,
                    flags
                ]
            ),
            0
        );
        assert_eq!(
            u32::from_ne_bytes(stat[16..20].try_into().unwrap()) & 0o170000,
            kind
        );
    }
    assert_eq!(link_at("new-target\0", dir, "new-link\0"), 0);
    assert_eq!(link_at("missing\0", dir, "new-link\0"), -17);
    assert_eq!(open_at(dir, "new-link\0", O_CREAT | O_EXCL | O_RDWR), -17);
    let new_fd = open_at(dir, "new-link\0", O_CREAT | O_RDWR);
    assert!(new_fd >= 0);
    assert_eq!(write(new_fd as usize, b"new"), 3);
    assert_eq!(close(new_fd as usize), 0);
    assert_eq!(
        syscall(
            38,
            [
                dir,
                b"new-link\0".as_ptr() as usize,
                dir,
                b"moved-link\0".as_ptr() as usize
            ]
        ),
        0
    );
    assert_eq!(unlink(dir, "moved-link\0"), 0);
    expect_contents(dir, "new-target\0", b"new");
    assert_eq!(link_at("loop-b\0", dir, "loop-a\0"), 0);
    assert_eq!(link_at("loop-a\0", dir, "loop-b\0"), 0);
    assert_eq!(open_at(dir, "loop-a\0", 0), -40);
    assert_eq!(read_link(dir, "loop-a\0", &mut data), 6);

    assert_eq!(syscall(49, [b"/fixture\0".as_ptr() as usize, 0, 0, 0]), 0);
    let cwd_len = read_link(CWD, "/proc/self/cwd\0", &mut data);
    assert_eq!(cwd_len, 8);
    assert_eq!(&data[..8], b"/fixture");
    let exe_len = read_link(CWD, "/proc/self/exe\0", &mut data);
    assert_eq!(exe_len, 13);
    assert_eq!(&data[..13], b"/vfs_abi_test");
    let status = open_at(CWD, "/proc/self/status\0", 0);
    assert!(status >= 0);
    let size = read(status as usize, &mut data) as usize;
    assert!(data[..size].windows(5).any(|bytes| bytes == b"Pid:\t"));
    assert_eq!(close(status as usize), 0);
    assert_eq!(open_at(CWD, "/proc/2147483647/status\0", 0), -2);
    assert_eq!(close(dir), 0);
    assert_eq!(syscall(49, [b"/\0".as_ptr() as usize, 0, 0, 0]), 0);

    for name in ["/tmp\0", "/var/tmp\0", "/dev/shm\0"] {
        let fd = open_at(CWD, name, O_DIRECTORY);
        assert!(fd >= 0, "open {} returned {}", name, fd);
        let file = open_at(fd as usize, "scratch\0", O_CREAT | O_EXCL | O_RDWR);
        assert!(file >= 0);
        assert_eq!(close(file as usize), 0);
        assert_eq!(unlink(fd as usize, "scratch\0"), 0);
        assert_eq!(close(fd as usize), 0);
    }

    test_mounts_and_tmpfs();
    test_devices_and_lib_links();
    test_shared_futex();

    println!("vfs-abi-test-ok");
    0
}
