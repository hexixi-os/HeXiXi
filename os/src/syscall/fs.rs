use crate::fs::{self, make_pipe, OpenFlags};
use crate::mm::{
    translated_byte_buffer, translated_path, MapPermission, UserBuffer,
};
use crate::task::{current_process, current_user_token};
use alloc::{string::String, sync::Arc, vec::Vec};

const AT_FDCWD: isize = -100;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const MAX_RW_COUNT: usize = (i32::MAX as usize) & !(crate::config::PAGE_SIZE - 1);

pub(super) fn with_io_file<T>(file: Arc<dyn fs::File>, operation: impl FnOnce(&dyn fs::File) -> T) -> T {
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
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        if !file.writable() || file.path_only() {
            return -9;
        }
        let file = file.clone();
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        drop(process);
        let len = len.min(MAX_RW_COUNT);
        match translated_byte_buffer(token, buf, len, MapPermission::R) {
            Some(buffers) => with_io_file(file, |file| file.write(UserBuffer::new(buffers))),
            None => -14,
        }
    } else {
        -9
    }
}

pub fn sys_read(fd: usize, buf: *const u8, len: usize) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -9;
    }
    if let Some(file) = &inner.fd_table[fd] {
        let file = file.clone();
        if file.path_only() {
            return -9;
        }
        if file.directory().is_some() {
            return -21; // EISDIR
        }
        if !file.readable() {
            return -9;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        drop(process);
        let len = len.min(MAX_RW_COUNT);
        match translated_byte_buffer(token, buf, len, MapPermission::W) {
            Some(buffers) => with_io_file(file, |file| file.read_result(UserBuffer::new(buffers))),
            None => -14,
        }
    } else {
        -9
    }
}

pub fn sys_close(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
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
        dst.write(byte);
    }
    0
}

pub fn sys_dup(fd: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -9;
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
    let result = (|| {
        let file = file_by_fd(fd)?;
        if !file.writable() || file.path_only() {
            return Err(-9);
        }
        let token = current_user_token();
        let segments = read_iovecs(token, iov as usize, iovcnt)?;
        for &(base, len) in &segments {
            if len != 0 && base.checked_add(len).map_or(true, |end| end > crate::mm::USER_END) {
                return Err(-14);
            }
        }
        let mut buffers = Vec::new();
        for &(base, len) in &segments {
            let (mut chunk, copied) =
                crate::mm::translated_byte_prefix(token, base as *const u8, len, MapPermission::R);
            buffers.append(&mut chunk);
            if copied < len {
                if buffers.is_empty() {
                    return Err(-14);
                }
                break;
            }
        }
        if buffers.is_empty() {
            return Ok(0);
        }
        Ok(with_io_file(file, |file| file.write(UserBuffer::new(buffers))))
    })();
    result.unwrap_or_else(|error| error)
}

// Both guest architectures use the asm-generic kernel termios layout:
// four 32-bit flags, one line discipline byte and 19 control characters.
const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;
const TCSETSW: usize = 0x5403;
const TCSETSF: usize = 0x5404;
const TIOCGPGRP: usize = 0x540f;
const TIOCSPGRP: usize = 0x5410;
const TIOCGWINSZ: usize = 0x5413;
const TIOCSWINSZ: usize = 0x5414;
const TIOCGSID: usize = 0x5429;

struct ConsoleSettings {
    termios: [u8; 36],
    winsize: [u8; 8],
    foreground: Option<Arc<crate::task::PidHandle>>,
}

lazy_static::lazy_static! {
    static ref CONSOLE_SETTINGS: crate::sync::UPSafeCell<ConsoleSettings> = {
        let mut termios = [0; 36];
        // The boot UART delivers raw bytes. B38400 | CS8 | CREAD, VMIN = 1.
        termios[8..12].copy_from_slice(&0o277u32.to_ne_bytes());
        termios[17 + 6] = 1;
        unsafe {
            crate::sync::UPSafeCell::new(ConsoleSettings {
                termios,
                winsize: [24, 0, 80, 0, 0, 0, 0, 0],
                foreground: None,
            })
        }
    };
}

fn ioctl_input<const N: usize>(arg: usize) -> Result<[u8; N], isize> {
    let buffers = translated_byte_buffer(current_user_token(), arg as *const u8, N, MapPermission::R)
        .ok_or(-14isize)?;
    let mut value = [0; N];
    for (dest, byte) in value.iter_mut().zip(UserBuffer::new(buffers)) {
        *dest = byte.read();
    }
    Ok(value)
}

pub fn sys_ioctl(fd: usize, request: usize, arg: usize) -> isize {
    let result = (|| {
        let file = file_by_fd(fd)?;
        if file.path_only() { return Err(-9); }
        if !file.is_tty() { return Err(-25); }

        let output = |bytes: &[u8]| match write_stat_bytes(arg as *mut u8, bytes) {
            0 => Ok(()),
            errno => Err(errno),
        };
        match request {
            TCGETS => {
                let value = CONSOLE_SETTINGS.exclusive_access().termios;
                output(&value)
            }
            TCSETS | TCSETSW | TCSETSF => {
                let value = ioctl_input(arg)?;
                CONSOLE_SETTINGS.exclusive_access().termios = value;
                Ok(())
            }
            TIOCGWINSZ => {
                let value = CONSOLE_SETTINGS.exclusive_access().winsize;
                output(&value)
            }
            TIOCSWINSZ => {
                let value = ioctl_input(arg)?;
                CONSOLE_SETTINGS.exclusive_access().winsize = value;
                Ok(())
            }
            TIOCGPGRP | TIOCSPGRP | TIOCGSID => {
                let sid = current_process().inner_exclusive_access().sid.0;
                let console_sid = crate::task::INITPROC.inner_exclusive_access().sid.0;
                if sid != console_sid { return Err(-25); }
                match request {
                    TIOCGSID => output(&(sid as i32).to_ne_bytes()),
                    TIOCGPGRP => {
                        let group = CONSOLE_SETTINGS.exclusive_access().foreground.as_ref()
                            .map_or(sid, |pgid| pgid.0);
                        output(&(group as i32).to_ne_bytes())
                    }
                    _ => {
                        let group = i32::from_ne_bytes(ioctl_input(arg)?);
                        if group <= 0 { return Err(-22); }
                        let group = crate::task::ProcessControlBlock::group_in_session(group as usize, sid)?;
                        CONSOLE_SETTINGS.exclusive_access().foreground = Some(group);
                        Ok(())
                    }
                }
            }
            _ => Err(-25),
        }
    })();
    result.err().unwrap_or(0)
}

fn resolve_at(dirfd: isize, path: &str) -> Result<String, isize> {
    resolve_at_with(dirfd, path, true)
}

fn resolve_at_with(dirfd: isize, path: &str, follow_final: bool) -> Result<String, isize> {
    if path.is_empty() {
        return Err(-2); // ENOENT
    }
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return fs::resolve_path_with(path, follow_final);
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
    fs::resolve_from(path, &directory.path(), follow_final)
}

fn check_mode_bits(
    uid: u32,
    gid: u32,
    groups: &[u32],
    file_mode: u16,
    file_uid: u32,
    file_gid: u32,
    want: u16,
) -> Result<(), isize> {
    if uid == 0 {
        if want & 1 != 0 && file_mode & 0o170000 == 0o100000 && file_mode & 0o111 == 0 {
            return Err(-13);
        }
        return Ok(());
    }
    let bits = if uid == file_uid {
        (file_mode >> 6) & 7
    } else if gid == file_gid || groups.contains(&file_gid) {
        (file_mode >> 3) & 7
    } else {
        file_mode & 7
    };
    if (bits & want) == want {
        Ok(())
    } else {
        Err(-13)
    }
}

fn current_fs_cred() -> (u32, u32, Vec<u32>) {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if inner.fsuid == 0 {
        (0, inner.fsgid, Vec::new())
    } else {
        (inner.fsuid, inner.fsgid, inner.groups.clone())
    }
}

fn check_path_search(path: &str) -> Result<(), isize> {
    let (fsuid, fsgid, groups) = current_fs_cred();
    if fsuid == 0 {
        return Ok(());
    }
    let (rmode, ruid, rgid) = fs::path_mode_owner("")?;
    check_mode_bits(fsuid, fsgid, &groups, rmode, ruid, rgid, 1)?;
    for (sep, _) in path.match_indices('/') {
        let prefix = &path[..sep];
        if !prefix.is_empty() {
            let (dmode, duid, dgid) = fs::path_mode_owner(prefix)?;
            if dmode & 0o170000 == 0o040000 {
                check_mode_bits(fsuid, fsgid, &groups, dmode, duid, dgid, 1)?;
            }
        }
    }
    Ok(())
}

fn check_parent_write(path: &str) -> Result<(u16, u32), isize> {
    let (fsuid, fsgid, groups) = current_fs_cred();
    if fsuid == 0 {
        return Ok((0o40755, 0));
    }
    check_path_search(path)?;
    let parent = path.rsplit_once('/').map_or("", |(p, _)| p);
    let (pmode, puid, pgid) = fs::path_mode_owner(parent)?;
    if pmode & 0o170000 != 0o040000 {
        return Err(-20);
    }
    check_mode_bits(fsuid, fsgid, &groups, pmode, puid, pgid, 3)?;
    Ok((pmode, puid))
}

fn check_sticky_delete(pmode: u16, puid: u32, target: &str) -> Result<(), isize> {
    let (fsuid, _, _) = current_fs_cred();
    if fsuid == 0 || pmode & 0o1000 == 0 {
        return Ok(());
    }
    if let Ok((_, tuid, _)) = fs::path_mode_owner(target) {
        if fsuid != puid && fsuid != tuid {
            return Err(-1);
        }
    }
    Ok(())
}

/// openat(56)：解析 cwd 或目录 fd，并把 Linux flags 翻译成内核 OpenFlags。
pub fn sys_openat(dirfd: isize, path: *const u8, flags: u32, mode: u32) -> isize {
    const O_NOFOLLOW: u32 = 0o400000;
    const O_DIRECTORY: u32 = 0o200000;
    const O_PATH: u32 = 0o10000000;
    let path_only = flags & O_PATH != 0;
    let flags = if path_only { flags & (O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW | O_PATH) } else { flags };
    let exclusive = flags & (0o100 | 0o200) == (0o100 | 0o200);
    let raw = match translated_path(current_user_token(), path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let follow_final = raw.ends_with('/') || (flags & O_NOFOLLOW == 0 && !exclusive);
    let path = match resolve_at_with(dirfd, &raw, follow_final) {
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
    let result = (|| -> Result<Arc<dyn fs::File + Send + Sync>, isize> {
        check_path_search(&path)?;
        if !follow_final {
            if let Some(link) = fs::find_symlink(&path)? {
                return if exclusive { Err(-17) }
                else if !path_only { Err(-40) }
                else if open_flags.contains(OpenFlags::DIRECTORY) { Err(-20) }
                else { Ok(Arc::new(fs::PathFile(link, flags & !O_CLOEXEC))) };
            }
        }
        let (fsuid, fsgid, groups) = current_fs_cred();
        if fsuid != 0 {
            if open_flags.contains(OpenFlags::TMPFILE) {
                let (dmode, duid, dgid) = fs::path_mode_owner(&path)?;
                if dmode & 0o170000 != 0o040000 {
                    return Err(-20);
                }
                check_mode_bits(fsuid, fsgid, &groups, dmode, duid, dgid, 3)?;
            } else {
                match fs::path_mode_owner(&path) {
                    Ok((fmode, fuid, fgid)) => {
                        if !exclusive && !path_only {
                            let is_dir = fmode & 0o170000 == 0o040000;
                            let mut want = if open_flags.contains(OpenFlags::RDWR) {
                                6
                            } else if open_flags.contains(OpenFlags::WRONLY) {
                                2
                            } else {
                                4
                            };
                            if open_flags.contains(OpenFlags::TRUNC) {
                                want |= 2;
                            }
                            if !(is_dir && want & 2 != 0) {
                                check_mode_bits(fsuid, fsgid, &groups, fmode, fuid, fgid, want)?;
                            }
                        }
                    }
                    Err(-2) if open_flags.contains(OpenFlags::CREATE) && !open_flags.contains(OpenFlags::DIRECTORY) => {
                        check_parent_write(&path)?;
                    }
                    Err(_) => {}
                }
            }
        }
        let permissions = creation_permissions(mode);
        let file = fs::open_dyn_with_mode(&path, open_flags, permissions)?;
        if path_only { Ok(Arc::new(fs::PathFile(file, flags & !O_CLOEXEC))) } else { Ok(file) }
    })();
    let file = match result {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let mut inner = process.inner_exclusive_access();
    inner.fd_table[fd] = Some(file);
    inner.set_close_on_exec(fd, flags & O_CLOEXEC != 0);
    fd as isize
}

pub fn sys_readlinkat(dirfd: isize, path: *const u8, buffer: *mut u8, len: usize) -> isize {
    if len == 0 { return -22; }
    let result = (|| {
        let token = current_user_token();
        let raw = translated_path(token, path)?;
        let file = if raw.is_empty() {
            file_by_fd(dirfd as usize)?
        } else {
            let trailing_slash = raw.ends_with('/');
            let path = resolve_at_with(dirfd, &raw, trailing_slash)?;
            if trailing_slash { fs::require_directory(&path)?; }
            match fs::find_symlink(&path)? {
                Some(link) => link,
                None => {
                    fs::path_is_directory(&path)?;
                    return Err(-22);
                }
            }
        };
        let target = file.link_target().ok_or(-22isize)?;
        let count = len.min(target.len());
        let output = translated_byte_buffer(token, buffer, count, MapPermission::W).ok_or(-14isize)?;
        UserBuffer::new(output).copy_from(&target[..count]);
        Ok(count as isize)
    })();
    result.unwrap_or_else(|errno| errno)
}

pub fn sys_symlinkat(target: *const u8, dirfd: isize, link: *const u8) -> isize {
    let result = (|| {
        let token = current_user_token();
        let target = crate::mm::translated_c_bytes(token, target)?;
        let raw = translated_path(token, link)?;
        let path = resolve_at_with(dirfd, &raw, false)?;
        check_parent_write(&path)?;
        if raw.ends_with('/') {
            return Err(if fs::exists(&path) { -17 } else { -2 });
        }
        fs::symlink(&path, target)
    })();
    result.err().unwrap_or(0)
}

/// dup3(24)：把 oldfd 复制到指定的 newfd
pub fn sys_dup3(oldfd: usize, newfd: usize, flags: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if flags & !O_CLOEXEC != 0 || oldfd == newfd {
        return -22;
    }
    if newfd >= inner.nofile_limit() || oldfd >= inner.fd_table.len() {
        return -9;
    }
    let file = match &inner.fd_table[oldfd] {
        Some(f) => f.clone(),
        None => return -9,
    };
    if let Err(error) = inner.ensure_fd_slot(newfd) {
        return error;
    }
    inner.fd_table[newfd] = Some(file);
    inner.set_close_on_exec(newfd, flags & O_CLOEXEC != 0);
    newfd as isize
}

/// chdir(49)：切换到解析后的目录。
pub fn sys_chdir(path: *const u8) -> isize {
    let directory = match translated_path(current_user_token(), path)
        .and_then(|raw| resolve_at(AT_FDCWD, &raw))
        .and_then(|path| {
            check_path_search(&path)?;
            let dir = fs::working_directory(&path)?;
            let (fsuid, fsgid, groups) = current_fs_cred();
            if fsuid != 0 {
                let (dmode, duid, dgid) = fs::path_mode_owner(&path)?;
                check_mode_bits(fsuid, fsgid, &groups, dmode, duid, dgid, 1)?;
            }
            Ok(dir)
        })
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
        dst.write(byte);
    }
    count as isize
}

fn creation_permissions(mode: u32) -> u16 {
    (mode & 0o7777 & !current_process().inner_exclusive_access().umask) as u16
}

pub fn sys_mkdirat(dirfd: isize, path: *const u8, mode: u32) -> isize {
    translated_path(current_user_token(), path)
        .and_then(|raw| resolve_at_with(dirfd, &raw, false))
        .and_then(|path| {
            check_parent_write(&path)?;
            fs::mkdir(&path, creation_permissions(mode))
        })
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
            let path = resolve_at_with(dirfd, &raw, false)?;
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
            let (pmode, puid) = check_parent_write(&path)?;
            check_sticky_delete(pmode, puid, &path)?;
            fs::unlink(&path, remove_directory)?;
            if !remove_directory {
                crate::net::unix_unlink(&path);
            }
            Ok(())
        })
        .err()
        .unwrap_or(0)
}

pub(super) fn file_by_fd(fd: usize) -> Result<Arc<dyn fs::File + Send + Sync>, isize> {
    current_process().inner_exclusive_access().fd_table.get(fd)
        .and_then(Option::as_ref).cloned().ok_or(-9)
}

pub fn sys_fsync(fd: usize, filesystem: bool) -> isize {
    file_by_fd(fd).and_then(|file| {
        if file.path_only() { return Err(-9); }
        if !filesystem && !file.supports_positioned_io() && file.directory().is_none() {
            return Err(-22);
        }
        if filesystem { file.sync_fs() } else { file.sync_range(0, usize::MAX) }
    }).err().unwrap_or(0)
}

pub fn sys_fstat(fd: usize, st: *mut u8) -> isize {
    match file_by_fd(fd) {
        Ok(file) => write_stat(st, file.as_ref(), fd as u64),
        Err(errno) => errno,
    }
}

pub fn sys_mount(
    source: *const u8,
    target: *const u8,
    fstype: *const u8,
    flags: usize,
) -> isize {
    let token = current_user_token();
    let result = (|| {
        let source = if source.is_null() {
            String::new()
        } else {
            translated_path(token, source)?
        };
        let raw_target = translated_path(token, target)?;
        let target = resolve_at(AT_FDCWD, &raw_target)?;
        let fstype = if fstype.is_null() {
            String::new()
        } else {
            translated_path(token, fstype)?
        };
        fs::mount::mount_fs(&source, &target, &fstype, flags)
    })();
    result.err().unwrap_or(0)
}

pub fn sys_umount2(target: *const u8, flags: usize) -> isize {
    const UMOUNT_NOFOLLOW: usize = 8;
    let token = current_user_token();
    let result = (|| {
        let raw_target = translated_path(token, target)?;
        let follow = flags & UMOUNT_NOFOLLOW == 0 || raw_target.ends_with('/');
        let target = resolve_at_with(AT_FDCWD, &raw_target, follow)?;
        fs::mount::umount_fs(&target, flags)
    })();
    result.err().unwrap_or(0)
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
    if file.path_only() { return -9; }
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
    target.copy_from(&out);
    directory.set_position(position);
    n as isize
}

fn write_stat_bytes(st: *mut u8, bytes: &[u8]) -> isize {
    let target = match translated_byte_buffer(current_user_token(), st, bytes.len(), MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    target.copy_from(bytes);
    0
}

fn write_stat(st: *mut u8, file: &dyn fs::File, fallback_inode: u64) -> isize {
    let (device, inode) = file.stat_identity().unwrap_or((0, fallback_inode));
    let (uid, gid) = file.owner();
    let size = file.size() as u64;
    let blocks = (size + 511) / 512;
    let mut buf = [0u8; 128];
    buf[0..8].copy_from_slice(&device.to_le_bytes());
    buf[8..16].copy_from_slice(&inode.to_le_bytes());
    buf[16..20].copy_from_slice(&u32::from(file.mode()).to_le_bytes());
    buf[20..24].copy_from_slice(&file.link_count().to_le_bytes());
    buf[24..28].copy_from_slice(&uid.to_le_bytes());
    buf[28..32].copy_from_slice(&gid.to_le_bytes());
    if let Some((major, minor)) = file.device_numbers() {
        let rdev = (u64::from(major & 0xfff) << 8) | u64::from(minor & 0xff) |
            (u64::from(major & !0xfff) << 32) | (u64::from(minor & !0xff) << 12);
        buf[32..40].copy_from_slice(&rdev.to_le_bytes());
    }
    buf[48..56].copy_from_slice(&size.to_le_bytes());
    buf[56..60].copy_from_slice(&4096u32.to_le_bytes());
    buf[64..72].copy_from_slice(&blocks.to_le_bytes());
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
        let follow_final = flags & AT_SYMLINK_NOFOLLOW == 0 || raw.ends_with('/');
        let path = resolve_at_with(dirfd, raw, follow_final)?;
        if !follow_final {
            if let Some(link) = fs::find_symlink(&path)? {
                return Ok((link, 0));
            }
        }
        path
    };
    if raw.ends_with('/') {
        fs::require_directory(&path)?;
    }
    fs::open_dyn(&path, OpenFlags::RDONLY).map(|file| (file, 0))
}

pub fn sys_faccessat(dirfd: isize, path: *const u8, mode: usize, flags: usize) -> isize {
    const AT_EACCESS: usize = 0x200;
    if mode & !7 != 0 || flags & !(AT_SYMLINK_NOFOLLOW | AT_EACCESS | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    translated_path(current_user_token(), path)
        .and_then(|raw| {
            if !(raw.is_empty() && flags & AT_EMPTY_PATH != 0) {
                let resolved = resolve_at_with(
                    dirfd,
                    &raw,
                    flags & AT_SYMLINK_NOFOLLOW == 0 || raw.ends_with('/'),
                )?;
                let (check_uid, check_gid, groups) = {
                    let process = current_process();
                    let inner = process.inner_exclusive_access();
                    if flags & AT_EACCESS != 0 {
                        (inner.euid, inner.egid, inner.groups.clone())
                    } else {
                        (inner.uid, inner.gid, inner.groups.clone())
                    }
                };
                if check_uid != 0 {
                    let (rmode, ruid, rgid) = fs::path_mode_owner("")?;
                    check_mode_bits(check_uid, check_gid, &groups, rmode, ruid, rgid, 1)?;
                    for (sep, _) in resolved.match_indices('/') {
                        let prefix = &resolved[..sep];
                        if !prefix.is_empty() {
                            let (dmode, duid, dgid) = fs::path_mode_owner(prefix)?;
                            if dmode & 0o170000 == 0o040000 {
                                check_mode_bits(check_uid, check_gid, &groups, dmode, duid, dgid, 1)?;
                            }
                        }
                    }
                }
            }
            stat_file_at(dirfd, &raw, flags)
        })
        .and_then(|(file, _)| {
            let permissions = file.mode();
            let (fuid, fgid) = file.owner();
            let (check_uid, check_gid, groups) = {
                let process = current_process();
                let inner = process.inner_exclusive_access();
                if flags & AT_EACCESS != 0 {
                    (inner.euid, inner.egid, inner.groups.clone())
                } else {
                    (inner.uid, inner.gid, inner.groups.clone())
                }
            };
            if mode & 2 != 0 && file.space_usage().map_or(false, |(_, readonly)| readonly) {
                return Err(-30);
            }
            if mode != 0 {
                check_mode_bits(check_uid, check_gid, &groups, permissions, fuid, fgid, mode as u16)?;
            }
            Ok(())
        })
        .err().unwrap_or(0)
}

pub fn sys_newfstatat(dirfd: isize, path: *const u8, st: *mut u8, flags: usize) -> isize {
    match translated_path(current_user_token(), path).and_then(|raw| {
        if !(raw.is_empty() && flags & AT_EMPTY_PATH != 0) {
            let resolved = resolve_at_with(
                dirfd,
                &raw,
                flags & AT_SYMLINK_NOFOLLOW == 0 || raw.ends_with('/'),
            )?;
            check_path_search(&resolved)?;
        }
        stat_file_at(dirfd, &raw, flags)
    }) {
        Ok((file, fallback_inode)) => write_stat(st, file.as_ref(), fallback_inode),
        Err(errno) => errno,
    }
}

pub fn sys_statx(dirfd: isize, path: *const u8, flags: usize, mask: u32, st: *mut u8) -> isize {
    const AT_NO_AUTOMOUNT: usize = 0x800;
    const AT_STATX_SYNC_TYPE: usize = 0x6000;
    const STATX_RESERVED: u32 = 0x8000_0000;
    const STATX_AVAILABLE: u32 = 0x61f; // type, mode, nlink, uid, gid, size, blocks
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
    let (uid, gid) = file.owner();
    let size = file.size() as u64;
    let blocks = (size + 511) / 512;
    let available = STATX_AVAILABLE | if identity.is_some() { STATX_INO } else { 0 };
    let major = ((device >> 8) as u32 & 0xfff) | ((device >> 32) as u32 & !0xfff);
    let minor = (device as u32 & 0xff) | ((device >> 12) as u32 & !0xff);
    let mut buf = [0u8; 256]; // Linux uapi struct statx
    buf[0..4].copy_from_slice(&available.to_le_bytes());
    buf[4..8].copy_from_slice(&4096u32.to_le_bytes());
    buf[16..20].copy_from_slice(&file.link_count().to_le_bytes());
    buf[20..24].copy_from_slice(&uid.to_le_bytes());
    buf[24..28].copy_from_slice(&gid.to_le_bytes());
    buf[28..30].copy_from_slice(&file.mode().to_le_bytes());
    buf[32..40].copy_from_slice(&inode.to_le_bytes());
    buf[40..48].copy_from_slice(&size.to_le_bytes());
    buf[48..56].copy_from_slice(&blocks.to_le_bytes());
    if let Some((rdev_major, rdev_minor)) = file.device_numbers() {
        buf[128..132].copy_from_slice(&rdev_major.to_le_bytes());
        buf[132..136].copy_from_slice(&rdev_minor.to_le_bytes());
    }
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
        let old_name = resolve_at_with(oldfd, &old, false)?;
        let new_name = resolve_at_with(newfd, &new, false)?;
        if old.ends_with('/') {
            fs::require_directory(&old_name)?;
        }
        if new.ends_with('/') {
            fs::require_directory(&old_name)?;
        }
        let (opmode, opuid) = check_parent_write(&old_name)?;
        check_sticky_delete(opmode, opuid, &old_name)?;
        let (npmode, npuid) = check_parent_write(&new_name)?;
        if fs::exists(&new_name) {
            check_sticky_delete(npmode, npuid, &new_name)?;
        }
        fs::rename_overlay(&old_name, &new_name, flags == RENAME_NOREPLACE)
    })();
    result.err().unwrap_or(0)
}

pub fn sys_fcntl(fd: usize, cmd: usize, arg: usize) -> isize {
    const F_DUPFD: usize = 0;
    const F_GETFD: usize = 1;
    const F_SETFD: usize = 2;
    const F_GETFL: usize = 3;
    const F_SETFL: usize = 4;
    const F_DUPFD_CLOEXEC: usize = 1030;
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let file = match inner.fd_table.get(fd).and_then(Option::as_ref) {
                Some(file) => file.clone(),
                None => return -9,
            };
            if arg >= inner.nofile_limit() {
                return -22;
            }
            let Some(new_fd) = inner.alloc_fd_from(arg) else { return -24 };
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
        F_GETFL => inner.fd_table.get(fd).and_then(Option::as_ref)
            .map_or(-9, |file| file.status_flags() as isize),
        F_SETFL => inner.fd_table.get(fd).and_then(Option::as_ref).map_or(-9, |file| {
            file.set_status_flags(arg as u32);
            0
        }),
        _ => 0,
    }
}

pub(super) fn poll_deadline(pointer: usize) -> Result<Option<usize>, isize> {
    use crate::mm::try_read_user;
    if pointer == 0 { return Ok(None); }
    let token = current_user_token();
    let seconds = try_read_user(token, pointer as *const i64).ok_or(-14isize)?;
    let nanos_ptr = pointer.checked_add(8).ok_or(-14isize)?;
    let nanos = try_read_user(token, nanos_ptr as *const i64).ok_or(-14isize)?;
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) { return Err(-22); }
    let duration = (seconds as usize).saturating_mul(1_000_000_000).saturating_add(nanos as usize);
    Ok(Some(crate::timer::get_time_ns().saturating_add(duration)))
}

pub(super) fn poll_wait(deadline: Option<usize>, mut scan: impl FnMut() -> Result<usize, isize>) -> Result<usize, isize> {
    loop {
        crate::net::poll();
        let ready = scan()?;
        if ready != 0 || deadline.map_or(false, |end| crate::timer::get_time_ns() >= end) {
            return Ok(ready);
        }
        crate::net::wait_for_io_until(deadline)?;
    }
}

pub(super) fn poll_file(file: &dyn fs::File) -> u16 {
    let ready = file.poll_events();
    // POLLRDNORM and POLLWRNORM are aliases for normal read/write readiness.
    ready | ((ready & (1 | 4)) << 6)
}

pub fn sys_ppoll([fds, nfds, timeout, mask, mask_size, _]: [usize; 6]) -> isize {
    use crate::mm::{try_read_user, try_write_user};
    const POLLERR: u16 = 8;
    const POLLHUP: u16 = 16;
    const POLLNVAL: u16 = 32;
    if nfds > current_process().inner_exclusive_access().nofile_limit() { return -22; }
    if nfds.checked_mul(8).and_then(|len| fds.checked_add(len)).is_none() { return -14; }
    let result = (|| {
        let deadline = poll_deadline(timeout)?;
        let token = current_user_token();
        super::signal::with_signal_mask(mask, mask_size, || {
            poll_wait(deadline, || {
                let mut ready = 0;
                for i in 0..nfds {
                    let base = fds + i * 8;
                    let fd = try_read_user(token, base as *const i32).ok_or(-14isize)?;
                    let events = try_read_user(token, (base + 4) as *const u16).ok_or(-14isize)?;
                    let returned = if fd < 0 {
                        0
                    } else {
                        file_by_fd(fd as usize)
                            .map_or(POLLNVAL, |file| poll_file(file.as_ref()) & (events | POLLERR | POLLHUP))
                    };
                    try_write_user(token, (base + 6) as *mut u16, returned).ok_or(-14isize)?;
                    ready += (returned != 0) as usize;
                }
                Ok(ready)
            })
        })
    })();
    result.map_or_else(|error| error, |ready| ready as isize)
}

/// sendfile(71)：将 in_fd 的数据拷贝到 out_fd，支持可选 offset 指针
pub fn sys_sendfile(out_fd: usize, in_fd: usize, offset_ptr: *mut i64, count: usize) -> isize {
    let result = (|| -> Result<isize, isize> {
        let out_file = file_by_fd(out_fd)?;
        if out_file.path_only() || !out_file.writable() {
            return Err(-9);
        }
        let in_file = file_by_fd(in_fd)?;
        if in_file.path_only() || !in_file.readable() {
            return Err(-9);
        }
        if in_file.directory().is_some() || !in_file.supports_positioned_io() {
            return Err(-22);
        }
        let token = current_user_token();
        let mut explicit_off = if !offset_ptr.is_null() {
            let Some(off) = crate::mm::try_read_user(token, offset_ptr as *const i64) else {
                return Err(-14);
            };
            if crate::mm::prepare_user_write(token, offset_ptr).is_none() {
                return Err(-14);
            }
            if off < 0 {
                return Err(-22);
            }
            Some(off)
        } else {
            None
        };
        let mut remaining = count.min(MAX_RW_COUNT);
        if remaining == 0 {
            return Ok(0);
        }
        if let Some(off) = explicit_off {
            if off.checked_add(remaining as i64).is_none() {
                return Err(-22);
            }
        }
        let frame = Arc::new(crate::mm::frame_alloc().ok_or(-12isize)?);
        crate::task::current_task()
            .unwrap()
            .inner_exclusive_access()
            .user_io_frames
            .push(Arc::clone(&frame));
        let ptr = frame.ppn.bytes_ptr();
        let mut transferred = 0usize;
        while remaining > 0 {
            let chunk = remaining.min(crate::config::PAGE_SIZE);
            let n_read = if let Some(off) = explicit_off {
                if (off as u64) >= in_file.size() as u64 {
                    0
                } else {
                    let rbuf = unsafe { UserBuffer::from_kernel_slice(ptr, chunk) };
                    in_file.read_positioned(off as usize, rbuf)
                }
            } else {
                let rbuf = unsafe { UserBuffer::from_kernel_slice(ptr, chunk) };
                in_file.read_result(rbuf)
            };
            if n_read < 0 {
                return if transferred > 0 { Ok(transferred as isize) } else { Err(n_read) };
            }
            if n_read == 0 {
                break;
            }
            let n_read = n_read as usize;
            let wbuf = unsafe { UserBuffer::from_kernel_slice(ptr, n_read) };
            let out_arc: Arc<dyn fs::File> = out_file.clone();
            let n_written = with_io_file(out_arc, |f| f.write(wbuf));
            if n_written < 0 {
                if explicit_off.is_none() {
                    let _ = in_file.seek(-(n_read as isize));
                }
                return if transferred > 0 { Ok(transferred as isize) } else { Err(n_written) };
            }
            let n_written = n_written as usize;
            if let Some(ref mut off) = explicit_off {
                *off = off.saturating_add(n_written as i64);
            } else if n_written < n_read {
                let _ = in_file.seek(-((n_read - n_written) as isize));
            }
            transferred += n_written;
            remaining -= n_written;
            if n_written < chunk {
                break;
            }
        }
        if let Some(off) = explicit_off {
            if crate::mm::try_write_user(token, offset_ptr, off).is_none() {
                return Err(-14);
            }
        }
        Ok(transferred as isize)
    })();
    result.unwrap_or_else(|e| e)
}

/// utimensat(88)：校验 fd/路径/timespec，并保留 "touch" 的文件创建语义。
pub fn sys_utimensat(dirfd: isize, path: *const u8, times: *const u8, flags: usize) -> isize {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return -22;
    }
    let token = current_user_token();
    if !times.is_null() {
        for idx in 0..2usize {
            let Some(base) = (times as usize).checked_add(idx * 16) else {
                return -14;
            };
            let Some(_sec) = crate::mm::try_read_user(token, base as *const i64) else {
                return -14;
            };
            let Some(nsec) = crate::mm::try_read_user(token, (base + 8) as *const i64) else {
                return -14;
            };
            if nsec != UTIME_NOW && nsec != UTIME_OMIT && !(0..1_000_000_000).contains(&nsec) {
                return -22;
            }
        }
    }
    if path.is_null() {
        if flags & AT_SYMLINK_NOFOLLOW != 0 {
            return -22;
        }
        return match file_by_fd(dirfd as usize) {
            Ok(file) if file.path_only() => -9,
            Ok(file) if file.space_usage().map_or(false, |(_, ro)| ro) => -30,
            Ok(_) => 0,
            Err(e) => e,
        };
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    translated_path(token, path)
        .and_then(|raw| {
            let resolved = resolve_at_with(dirfd, &raw, follow || raw.ends_with('/'))?;
            check_path_search(&resolved)?;
            if raw.ends_with('/') {
                fs::require_directory(&resolved)
            } else if fs::exists(&resolved) {
                if fs::space_usage(&resolved).map_or(false, |(_, ro)| ro) {
                    return Err(-30);
                }
                Ok(())
            } else {
                Err(-2)
            }
        })
        .err()
        .unwrap_or(0)
}

fn write_statfs(buf: *mut u8, magic: u64, usage: Option<(easy_fs::SpaceUsage, bool)>) -> isize {
    let (block_size, used, name_max, readonly) = match usage {
        Some((usage, readonly)) => (easy_fs::BLOCK_SZ as u64, usage, easy_fs::NAME_LENGTH_LIMIT as u64, readonly),
        None => (4096, easy_fs::SpaceUsage { blocks: 16384, free_blocks: 12000, inodes: 4096, free_inodes: 3000 }, 255, false),
    };
    let mut b = [0u8; 120];
    let values = [magic, block_size, used.blocks, used.free_blocks, used.free_blocks,
        used.inodes, used.free_inodes, 0, name_max, block_size, u64::from(readonly)];
    for (slot, value) in b.chunks_exact_mut(8).zip(values) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
    write_stat_bytes(buf, &b)
}

pub fn sys_statfs(path: *const u8, buf: *mut u8) -> isize {
    let result = (|| {
        let raw = translated_path(current_user_token(), path)?;
        let resolved = resolve_at(AT_FDCWD, &raw)?;
        if raw.ends_with('/') {
            fs::require_directory(&resolved)?;
        } else if !fs::exists(&resolved) {
            return Err(-2);
        }
        Ok((fs::statfs_magic(&resolved), fs::space_usage(&resolved)))
    })();
    match result {
        Ok((magic, usage)) => write_statfs(buf, magic, usage),
        Err(errno) => errno,
    }
}

pub fn sys_fstatfs(fd: usize, buf: *mut u8) -> isize {
    let file = match file_by_fd(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    let magic = if let Some(directory) = file.directory() {
        fs::statfs_magic(&directory.location().path())
    } else if let Some((dev, _)) = file.stat_identity() {
        fs::statfs_magic_for_dev(dev)
    } else {
        fs::mount::EXT4_MAGIC
    };
    write_statfs(buf, magic, file.space_usage())
}

/// lseek(62)：支持 SEEK_SET(0) / SEEK_CUR(1) / SEEK_END(2)
pub fn sys_lseek(fd: usize, offset: isize, whence: usize) -> isize {
    let file = match file_by_fd(fd) {
        Ok(file) => file,
        Err(errno) => return errno,
    };
    if file.path_only() { return -9; }
    match whence {
        0 => file.seek_to(offset),
        1 => file.seek(offset),
        2 => (file.size() as isize).checked_add(offset).map_or(-75, |pos| file.seek_to(pos)),
        _ => -22,
    }
}

fn read_iovecs(token: usize, iov: usize, count: usize) -> Result<alloc::vec::Vec<(usize, usize)>, isize> {
    const MAX_IOVECS: usize = 1024;
    const WORD: usize = core::mem::size_of::<usize>();
    if (count as isize) < 0 || count > MAX_IOVECS {
        return Err(-22);
    }
    let mut vectors = alloc::vec::Vec::with_capacity(count);
    let mut remaining = MAX_RW_COUNT;
    for index in 0..count {
        let entry = iov.checked_add(index * 2 * WORD).ok_or(-14isize)?;
        let fields = translated_byte_buffer(token, entry as *const u8, 2 * WORD, MapPermission::R)
            .ok_or(-14isize)?;
        let mut words = [[0; WORD]; 2];
        for (dst, src) in words.iter_mut().flatten().zip(UserBuffer::new(fields)) {
            *dst = src.read();
        }
        let [base, len] = words.map(usize::from_ne_bytes);
        if len > isize::MAX as usize {
            return Err(-22);
        }
        let len = len.min(remaining);
        vectors.push((base, len));
        remaining -= len;
    }
    Ok(vectors)
}

pub(super) fn sys_positioned_io(args: [usize; 6], write: bool, vectored: bool, v2: bool) -> isize {
    let [fd, address, count, position, _, flags] = args;
    // Both native 64-bit ABIs carry the full off_t in the low argument.
    let position = position as isize;
    let use_cursor = v2 && position == -1;
    if position < 0 && !use_cursor {
        return -22;
    }
    let result = (|| {
        let file = file_by_fd(fd)?;
        if file.path_only() {
            return Err(-9);
        }
        if file.directory().is_some() {
            return Err(if write { -9 } else { -21 });
        }
        if !use_cursor && !file.supports_positioned_io() {
            return Err(-29);
        }
        if (write && !file.writable()) || (!write && !file.readable()) {
            return Err(-9);
        }
        if v2 && flags as u32 != 0 {
            return Err(-95);
        }
        let token = current_user_token();
        let access = if write { MapPermission::R } else { MapPermission::W };
        let single = [(address, count.min(MAX_RW_COUNT))];
        let vectors;
        let segments: &[(usize, usize)] = if vectored {
            vectors = read_iovecs(token, address, count)?;
            &vectors
        } else {
            &single
        };
        let length = if vectored { segments.iter().map(|&(_, len)| len).sum() } else { count };
        if !use_cursor && (length > isize::MAX as usize || position.checked_add(length as isize).is_none()) {
            return Err(-22);
        }
        for &(base, len) in segments {
            if len != 0 && base.checked_add(len).map_or(true, |end| end > crate::mm::USER_END) {
                return Err(-14);
            }
        }
        let mut remaining = if write || use_cursor {
            MAX_RW_COUNT
        } else {
            file.size().saturating_sub(position as usize)
        };
        let mut buffers = alloc::vec::Vec::new();
        for &(base, len) in segments {
            let len = len.min(remaining);
            let (mut chunk, copied) = crate::mm::translated_byte_prefix(token, base as *const u8, len, access);
            buffers.append(&mut chunk);
            if copied < len {
                if buffers.is_empty() {
                    return Err(-14);
                }
                break;
            }
            remaining -= copied;
        }
        if buffers.is_empty() {
            return Ok(0);
        }
        let buffer = UserBuffer::new(buffers);
        Ok(with_io_file(file, |file| match (write, use_cursor) {
            (true, true) => file.write(buffer),
            (false, true) => file.read_result(buffer),
            (true, false) => file.write_positioned(position as usize, buffer),
            (false, false) => file.read_positioned(position as usize, buffer),
        }))
    })();
    result.unwrap_or_else(|error| error)
}

/// readv(65)：把多个 iovec 拼成一个 UserBuffer 后读出
pub fn sys_readv(fd: usize, iov: *const u8, iovcnt: usize) -> isize {
    let result = (|| {
        let file = file_by_fd(fd)?;
        if file.path_only() {
            return Err(-9);
        }
        if file.directory().is_some() {
            return Err(-21); // EISDIR
        }
        if !file.readable() {
            return Err(-9);
        }
        let token = current_user_token();
        let segments = read_iovecs(token, iov as usize, iovcnt)?;
        for &(base, len) in &segments {
            if len != 0 && base.checked_add(len).map_or(true, |end| end > crate::mm::USER_END) {
                return Err(-14);
            }
        }
        let mut buffers = Vec::new();
        for &(base, len) in &segments {
            let (mut chunk, copied) =
                crate::mm::translated_byte_prefix(token, base as *const u8, len, MapPermission::W);
            buffers.append(&mut chunk);
            if copied < len {
                if buffers.is_empty() {
                    return Err(-14);
                }
                break;
            }
        }
        if buffers.is_empty() {
            return Ok(0);
        }
        Ok(with_io_file(file, |file| {
            file.read_result(UserBuffer::new(buffers))
        }))
    })();
    result.unwrap_or_else(|error| error)
}

pub fn sys_ftruncate(fd: usize, len: isize) -> isize {
    if len < 0 {
        return -22;
    }
    match file_by_fd(fd) {
        Ok(file) if file.path_only() => -9,
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
        check_path_search(&path)?;
        if raw.ends_with('/') {
            fs::require_directory(&path)?;
        }
        let (fsuid, fsgid, groups) = current_fs_cred();
        if fsuid != 0 {
            let (fmode, fuid, fgid) = fs::path_mode_owner(&path)?;
            if fmode & 0o170000 != 0o040000 {
                check_mode_bits(fsuid, fsgid, &groups, fmode, fuid, fgid, 2)?;
            }
        }
        fs::open_dyn(&path, OpenFlags::WRONLY)
    })();
    match result {
        Ok(file) => file.set_len(len as usize),
        Err(errno) => errno,
    }
}

pub fn sys_fallocate(fd: usize, mode: u32, offset: isize, len: isize) -> isize {
    const FALLOC_FL_KEEP_SIZE: u32 = 0x01;
    const FALLOC_FL_PUNCH_HOLE: u32 = 0x02;
    let file = match file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if file.path_only() || !file.writable() {
        return -9; // EBADF
    }
    if !file.supports_positioned_io() {
        return if file.device_numbers().is_some() { -19 } else { -29 };
    }
    if offset < 0 || len <= 0 {
        return -22; // EINVAL
    }
    if mode & FALLOC_FL_PUNCH_HOLE != 0 && mode & FALLOC_FL_KEEP_SIZE == 0 {
        return -22; // EINVAL
    }
    if mode & !(FALLOC_FL_KEEP_SIZE | FALLOC_FL_PUNCH_HOLE) != 0 {
        return -95; // EOPNOTSUPP
    }
    let Some(end) = offset.checked_add(len) else {
        return -27; // EFBIG
    };
    if file.space_usage().map_or(false, |(_, readonly)| readonly) {
        return -30; // EROFS
    }
    if mode & FALLOC_FL_PUNCH_HOLE != 0 {
        return file.punch_hole(offset as usize, len as usize);
    }
    if mode & FALLOC_FL_KEEP_SIZE == 0 && (end as usize) > file.size() {
        let rc = file.set_len(end as usize);
        if rc < 0 {
            return rc;
        }
    }
    0
}

fn sanitize_chmod(caller_euid: u32, caller_egid: u32, groups: &[u32], file_uid: u32, file_gid: u32, mode: u32) -> Result<u16, isize> {
    if caller_euid != 0 && caller_euid != file_uid {
        return Err(-1); // EPERM
    }
    let mut perm = (mode & 0o7777) as u16;
    if caller_euid != 0 && caller_egid != file_gid && !groups.contains(&file_gid) {
        perm &= !0o2000; // Drop S_ISGID if caller is not in file's group.
    }
    Ok(perm)
}

fn validate_chown(
    caller_euid: u32,
    caller_egid: u32,
    groups: &[u32],
    file_uid: u32,
    file_gid: u32,
    uid: u32,
    gid: u32,
) -> Result<(Option<u32>, Option<u32>), isize> {
    let new_uid = (uid != u32::MAX).then_some(uid);
    let new_gid = (gid != u32::MAX).then_some(gid);
    if caller_euid != 0 {
        if let Some(u) = new_uid {
            if caller_euid != file_uid || u != file_uid {
                return Err(-1);
            }
        }
        if let Some(g) = new_gid {
            if caller_euid != file_uid || (g != file_gid && g != caller_egid && !groups.contains(&g)) {
                return Err(-1);
            }
        }
    }
    Ok((new_uid, new_gid))
}

pub fn sys_fchmod(fd: usize, mode: u32) -> isize {
    let file = match file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if file.path_only() {
        return -9;
    }
    let (euid, egid, groups) = {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        (inner.euid, inner.egid, inner.groups.clone())
    };
    let (fuid, fgid) = file.owner();
    let perm = match sanitize_chmod(euid, egid, &groups, fuid, fgid, mode) {
        Ok(p) => p,
        Err(e) => return e,
    };
    file.set_mode(perm)
}

pub fn sys_fchmodat(dirfd: isize, path: *const u8, mode: u32, flags: usize) -> isize {
    if flags & !AT_SYMLINK_NOFOLLOW != 0 {
        return -22;
    }
    if flags & AT_SYMLINK_NOFOLLOW != 0 {
        return -95; // Linux returns EOPNOTSUPP for AT_SYMLINK_NOFOLLOW in fchmodat.
    }
    let result = (|| {
        let raw = translated_path(current_user_token(), path)?;
        let resolved = resolve_at_with(dirfd, &raw, true)?;
        check_path_search(&resolved)?;
        if raw.ends_with('/') {
            fs::require_directory(&resolved)?;
        }
        let (euid, egid, groups) = {
            let p = current_process();
            let inner = p.inner_exclusive_access();
            (inner.euid, inner.egid, inner.groups.clone())
        };
        fs::mutate_path_meta(&resolved, |_fmode, fuid, fgid| {
            let perm = sanitize_chmod(euid, egid, &groups, fuid, fgid, mode)?;
            Ok((Some(perm), None, None))
        })
    })();
    result.err().unwrap_or(0)
}

pub fn sys_fchown(fd: usize, uid: u32, gid: u32) -> isize {
    let file = match file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if file.path_only() {
        return -9;
    }
    let (euid, egid, groups) = {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        (inner.euid, inner.egid, inner.groups.clone())
    };
    let (fuid, fgid) = file.owner();
    let (nu, ng) = match validate_chown(euid, egid, &groups, fuid, fgid, uid, gid) {
        Ok(pair) => pair,
        Err(e) => return e,
    };
    file.set_owner(nu, ng)
}

pub fn sys_fchownat(dirfd: isize, path: *const u8, uid: u32, gid: u32, flags: usize) -> isize {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let result = (|| {
        let raw = translated_path(current_user_token(), path)?;
        if raw.is_empty() && flags & AT_EMPTY_PATH != 0 {
            let rc = sys_fchown(dirfd as usize, uid, gid);
            return if rc < 0 { Err(rc) } else { Ok(()) };
        }
        let follow = flags & AT_SYMLINK_NOFOLLOW == 0 || raw.ends_with('/');
        let resolved = resolve_at_with(dirfd, &raw, follow)?;
        check_path_search(&resolved)?;
        if raw.ends_with('/') {
            fs::require_directory(&resolved)?;
        }
        let (euid, egid, groups) = {
            let p = current_process();
            let inner = p.inner_exclusive_access();
            (inner.euid, inner.egid, inner.groups.clone())
        };
        fs::mutate_path_meta(&resolved, |_fmode, fuid, fgid| {
            let (nu, ng) = validate_chown(euid, egid, &groups, fuid, fgid, uid, gid)?;
            Ok((None, nu, ng))
        })
    })();
    result.err().unwrap_or(0)
}
