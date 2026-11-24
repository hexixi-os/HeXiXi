#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;
extern crate alloc;

use alloc::string::String;
use core::arch::asm;
use user_lib::{close, exec, exit, fork, read, write};

const PAGE: usize = 4096;
const MIB: usize = 1024 * 1024;
const CWD: usize = (-100isize) as usize;
const RDWR: usize = 2;
const CREATE: usize = 0o100;
const EXCL: usize = 0o200;
const TRUNC: usize = 0o1000;
const APPEND: usize = 0o2000;
const DIRECTORY: usize = 0o200000;
const TMPFILE: usize = 0o20000000 | DIRECTORY;

fn call(number: usize, args: [usize; 6]) -> isize {
    let result;
    unsafe {
        asm!("ecall", in("a7") number, inlateout("a0") args[0] => result,
             in("a1") args[1], in("a2") args[2], in("a3") args[3],
             in("a4") args[4], in("a5") args[5]);
    }
    result
}

fn open_at(dir: usize, name: &str, flags: usize) -> isize {
    call(56, [dir, name.as_ptr() as usize, flags, 0o600, 0, 0])
}

fn open(name: &str, flags: usize) -> usize {
    let fd = open_at(CWD, name, flags);
    assert!(fd >= 0, "open {} returned {}", name, fd);
    fd as usize
}

fn seek(fd: usize, offset: usize) -> isize {
    call(62, [fd, offset, 0, 0, 0, 0])
}

fn resize(fd: usize, len: usize) -> isize {
    call(46, [fd, len, 0, 0, 0, 0])
}

fn unlink(dir: usize, name: &str, flags: usize) -> isize {
    call(35, [dir, name.as_ptr() as usize, flags, 0, 0, 0])
}

fn rename(from: usize, old: &str, to: usize, new: &str, flags: usize) -> isize {
    call(
        276,
        [
            from,
            old.as_ptr() as usize,
            to,
            new.as_ptr() as usize,
            flags,
            0,
        ],
    )
}

fn stat(dir: usize, name: &str, flags: usize) -> (u64, u64, u64) {
    let mut st = [0u64; 16];
    assert_eq!(
        call(
            79,
            [
                dir,
                name.as_ptr() as usize,
                st.as_mut_ptr() as usize,
                flags,
                0,
                0
            ]
        ),
        0
    );
    (st[1], st[2] >> 32, st[6])
}

fn map(fd: usize, flags: usize, len: usize) -> usize {
    let mapping = call(222, [0, len, 3, flags, fd, 0]);
    assert!(mapping > 0);
    mapping as usize
}

fn unmap(address: usize, len: usize) {
    assert_eq!(call(215, [address, len, 0, 0, 0, 0]), 0);
}

fn byte(address: usize) -> u8 {
    unsafe { (address as *const u8).read_volatile() }
}

fn store(address: usize, value: u8) {
    unsafe { (address as *mut u8).write_volatile(value) }
}

fn mkdir(dir: usize, name: &str) {
    assert_eq!(call(34, [dir, name.as_ptr() as usize, 0o700, 0, 0, 0]), 0);
}

fn test_budget() {
    let fd = open("big\0", RDWR | CREATE | EXCL);
    static BYTES: [u8; 64 * 1024] = [0x5a; 64 * 1024];
    for offset in (0..100 * MIB).step_by(BYTES.len()) {
        assert_eq!(write(fd, &BYTES), BYTES.len() as isize, "offset {}", offset);
    }
    assert_eq!(stat(CWD, "big\0", 0).2, 100 * MIB as u64);
    assert_eq!(seek(fd, 100 * MIB - PAGE), (100 * MIB - PAGE) as isize);
    let mut data = [0; PAGE];
    assert_eq!(read(fd, &mut data), PAGE as isize);
    assert!(data.iter().all(|&value| value == 0x5a));
    let keep = map(fd, 1, PAGE);
    assert_eq!(byte(keep), 0x5a);
    assert_eq!(unlink(CWD, "big\0", 0), 0);
    assert_eq!(close(fd), 0);
    let pressure = open("pressure\0", RDWR | CREATE | EXCL);
    for _ in 0..28 * MIB / BYTES.len() - 1 {
        assert_eq!(write(pressure, &BYTES), BYTES.len() as isize);
    }
    assert_eq!(
        write(pressure, &BYTES[..BYTES.len() - PAGE]),
        (BYTES.len() - PAGE) as isize
    );
    let iov = [[BYTES.as_ptr() as usize, 2 * PAGE]];
    assert_eq!(
        call(66, [pressure, iov.as_ptr() as usize, iov.len(), 0, 0, 0]),
        PAGE as isize
    );
    assert_eq!(write(pressure, b"!"), -28);
    assert_eq!(resize(pressure, 29 * MIB), 0);
    let hole = call(222, [0, PAGE, 3, 1, pressure, 28 * MIB]);
    assert!(hole > 0);
    let pid = fork();
    if pid == 0 {
        store(hole as usize, b'!');
        exit(123);
    }
    let mut status = 0;
    assert_eq!(
        call(
            260,
            [pid as usize, &mut status as *mut i32 as usize, 0, 0, 0, 0]
        ),
        pid
    );
    assert_eq!(status, 7);
    unmap(hole as usize, PAGE);
    assert_eq!(resize(pressure, 28 * MIB), 0);
    unmap(keep, PAGE);
    assert_eq!(write(pressure, b"!"), 1);
    assert_eq!(unlink(CWD, "pressure\0", 0), 0);
    assert_eq!(close(pressure), 0);
    println!("overlay large and global budget ok");
}

fn test_sparse_and_mappings() {
    let fd = open("sparse\0", RDWR | CREATE | EXCL);
    let last = 2 * 1024 * MIB + 19;
    assert_eq!(seek(fd, last), last as isize);
    assert_eq!(write(fd, b"x"), 1);
    assert_eq!(stat(CWD, "sparse\0", 0).2, (last + 1) as u64);
    assert_eq!(seek(fd, last - 2), (last - 2) as isize);
    let mut data = [0xff; 3];
    assert_eq!(read(fd, &mut data), 3);
    assert_eq!(&data, b"\0\0x");
    assert_eq!(resize(fd, 3 * PAGE), 0);
    let shared = map(fd, 1, 3 * PAGE);
    let private = map(fd, 2, 3 * PAGE);
    store(shared + 4, b's');
    store(shared + PAGE, b'd');
    store(private + PAGE, b'p');
    assert_eq!(seek(fd, 4), 4);
    assert_eq!(read(fd, &mut data[..1]), 1);
    assert_eq!(data[0], b's');
    assert_eq!(write(fd, b"f"), 1);
    assert_eq!(byte(shared + 5), b'f');
    assert_eq!(call(227, [shared, 3 * PAGE, 4, 0, 0, 0]), 0);
    assert_eq!(call(227, [shared + 1, PAGE, 4, 0, 0, 0]), -22);
    assert_eq!(call(227, [shared, PAGE, 5, 0, 0, 0]), -22);
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        assert_eq!(byte(shared + PAGE), b'd');
        assert_eq!(byte(private + PAGE), b'p');
        assert_eq!(resize(fd, 5), 0);
        assert_eq!(byte(shared + 5), 0);
        byte(private + PAGE);
        exit(123);
    }
    let mut status = 0;
    assert_eq!(
        call(
            260,
            [pid as usize, &mut status as *mut i32 as usize, 0, 0, 0, 0]
        ),
        pid
    );
    assert_eq!(status, 7);
    assert_eq!(byte(shared + 5), 0);
    store(shared + 6, b't');
    assert_eq!(resize(fd, 3 * PAGE), 0);
    assert_eq!(byte(shared + 6), 0);
    assert_eq!(byte(shared + PAGE), 0);
    assert_eq!(byte(private + PAGE), 0);
    assert_eq!(call(45, ["sparse\0".as_ptr() as usize, 0, 0, 0, 0, 0]), 0);
    assert_eq!(stat(CWD, "sparse\0", 0).2, 0);
    unmap(shared, 3 * PAGE);
    unmap(private, 3 * PAGE);
    assert_eq!(call(227, [shared, PAGE, 4, 0, 0, 0]), -12);
    assert_eq!(close(fd), 0);
    println!("overlay sparse and truncate mapping ok");
}

fn test_names_and_temporary() {
    mkdir(CWD, "a\0");
    mkdir(CWD, "b\0");
    let a = open("a\0", DIRECTORY);
    let b = open("b\0", DIRECTORY);
    let source = open("a/src\0", CREATE | EXCL | RDWR);
    let target = open("b/dst\0", CREATE | EXCL | RDWR);
    assert_eq!(write(source, b"src"), 3);
    assert_eq!(write(target, b"dst"), 3);
    let identity = stat(CWD, "a/src\0", 0).0;
    assert_ne!(stat(CWD, "b/dst\0", 0).0, identity);
    assert_eq!(rename(a, "src\0", CWD, "/dev/null\0", 0), -30);
    assert_eq!(open_at(CWD, "/dev/hidden\0", CREATE | RDWR), -30);
    assert_eq!(rename(a, "src\0", b, "dst\0", 1), -17);
    assert_eq!(rename(a, "src\0", b, "dst\0", 0), 0);
    assert_eq!(stat(b, "dst\0", 0).0, identity);
    assert_eq!(stat(source, "\0", 0x1000).0, identity);
    assert_eq!(stat(target, "\0", 0x1000).1, 0);
    assert_eq!(open_at(a, "src\0", 0), -2);
    assert_eq!(seek(target, 0), 0);
    let mut data = [0; 3];
    assert_eq!(read(target, &mut data), 3);
    assert_eq!(&data, b"dst");
    assert_eq!(rename(b, ".\0", a, "sub\0", 0), -22);
    assert_eq!(user_lib::chdir("/b\0"), 0);
    assert_eq!(rename(CWD, "/b\0", a, "sub\0", 0), 0);
    let mut cwd = [0u8; 16];
    assert_eq!(
        call(17, [cwd.as_mut_ptr() as usize, cwd.len(), 0, 0, 0, 0]),
        7
    );
    assert_eq!(&cwd[..7], b"/a/sub\0");
    assert_eq!(user_lib::chdir("/\0"), 0);
    assert_eq!(stat(b, "dst\0", 0).0, identity);
    assert_eq!(stat(CWD, "a/sub/dst\0", 0).0, identity);
    assert_eq!(unlink(CWD, "a/sub\0", 0x200), -39);
    assert_eq!(unlink(b, "dst\0", 0), 0);
    assert_eq!(stat(source, "\0", 0x1000).1, 0);
    let anonymous = open("a\0", TMPFILE | RDWR);
    assert_eq!(stat(anonymous, "\0", 0x1000).1, 0);
    assert_eq!(write(anonymous, b"tmp"), 3);
    let mapped = map(anonymous, 1, PAGE);
    assert_eq!(close(anonymous), 0);
    assert_eq!(byte(mapped), b't');
    unmap(mapped, PAGE);
    assert_eq!(unlink(a, "sub\0", 0x200), 0);
    assert_eq!(open_at(b, "dst\0", CREATE | RDWR), -2);
    for fd in [source, target, a, b] {
        assert_eq!(close(fd), 0);
    }
    println!("overlay rename and temporary lifetime ok");
}

fn test_lower() {
    let old = open("lower.bin\0", CREATE);
    let fd = open("lower.bin\0", RDWR | APPEND);
    assert_eq!(seek(fd, 0), 0);
    assert_eq!(write(fd, b"!"), 1);
    assert_eq!(stat(CWD, "lower.bin\0", 0).2, 6);
    assert_eq!(seek(fd, 0), 0);
    let mut data = [0; 6];
    assert_eq!(read(fd, &mut data), 6);
    assert_eq!(&data, b"lower!");
    assert_eq!(read(old, &mut data), 5);
    assert_eq!(&data[..5], b"lower");
    assert_eq!(resize(fd, 2), 0);
    assert_eq!(resize(fd, 5), 0);
    assert_eq!(seek(fd, 0), 0);
    assert_eq!(read(fd, &mut data), 5);
    assert_eq!(&data[..5], b"lo\0\0\0");
    assert_eq!(close(open("lower.bin\0", RDWR | TRUNC)), 0);
    assert_eq!(stat(CWD, "lower.bin\0", 0).2, 0);
    assert_eq!(rename(CWD, "lower.bin\0", CWD, "lower2.bin\0", 0), 0);
    assert_eq!(open_at(CWD, "lower.bin\0", 0), -2);
    assert_eq!(rename(CWD, "untouched.bin\0", CWD, "lower2.bin\0", 0), 0);
    assert_eq!(open_at(CWD, "untouched.bin\0", 0), -2);
    assert_eq!(stat(CWD, "lower2.bin\0", 0).2, 9);
    assert_eq!(unlink(CWD, "lower2.bin\0", 0), 0);
    assert_eq!(open_at(CWD, "lower2.bin\0", 0), -2);
    assert_eq!(close(old), 0);
    assert_eq!(close(fd), 0);
    println!("overlay lower copy-up and whiteout ok");
}

fn test_metadata_budget() {
    let mut path = String::new();
    for _ in 0..14 {
        path.push_str(&"d".repeat(230));
        path.push('\0');
        mkdir(CWD, &path);
        path.pop();
        path.push('/');
    }
    // Force the byte budget before the entry-count budget with long valid paths.
    let mut created = 0;
    let mut first = String::new();
    for number in 0..1024 {
        let name = alloc::format!("{}{:04}\0", path, number);
        let fd = open_at(CWD, &name, RDWR | CREATE | EXCL);
        if fd == -28 {
            assert!(created > 500);
            assert_eq!(unlink(CWD, &first, 0), 0);
            let fd = open_at(CWD, &name, RDWR | CREATE | EXCL);
            assert!(fd >= 0);
            assert_eq!(close(fd as usize), 0);
            println!("overlay metadata byte budget ok");
            return;
        }
        assert!(fd >= 0);
        assert_eq!(close(fd as usize), 0);
        created += 1;
        if created == 1 {
            first = name;
        }
    }
    panic!("metadata budget was not enforced");
}

fn test_sparse_exec() {
    let fd = open("exec-fixture\0", RDWR);
    assert_eq!(resize(fd, 2 * 1024 * MIB), 0);
    let end = 2 * 1024 * MIB - PAGE;
    assert_eq!(seek(fd, end), end as isize);
    assert_eq!(write(fd, b"!"), 1);
    assert_eq!(close(fd), 0);
    assert_exec(
        "exec-fixture\0",
        &[
            "exec-fixture\0".as_ptr(),
            "child\0".as_ptr(),
            core::ptr::null(),
        ],
    );

    let script = open("exec-script\0", RDWR | CREATE | EXCL);
    let shebang = b"#!/exec-fixture\n";
    assert_eq!(write(script, shebang), shebang.len() as isize);
    assert_eq!(close(script), 0);
    assert_exec(
        "exec-script\0",
        &["exec-script\0".as_ptr(), core::ptr::null()],
    );
    assert_eq!(unlink(CWD, "exec-script\0", 0), 0);
    assert_eq!(unlink(CWD, "exec-fixture\0", 0), 0);
    println!("overlay sparse exec snapshot ok");
}

fn assert_exec(path: &str, args: &[*const u8]) {
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        let result = exec(path, args);
        panic!("exec {:?} returned {}", path, result);
    }
    let mut status = 0;
    assert_eq!(
        call(
            260,
            [pid as usize, &mut status as *mut i32 as usize, 0, 0, 0, 0]
        ),
        pid
    );
    assert_eq!(status, 0);
}

#[no_mangle]
fn main(argc: usize, _argv: &[&str]) -> i32 {
    if argc == 2 {
        return 0;
    }
    test_budget();
    test_sparse_and_mappings();
    test_names_and_temporary();
    test_lower();
    test_sparse_exec();
    test_metadata_budget();
    println!("overlay-fs-test-ok");
    0
}
