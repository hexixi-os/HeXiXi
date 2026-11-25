#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::convert::TryInto;
use user_lib::{close, exit, fork, read, write};

const AT_FDCWD: isize = -100;
const AT_REMOVEDIR: usize = 0x200;
const O_DIRECTORY: usize = 0o200000;
const O_CREAT: usize = 0o100;
const O_EXCL: usize = 0o200;
const O_TRUNC: usize = 0o1000;
const O_RDWR: usize = 2;
const CROSS_PAGE: usize = 1024 * 1024;

fn syscall(number: usize, args: [usize; 4]) -> isize {
    let result;
    unsafe {
        asm!("ecall", in("a7") number, inlateout("a0") args[0] => result,
            in("a1") args[1], in("a2") args[2], in("a3") args[3],
            in("a4") 0usize, in("a5") 0usize);
    }
    result
}

fn openat(dir: isize, path: &str, flags: usize) -> isize {
    syscall(56, [dir as usize, path.as_ptr() as usize, flags, 0o666])
}

fn mkdirat(dir: isize, path: &str) -> isize {
    syscall(34, [dir as usize, path.as_ptr() as usize, 0o755, 0])
}

fn unlinkat(dir: isize, path: &str, flags: usize) -> isize {
    syscall(35, [dir as usize, path.as_ptr() as usize, flags, 0])
}

fn write_across_pages(vectored: bool) {
    let path = if vectored {
        "writev-probe\0"
    } else {
        "write-probe\0"
    };
    let fd = openat(AT_FDCWD, path, O_CREAT | O_RDWR);
    assert!(fd >= 0);
    let fd = fd as usize;
    assert_eq!(syscall(46, [fd, CROSS_PAGE - 8, 0, 0]), 0);
    assert_eq!(
        syscall(62, [fd, CROSS_PAGE - 8, 0, 0]),
        (CROSS_PAGE - 8) as isize
    );
    let bytes = *b"0123456789ABCDEF";
    let iov = [
        [bytes.as_ptr() as usize, 5],
        [bytes[5..].as_ptr() as usize, 11],
    ];
    let put = || {
        if vectored {
            syscall(66, [fd, iov.as_ptr() as usize, iov.len(), 0])
        } else {
            write(fd, &bytes)
        }
    };
    assert_eq!(put(), 16, "write across the former 1 MiB limit");
    assert_eq!(write(fd, &[]), 0);
    assert_eq!(syscall(66, [fd, iov.as_ptr() as usize, 0, 0]), 0);
    assert_eq!(
        syscall(62, [fd, CROSS_PAGE - 8, 0, 0]),
        (CROSS_PAGE - 8) as isize
    );
    let mut actual = [0; 16];
    assert_eq!(read(fd, &mut actual), 16);
    assert_eq!(actual, bytes);
    assert_eq!(close(fd), 0);
}

fn directories() {
    assert_eq!(user_lib::chdir("/basic\0"), 0);
    assert_eq!(mkdirat(AT_FDCWD, "abi-dir\0"), 0);
    let dir = openat(AT_FDCWD, "./abi-dir\0", O_DIRECTORY);
    assert!(dir >= 0, "mkdirat must create a directory relative to cwd");
    assert_eq!(mkdirat(AT_FDCWD, "abi-dir\0"), -17);
    assert_eq!(openat(AT_FDCWD, "/abi-dir\0", O_DIRECTORY), -2);
    assert_eq!(mkdirat(-99, "child\0"), -9);
    let dup = syscall(23, [dir as usize, 0, 0, 0]);
    assert!(dup >= 0);
    assert_eq!(mkdirat(dup, "child\0"), 0);
    assert_eq!(openat(dir, "child\0", O_CREAT | O_EXCL | O_RDWR), -17);
    assert_eq!(unlinkat(dir, "child\0", 0), -21);
    let file = openat(dir, "child/file\0", O_CREAT | O_RDWR);
    assert!(file >= 0);
    assert_eq!(openat(dir, "child/file\0", O_DIRECTORY), -20);
    assert_eq!(mkdirat(file, "nested\0"), -20);
    assert_eq!(mkdirat(dir, "missing/nested\0"), -2);
    assert_eq!(mkdirat(dir, "child/file/nested\0"), -20);
    assert_eq!(unlinkat(dir, "child\0", AT_REMOVEDIR), -39);
    let mut status = [0u8; 128];
    assert_eq!(
        syscall(80, [dir as usize, status.as_mut_ptr() as usize, 0, 0]),
        0
    );
    let mode = u32::from_ne_bytes(status[16..20].try_into().unwrap());
    assert_eq!(mode & 0o170000, 0o040000);
    assert_eq!(unlinkat(dir, "child/file\0", 0), 0);
    assert_eq!(unlinkat(dir, "child\0", AT_REMOVEDIR), 0);
    assert_eq!(openat(dir, "child\0", O_DIRECTORY), -2);
}

fn list_contains(dir: isize, wanted: &[u8]) -> bool {
    let mut entries = [0u8; 512];
    let len = syscall(
        61,
        [
            dir as usize,
            entries.as_mut_ptr() as usize,
            entries.len(),
            0,
        ],
    );
    assert!(len >= 0);
    entries[..len as usize]
        .windows(wanted.len())
        .any(|part| part == wanted)
}

fn unlink_files() {
    assert_eq!(user_lib::chdir("/basic\0"), 0);
    let file = openat(AT_FDCWD, "./temp-file\0", O_CREAT | O_RDWR);
    assert!(file >= 0);
    assert_eq!(write(file as usize, b"upper"), 5);
    assert_eq!(unlinkat(AT_FDCWD, "./temp-file\0", 0), 0);
    assert_eq!(
        openat(AT_FDCWD, "./temp-file\0", 0),
        -2,
        "unlink must use cwd"
    );
    assert_eq!(unlinkat(AT_FDCWD, "./temp-file\0", 0), -2);
    assert_eq!(syscall(62, [file as usize, 0, 0, 0]), 0);
    let mut actual = [0; 5];
    assert_eq!(read(file as usize, &mut actual), 5);
    assert_eq!(&actual, b"upper");

    let lower = openat(AT_FDCWD, "lower-file\0", 0);
    assert!(lower >= 0);
    assert_eq!(unlinkat(AT_FDCWD, "lower-file\0", 0), 0);
    assert_eq!(openat(AT_FDCWD, "lower-file\0", 0), -2);
    let dir = openat(-99, "/basic\0", O_DIRECTORY);
    assert!(dir >= 0);
    assert!(!list_contains(dir, b"lower-file\0"));
    let replacement = openat(dir, "lower-file\0", O_CREAT | O_EXCL | O_RDWR);
    assert!(replacement >= 0);
    assert_eq!(read(replacement as usize, &mut actual), 0);
    assert_eq!(read(lower as usize, &mut actual), 5);
    assert_eq!(&actual, b"lower");
    assert_eq!(unlinkat(dir, "lower-file\0", 0), 0);
    assert_eq!(
        openat(dir, "lower-file\0", 0),
        -2,
        "lower inode stays hidden"
    );
    let lower_dir = openat(dir, "lower-dir\0", O_DIRECTORY);
    assert!(lower_dir >= 0);
    assert_eq!(unlinkat(dir, "lower-dir\0", AT_REMOVEDIR), 0);
    assert_eq!(openat(dir, "lower-dir\0", O_DIRECTORY), -2);
    assert_eq!(mkdirat(dir, "lower-dir\0"), 0);
    assert_eq!(openat(lower_dir, "new-file\0", O_CREAT | O_RDWR), -2);
}

fn clone_stack() {
    let mut stack = [0usize; 32];
    let child_sp = unsafe { stack.as_mut_ptr().add(stack.len()) } as usize & !15;
    let pid: isize;
    unsafe {
        asm!("ecall", "bnez a0, 2f", "sub a0, sp, a1", "snez a0, a0",
            "li a7, 93", "ecall", "unimp", "2:",
            inlateout("a0") 17usize => pid, in("a1") child_sp,
            in("a2") 0usize, in("a3") 0usize, in("a4") 0usize, inlateout("a7") 220usize => _);
    }
    assert!(pid > 0);
    let mut status = 0i32;
    assert_eq!(user_lib::waitpid(pid as usize, &mut status), pid);
    assert_eq!(
        status, 0,
        "the cloned process must run on the supplied stack"
    );
}

fn path_components() {
    assert_eq!(user_lib::chdir("/basic\0"), 0);
    let file = openat(AT_FDCWD, "path-guard\0", O_CREAT | O_RDWR);
    assert!(file >= 0);
    assert_eq!(write(file as usize, b"data"), 4);
    assert_eq!(unlinkat(AT_FDCWD, "missing/../path-guard\0", 0), -2);
    assert_eq!(unlinkat(AT_FDCWD, "path-guard/../path-guard\0", 0), -20);
    assert_eq!(unlinkat(AT_FDCWD, "path-guard/\0", 0), -20);
    assert_eq!(openat(AT_FDCWD, "path-guard/\0", O_TRUNC | O_RDWR), -20);
    assert_eq!(syscall(62, [file as usize, 0, 2, 0]), 4);
    assert_eq!(mkdirat(AT_FDCWD, "dot-dir\0"), 0);
    assert_eq!(unlinkat(AT_FDCWD, "dot-dir/.\0", AT_REMOVEDIR), -22);
    assert_eq!(unlinkat(AT_FDCWD, "dot-dir/\0", AT_REMOVEDIR), 0);
}

fn invalid_path() {
    const PAGE: usize = 4096;
    assert_eq!(user_lib::chdir("/basic\0"), 0);
    let name = "fault-guard";
    assert!(openat(AT_FDCWD, "fault-guard\0", O_CREAT | O_RDWR) >= 0);
    let mapping = syscall(222, [0, 2 * PAGE, 3, 0x22]); // anonymous, private
    assert!(mapping > 0);
    let mapping = mapping as usize;
    assert_eq!(syscall(215, [mapping + PAGE, PAGE, 0, 0]), 0);
    let path = mapping + PAGE - name.len();
    unsafe { core::ptr::copy_nonoverlapping(name.as_ptr(), path as *mut u8, name.len()) };
    assert_eq!(syscall(35, [AT_FDCWD as usize, path, 0, 0]), -14);
    assert!(openat(AT_FDCWD, "fault-guard\0", 0) >= 0);
}

fn removed_directory_fd() {
    assert_eq!(user_lib::chdir("/basic\0"), 0);
    assert_eq!(mkdirat(AT_FDCWD, "replace-dir\0"), 0);
    let old = openat(AT_FDCWD, "replace-dir\0", O_DIRECTORY);
    assert!(old >= 0);
    assert_eq!(unlinkat(AT_FDCWD, "replace-dir\0", AT_REMOVEDIR), 0);
    assert_eq!(mkdirat(AT_FDCWD, "replace-dir\0"), 0);
    assert_eq!(openat(old, "different-entry\0", O_CREAT | O_RDWR), -2);
    let mut entries = [0u8; 128];
    assert_eq!(
        syscall(
            61,
            [
                old as usize,
                entries.as_mut_ptr() as usize,
                entries.len(),
                0
            ]
        ),
        -2
    );
    let new = openat(AT_FDCWD, "replace-dir\0", O_DIRECTORY);
    assert!(new >= 0);
    assert_eq!(openat(new, "different-entry\0", 0), -2);

    assert_eq!(user_lib::chdir("replace-dir\0"), 0);
    assert_eq!(unlinkat(AT_FDCWD, "/basic/replace-dir\0", AT_REMOVEDIR), 0);
    assert_eq!(mkdirat(AT_FDCWD, "/basic/replace-dir\0"), 0);
    assert_eq!(openat(AT_FDCWD, "different-entry\0", O_CREAT | O_RDWR), -2);
}

fn check(name: &str, test: impl FnOnce()) -> bool {
    let pid = fork();
    assert!(pid >= 0);
    if pid == 0 {
        test();
        exit(0);
    }
    let mut status = 0;
    assert_eq!(user_lib::waitpid(pid as usize, &mut status), pid);
    println!("basic-abi {} status={}", name, status);
    status == 0
}

fn default_paths() {
    assert_eq!(openat(AT_FDCWD, "/basic/mnt\0", O_DIRECTORY), -20);
    let argv = [core::ptr::null()];
    assert_eq!(user_lib::exec("/bad-shebang\0", &argv), -2);
    assert_eq!(user_lib::exec("/missing/busybox\0", &argv), -2);
}

#[no_mangle]
fn main() -> i32 {
    let write = check("write", || write_across_pages(false));
    let writev = check("writev", || write_across_pages(true));
    let mkdir = check("mkdirat", directories);
    let unlink = check("unlinkat", unlink_files);
    let clone = check("clone", clone_stack);
    let components = check("path-components", path_components);
    let invalid = check("invalid-path", invalid_path);
    let dir = check("removed-directory-fd", removed_directory_fd);
    let default_paths = check("default-paths", default_paths);
    assert!(write && writev && mkdir && unlink && clone && components && invalid && dir && default_paths);
    println!("basic-abi-test-ok");
    0
}
