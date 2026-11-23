use crate::fs::{make_pipe, open_file, OpenFlags};
use crate::mm::{translated_byte_buffer, translated_ref, translated_refmut, translated_str, UserBuffer};
use crate::task::{current_process, current_user_token};
use alloc::sync::Arc;

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
        file.write(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
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
        if !file.readable() {
            return -1;
        }
        // release current task TCB manually to avoid multi-borrow
        drop(inner);
        file.read(UserBuffer::new(translated_byte_buffer(token, buf, len))) as isize
    } else {
        -1
    }
}

pub fn sys_open(path: *const u8, flags: u32) -> isize {
    let process = current_process();
    let token = current_user_token();
    let path = translated_str(token, path);
    if let Some(inode) = crate::fs::open_dyn(path.as_str(), OpenFlags::from_bits(flags).unwrap()) {
        let mut inner = process.inner_exclusive_access();
        let fd = match inner.alloc_fd() {
            Some(fd) => fd,
            None => return -24, // EMFILE
        };
        inner.fd_table[fd] = Some(inode);
        fd as isize
    } else {
        -1
    }
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
    0
}

pub fn sys_pipe(pipe: *mut usize) -> isize {
    let process = current_process();
    let token = current_user_token();
    let mut inner = process.inner_exclusive_access();
    let (pipe_read, pipe_write) = make_pipe();
    let read_fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => return -24, // EMFILE
    };
    inner.fd_table[read_fd] = Some(pipe_read);
    let write_fd = match inner.alloc_fd() {
        Some(fd) => fd,
        None => return -24, // EMFILE
    };
    inner.fd_table[write_fd] = Some(pipe_write);
    // Linux 的 pipe(int fd[2]) 是 4 字节 int，按 4 字节写入
    *translated_refmut(token, pipe as *mut i32) = read_fd as i32;
    *translated_refmut(token, (pipe as usize + 4) as *mut i32) = write_fd as i32;
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
        let mut buffers = alloc::vec::Vec::new();
        for i in 0..iovcnt {
            let entry = (iov as usize + i * 16) as *const usize;
            let base = *translated_ref(token, entry);
            let len = *translated_ref(token, (entry as usize + 8) as *const usize);
            if len == 0 {
                continue;
            }
            buffers.append(&mut translated_byte_buffer(token, base as *const u8, len));
        }
        file.write(UserBuffer::new(buffers)) as isize
    } else {
        -1
    }
}

/// ioctl(2)：只支持 isatty 探测，其余返回 ENOTTY
pub fn sys_ioctl() -> isize {
    -25 // ENOTTY
}

/// openat(2)：只支持 AT_FDCWD；把 Linux 的 open flags 翻译成内核 OpenFlags
pub fn sys_openat(_dirfd: isize, path: *const u8, flags: u32, _mode: u32) -> isize {
    // easy-fs 是扁平命名空间，忽略 dirfd，按绝对名字查找
    let token = current_user_token();
    // easy-fs 是扁平命名空间：去掉 "./" 与前导 "/"
    let path = translated_str(token, path);
    let path = crate::fs::resolve_path(&path);
    let mut of = OpenFlags::RDONLY;
    match flags & 0b11 {
        1 => of = OpenFlags::WRONLY,
        2 => of = OpenFlags::RDWR,
        _ => {}
    }
    if flags & 0o100 != 0 {
        of |= OpenFlags::CREATE;
    }
    if flags & 0o1000 != 0 {
        of |= OpenFlags::TRUNC;
    }
    if let Some(inode) = crate::fs::open_dyn(path.as_str(), of) {
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let fd = match inner.alloc_fd() {
            Some(fd) => fd,
            None => return -24, // EMFILE
        };
        inner.fd_table[fd] = Some(inode);
        if crate::fs::is_dir_path(path.as_str()) {
            inner.dir_paths.insert(fd, path.clone());
        }
        fd as isize
    } else {
        -1
    }
}

/// dup3(24)：把 oldfd 复制到指定的 newfd
pub fn sys_dup3(oldfd: usize, newfd: usize, _flags: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
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
    newfd as isize
}

/// chdir(49)：更新工作目录，支持相对路径和上级目录。
pub fn sys_chdir(path: *const u8) -> isize {
    let token = current_user_token();
    let path = translated_str(token, path);
    let resolved = crate::fs::resolve_path(&path);
    let mut components = alloc::vec::Vec::new();
    for component in resolved.split('/') {
        match component {
            "" | "." => (),
            ".." => {
                components.pop();
            }
            name => components.push(name),
        }
    }
    let process = current_process();
    process.inner_exclusive_access().cwd = components.join("/");
    0
}

/// getcwd(17)：返回 "[/cwd]" 的形式
pub fn sys_getcwd(buf: *mut u8, size: usize) -> isize {
    let token = current_user_token();
    let cwd = current_process().inner_exclusive_access().cwd.clone();
    let mut s = alloc::string::String::from("/");
    if !cwd.is_empty() && cwd != "/" {
        s.push_str(cwd.as_str());
    }
    let n = s.len().min(size.saturating_sub(1));
    for (i, c) in s.as_bytes().iter().take(n).enumerate() {
        *translated_refmut(token, (buf as usize + i) as *mut u8) = *c;
    }
    *translated_refmut(token, (buf as usize + n) as *mut u8) = 0;
    buf as isize
}

/// mkdirat(34)：easy-fs 无目录，用空文件模拟
pub fn sys_mkdirat(_dirfd: isize, path: *const u8) -> isize {
    let token = current_user_token();
    let p = translated_str(token, path);
    let p = alloc::string::String::from(p.trim_start_matches("./").trim_start_matches('/'));
    if crate::fs::has_ext4() {
        // EXT4 只读，用内存 overlay 记录
        crate::fs::overlay_create(p.as_str());
        return 0;
    }
    match open_file(p.as_str(), OpenFlags::CREATE | OpenFlags::RDONLY) {
        Some(_) => 0,
        None => -1,
    }
}

/// unlinkat(35)：记入墓碑集合，之后 open 找不到它
pub fn sys_unlinkat(_dirfd: isize, path: *const u8) -> isize {
    let token = current_user_token();
    let p = translated_str(token, path);
    let p = alloc::string::String::from(p.trim_start_matches("./").trim_start_matches('/'));
    if crate::fs::has_ext4() {
        crate::fs::overlay_remove(p.as_str());
        return 0;
    }
    crate::fs::mark_unlinked(p.as_str());
    0
}

/// fstat(80)：填 riscv64 的 struct stat（128 字节）
pub fn sys_fstat(fd: usize, st: *mut u8) -> isize {
    let token = current_user_token();
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() || inner.fd_table[fd].is_none() {
        return -1;
    }
    let file_size = inner.fd_table[fd].as_ref().unwrap().size();
    drop(inner);
    let mut buf = [0u8; 128];
    // st_ino = fd
    buf[8..16].copy_from_slice(&(fd as u64).to_le_bytes());
    // st_mode = S_IFREG | 0644
    buf[16..20].copy_from_slice(&(0o100644u32).to_le_bytes());
    // st_nlink = 1
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    // st_size
    buf[48..56].copy_from_slice(&(file_size as u64).to_le_bytes());
    // st_blksize = 512
    buf[56..60].copy_from_slice(&512u32.to_le_bytes());
    for (i, b) in buf.iter().enumerate() {
        *translated_refmut(token, (st as usize + i) as *mut u8) = *b;
    }
    0
}

/// mount / umount2：无真实挂载，直接成功
pub fn sys_mount_stub() -> isize {
    0
}

/// getdents64(61)：枚举 easy-fs 根目录
pub fn sys_getdents64(fd: usize, buf: *mut u8, len: usize) -> isize {
    let token = current_user_token();
    let dir = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        inner.dir_paths.get(&fd).cloned().unwrap_or_default()
    };
    // 支持多次读取直到 EOF（busybox 的 readdir 依赖）
    let (names, mut offset) = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let names: alloc::vec::Vec<alloc::string::String> = crate::fs::list_dir_path(dir.as_str())
            .into_iter()
            .map(|(n, _, _)| n)
            .collect();
        let offset = inner.dir_offsets.get(&fd).copied().unwrap_or(0);
        (names, offset)
    };
    if offset >= names.len() {
        return 0;
    }
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    for (i, name) in names.iter().enumerate().skip(offset) {
        // struct linux_dirent64 { u64 d_ino; s64 d_off; u16 d_reclen; u8 d_type; char d_name[]; }
        let reclen = (19 + name.len() + 1 + 7) & !7usize;
        if out.len() + reclen > len {
            break;
        }
        let start = out.len();
        out.resize(start + reclen, 0);
        out[start..start + 8].copy_from_slice(&((i + 1) as u64).to_le_bytes());
        out[start + 16..start + 18].copy_from_slice(&(reclen as u16).to_le_bytes());
        out[start + 18] = 8; // DT_REG
        out[start + 19..start + 19 + name.len()].copy_from_slice(name.as_bytes());
        offset = i + 1;
    }
    {
        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        inner.dir_offsets.insert(fd, offset);
    }
    let n = out.len();
    for (i, b) in out.iter().enumerate() {
        *translated_refmut(token, (buf as usize + i) as *mut u8) = *b;
    }
    n as isize
}

fn write_stat(token: usize, st: *mut u8, size: usize, mode: u32) {
    let mut buf = [0u8; 128];
    buf[8..16].copy_from_slice(&0u64.to_le_bytes()); // ino
    buf[16..20].copy_from_slice(&mode.to_le_bytes());
    buf[20..24].copy_from_slice(&1u32.to_le_bytes());
    buf[48..56].copy_from_slice(&(size as u64).to_le_bytes());
    buf[56..60].copy_from_slice(&512u32.to_le_bytes());
    for (i, b) in buf.iter().enumerate() {
        *translated_refmut(token, (st as usize + i) as *mut u8) = *b;
    }
}

/// newfstatat(79)：按路径取文件信息
pub fn sys_newfstatat(path: *const u8, st: *mut u8) -> isize {
    let token = current_user_token();
    let raw = translated_str(token, path);
    let p = crate::fs::resolve_path(&raw);
    if crate::fs::is_dir_path(p.as_str()) {
        write_stat(token, st, 4096, 0o40755);
        return 0;
    }
    match crate::fs::read_file_all(p.as_str()) {
        Some(data) => {
            write_stat(token, st, data.len(), 0o100644);
            0
        }
        None => -2,
    }
}

/// fcntl(25)：busybox sh 用 F_DUPFD 复制脚本 fd，必须返回真实的 fd
/// renameat(38)：在 overlay 里做重命名
pub fn sys_renameat(oldpath: *const u8, newpath: *const u8) -> isize {
    let token = current_user_token();
    let old = translated_str(token, oldpath);
    let new = translated_str(token, newpath);
    let old = crate::fs::resolve_path(&old);
    let new = crate::fs::resolve_path(&new);
    match crate::fs::overlay_get(old.as_str()) {
        Some(data) => {
            let bytes = data.exclusive_access().clone();
            let dst = crate::fs::overlay_create(new.as_str());
            *dst.exclusive_access() = bytes;
            crate::fs::overlay_remove(old.as_str());
            0
        }
        None => -2,
    }
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
            new_fd as isize
        }
        F_GETFD => 0,
        F_SETFD => 0,
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
    let token = current_user_token();
    let raw = translated_str(token, path);
    let p = crate::fs::resolve_path(&raw);
    if crate::fs::exists(&p) {
        return 0;
    }
    crate::fs::overlay_create(&p);
    0
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
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    let file = match &inner.fd_table[fd] {
        Some(f) => f.clone(),
        None => return -1,
    };
    drop(inner);
    match whence {
        0 => file.seek_to(offset),
        2 => file.seek_to(file.size() as isize + offset),
        _ => file.seek(offset),
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
        if !file.readable() {
            return -1;
        }
        let file = file.clone();
        drop(inner);
        let mut buffers = alloc::vec::Vec::new();
        for i in 0..iovcnt {
            let entry = (iov as usize + i * 16) as *const usize;
            let base = *translated_ref(token, entry);
            let len = *translated_ref(token, (entry as usize + 8) as *const usize);
            if len == 0 {
                continue;
            }
            buffers.append(&mut translated_byte_buffer(token, base as *const u8, len));
        }
        file.read(UserBuffer::new(buffers)) as isize
    } else {
        -1
    }
}

/// ftruncate(46)：调整文件长度
pub fn sys_ftruncate(fd: usize, len: usize) -> isize {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return -1;
    }
    match &inner.fd_table[fd] {
        Some(f) => f.set_len(len),
        None => -1,
    }
}

/// truncate(45)：只对 overlay 文件有意义，其他情况假装成功
pub fn sys_truncate() -> isize {
    0
}
