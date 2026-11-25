#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use core::convert::TryInto;
use user_lib::{chdir, close, exec, fork, open, read, waitpid, write, OpenFlags};

const AT_FDCWD: usize = (-100isize) as usize;
const PAGE: usize = 4096;
const ENOENT: isize = -2;
const EBUSY: isize = -16;
const EINVAL: isize = -22;
const EROFS: isize = -30;
const MS_RDONLY: usize = 1;
const MS_SILENT: usize = 1 << 15;
const MS_SYNC: usize = 4;
const MAP_SHARED: usize = 1;
const PROT_READ_WRITE: usize = 3;
const O_DIRECTORY: usize = 0o200000;
const EASYFS_MAGIC: u64 = 0x3b80_0001;
const EXT4_MAGIC: u64 = 0xef53;

const SOURCE: &str = "/dev/vdb\0";
const TARGET: &str = "/persist\0";
const LOWER: &str = "/persist/lower\0";
const TREE: &str = "/persist/tree\0";
const NESTED: &str = "/persist/tree/nested\0";
const PENDING: &str = "/persist/tree/nested/pending\0";
const PAYLOAD: &str = "/persist/tree/payload\0";
const REMOVED: &str = "/persist/tree/nested/removed\0";
const MSYNC_FILE: &str = "/persist/msync\0";
const FSYNC_FILE: &str = "/persist/fsync\0";
const CHILD: &str = "/persist/child\0";
const CHILD_ARG: &str = "--verify-persisted-exec\0";
const ORIGINAL_LEN: usize = 4 * PAGE + 103;
const SHRUNK_LEN: usize = PAGE + 37;
const REGROWN_LEN: usize = 3 * PAGE + 123;

fn syscall(id: usize, [a0, a1, a2, a3, a4, a5]: [usize; 6]) -> isize {
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

fn call(id: usize, [a, b, c, d]: [usize; 4]) -> isize {
    syscall(id, [a, b, c, d, 0, 0])
}

fn mount(target: &str, flags: usize) -> isize {
    call(
        40,
        [
            SOURCE.as_ptr() as usize,
            target.as_ptr() as usize,
            b"easyfs\0".as_ptr() as usize,
            flags,
        ],
    )
}

fn unmount() -> isize {
    call(39, [TARGET.as_ptr() as usize, 0, 0, 0])
}

fn fsync(fd: usize) -> isize {
    call(82, [fd, 0, 0, 0])
}

fn mkdir(path: &str) -> isize {
    call(34, [AT_FDCWD, path.as_ptr() as usize, 0o755, 0])
}

fn rename(old: &str, new: &str) -> isize {
    call(
        38,
        [
            AT_FDCWD,
            old.as_ptr() as usize,
            AT_FDCWD,
            new.as_ptr() as usize,
        ],
    )
}

fn unlink(path: &str) -> isize {
    call(35, [AT_FDCWD, path.as_ptr() as usize, 0, 0])
}

fn statfs(path: &str) -> u64 {
    let mut buf = [0u8; 120];
    assert_eq!(
        call(
            43,
            [path.as_ptr() as usize, buf.as_mut_ptr() as usize, 0, 0]
        ),
        0,
        "statfs {}",
        path
    );
    if u64::from_le_bytes(buf[..8].try_into().unwrap()) == EASYFS_MAGIC {
        assert_eq!(
            u64::from_le_bytes(buf[8..16].try_into().unwrap()),
            512,
            "EasyFS sector size"
        );
        let blocks = u64::from_le_bytes(buf[16..24].try_into().unwrap());
        let free = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        assert!(
            0 < free && free <= blocks && blocks < 32768,
            "EasyFS allocation stats"
        );
        assert_eq!(
            u64::from_le_bytes(buf[64..72].try_into().unwrap()),
            27,
            "EasyFS filename limit"
        );
    }
    u64::from_le_bytes(buf[..8].try_into().unwrap())
}

fn open_fd(path: &str, flags: OpenFlags) -> usize {
    let fd = open(path, flags);
    assert!(fd >= 0, "open {} returned {}", path, fd);
    fd as usize
}

fn assert_lower_visible() {
    assert_eq!(statfs(TARGET), EXT4_MAGIC);
    let fd = open_fd(LOWER, OpenFlags::RDONLY);
    let mut data = [0; 5];
    assert_eq!(read(fd, &mut data), 5);
    assert_eq!(&data, b"lower");
    assert_eq!(close(fd), 0);
}

fn mount_writable() {
    assert_eq!(mount(TARGET, MS_SILENT), 0, "mount /dev/vdb at /persist");
    assert_eq!(statfs(TARGET), EASYFS_MAGIC);
    assert_eq!(open(LOWER, OpenFlags::RDONLY), ENOENT);
    let fd = open_fd("/proc/mounts\0", OpenFlags::RDONLY);
    let mut mounts = [0; 1024];
    let len = read(fd, &mut mounts);
    assert!(len > 0);
    let entry = b"/dev/vdb /persist easyfs rw";
    assert!(mounts[..len as usize]
        .windows(entry.len())
        .any(|line| line == entry));
    assert_eq!(close(fd), 0);
}

fn pattern(offset: usize) -> u8 {
    ((offset * 37) ^ (offset >> 9)) as u8
}

fn assert_payload(fd: usize, preserved_len: usize, size: usize) {
    assert_eq!(call(62, [fd, 0, 0, 0]), 0, "seek to start of payload");
    let mut buffer = [0; PAGE];
    for offset in (0..size).step_by(PAGE) {
        let chunk = &mut buffer[..(size - offset).min(PAGE)];
        assert_eq!(read(fd, chunk), chunk.len() as isize, "read at {}", offset);
        for (index, byte) in chunk.iter().enumerate() {
            let position = offset + index;
            let expected = if position < preserved_len {
                pattern(position)
            } else {
                0
            };
            assert_eq!(*byte, expected, "payload at byte {}", position);
        }
    }
    assert_eq!(read(fd, &mut buffer), 0, "payload should end at {}", size);
}

fn open_mapping(path: &str, initial: u8) -> (usize, usize) {
    let fd = open_fd(path, OpenFlags::CREATE | OpenFlags::RDWR);
    assert_eq!(write(fd, &[initial; PAGE]), PAGE as isize);
    let addr = syscall(222, [0, PAGE, PROT_READ_WRITE, MAP_SHARED, fd, 0]);
    assert!(addr > 0, "mmap {} returned {}", path, addr);
    let addr = addr as usize;
    assert_eq!(unsafe { (addr as *const u8).read_volatile() }, initial);
    (fd, addr)
}

fn assert_mapped_data(path: &str, initial: u8, changes: &[(usize, u8)]) {
    let fd = open_fd(path, OpenFlags::RDONLY);
    let mut data = [0; PAGE];
    assert_eq!(read(fd, &mut data), PAGE as isize);
    for (index, byte) in data.iter().enumerate() {
        let expected = changes
            .iter()
            .find(|&&(position, _)| position == index)
            .map_or(initial, |&(_, value)| value);
        assert_eq!(*byte, expected, "{} at byte {}", path, index);
    }
    assert_eq!(read(fd, &mut data), 0);
    assert_eq!(close(fd), 0);
}

fn copy_child_executable() {
    let source = open_fd("/persist_abi_test\0", OpenFlags::RDONLY);
    let child = open_fd(CHILD, OpenFlags::CREATE | OpenFlags::RDWR);
    let mut buffer = [0; PAGE];
    loop {
        let count = read(source, &mut buffer);
        assert!(count >= 0, "read executable returned {}", count);
        if count == 0 {
            break;
        }
        assert_eq!(write(child, &buffer[..count as usize]), count);
    }
    assert_eq!(fsync(child), 0, "fsync copied executable");
    assert_eq!(close(child), 0);
    assert_eq!(close(source), 0);
}

fn assert_child_exec() {
    let pid = fork();
    assert!(pid >= 0, "fork returned {}", pid);
    if pid == 0 {
        let result = exec(
            CHILD,
            &[CHILD.as_ptr(), CHILD_ARG.as_ptr(), core::ptr::null()],
        );
        panic!("exec persisted child returned {}", result);
    }
    let mut status = -1;
    assert_eq!(waitpid(pid as usize, &mut status), pid);
    assert_eq!(status, 0, "persisted child exit status");
}

fn write_phase() {
    assert_lower_visible();
    mount_writable();
    assert_eq!(mount("/other\0", 0), EBUSY, "mount the same device twice");
    assert_eq!(mkdir(TREE), 0);
    assert_eq!(mkdir(NESTED), 0);

    let fd = open_fd(PENDING, OpenFlags::CREATE | OpenFlags::RDWR);
    let mut contents = [0; ORIGINAL_LEN];
    for (index, byte) in contents.iter_mut().enumerate() {
        *byte = pattern(index);
    }
    assert_eq!(write(fd, &contents), ORIGINAL_LEN as isize);
    assert_payload(fd, ORIGINAL_LEN, ORIGINAL_LEN);
    assert_eq!(call(46, [fd, SHRUNK_LEN, 0, 0]), 0, "shrink file");
    assert_payload(fd, SHRUNK_LEN, SHRUNK_LEN);
    assert_eq!(call(46, [fd, REGROWN_LEN, 0, 0]), 0, "extend with zeroes");
    assert_payload(fd, SHRUNK_LEN, REGROWN_LEN);
    assert_eq!(rename(PENDING, PAYLOAD), 0);
    assert_eq!(open(PENDING, OpenFlags::RDONLY), ENOENT);

    let removed = open_fd(REMOVED, OpenFlags::CREATE | OpenFlags::RDWR);
    assert_eq!(write(removed, b"removed"), 7);
    assert_eq!(close(removed), 0);
    assert_eq!(unlink(REMOVED), 0);
    assert_eq!(open(REMOVED, OpenFlags::RDONLY), ENOENT);

    assert_eq!(unmount(), EBUSY, "open file should pin the mount");
    assert_eq!(close(fd), 0);
    assert_eq!(chdir(NESTED), 0);
    assert_eq!(unmount(), EBUSY, "cwd should pin the mount");
    assert_eq!(chdir("/\0"), 0);
    assert_eq!(unmount(), 0);
    assert_lower_visible();
    assert_eq!(open(PAYLOAD, OpenFlags::RDONLY), ENOENT);
    mount_writable();
    copy_child_executable();

    let (fd, addr) = open_mapping(MSYNC_FILE, b'm');
    unsafe { ((addr + 71) as *mut u8).write_volatile(b'M') };
    assert_eq!(
        call(227, [addr, PAGE, MS_SYNC, 0]),
        0,
        "msync MAP_SHARED data"
    );
    unsafe { ((addr + 72) as *mut u8).write_volatile(b'N') };
    assert_eq!(call(215, [addr, PAGE, 0, 0]), 0);
    assert_eq!(fsync(fd), 0, "fsync data dirtied after msync");
    assert_eq!(close(fd), 0);

    let (fd, addr) = open_mapping(FSYNC_FILE, b'f');
    unsafe { ((addr + PAGE - 1) as *mut u8).write_volatile(b'F') };
    assert_eq!(fsync(fd), 0, "fsync MAP_SHARED data");
    assert_eq!(close(fd), 0);
    // Keep the mapping and mount live when the guest shuts down after fsync.
}

fn verify_phase() {
    mount_writable();
    let directory = call(56, [AT_FDCWD, NESTED.as_ptr() as usize, O_DIRECTORY, 0]);
    assert!(
        directory >= 0,
        "open nested directory returned {}",
        directory
    );
    assert_eq!(close(directory as usize), 0);
    assert_eq!(open(PENDING, OpenFlags::RDONLY), ENOENT);
    assert_eq!(open(REMOVED, OpenFlags::RDONLY), ENOENT);
    let fd = open_fd(PAYLOAD, OpenFlags::RDONLY);
    assert_payload(fd, SHRUNK_LEN, REGROWN_LEN);
    assert_eq!(close(fd), 0);
    assert_mapped_data(MSYNC_FILE, b'm', &[(71, b'M'), (72, b'N')]);
    assert_mapped_data(FSYNC_FILE, b'f', &[(PAGE - 1, b'F')]);
    assert_child_exec();
    assert_eq!(unmount(), 0);
    assert_lower_visible();
}

fn blank_phase() {
    assert_eq!(mount(TARGET, 0), EINVAL, "reject an unformatted device");
    assert_lower_visible();
}

fn readonly_phase() {
    assert_eq!(
        mount(TARGET, 0),
        EROFS,
        "reject a writable mount of a read-only device"
    );
    assert_lower_visible();
    assert_eq!(
        mount(TARGET, MS_RDONLY),
        0,
        "mount a read-only device with MS_RDONLY"
    );
    assert_eq!(statfs(TARGET), EASYFS_MAGIC);
    assert_eq!(mkdir(TREE), EROFS);
    assert_eq!(
        open("/persist/new\0", OpenFlags::CREATE | OpenFlags::RDWR),
        EROFS
    );
    assert_eq!(unmount(), 0);
    assert_lower_visible();
}

#[no_mangle]
fn main(_argc: usize, argv: &[&str]) -> i32 {
    if argv.get(1).copied() == CHILD_ARG.strip_suffix('\0') {
        return 0;
    }
    let fd = open_fd("/persist-phase\0", OpenFlags::RDONLY);
    let mut phase = [0];
    assert_eq!(read(fd, &mut phase), 1);
    assert_eq!(close(fd), 0);
    let completed = match phase[0] {
        b'w' => {
            write_phase();
            "WRITE"
        }
        b'v' => {
            verify_phase();
            "VERIFY"
        }
        b'b' => {
            blank_phase();
            "BLANK"
        }
        b'r' => {
            readonly_phase();
            "READONLY"
        }
        unknown => panic!("unknown persistence test phase {}", unknown),
    };
    println!("[persist-abi] {} PASS", completed);
    0
}
