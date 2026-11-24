use crate::fs::{self, make_pipe, OpenFlags};
use crate::mm::{
    translated_byte_buffer, translated_path, translated_ref, translated_refmut, translated_str,
    MapPermission, UserBuffer,
};
use crate::task::{current_process, current_user_token};
use alloc::{string::String, sync::Arc};

const AT_FDCWD: isize = -100;
const AT_EMPTY_PATH: usize = 0x1000;

fn with_io_file<T>(file: Arc<dyn fs::File>, operation: impl FnOnce(&dyn fs::File) -> T) -> T {
    let pointer = Arc::as_ptr(&file);
    crate::task::current_task().unwrap().inner_exclusive_access().user_io_file = Some(file);
    // The task pins the file through the syscall. If the thread group exits,
    // the syscall is discarded and the task's cleanup drops the reference.
    operation(unsafe { &*pointer })
}

pub fn sys_write(fd: usize, buf: *const u8, len: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        drop(process);
        match translated_byte_buffer(token, buf, len, MapPermission::R) {
            Some(buffers) => with_io_file(file, |file| file.write(UserBuffer::new(buffers))),
            None => -14,
        }
    } else {
        -1
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if file.directory().is_some() {
            return -21; // EISDIR
        }
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        drop(process);
        match translated_byte_buffer(token, buf, len, MapPermission::W) {
            Some(buffers) => with_io_file(file, |file| file.read(UserBuffer::new(buffers)) as isize),
            None => -14,
        }
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    let inode = match crate::fs::open_dyn(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let mut inner = process.inner_exclusive_access();
    let fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => return -24, // EMFILE
    };
    inner.fd_table[fd] = Some(inode);
    fd as isize
}

pub fn sys_close(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    inner.fd_table[fd].take();
    inner.set_close_on_exec(fd, false);
    0
}

const O_CLOEXEC: u32 = 0o2000000;

pub fn sys_pipe(pipe: *mut usize, flags: u32) -> isize {
    if flags & !O_CLOEXEC != 0 {
        return -22;
    }
    let process = current_process();
    let token = current_user_token();
    // Resolve writable pages before borrowing the process to install descriptors.
    let output = match translated_byte_buffer(token, pipe.cast(), 8, MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14, // EFAULT
    };
    let mut inner = process.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => return -24, // EMFILE
    };
    inner.fd_table[read_fd] = Some(pipe_read);
    let write_fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => {
            inner.fd_table[read_fd] = None;
            return -24; // EMFILE
        }
    };
    inner.fd_table[write_fd] = Some(pipe_write);
    if flags & O_CLOEXEC != 0 {
        inner.set_close_on_exec(read_fd, true);
        inner.set_close_on_exec(write_fd, true);
    }
    // Linux 的 pipe(int fd[2]) 使用两个 4 字节整数。
    let descriptors = (read_fd as i32)
        .to_ne_bytes()
        .into_iter()
        .chain((write_fd as i32).to_ne_bytes());
    for (dst, byte) in output.into_iter().zip(descriptors) {
        unsafe { dst.write(byte) };
    }
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if inner.fd_table[fd].is_none() {
        return -1;
    }
    let new_fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => return -24, // EMFILE
    };
    inner.fd_table[new_fd] = Some(Arc::clone(inner.fd_table[fd].as_ref().unwrap()));
    new_fd as isize
}

/// writev(2)：把多个 iovec 拼成一个 UserBuffer 后写出
pub fn sys_writev(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() {
            return -1;
        }
        let file = file.clone();
        drop(inner);
        drop(process);
        let mut buffers = alloc::vec::Vec::new();
        for i in 0..iovcnt {
            let entry = (iov as usize + i * 16) as *const usize;
            let base = *translated_ref(token, entry);
            let len = *translated_ref(token, (entry as usize + 8) as *const usize);
            if len == 0 {
                continue;
            }
            match translated_byte_buffer(token, base as *const u8, len, MapPermission::R) {
                Some(mut chunk) => buffers.append(&mut chunk),
                None => return -14,
            }
        }
        with_io_file(file, |file| file.write(UserBuffer::new(buffers)))
    } else {
        -1
    }
}

/// ioctl(2)：只支持 isatty 探测，其余返回 ENOTTY
pub fn sys_ioctl() -> isize {
    -25 // ENOTTY
}

fn resolve_at(dirfd: isize, path: &str) -> Result<String, isize> {
    if path.is_empty() {
        return Err(-2); // ENOENT
    }
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return fs::resolve_path(path);
    }

    let process = current_process();
    let inner = process.inner_exclusive_access();
    let file = inner
        .fd_table
        .get(dirfd as usize)
        .and_then(Option::as_ref)
        .cloned()
        .ok_or(-9isize)?;
    drop(inner);
    let directory = file.directory().ok_or(-20isize)?.location();
    if !fs::directory_is_live(directory) {
        return Err(-2);
    }
    fs::resolve_from(path, &directory.path())
}

/// openat(56)：解析 cwd 或目录 fd，并把 Linux flags 翻译成内核 OpenFlags。
pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> isize {
    let raw = match translated_path(current_user_token(), path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let path = match resolve_at(dirfd, &raw) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let mut open_flags = match flags & 0b11 {
        1 => OpenFlags::WRONLY,
        2 => OpenFlags::RDWR,
        _ => OpenFlags::RDONLY,
    };
    for (linux, internal) in [
        (0o100, OpenFlags::CREATE),
        (0o200, OpenFlags::EXCL),
        (0o1000, OpenFlags::TRUNC),
        (0o2000, OpenFlags::APPEND),
        (0o200000, OpenFlags::DIRECTORY),
    ] {
        if flags & linux != 0 {
            open_flags |= internal;
        }
    }
    if flags & 0o20000000 != 0 {
        if !open_flags.contains(OpenFlags::DIRECTORY) {
            return -22;
        }
        open_flags |= OpenFlags::TMPFILE;
    }
    if raw.ends_with('/') {
        open_flags |= OpenFlags::DIRECTORY;
    }
    let process = current_process();
    let fd = match process.inner_exclusive_access().alloc_fd() {
        Some(fd) => fd,
        None => return -24,
    };
    let file = match fs::open_dyn(&path, open_flags) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let mut inner = process.inner_exclusive_access();
    inner.fd_table[fd] = Some(file);
    inner.set_close_on_exec(fd, flags & O_CLOEXEC != 0);
    fd as isize
}

/// dup3(24)：把 oldfd 复制到指定的 newfd
pub fn sys_dup3(oldfd: usize, newfd: usize, flags: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if flags & !O_CLOEXEC != 0 {
        return -22;
    }
    if newfd >= inner.nofile_limit {
        return -9;
    }
    if oldfd >= inner.fd_table.len() || oldfd == newfd {
        return -1;
    }
    let file = match &inner.fd_table[oldfd] {
        Some(f) => f.clone(),
        None => return -1,
    };
    while inner.fd_table.len() <= newfd {
        inner.fd_table.push(None);
    }
    inner.fd_table[newfd] = Some(file);
    inner.set_close_on_exec(newfd, flags & O_CLOEXEC != 0);
    newfd as isize
}

/// chdir(49)：切换到解析后的目录。
pub fn sys_chdir(path: *const u8) -> isize {
    let directory = match translated_path(current_user_token(), path)
        .and_then(|raw| resolve_at(AT_FDCWD, &raw))
        .and_then(|path| fs::working_directory(&path))
    {
        Ok(directory) => directory,
        Err(errno) => return errno,
    };
    current_process().inner_exclusive_access().cwd = directory;
    0
}

/// getcwd(17) returns the path length including the trailing NUL.
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let cwd = current_process().inner_exclusive_access().cwd.clone();
    if !fs::directory_is_live(&cwd) {
        return -2;
    }
    let path = cwd.path();
    let count = 1 + path.len() + 1;
    if size < count {
        return -34;
    }
    let target = match translated_byte_buffer(current_user_token(), buf, count, MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    let bytes = core::iter::once(b'/').chain(path.bytes()).chain(core::iter::once(0));
    for (dst, byte) in target.into_iter().zip(bytes) {
        unsafe { dst.write(byte); }
    }
    count as isize
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8) -> isize {
    translated_path(current_user_token(), path)
        .and_then(|raw| resolve_at(dirfd, &raw))
        .and_then(|path| fs::mkdir(&path))
        .err()
        .unwrap_or(0)
}

pub fn sys_unlinkat(dirfd: isize, path: *const u8, flags: usize) -> isize {
    const AT_REMOVEDIR: usize = 0x200;
    if flags & !AT_REMOVEDIR != 0 {
        return -22; // EINVAL
    }
    let remove_directory = flags & AT_REMOVEDIR != 0;
    translated_path(current_user_token(), path)
        .and_then(|raw| {
            let path = resolve_at(dirfd, &raw)?;
            if remove_directory {
                match raw.trim_end_matches('/').rsplit('/').next() {
                    Some(".") => return Err(-22),
                    Some("..") => return Err(-39),
                    _ => {}
                }
            }
            if raw.ends_with('/') {
                fs::require_directory(&path)?;
            }
            fs::unlink(&path, remove_directory)
        })
        .err()
        .unwrap_or(0)
}

fn file_by_fd(fd: usize) -> Result<Arc<dyn fs::File + Send + Sync>, isize> {
    current_process().inner_exclusive_access().fd_table.get(fd)
        .and_then(Option::as_ref).cloned().ok_or(-9)
}

pub fn sys_fstat(fd: usize, st: *mut u8) -> isize {
    match file_by_fd(fd) {
        Ok(file) => write_stat(st, file.as_ref(), fd as u64),
        Err(errno) => errno,
    }
}

/// mount / umount2：无真实挂载，直接成功
pub fn sys_mount_stub() -> isize {
    0
}

/// getdents64(61)：按打开的目录描述读取目录项。
pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> isize {
    let file = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        match inner.fd_table.get(fd).and_then(Option::as_ref) {
            Some(file) => file.clone(),
            None => return -9,
        }
    };
    let directory = match file.directory() {
        Some(directory) => directory,
        None => return -20,
    };
    if !fs::directory_is_live(directory.location()) {
        return -2;
    }
    let entries = fs::list_dir_path(&directory.location().path());
    let mut position = directory.position();
    if position >= entries.len() {
        return 0;
    }

    let mut out = alloc::vec::Vec::new();
    for (name, ino, file_type) in entries.iter().skip(position) {
        // struct linux_dirent64 { u64 ino; s64 off; u16 reclen; u8 type; char name[]; }
        let reclen = (19 + name.len() + 1 + 7) & !7usize;
        if out.len() + reclen > len {
            if out.is_empty() {
                return -22;
            }
            break;
        }
        let start = out.len();
        out.resize(start + reclen, 0);
        position += 1;
        out[start..start + 8].copy_from_slice(&u64::from(*ino).to_le_bytes());
        out[start + 8..start + 16].copy_from_slice(&(position as i64).to_le_bytes());
        out[start + 16..start + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        out[start + 18] = match file_type {
            1 => 8,  // DT_REG
            2 => 4,  // DT_DIR
            3 => 3,  // DT_CHR
            4 => 6,  // DT_BLK
            5 => 1,  // DT_FIFO
            6 => 12, // DT_SOCK
            7 => 10, // DT_LNK
            _ => 0,  // DT_UNKNOWN
        };
        out[start + 19..start + 19 + name.len()].copy_from_slice(name.as_bytes());
    }
    let n = out.len();
    let target = match translated_byte_buffer(current_user_token(), buf, n, MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    for (dst, byte) in target.into_iter().zip(out) {
        unsafe { dst.write(byte) };
    }
    directory.set_position(position);
    n as isize
}

fn stat_mode(file: &dyn fs::File) -> u16 {
    if file.directory().is_some() { 0o40755 } else { 0o100644 }
}

fn write_stat_bytes(st: *mut u8, bytes: &[u8]) -> isize {
    let target = match translated_byte_buffer(current_user_token(), st, bytes.len(), MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    for (dst, &byte) in target.into_iter().zip(bytes) {
        unsafe { dst.write(byte); }
    }
    0
}

fn write_stat(st: *mut u8, file: &dyn fs::File, fallback_inode: u64) -> isize {
    let (device, inode) = file.stat_identity().unwrap_or((0, fallback_inode));
    let mut buf = [0u8; 128];
    buf[0..8].copy_from_slice(&device.to_le_bytes());
    buf[8..16].copy_from_slice(&inode.to_le_bytes());
    buf[16..20].copy_from_slice(&u32::from(stat_mode(file)).to_le_bytes());
    buf[20..24].copy_from_slice(&file.link_count().to_le_bytes());
    buf[48..56].copy_from_slice(&(file.size() as u64).to_le_bytes());
    buf[56..60].copy_from_slice(&4096u32.to_le_bytes());
    write_stat_bytes(st, &buf)
}

fn stat_file_at(
    dirfd: isize,
    raw: &str,
    flags: usize,
) -> Result<(Arc<dyn fs::File + Send + Sync>, u64), isize> {
    let path = if raw.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd != AT_FDCWD {
            return file_by_fd(dirfd as usize).map(|file| (file, dirfd as u64));
        }
        fs::resolve_path(".")?
    } else {
        resolve_at(dirfd, raw)?
    };
    if raw.ends_with('/') {
        fs::require_directory(&path)?;
    }
    fs::open_dyn(&path, OpenFlags::RDONLY).map(|file| (file, 0))
}

pub fn sys_newfstatat(dirfd: isize, path: *const u8, st: *mut u8, flags: usize) -> isize {
    match translated_path(current_user_token(), path).and_then(|raw| stat_file_at(dirfd, &raw, flags)) {
        Ok((file, fallback_inode)) => write_stat(st, file.as_ref(), fallback_inode),
        Err(errno) => errno,
    }
}

pub fn sys_statx(dirfd: isize, path: *const u8, flags: usize, mask: u32, st: *mut u8) -> isize {
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_NO_AUTOMOUNT: usize = 0x800;
    const AT_STATX_SYNC_TYPE: usize = 0x6000;
    const STATX_RESERVED: u32 = 0x8000_0000;
    const STATX_AVAILABLE: u32 = 0x207; // type, mode, link count, size
    const STATX_INO: u32 = 0x100;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE) != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return -22;
    }
    let raw = if path.is_null() && flags & AT_EMPTY_PATH != 0 {
        Ok(String::new())
    } else {
        translated_path(current_user_token(), path)
    };
    let (file, _) = match raw.and_then(|raw| stat_file_at(dirfd, &raw, flags)) {
        Ok(info) => info,
        Err(errno) => return errno,
    };
    let identity = file.stat_identity();
    let (device, inode) = identity.unwrap_or_default();
    let available = STATX_AVAILABLE | if identity.is_some() { STATX_INO } else { 0 };
    let major = ((device >> 8) as u32 & 0xfff) | ((device >> 32) as u32 & !0xfff);
    let minor = (device as u32 & 0xff) | ((device >> 12) as u32 & !0xff);
    let mut buf = [0u8; 256]; // Linux uapi struct statx
    buf[0..4].copy_from_slice(&available.to_le_bytes());
    buf[4..8].copy_from_slice(&4096u32.to_le_bytes());
    buf[16..20].copy_from_slice(&file.link_count().to_le_bytes());
    buf[28..30].copy_from_slice(&stat_mode(file.as_ref()).to_le_bytes());
    buf[32..40].copy_from_slice(&inode.to_le_bytes());
    buf[40..48].copy_from_slice(&(file.size() as u64).to_le_bytes());
    buf[136..140].copy_from_slice(&major.to_le_bytes());
    buf[140..144].copy_from_slice(&minor.to_le_bytes());
    write_stat_bytes(st, &buf)
}

pub fn sys_renameat(oldfd: isize, oldpath: *const u8, newfd: isize, newpath: *const u8, flags: usize) -> isize {
    const RENAME_NOREPLACE: usize = 1;
    if flags & !RENAME_NOREPLACE != 0 {
        return -22;
    }
    let token = current_user_token();
    let result = (|| {
        let old = translated_path(token, oldpath)?;
        let new = translated_path(token, newpath)?;
        for path in [&old, &new] {
            if matches!(path.trim_end_matches('/').rsplit('/').next(), Some("." | "..")) {
                return Err(-22);
            }
        }
        let old_name = resolve_at(oldfd, &old)?;
        let new_name = resolve_at(newfd, &new)?;
        if old.ends_with('/') {
            fs::require_directory(&old_name)?;
        }
        if new.ends_with('/') {
            fs::require_directory(&old_name)?;
        }
        fs::rename_overlay(&old_name, &new_name, flags == RENAME_NOREPLACE)
    })();
    result.err().unwrap_or(0)
}

pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_DUPFD_CLOEXEC: usize = 1030;
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            if fd >= inner.fd_table.len() {
                return -1;
            }
            let file = match &inner.fd_table[fd] {
                Some(f) => f.clone(),
                None => return -1,
            };
            let mut new_fd = arg;
            while inner.fd_table.len() <= new_fd {
                if inner.fd_table.len() >= inner.nofile_limit {
                    return -24; // EMFILE
                }
                inner.fd_table.push(None);
            }
            while inner.fd_table[new_fd].is_some() {
                new_fd += 1;
                if inner.fd_table.len() <= new_fd {
                    if inner.fd_table.len() >= inner.nofile_limit {
                        return -24; // EMFILE
                    }
                    inner.fd_table.push(None);
                }
            }
            inner.fd_table[new_fd] = Some(file);
            inner.set_close_on_exec(new_fd, cmd == F_DUPFD_CLOEXEC);
            new_fd as isize
        }
        F_GETFD | F_SETFD => {
            if inner.fd_table.get(fd).and_then(Option::as_ref).is_none() {
                return -9;
            }
            if cmd == F_GETFD {
                inner.close_on_exec.contains(&fd) as isize
            } else {
                inner.set_close_on_exec(fd, arg & 1 != 0);
                0
            }
        }
        _ => 0,
    }
}

/// ppoll(73)：内核没有真正的 poll，这里把所有 fd 都标记为可读
pub fn sys_ppoll(fds: *mut u8, nfds: usize) -> isize {
    let token = current_user_token();
    for i in 0..nfds {
        let base = fds as usize + i * 8;
        let events = *translated_ref(token, (base + 4) as *const u16);
        *translated_refmut(token, (base + 6) as *mut u16) = events | 0x1; // POLLIN
    }
    nfds as isize
}

/// sendfile(71)：暂不支持，返回 ENOSYS（busybox 会自动回退到 read/write）
pub fn sys_sendfile(_in_fd: usize, _out_fd: usize, _count: usize) -> isize {
    -38
}

/// utimensat(88)：时间戳先不真正实现，但必须补上 "touch" 的文件创建语义。
/// busybox touch 先调 utimensat，我们原来直接返回 0，它就不会创建文件，
/// 后面 lmbench 打开 /var/tmp/lmbench 全是 EPERM。
pub fn sys_utimensat(path: *const u8) -> isize {
    translated_path(current_user_token(), path)
        .and_then(|raw| {
            let path = resolve_at(AT_FDCWD, &raw)?;
            if raw.ends_with('/') {
                fs::require_directory(&path)
            } else if fs::exists(&path) {
                Ok(())
            } else {
                fs::open_dyn(&path, OpenFlags::CREATE | OpenFlags::RDWR).map(|_| ())
            }
        })
        .err()
        .unwrap_or(0)
}

/// statfs(43)/fstatfs(44)：返回最小的 ext4 统计信息
pub fn sys_statfs(buf: *mut u8) -> isize {
    let token = current_user_token();
    let mut b = [0u8; 120];
    // f_type, f_bsize, f_blocks, f_bfree, f_bavail, f_files, f_ffree, f_namelen
    b[0..8].copy_from_slice(&0xEF53u64.to_le_bytes());
    b[8..16].copy_from_slice(&4096u64.to_le_bytes());
    b[16..24].copy_from_slice(&16384u64.to_le_bytes());
    b[24..32].copy_from_slice(&12000u64.to_le_bytes());
    b[32..40].copy_from_slice(&12000u64.to_le_bytes());
    b[40..48].copy_from_slice(&4096u64.to_le_bytes());
    b[48..56].copy_from_slice(&3000u64.to_le_bytes());
    b[56..64].copy_from_slice(&255u64.to_le_bytes());
    for (i, byte) in b.iter().enumerate() {
        *translated_refmut(token, (buf as usize + i) as *mut u8) = *byte;
    }
    0
}

/// lseek(62)：支持 SEEK_SET(0) / SEEK_CUR(1) / SEEK_END(2)
pub fn sys_lseek(fd: usize, offset: isize, whence: usize) -> isize {
    let file = match file_by_fd(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    match whence {
        0 => file.seek_to(offset),
        1 => file.seek(offset),
        2 => (file.size() as isize).checked_add(offset).map_or(-75, |pos| file.seek_to(pos)),
        _ => -22,
    }
}

/// readv(65)：把多个 iovec 拼成一个 UserBuffer 后读出
pub fn sys_readv(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if file.directory().is_some() {
            return -21; // EISDIR
        }
        if !file.readable() {
            return -1;
        }
        let file = file.clone();
        drop(inner);
        drop(process);
        let mut buffers = alloc::vec::Vec::new();
        for i in 0..iovcnt {
            let entry = (iov as usize + i * 16) as *const usize;
            let base = *translated_ref(token, entry);
            let len = *translated_ref(token, (entry as usize + 8) as *const usize);
            if len == 0 {
                continue;
            }
            match translated_byte_buffer(token, base as *const u8, len, MapPermission::W) {
                Some(mut chunk) => buffers.append(&mut chunk),
                None => return -14,
            }
        }
        with_io_file(file, |file| file.read(UserBuffer::new(buffers)) as isize)
    } else {
        -1
    }
}

pub fn sys_ftruncate(fd: usize, len: isize) -> isize {
    if len < 0 {
        return -22;
    }
    match file_by_fd(fd) {
        Ok(file) => file.set_len(len as usize),
        Err(errno) => errno,
    }
}

pub fn sys_truncate(path: *const u8, len: isize) -> isize {
    if len < 0 {
        return -22;
    }
    let result = (|| {
        let raw = translated_path(current_user_token(), path)?;
        let path = resolve_at(AT_FDCWD, &raw)?;
        if raw.ends_with('/') {
            fs::require_directory(&path)?;
        }
        fs::open_dyn(&path, OpenFlags::WRONLY)
    })();
    match result {
        Ok(file) => file.set_len(len as usize),
        Err(errno) => errno,
    }
}
