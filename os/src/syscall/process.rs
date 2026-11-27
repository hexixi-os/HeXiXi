use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::mm::{MapPermission, VirtPageNum, VmaBacking, USER_END};
use core::ops::Range;
use crate::fs::{open_executable, Executable};
use crate::mm::{
    read_user, translated_byte_buffer, translated_c_bytes, translated_path, try_read_user,
    try_write_user, write_user, UserBuffer,
};
use crate::task::{
    current_process, current_task, current_user_token, exit_current_and_run_next, pid2process,
    process_ids, remove_from_pid2process, suspend_current_and_run_next, ProcessControlBlock,
};
use alloc::sync::Arc;
use alloc::vec::Vec;

pub fn sys_exit(exit_code: i32) -> ! {
    exit_current_and_run_next(exit_code);
    panic!("Unreachable in sys_exit!");
}

pub fn sys_yield() -> isize {
    suspend_current_and_run_next();
    0
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

pub(super) fn fork_with_child_tid(
    stack: usize,
    set_child_tid: Option<usize>,
    clear_child_tid: Option<usize>,
) -> isize {
    current_process().fork(stack, set_child_tid, clear_child_tid)
        .map(|pid| pid as isize).unwrap_or_else(|errno| errno)
}

fn open_executable_path(path: &str) -> Result<(Executable, alloc::string::String), isize> {
    let path = crate::fs::resolve_path(path)?;
    open_executable(&path).map(|source| (source, path))
}

fn open_busybox() -> Result<(Executable, alloc::string::String), isize> {
    // 评测脚本可能已进入 basic 等子目录，继续查找根目录和官方盘的 musl 目录。
    open_executable_path("busybox")
        .or_else(|_| open_executable_path("/busybox"))
        .or_else(|_| open_executable_path("/musl/busybox"))
}

pub fn sys_exec(path: *const u8, args: *const usize, envp: *const usize) -> isize {
    let token = current_user_token();
    let raw_path = match translated_path(token, path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let args = match exec_arguments(token, args) {
        Ok(args) => args,
        Err(errno) => return errno,
    };
    let envs = match exec_arguments(token, envp) {
        Ok(envs) => envs,
        Err(errno) => return errno,
    };
    let (source, path) = match open_executable_path(&raw_path).or_else(|errno| {
        if errno == -2
            && crate::config::legacy_oscomp_image()
            && (raw_path.rsplit('/').next() == Some("busybox") || raw_path == "/bin/sh")
        {
            open_busybox()
        } else {
            Err(errno)
        }
    }) {
        Ok(source) => source,
        Err(errno) => return errno,
    };
    let mut prefix = [0; 256];
    let prefix_len = source.read_at(0, &mut prefix);
    let prefix = &prefix[..prefix_len];
    let (source, args, path) = if prefix.starts_with(b"#!") {
        match script_interpreter(prefix, prefix_len < source.len(), &raw_path, args) {
            Ok(interpreter) => interpreter,
            Err(errno) => return errno,
        }
    } else {
        (source, args, path)
    };
    match current_process().exec(source, &path, &raw_path, args, envs) {
        Ok(()) => 0,
        Err(errno) => errno,
    }
}

fn exec_arguments(token: usize, args: *const usize) -> Result<Vec<Vec<u8>>, isize> {
    const WORD: usize = core::mem::size_of::<usize>();
    let mut result = Vec::new();
    if args.is_null() {
        return Ok(result);
    }
    let mut address = args as usize;
    let mut remaining = USER_STACK_SIZE;
    loop {
        let buffers = translated_byte_buffer(token, address as *const u8, WORD, MapPermission::R)
            .ok_or(-14isize)?;
        let mut bytes = [0; WORD];
        for (byte, user) in bytes.iter_mut().zip(UserBuffer::new(buffers)) {
            *byte = user.read();
        }
        let pointer = usize::from_ne_bytes(bytes);
        if pointer == 0 {
            return Ok(result);
        }
        let arg = translated_c_bytes(token, pointer as *const u8)
            .map_err(|errno| if errno == -36 { -7 } else { errno })?;
        remaining = remaining.checked_sub(arg.len() + 1 + WORD).ok_or(-7isize)?;
        result.push(arg);
        address = address.checked_add(WORD).ok_or(-14isize)?;
    }
}

fn script_interpreter(
    prefix: &[u8],
    more_data: bool,
    path: &str,
    args: Vec<Vec<u8>>,
) -> Result<(Executable, Vec<Vec<u8>>, alloc::string::String), isize> {
    let newline = prefix.iter().position(|&byte| byte == b'\n');
    let end = newline.unwrap_or(prefix.len());
    let line = core::str::from_utf8(&prefix[2..end]).map_err(|_| -8isize)?.trim_start();
    let mut tokens = line.split_whitespace();
    let interpreter = tokens.next().ok_or(-8isize)?;
    if more_data && newline.is_none() && interpreter.len() == line.len() {
        return Err(-8); // The window ended inside the interpreter name.
    }
    let argument = tokens.next();
    let (program, argument, (source, interpreter_path)) = match open_executable_path(interpreter) {
        Ok(resolved) => (interpreter, argument, resolved),
        Err(-2) if crate::config::legacy_oscomp_image() => {
            ("busybox", argument.or(Some("sh")), open_busybox()?)
        }
        Err(errno) => return Err(errno),
    };
    let mut new_args = Vec::with_capacity(args.len() + 3);
    new_args.push(program.as_bytes().to_vec());
    if let Some(argument) = argument {
        new_args.push(argument.as_bytes().to_vec());
    }
    new_args.push(path.as_bytes().to_vec());
    new_args.extend(args.into_iter().skip(1));
    Ok((source, new_args, interpreter_path))
}

pub fn sys_execveat(
    dirfd: isize,
    path: *const u8,
    args: *const usize,
    envp: *const usize,
    flags: usize,
) -> isize {
    const AT_SYMLINK_NOFOLLOW: usize = 0x100;
    const AT_EMPTY_PATH: usize = 0x1000;
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return -22;
    }
    let token = current_user_token();
    let raw_path = match translated_path(token, path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let args = match exec_arguments(token, args) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let envs = match exec_arguments(token, envp) {
        Ok(e) => e,
        Err(e) => return e,
    };
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let resolved = if raw_path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return -13;
    } else if raw_path.starts_with('/') || dirfd == -100 {
        match crate::fs::resolve_path_with(&raw_path, follow) {
            Ok(p) => p,
            Err(e) => return e,
        }
    } else {
        let proc = current_process();
        let inner = proc.inner_exclusive_access();
        let root = inner.root.clone();
        let Some(file) = inner.fd_table.get(dirfd as usize).and_then(Option::as_ref).cloned() else {
            return -9;
        };
        drop(inner);
        let Some(dir) = file.directory() else {
            return -20;
        };
        if !crate::fs::directory_is_live(dir.location()) {
            return -2;
        }
        match crate::fs::resolve_from_in_root(&raw_path, &dir.location().path(), &root, follow) {
            Ok(p) => p,
            Err(e) => return e,
        }
    };
    if !follow {
        if let Ok(Some(_)) = crate::fs::find_symlink(&resolved) {
            return -40; // ELOOP when AT_SYMLINK_NOFOLLOW targets a symlink
        }
    }
    let source = match open_executable(&resolved) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let mut prefix = [0; 256];
    let prefix_len = source.read_at(0, &mut prefix);
    let prefix = &prefix[..prefix_len];
    let (source, args, final_path) = if prefix.starts_with(b"#!") {
        match script_interpreter(prefix, prefix_len < source.len(), &raw_path, args) {
            Ok(interp) => interp,
            Err(e) => return e,
        }
    } else {
        (source, args, resolved)
    };
    match current_process().exec(source, &final_path, &raw_path, args, envs) {
        Ok(()) => 0,
        Err(e) => e,
    }
}

pub fn sys_waitid(
    idtype: usize,
    id: usize,
    infop: usize,
    options: usize,
    rusage: usize,
) -> isize {
    const P_ALL: usize = 0;
    const P_PID: usize = 1;
    const P_PGID: usize = 2;
    const WNOHANG: usize = 1;
    const WSTOPPED: usize = 2;
    const WEXITED: usize = 4;
    const WCONTINUED: usize = 8;
    const WNOWAIT: usize = 0x0100_0000;
    const WNOTHREAD: usize = 0x2000_0000;
    const WALL: usize = 0x4000_0000;
    const WCLONE: usize = 0x8000_0000;

    if !matches!(idtype, P_ALL | P_PID | P_PGID) {
        return -22;
    }
    if options & (WEXITED | WSTOPPED | WCONTINUED) == 0
        || options & !(WNOHANG | WSTOPPED | WEXITED | WCONTINUED | WNOWAIT | WNOTHREAD | WALL | WCLONE) != 0
    {
        return -22;
    }
    if matches!(idtype, P_PID | P_PGID) && (id as isize) <= 0 && !(idtype == P_PGID && id == 0) {
        return -22;
    }
    loop {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let caller_pgid = inner.pgid.0;
        let mut has_child = false;
        let event = inner.children.iter().enumerate().find_map(|(idx, child)| {
            let state = child.inner_exclusive_access();
            match idtype {
                P_ALL => {}
                P_PID => {
                    if child.getpid() != id {
                        return None;
                    }
                }
                P_PGID => {
                    let want = if id == 0 { caller_pgid } else { id };
                    if state.pgid.0 != want {
                        return None;
                    }
                }
                _ => return None,
            }
            has_child = true;
            if state.is_zombie {
                if options & WEXITED == 0 {
                    return None;
                }
                let ws = state.wait_status;
                let (code, sig_or_exit) = if ws & 0x7f == 0 {
                    (1i32, (ws >> 8) & 0xff) // CLD_EXITED
                } else {
                    (2i32, ws & 0x7f) // CLD_KILLED
                };
                return Some((idx, child.getpid(), state.uid, code, sig_or_exit, true));
            }
            let ws = state.job_wait_status?;
            if ws == 0xffff {
                (options & WCONTINUED != 0).then_some((idx, child.getpid(), state.uid, 6i32, 18i32, false))
            } else {
                (options & WSTOPPED != 0).then_some((
                    idx,
                    child.getpid(),
                    state.uid,
                    5i32,
                    (ws >> 8) & 0xff,
                    false,
                ))
            }
        });
        let waiters = inner.child_waiters.clone();
        let token = inner.get_user_token();
        drop(inner);
        if let Some((idx, found_pid, cuid, code, status, exited)) = event {
            if infop != 0 {
                let mut si = [0u8; 128];
                si[0..4].copy_from_slice(&17i32.to_ne_bytes()); // SIGCHLD
                si[8..12].copy_from_slice(&code.to_ne_bytes());
                si[16..20].copy_from_slice(&(found_pid as i32).to_ne_bytes());
                si[20..24].copy_from_slice(&cuid.to_ne_bytes());
                si[24..28].copy_from_slice(&status.to_ne_bytes());
                let Some(b) = translated_byte_buffer(token, infop as *const u8, 128, MapPermission::W) else {
                    return -14;
                };
                UserBuffer::new(b).copy_from(&si);
            }
            if rusage != 0 {
                let Some(b) = translated_byte_buffer(token, rusage as *const u8, 144, MapPermission::W) else {
                    return -14;
                };
                UserBuffer::new(b).fill_zero();
            }
            if options & WNOWAIT == 0 {
                let mut inner = process.inner_exclusive_access();
                if exited {
                    remove_from_pid2process(found_pid);
                    inner.children.remove(idx);
                } else if let Some(ch) = inner.children.get(idx) {
                    ch.inner_exclusive_access().job_wait_status = None;
                }
            }
            return 0;
        }
        drop(process);
        if !has_child {
            return -10; // ECHILD
        }
        if options & WNOHANG != 0 {
            if infop != 0 {
                let Some(b) = translated_byte_buffer(token, infop as *const u8, 128, MapPermission::W) else {
                    return -14;
                };
                UserBuffer::new(b).fill_zero();
            }
            if rusage != 0 {
                let Some(b) = translated_byte_buffer(token, rusage as *const u8, 144, MapPermission::W) else {
                    return -14;
                };
                UserBuffer::new(b).fill_zero();
            }
            return 0;
        }
        crate::task::check_kill_current();
        if !waiters.register_current_interruptible() {
            return -4;
        }
        crate::task::block_current_and_run_next();
        if let Some(current) = crate::task::current_task() {
            crate::task::WaitQueue::cancel(&current);
        }
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4;
        }
    }
}

pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32, options: usize) -> isize {
    const WNOHANG: usize = 1;
    const WUNTRACED: usize = 2;
    const WCONTINUED: usize = 8;
    const WALL: usize = 0x4000_0000;
    if options & !(WNOHANG | WUNTRACED | WCONTINUED | WALL) != 0 {
        return -22;
    }
    let pid = pid as i32;
    if pid == i32::MIN {
        return -3;
    }
    let group = match pid {
        0 => Some(current_process().inner_exclusive_access().pgid.0),
        p if p < -1 => Some(p.unsigned_abs() as usize),
        _ => None,
    };
    loop {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let mut has_child = false;
        let event = inner.children.iter().enumerate().find_map(|(idx, child)| {
            let state = child.inner_exclusive_access();
            if (pid > 0 && child.getpid() != pid as usize)
                || group.map_or(false, |group| state.pgid.0 != group) {
                return None;
            }
            has_child = true;
            if state.is_zombie {
                return Some((idx, child.getpid(), state.wait_status, true));
            }
            let status = state.job_wait_status?;
            let flag = if status == 0xffff { WCONTINUED } else { WUNTRACED };
            (options & flag != 0).then_some((idx, child.getpid(), status, false))
        });
        let waiters = inner.child_waiters.clone();
        let token = inner.get_user_token();
        drop(inner); // Copyout may fault in a page through the calling PCB.
        if let Some((idx, found_pid, status, exited)) = event {
            if !exit_code_ptr.is_null() && try_write_user(token, exit_code_ptr, status).is_none() {
                return -14;
            }
            let mut inner = process.inner_exclusive_access();
            if exited {
                remove_from_pid2process(found_pid);
                inner.children.remove(idx);
            } else {
                inner.children[idx].inner_exclusive_access().job_wait_status = None;
            }
            return found_pid as isize;
        }
        drop(process);
        if !has_child {
            return -10; // ECHILD
        }
        if options & WNOHANG != 0 {
            return 0;
        }
        crate::task::check_kill_current();
        if !waiters.register_current_interruptible() {
            return -4; // EINTR
        }
        crate::task::block_current_and_run_next();
        if let Some(current) = crate::task::current_task() {
            crate::task::WaitQueue::cancel(&current);
        }
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4; // EINTR
        }
    }
}

pub fn sys_getppid() -> isize {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    match &inner.parent {
        Some(p) => p.upgrade().map(|p| p.getpid() as isize).unwrap_or(0),
        None => 0,
    }
}

fn process_by_pid_or_self(pid: i32) -> Option<Arc<ProcessControlBlock>> {
    if pid == 0 { Some(current_process()) } else { pid2process(pid as usize) }
}

pub fn sys_getpgid(pid: i32) -> isize {
    process_by_pid_or_self(pid).map_or(-3, |p| p.inner_exclusive_access().pgid.0 as isize)
}

pub fn sys_getsid(pid: i32) -> isize {
    process_by_pid_or_self(pid).map_or(-3, |p| p.inner_exclusive_access().sid.0 as isize)
}

pub fn sys_setpgid(pid: i32, pgid: i32) -> isize {
    if pgid < 0 {
        return -22;
    }
    let caller = current_process();
    let target = match process_by_pid_or_self(pid) {
        Some(process) => process,
        None => return -3,
    };
    let sid = caller.inner_exclusive_access().sid.0;
    {
        let inner = target.inner_exclusive_access();
        if !Arc::ptr_eq(&target, &caller) {
            let parent = inner.parent.as_ref().and_then(|p| p.upgrade());
            if !parent.map_or(false, |p| Arc::ptr_eq(&caller, &p)) {
                return -3; // Only a direct child can be moved.
            }
            if inner.sid.0 != sid {
                return -1; // EPERM
            }
            if inner.execed {
                return -13; // EACCES
            }
        }
        if target.getpid() == inner.sid.0 {
            return -1;
        }
    }
    let group = if pgid == 0 || pgid as usize == target.getpid() {
        target.pid.clone()
    } else {
        match ProcessControlBlock::group_in_session(pgid as usize, sid) {
            Ok(group) => group,
            Err(_) => return -1,
        }
    };
    target.inner_exclusive_access().pgid = group;
    0
}

pub fn sys_setsid() -> isize {
    let caller = current_process();
    if caller.getpid() == crate::task::IDLE_PID {
        return -1;
    }
    let (sid, is_leader) = {
        let inner = caller.inner_exclusive_access();
        (inner.sid.0, inner.pgid.0 == caller.getpid() || inner.sid.0 == caller.getpid())
    };
    if is_leader || ProcessControlBlock::group_in_session(caller.getpid(), sid).is_ok() {
        return -1; // EPERM, including when other members still occupy that group.
    }
    let mut inner = caller.inner_exclusive_access();
    inner.sid = caller.pid.clone();
    inner.pgid = caller.pid.clone();
    caller.getpid() as isize
}

pub fn sys_umask(mask: u32) -> isize {
    let caller = current_process();
    let mut inner = caller.inner_exclusive_access();
    core::mem::replace(&mut inner.umask, mask & 0o777) as isize
}

struct UtsNames {
    nodename: [u8; 65],
    nodename_len: usize,
    domainname: [u8; 65],
    domainname_len: usize,
}

lazy_static::lazy_static! {
    static ref UTS_NAMES: crate::sync::UPSafeCell<UtsNames> = {
        let mut nodename = [0u8; 65];
        nodename[..5].copy_from_slice(b"rcore");
        let mut domainname = [0u8; 65];
        domainname[..11].copy_from_slice(b"localdomain");
        unsafe {
            crate::sync::UPSafeCell::new(UtsNames {
                nodename,
                nodename_len: 5,
                domainname,
                domainname_len: 11,
            })
        }
    };
}

/// uname(160)：填 6 个 65 字节字段，非法指针返回 EFAULT
pub fn sys_uname(buf: *mut u8) -> isize {
    if buf.is_null() {
        return -14;
    }
    let token = current_user_token();
    let Some(buffers) = translated_byte_buffer(token, buf, 390, MapPermission::W) else {
        return -14;
    };
    let mut raw = [0u8; 390];
    let uts = UTS_NAMES.exclusive_access();
    let fixed: [&[u8]; 6] = [
        b"Linux",
        &uts.nodename[..uts.nodename_len],
        b"6.0.0-rcore",
        b"#1 rCore-Tutorial",
        crate::arch::NAME.as_bytes(),
        &uts.domainname[..uts.domainname_len],
    ];
    for (i, f) in fixed.iter().enumerate() {
        let n = f.len().min(64);
        raw[i * 65..i * 65 + n].copy_from_slice(&f[..n]);
    }
    drop(uts);
    UserBuffer::new(buffers).copy_from(&raw);
    0
}

pub fn sys_sethostname(name: *const u8, len: usize) -> isize {
    let proc = current_process();
    let inner = proc.inner_exclusive_access();
    if inner.euid != 0 && (inner.caps[0][0] & (1 << 21)) == 0 {
        return -1; // EPERM (CAP_SYS_ADMIN)
    }
    drop(inner);
    if (len as isize) < 0 || len > 64 {
        return -22; // EINVAL
    }
    let mut tmp = [0u8; 65];
    if len > 0 {
        if name.is_null() {
            return -14;
        }
        let Some(bufs) = translated_byte_buffer(current_user_token(), name, len, MapPermission::R) else {
            return -14;
        };
        for (d, s) in tmp[..len].iter_mut().zip(UserBuffer::new(bufs)) {
            *d = s.read();
        }
    }
    let mut uts = UTS_NAMES.exclusive_access();
    uts.nodename = tmp;
    uts.nodename_len = len;
    0
}

pub fn sys_setdomainname(name: *const u8, len: usize) -> isize {
    let proc = current_process();
    let inner = proc.inner_exclusive_access();
    if inner.euid != 0 && (inner.caps[0][0] & (1 << 21)) == 0 {
        return -1; // EPERM (CAP_SYS_ADMIN)
    }
    drop(inner);
    if (len as isize) < 0 || len > 64 {
        return -22; // EINVAL
    }
    let mut tmp = [0u8; 65];
    if len > 0 {
        if name.is_null() {
            return -14;
        }
        let Some(bufs) = translated_byte_buffer(current_user_token(), name, len, MapPermission::R) else {
            return -14;
        };
        for (d, s) in tmp[..len].iter_mut().zip(UserBuffer::new(bufs)) {
            *d = s.read();
        }
    }
    let mut uts = UTS_NAMES.exclusive_access();
    uts.domainname = tmp;
    uts.domainname_len = len;
    0
}

/// times(153)：返回 100Hz 的时钟滴答数；buf == NULL 合法，非法非空指针返回 EFAULT
pub fn sys_times(buf: *mut u8) -> isize {
    if !buf.is_null() {
        let Some(buffers) = translated_byte_buffer(current_user_token(), buf, 32, MapPermission::W) else {
            return -14;
        };
        UserBuffer::new(buffers).fill_zero();
    }
    (crate::timer::get_time_ms() / 10).max(1) as isize
}

pub fn sys_sysinfo(buf: *mut u8) -> isize {
    // Linux's LP64 struct sysinfo has two padding gaps and is 112 bytes long.
    let mut data = [0u8; 112];
    let buffers = match translated_byte_buffer(current_user_token(), buf, data.len(), MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    let uptime = (crate::timer::get_time_ms() / 1000) as i64;
    let totalram = crate::mm::ram_end().saturating_sub(crate::arch::RAM_BASE);
    let freeram = crate::mm::free_frame_count() * PAGE_SIZE;
    let procs = process_ids().len().min(u16::MAX as usize) as u16;
    data[0..8].copy_from_slice(&uptime.to_ne_bytes());
    data[32..40].copy_from_slice(&totalram.to_ne_bytes());
    data[40..48].copy_from_slice(&freeram.to_ne_bytes());
    data[80..82].copy_from_slice(&procs.to_ne_bytes());
    data[104..108].copy_from_slice(&1u32.to_ne_bytes());
    buffers.copy_from(&data);
    0
}

pub fn sys_kill(pid: usize, signal: u32) -> isize {
    if signal > 64 {
        return -22;
    }
    let selector = pid as i32;
    let caller = current_process();
    let sender = caller.getpid();
    let (caller_uid, caller_euid, caller_sid) = {
        let inner = caller.inner_exclusive_access();
        (inner.uid, inner.euid, inner.sid.0)
    };
    let can_signal = |target: &Arc<ProcessControlBlock>| -> bool {
        if caller_euid == 0 || target.getpid() == sender {
            return true;
        }
        let inner = target.inner_exclusive_access();
        if signal == 18 && inner.sid.0 == caller_sid {
            return true;
        }
        caller_uid == inner.uid
            || caller_uid == inner.suid
            || caller_euid == inner.uid
            || caller_euid == inner.suid
    };
    let send = |target: &Arc<ProcessControlBlock>| {
        if signal != 0 && !target.inner_exclusive_access().is_zombie {
            super::signal::send_process_signal(target, signal as usize, sender, caller_uid);
        }
    };
    if selector > 0 {
        return pid2process(selector as usize).map_or(-3, |target| {
            if !can_signal(&target) {
                return -1;
            }
            send(&target);
            0
        });
    }
    let group = if selector == 0 {
        caller.inner_exclusive_access().pgid.0
    } else {
        selector.unsigned_abs() as usize
    };
    let mut found = false;
    let mut permitted = false;
    for target in process_ids().into_iter().filter_map(pid2process) {
        if selector == -1 {
            if target.getpid() == crate::task::IDLE_PID || target.getpid() == sender {
                continue;
            }
        } else if target.inner_exclusive_access().pgid.0 != group {
            continue;
        }
        found = true;
        if can_signal(&target) {
            permitted = true;
            send(&target);
        }
    }
    if !found { -3 } else if !permitted { -1 } else { 0 }
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    crate::task::exit_current_group(exit_code);
    unreachable!("exited thread resumed")
}

pub fn sys_set_tid_address(tidptr: usize) -> isize {
    let task = current_task().unwrap();
    task.inner_exclusive_access().clear_child_tid = tidptr;
    task.linux_tid() as isize
}

/// gettimeofday(169)：填 struct timeval { long sec; long usec; }，返回 0
pub fn sys_gettimeofday(tv: *mut u8, tz: *mut u8) -> isize {
    let token = current_user_token();
    if !tv.is_null() {
        let Some(buffers) = translated_byte_buffer(token, tv, 16, MapPermission::W) else {
            return -14;
        };
        let ns = crate::timer::get_realtime_ns();
        let mut raw = [0u8; 16];
        raw[0..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_ne_bytes());
        raw[8..16].copy_from_slice(&(((ns / 1_000) % 1_000_000) as i64).to_ne_bytes());
        UserBuffer::new(buffers).copy_from(&raw);
    }
    if !tz.is_null() {
        let Some(buffers) = translated_byte_buffer(token, tz, 8, MapPermission::W) else {
            return -14;
        };
        UserBuffer::new(buffers).fill_zero();
    }
    0
}

/// brk(214): grow or shrink the anonymous heap without overlapping another mapping.
pub fn sys_brk(addr: usize) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let old = inner.brk;
    if addr == 0 || addr < inner.brk_base || addr >= USER_END {
        return old as isize;
    }
    let old_end = VirtPageNum((old + PAGE_SIZE - 1) / PAGE_SIZE);
    let new_end = VirtPageNum((addr + PAGE_SIZE - 1) / PAGE_SIZE);
    if new_end > old_end {
        let range = old_end..new_end;
        if !inner.memory_set.vacant(&range) {
            return old as isize;
        }
        inner.memory_set.insert_vma(
            range,
            MapPermission::U | MapPermission::R | MapPermission::W,
            VmaBacking::Anonymous,
        );
    } else if new_end < old_end && inner.memory_set.munmap(new_end..old_end).is_err() {
        return old as isize;
    }
    inner.brk = addr;
    addr as isize
}

fn page_range(addr: usize, size: usize) -> Option<Range<VirtPageNum>> {
    let end = addr.checked_add(size)?;
    if addr % PAGE_SIZE != 0 || end > USER_END {
        return None;
    }
    Some(VirtPageNum(addr / PAGE_SIZE)..VirtPageNum(end / PAGE_SIZE))
}

fn round_mapping_size(len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    len.checked_add(PAGE_SIZE - 1).map(|n| n & !(PAGE_SIZE - 1))
}

pub fn sys_mmap(
    addr: usize,
    len: usize,
    prot: usize,
    flags: usize,
    fd: usize,
    offset: usize,
) -> isize {
    const MAP_SHARED: usize = 1;
    const MAP_PRIVATE: usize = 2;
    const MAP_FIXED: usize = 0x10;
    const MAP_ANONYMOUS: usize = 0x20;
    const MAP_FIXED_NOREPLACE: usize = 0x100000;

    let size = match round_mapping_size(len) {
        Some(size) if size <= USER_END => size,
        _ => return -22,
    };
    let permissions = match prot_to_perm(prot) {
        Some(permissions) => permissions,
        None => return -22,
    };
    let shared = match flags & 0xf {
        MAP_SHARED => true,
        MAP_PRIVATE => false,
        _ => return -22,
    };
    if offset % PAGE_SIZE != 0
        || offset.checked_add(size).map_or(true, |end| end > isize::MAX as usize)
    {
        return -22;
    }

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let backing = if flags & MAP_ANONYMOUS != 0 {
        VmaBacking::anonymous(shared)
    } else {
        let file = match inner.fd_table.get(fd).and_then(|file| file.as_ref()) {
            Some(file) => file.clone(),
            None => return -9,
        };
        if !file.readable() {
            return -13;
        }
        if file.is_zero_device() {
            VmaBacking::anonymous(shared)
        } else {
            if file.mapping_size().is_none() {
                return -19;
            }
            VmaBacking::file(file, offset, shared)
        }
    };
    if !backing.allows(permissions) {
        return -13;
    }
    let range = if flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        let range = match page_range(addr, size) {
            Some(range) if addr >= PAGE_SIZE => range,
            _ => return -22,
        };
        if flags & MAP_FIXED_NOREPLACE != 0 {
            if !inner.memory_set.vacant(&range) {
                return -17;
            }
        } else if let Err(error) = inner.unmap(range.clone(), process.getpid()) {
            return error;
        }
        range
    } else {
        const MMAP_BASE: usize = 0x1000_0000;
        let hint = addr & !(PAGE_SIZE - 1);
        if let Some(range) = page_range(hint, size)
            .filter(|range| hint >= PAGE_SIZE && inner.memory_set.vacant(range))
        {
            range
        } else {
            let pages = size / PAGE_SIZE;
            let start = VirtPageNum(inner.mmap_top.max(MMAP_BASE) / PAGE_SIZE);
            match inner
                .memory_set
                .find_vacant(start, pages)
                .or_else(|| inner.memory_set.find_vacant(VirtPageNum(MMAP_BASE / PAGE_SIZE), pages))
            {
                Some(range) => {
                    inner.mmap_top = range.end.0 * PAGE_SIZE;
                    range
                }
                None => return -12,
            }
        }
    };
    let start = range.start.0 * PAGE_SIZE;
    inner.memory_set.insert_vma(range, permissions, backing);
    start as isize
}

pub fn sys_munmap(addr: usize, len: usize) -> isize {
    const MMAP_BASE: usize = 0x1000_0000;
    let range = match round_mapping_size(len).and_then(|size| page_range(addr, size)) {
        Some(range) => range,
        None => return -22,
    };
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    match inner.unmap(range.clone(), process.getpid()) {
        Ok(()) => {
            if inner.mmap_top == range.end.0 * PAGE_SIZE {
                inner.mmap_top = (range.start.0 * PAGE_SIZE).max(MMAP_BASE);
            }
            0
        }
        Err(errno) => errno,
    }
}

pub fn sys_mremap(
    old_addr: usize,
    old_size: usize,
    new_size: usize,
    flags: usize,
    new_addr: usize,
) -> isize {
    const MREMAP_MAYMOVE: usize = 1;
    const MREMAP_FIXED: usize = 2;
    const MMAP_BASE: usize = 0x1000_0000;

    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0
        || (flags & MREMAP_FIXED != 0 && flags & MREMAP_MAYMOVE == 0)
        || old_addr % PAGE_SIZE != 0
        || old_addr == 0
    {
        return -22;
    }
    let Some(aligned_new) = round_mapping_size(new_size) else {
        return -22;
    };
    if aligned_new > USER_END {
        return -12;
    }
    let aligned_old = if old_size == 0 {
        0
    } else {
        match round_mapping_size(old_size) {
            Some(size) => size,
            None => return -22,
        }
    };
    let Some(old_end) = old_addr.checked_add(aligned_old) else {
        return -22;
    };
    if old_end > USER_END {
        return -22;
    }
    let old_range = VirtPageNum(old_addr / PAGE_SIZE)..VirtPageNum(old_end / PAGE_SIZE);
    let new_pages = aligned_new / PAGE_SIZE;

    let fixed_dest = if flags & MREMAP_FIXED != 0 {
        let Some(new_end) = new_addr.checked_add(aligned_new) else {
            return -22;
        };
        if new_addr % PAGE_SIZE != 0
            || new_addr < PAGE_SIZE
            || new_end > USER_END
            || (old_addr < new_end && new_addr < old_end.max(old_addr + PAGE_SIZE))
        {
            return -22;
        }
        Some(VirtPageNum(new_addr / PAGE_SIZE))
    } else {
        None
    };

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let source = old_range.start..old_range.end.max(VirtPageNum(old_range.start.0 + 1));
    if inner.shm_attachments.iter().any(|attachment| attachment.overlaps(&source)) {
        return -22; // SysV attachments retain their shmat address for shmdt.
    }
    let mmap_base = VirtPageNum(MMAP_BASE / PAGE_SIZE);
    match inner.memory_set.mremap(
        old_range,
        new_pages,
        fixed_dest,
        flags & MREMAP_MAYMOVE != 0,
        mmap_base,
    ) {
        Ok(start) => {
            if let Some(dest) = fixed_dest {
                inner.forget_shm_range(&(dest..VirtPageNum(dest.0 + new_pages)), process.getpid());
            }
            let end_va = (start.0 + new_pages) * PAGE_SIZE;
            if end_va > inner.mmap_top {
                inner.mmap_top = end_va;
            }
            (start.0 * PAGE_SIZE) as isize
        }
        Err(errno) => errno,
    }
}

pub fn sys_msync(addr: usize, len: usize, flags: usize) -> isize {
    const MS_ASYNC: usize = 1;
    const MS_INVALIDATE: usize = 2;
    const MS_SYNC: usize = 4;
    if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0
        || flags & (MS_ASYNC | MS_SYNC) == (MS_ASYNC | MS_SYNC)
    {
        return -22;
    }
    let size = if len == 0 { Some(0) } else { round_mapping_size(len) };
    let range = match size.and_then(|size| page_range(addr, size)) {
        Some(range) => range,
        None => return -22,
    };
    let process = current_process();
    let inner = process.inner_exclusive_access();
    let result = inner.memory_set.check_user_range(&range, MapPermission::empty()).and_then(|()| {
        if flags & MS_SYNC != 0 && range.start < range.end {
            inner.memory_set.sync_mapped_files(&range)
        } else {
            Ok(())
        }
    });
    result.err().unwrap_or(0)
}

fn sched_target_process(pid: i32) -> Result<Arc<ProcessControlBlock>, isize> {
    if pid < 0 {
        return Err(-22);
    }
    if pid == 0 {
        return Ok(current_process());
    }
    if let Some(p) = pid2process(pid as usize) {
        if !p.inner_exclusive_access().is_zombie {
            return Ok(p);
        }
    }
    if let Some(t) = crate::task::tid2task(pid as usize) {
        if let Some(p) = t.process.upgrade() {
            if !p.inner_exclusive_access().is_zombie {
                return Ok(p);
            }
        }
    }
    Err(-3) // ESRCH
}

pub fn sys_sched_get_priority_max(policy: i32) -> isize {
    match policy {
        1 | 2 => 99,
        0 | 3 | 5 | 6 => 0,
        _ => -22,
    }
}

pub fn sys_sched_get_priority_min(policy: i32) -> isize {
    match policy {
        1 | 2 => 1,
        0 | 3 | 5 | 6 => 0,
        _ => -22,
    }
}

pub fn sys_sched_getscheduler(pid: i32) -> isize {
    match sched_target_process(pid) {
        Ok(p) => p.inner_exclusive_access().sched_policy as isize,
        Err(e) => e,
    }
}

pub fn sys_sched_setscheduler(pid: i32, policy: i32, param: *const u8) -> isize {
    const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
    if pid < 0 || param.is_null() {
        return -22;
    }
    let base_policy = policy & !SCHED_RESET_ON_FORK;
    if policy != base_policy && (policy & !SCHED_RESET_ON_FORK) != base_policy {
        return -22;
    }
    if !matches!(base_policy, 0 | 1 | 2 | 3 | 5) {
        return -22;
    }
    let target = match sched_target_process(pid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(prio) = try_read_user(current_user_token(), param as *const i32) else {
        return -14;
    };
    if matches!(base_policy, 1 | 2) {
        if !(1..=99).contains(&prio) {
            return -22;
        }
        let caller = current_process();
        let ci = caller.inner_exclusive_access();
        if ci.euid != 0 && (ci.caps[0][0] & (1 << 23)) == 0 {
            return -1; // EPERM (CAP_SYS_NICE)
        }
    } else if prio != 0 {
        return -22;
    }
    let mut ti = target.inner_exclusive_access();
    ti.sched_policy = base_policy as u32;
    ti.sched_priority = prio;
    0
}

pub fn sys_sched_setparam(pid: i32, param: *const u8) -> isize {
    if pid < 0 || param.is_null() {
        return -22;
    }
    let target = match sched_target_process(pid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(prio) = try_read_user(current_user_token(), param as *const i32) else {
        return -14;
    };
    let cur_policy = target.inner_exclusive_access().sched_policy;
    if matches!(cur_policy, 1 | 2) {
        if !(1..=99).contains(&prio) {
            return -22;
        }
        let caller = current_process();
        let ci = caller.inner_exclusive_access();
        if ci.euid != 0 && (ci.caps[0][0] & (1 << 23)) == 0 {
            return -1;
        }
    } else if prio != 0 {
        return -22;
    }
    target.inner_exclusive_access().sched_priority = prio;
    0
}

/// sched_getparam(121)：写一个 4 字节的 sched_priority
pub fn sys_sched_getparam(pid: i32, param: *mut u8) -> isize {
    if pid < 0 || param.is_null() {
        return -22;
    }
    let target = match sched_target_process(pid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let prio = target.inner_exclusive_access().sched_priority;
    if try_write_user(current_user_token(), param as *mut i32, prio).is_none() {
        return -14;
    }
    0
}

pub fn sys_sched_rr_get_interval(pid: i32, tp: *mut u8) -> isize {
    if pid < 0 {
        return -22;
    }
    let _target = match sched_target_process(pid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !tp.is_null() {
        let Some(bufs) = translated_byte_buffer(current_user_token(), tp, 16, MapPermission::W) else {
            return -14;
        };
        let mut raw = [0u8; 16];
        raw[8..16].copy_from_slice(&10_000_000i64.to_ne_bytes());
        UserBuffer::new(bufs).copy_from(&raw);
    }
    0
}

pub fn sys_sched_setaffinity(tid: i32, size: usize, mask: *const u8) -> isize {
    if tid < 0 || size == 0 {
        return -22;
    }
    let _target = match sched_target_process(tid) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if mask.is_null() {
        return -14;
    }
    let check_len = size.min(128);
    let Some(bufs) = translated_byte_buffer(current_user_token(), mask, check_len, MapPermission::R) else {
        return -14;
    };
    let mut raw = [0u8; 128];
    for (d, s) in raw[..check_len].iter_mut().zip(UserBuffer::new(bufs)) {
        *d = s.read();
    }
    #[cfg(feature = "smp")]
    let online_mask: u8 = crate::arch::online_hart_mask() as u8;
    #[cfg(not(feature = "smp"))]
    let online_mask: u8 = 1;
    if (raw[0] & online_mask) == 0 {
        return -22; // EINVAL: no online CPUs in mask
    }
    0
}

pub fn sys_sched_getaffinity(tid: i32, size: usize, mask: *mut u8) -> isize {
    let word = core::mem::size_of::<usize>();
    if tid < 0 || size < word || size % word != 0 {
        return -22;
    }
    let process = match sched_target_process(tid) {
        Ok(process) => process,
        Err(e) => return e,
    };
    let cpuset = (1usize << process.affinity_hart()).to_le_bytes();
    let buffers = match translated_byte_buffer(current_user_token(), mask, word, MapPermission::W) {
        Some(buffers) => UserBuffer::new(buffers),
        None => return -14,
    };
    for (output, byte) in buffers.into_iter().zip(cpuset) {
        output.write(byte);
    }
    word as isize
}

/// pselect6(72)：按真实 fd 就绪状态轮询。
/// - nfds == 0（或三组 fd 集全为 NULL）：按 timeout 睡眠（lmbench 用它做精确延时）；
/// - 有 fd 集：用 File::poll_readable/poll_writable 探测，超时前每 10ms 重扫一次。
///   lmbench 的 benchmp 用 select(0) 轮询管道做父子同步，必须反映真实就绪状态，
///   否则子进程会在 read(尚未写入的 start_signal) 上永久阻塞，父子互等死锁。
fn read_fdset(token: usize, ptr: *mut u8, bytes: usize) -> alloc::vec::Vec<u8> {
    let mut v = alloc::vec![0u8; bytes];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = read_user(token, (ptr as usize + i) as *const u8);
    }
    v
}

fn write_fdset(token: usize, ptr: *mut u8, v: &[u8]) {
    for (i, b) in v.iter().enumerate() {
        write_user(token, (ptr as usize + i) as *mut u8, *b);
    }
}

fn fdset_get(v: &[u8], fd: usize) -> bool {
    (v[fd / 8] >> (fd % 8)) & 1 == 1
}

pub fn sys_pselect6(
    nfds: usize,
    readfds: *mut u8,
    writefds: *mut u8,
    exceptfds: *mut u8,
    timeout: *const u8,
    sigmask_arg: usize,
) -> isize {
    let limit = current_process().inner_exclusive_access().nofile_limit();
    if nfds > limit {
        return -22;
    }
    let deadline = match super::fs::poll_deadline(timeout as usize) {
        Ok(deadline) => deadline,
        Err(error) => return error,
    };
    let token = current_user_token();
    let (mask_ptr, mask_size) = if sigmask_arg == 0 {
        (0, 0)
    } else {
        let Some(ss) = try_read_user(token, sigmask_arg as *const usize) else {
            return -14;
        };
        let Some(ss_len) = sigmask_arg
            .checked_add(core::mem::size_of::<usize>())
            .and_then(|p| try_read_user(token, p as *const usize))
        else {
            return -14;
        };
        (ss, ss_len)
    };
    let bytes = (nfds + 7) / 8;
    let sleep_only =
        bytes == 0 || (readfds.is_null() && writefds.is_null() && exceptfds.is_null());
    if sleep_only {
        return match super::signal::with_signal_mask(mask_ptr, mask_size, || {
            super::fs::poll_wait(deadline, || Ok(0))
        }) {
            Ok(_) => 0,
            Err(error) => error,
        };
    }
    let req_r = if readfds.is_null() {
        None
    } else {
        Some(read_fdset(token, readfds, bytes))
    };
    let req_w = if writefds.is_null() {
        None
    } else {
        Some(read_fdset(token, writefds, bytes))
    };
    let req_e = if exceptfds.is_null() {
        None
    } else {
        Some(read_fdset(token, exceptfds, bytes))
    };
    let mut final_r = alloc::vec![0u8; bytes];
    let mut final_w = alloc::vec![0u8; bytes];
    let mut final_e = alloc::vec![0u8; bytes];
    let result = super::signal::with_signal_mask(mask_ptr, mask_size, || {
        super::fs::poll_wait(deadline, || {
            final_r.fill(0);
            final_w.fill(0);
            final_e.fill(0);
            let files: alloc::vec::Vec<Option<alloc::sync::Arc<dyn crate::fs::File + Send + Sync>>> = {
                let process = current_process();
                let inner = process.inner_exclusive_access();
                (0..nfds)
                    .map(|fd| {
                        let want = req_r.as_ref().map_or(false, |r| fdset_get(r, fd))
                            || req_w.as_ref().map_or(false, |w| fdset_get(w, fd))
                            || req_e.as_ref().map_or(false, |e| fdset_get(e, fd));
                        if want {
                            inner.fd_table.get(fd).and_then(|f| f.clone())
                        } else {
                            None
                        }
                    })
                    .collect()
            };
            let mut nready = 0usize;
            for fd in 0..nfds {
                let want_r = req_r.as_ref().map_or(false, |r| fdset_get(r, fd));
                let want_w = req_w.as_ref().map_or(false, |w| fdset_get(w, fd));
                let want_e = req_e.as_ref().map_or(false, |e| fdset_get(e, fd));
                if !want_r && !want_w && !want_e {
                    continue;
                }
                let Some(file) = &files[fd] else {
                    return Err(-9); // EBADF
                };
                let events = file.poll_events();
                if want_r && (events & (1 | 8 | 16) != 0 || file.poll_readable()) {
                    final_r[fd / 8] |= 1 << (fd % 8);
                    nready += 1;
                }
                if want_w && (events & (4 | 8 | 16) != 0 || file.poll_writable()) {
                    final_w[fd / 8] |= 1 << (fd % 8);
                    nready += 1;
                }
                if want_e && (events & 2 != 0) {
                    final_e[fd / 8] |= 1 << (fd % 8);
                    nready += 1;
                }
            }
            Ok(nready)
        })
    });
    match result {
        Ok(nready) => {
            if !readfds.is_null() {
                write_fdset(token, readfds, &final_r);
            }
            if !writefds.is_null() {
                write_fdset(token, writefds, &final_w);
            }
            if !exceptfds.is_null() {
                write_fdset(token, exceptfds, &final_e);
            }
            nready as isize
        }
        Err(error) => error,
    }
}

/// getrusage(165)：返回全 0 的 struct rusage（144 字节）。
pub fn sys_getrusage(who: i32, buf: *mut u8) -> isize {
    if !matches!(who, 0 | -1 | 1) {
        return -22; // EINVAL
    }
    if buf.is_null() {
        return -14;
    }
    let Some(buffers) = translated_byte_buffer(current_user_token(), buf, 144, MapPermission::W) else {
        return -14;
    };
    UserBuffer::new(buffers).fill_zero();
    0
}

/// getrandom(278)：校验标志位与用户缓冲区，输出与 /dev/{random,urandom} 同源的随机流
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> isize {
    const GRND_VALID: u32 = 0x7; // GRND_NONBLOCK | GRND_RANDOM | GRND_INSECURE
    if flags & !GRND_VALID != 0 {
        return -22; // EINVAL
    }
    if len == 0 {
        return 0;
    }
    let count = len.min(0x01ff_ffff);
    let token = current_user_token();
    match translated_byte_buffer(token, buf, count, MapPermission::W) {
        Some(buffers) => crate::fs::random::fill_random(&UserBuffer::new(buffers)) as isize,
        None => -14, // EFAULT
    }
}

/// rt_sigtimedwait(137)：简化版——有挂起信号就取走并返回信号号，
/// 否则睡眠等待（每 5ms 查一次），超时返回 -EAGAIN。
/// libc-test 的 runtest 用它等 SIGCHLD（子进程退出），是 5s 看门狗的核心。
pub fn sys_rt_sigtimedwait(set: *const u8, info: *mut u8, timeout: *const u8, size: usize) -> isize {
    if size != core::mem::size_of::<u64>() {
        return -22;
    }
    let token = current_user_token();
    let setbits = match try_read_user(token, set as *const u64) {
        Some(set) => set & !((1 << (9 - 1)) | (1 << (19 - 1))),
        None => return -14,
    };
    let deadline = if timeout.is_null() {
        usize::MAX
    } else {
        let sec = try_read_user(token, timeout.cast::<usize>());
        let nsec = try_read_user(token, timeout.wrapping_add(8).cast::<usize>());
        let (sec, nsec) = match (sec, nsec) {
            (Some(s), Some(ns)) if s <= isize::MAX as usize && ns < 1_000_000_000 => (s, ns),
            (Some(_), Some(_)) => return -22,
            _ => return -14,
        };
        crate::timer::get_time_ms().saturating_add(sec.saturating_mul(1000))
            .saturating_add((nsec + 999_999) / 1_000_000)
    };
    loop {
        if let Some(pending) = super::signal::take_pending_info(setbits) {
            if !info.is_null() && pending.write_to(token, info as usize).is_none() {
                return -14;
            }
            return pending.signo as isize;
        }
        if crate::timer::get_time_ms() >= deadline {
            return -11; // EAGAIN
        }
        if super::signal::current_wait_interrupted() {
            return -4;
        }
        crate::syscall::sync::sys_sleep(5);
    }
}

/// 把 Linux 的 prot 位翻译成 MapPermission
fn prot_to_perm(prot: usize) -> Option<MapPermission> {
    if prot & !7 != 0 {
        return None;
    }
    let mut permissions = MapPermission::U;
    if prot & 3 != 0 {
        // Sv39 reserves leaf entries with W=1 and R=0.
        permissions |= MapPermission::R;
    }
    if prot & 2 != 0 {
        permissions |= MapPermission::W;
    }
    if prot & 4 != 0 {
        permissions |= MapPermission::X;
    }
    Some(permissions)
}

pub fn sys_mprotect(addr: usize, len: usize, prot: usize) -> isize {
    if addr % PAGE_SIZE != 0 {
        return -22;
    }
    if len == 0 {
        return 0;
    }
    let (range, permissions) = match (
        round_mapping_size(len).and_then(|size| page_range(addr, size)),
        prot_to_perm(prot),
    ) {
        (Some(range), Some(permissions)) => (range, permissions),
        _ => return -22,
    };
    let process = current_process();
    let result = process.inner_exclusive_access().memory_set.mprotect(range, permissions);
    result.err().unwrap_or(0)
}

pub fn sys_madvise(addr: usize, len: usize, advice: usize) -> isize {
    if addr % PAGE_SIZE != 0 {
        return -22;
    }
    if !matches!(advice, 0..=4 | 8..=11 | 14..=21) {
        return -22;
    }
    if len == 0 {
        return 0;
    }
    let Some(range) = round_mapping_size(len).and_then(|size| page_range(addr, size)) else {
        return -22;
    };
    let process = current_process();
    let result = process.inner_exclusive_access().memory_set.madvise(range, advice);
    result.err().unwrap_or(0)
}

pub fn sys_capget(hdrp: *mut u32, datap: *mut u32) -> isize {
    const CAP_V1: u32 = 0x1998_0330;
    const CAP_V2: u32 = 0x2007_1026;
    const CAP_V3: u32 = 0x2008_0522;
    if hdrp.is_null() {
        return -14;
    }
    let token = current_user_token();
    let Some(version) = try_read_user(token, hdrp as *const u32) else {
        return -14;
    };
    let Some(pid_ptr) = (hdrp as usize).checked_add(4) else {
        return -14;
    };
    let Some(pid) = try_read_user(token, pid_ptr as *const i32) else {
        return -14;
    };
    let words = match version {
        CAP_V1 => 1usize,
        CAP_V2 | CAP_V3 => 2usize,
        _ => {
            if try_write_user(token, hdrp, CAP_V3).is_none() {
                return -14;
            }
            return if datap.is_null() { 0 } else { -22 };
        }
    };
    if pid < 0 {
        return -22;
    }
    let caps = if pid == 0 {
        current_process().inner_exclusive_access().caps
    } else {
        let Some(target) = pid2process(pid as usize).or_else(|| {
            crate::task::tid2task(pid as usize).and_then(|t| t.process.upgrade())
        }) else {
            return -3;
        };
        let inner = target.inner_exclusive_access();
        if inner.is_zombie {
            return -3;
        }
        inner.caps
    };
    if datap.is_null() {
        return 0;
    }
    for w in 0..words {
        for (slot, val) in caps[w].into_iter().enumerate() {
            let Some(off) = (datap as usize).checked_add((w * 3 + slot) * 4) else {
                return -14;
            };
            if try_write_user(token, off as *mut u32, val).is_none() {
                return -14;
            }
        }
    }
    0
}

pub fn sys_capset(hdrp: *mut u32, datap: *const u32) -> isize {
    const CAP_V1: u32 = 0x1998_0330;
    const CAP_V2: u32 = 0x2007_1026;
    const CAP_V3: u32 = 0x2008_0522;
    if hdrp.is_null() {
        return -14;
    }
    let token = current_user_token();
    let Some(version) = try_read_user(token, hdrp as *const u32) else {
        return -14;
    };
    let Some(pid_ptr) = (hdrp as usize).checked_add(4) else {
        return -14;
    };
    let Some(pid) = try_read_user(token, pid_ptr as *const i32) else {
        return -14;
    };
    let words = match version {
        CAP_V1 => 1usize,
        CAP_V2 | CAP_V3 => 2usize,
        _ => {
            if try_write_user(token, hdrp, CAP_V3).is_none() {
                return -14;
            }
            return -22;
        }
    };
    if datap.is_null() {
        return -14;
    }
    let self_pid = current_process().getpid() as i32;
    let self_tid = current_task().unwrap().linux_tid() as i32;
    if pid != 0 && pid != self_pid && pid != self_tid {
        return -1; // EPERM (cannot modify another process's capabilities)
    }
    let mut next = [[0u32; 3]; 2];
    for w in 0..words {
        for slot in 0..3 {
            let Some(off) = (datap as usize).checked_add((w * 3 + slot) * 4) else {
                return -14;
            };
            let Some(val) = try_read_user(token, off as *const u32) else {
                return -14;
            };
            next[w][slot] = val;
        }
    }
    let proc = current_process();
    let mut inner = proc.inner_exclusive_access();
    let has_setpcap = (inner.caps[0][0] & (1 << 8)) != 0;
    for w in 0..words {
        let [ne, np, ni] = next[w];
        let [_oe, op, oi] = inner.caps[w];
        if (ne & !np) != 0 || (np & !op) != 0 {
            return -1; // EPERM: eff must be subset of perm, perm cannot exceed old perm
        }
        if !has_setpcap && (ni & !(oi | op)) != 0 {
            return -1; // EPERM: inh cannot exceed old inh | old perm without CAP_SETPCAP
        }
    }
    for w in 0..words {
        inner.caps[w] = next[w];
    }
    if words == 1 {
        inner.caps[1] = [0; 3];
    }
    0
}

pub fn sys_prctl(option: usize, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> isize {
    let token = current_user_token();
    match option {
        1 => {
            // PR_SET_PDEATHSIG
            if arg2 > 64 {
                return -22;
            }
            current_process().inner_exclusive_access().pdeathsig = arg2 as u8;
            0
        }
        2 => {
            // PR_GET_PDEATHSIG
            let sig = current_process().inner_exclusive_access().pdeathsig as i32;
            if arg2 == 0 || try_write_user(token, arg2 as *mut i32, sig).is_none() {
                -14
            } else {
                0
            }
        }
        3 => current_process().inner_exclusive_access().dumpable as isize, // PR_GET_DUMPABLE
        4 => {
            // PR_SET_DUMPABLE
            if arg2 > 1 {
                return -22;
            }
            current_process().inner_exclusive_access().dumpable = arg2 as u8;
            0
        }
        7 => current_process().inner_exclusive_access().keepcaps as isize, // PR_GET_KEEPCAPS
        8 => {
            // PR_SET_KEEPCAPS
            if arg2 > 1 {
                return -22;
            }
            current_process().inner_exclusive_access().keepcaps = arg2 != 0;
            0
        }
        15 => {
            // PR_SET_NAME
            if arg2 == 0 {
                return -14;
            }
            let mut comm = [0u8; 16];
            for i in 0..15 {
                let Some(ptr) = arg2.checked_add(i) else {
                    return -14;
                };
                let Some(b) = try_read_user(token, ptr as *const u8) else {
                    if i == 0 {
                        return -14;
                    }
                    break;
                };
                if b == 0 {
                    break;
                }
                comm[i] = b;
            }
            current_task().unwrap().inner_exclusive_access().comm = comm;
            0
        }
        16 => {
            // PR_GET_NAME
            if arg2 == 0 {
                return -14;
            }
            let comm = current_task().unwrap().inner_exclusive_access().comm;
            let Some(bufs) = translated_byte_buffer(token, arg2 as *const u8, 16, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(bufs).copy_from(&comm);
            0
        }
        23 => {
            // PR_CAPBSET_READ
            if arg2 > 40 { -22 } else { 1 }
        }
        24 => {
            // PR_CAPBSET_DROP
            if arg2 > 40 {
                -22
            } else {
                let proc = current_process();
                let inner = proc.inner_exclusive_access();
                if inner.euid != 0 && (inner.caps[0][0] & (1 << 8)) == 0 {
                    -1
                } else {
                    0
                }
            }
        }
        29 => {
            // PR_SET_TIMERSLACK
            if (arg2 as isize) < 0 {
                return -22;
            }
            current_process().inner_exclusive_access().timerslack_ns =
                if arg2 == 0 { 50_000 } else { arg2 as u64 };
            0
        }
        30 => current_process().inner_exclusive_access().timerslack_ns as isize, // PR_GET_TIMERSLACK
        38 => {
            // PR_SET_NO_NEW_PRIVS
            if arg2 != 1 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return -22;
            }
            current_process().inner_exclusive_access().no_new_privs = true;
            0
        }
        39 => {
            // PR_GET_NO_NEW_PRIVS
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return -22;
            }
            current_process().inner_exclusive_access().no_new_privs as isize
        }
        41 | 42 => 0, // PR_SET_THP_DISABLE / PR_GET_THP_DISABLE
        0x5356_4d41 => {
            // PR_SET_VMA (PR_SET_VMA_ANON_NAME = 0)
            if arg2 == 0 { 0 } else { -22 }
        }
        _ => -22,
    }
}

fn collect_priority_targets(which: usize, who: i32) -> Result<Vec<Arc<ProcessControlBlock>>, isize> {
    let caller = current_process();
    match which {
        0 => {
            // PRIO_PROCESS
            if who < 0 {
                return Err(-3);
            }
            let target = if who == 0 {
                Some(caller)
            } else {
                pid2process(who as usize).or_else(|| {
                    crate::task::tid2task(who as usize).and_then(|t| t.process.upgrade())
                })
            };
            match target {
                Some(p) if !p.inner_exclusive_access().is_zombie => Ok(alloc::vec![p]),
                _ => Err(-3),
            }
        }
        1 => {
            // PRIO_PGRP
            if who < 0 {
                return Err(-3);
            }
            let pgid = if who == 0 {
                caller.inner_exclusive_access().pgid.0
            } else {
                who as usize
            };
            let list: Vec<_> = process_ids()
                .into_iter()
                .filter_map(pid2process)
                .filter(|p| {
                    let i = p.inner_exclusive_access();
                    !i.is_zombie && i.pgid.0 == pgid
                })
                .collect();
            if list.is_empty() { Err(-3) } else { Ok(list) }
        }
        2 => {
            // PRIO_USER
            if who < 0 {
                return Err(-3);
            }
            let uid = if who == 0 {
                caller.inner_exclusive_access().uid
            } else {
                who as u32
            };
            let list: Vec<_> = process_ids()
                .into_iter()
                .filter_map(pid2process)
                .filter(|p| {
                    let i = p.inner_exclusive_access();
                    !i.is_zombie && i.uid == uid
                })
                .collect();
            if list.is_empty() { Err(-3) } else { Ok(list) }
        }
        _ => Err(-22),
    }
}

pub fn sys_getpriority(which: usize, who: i32) -> isize {
    let targets = match collect_priority_targets(which, who) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let mut best = 19i8;
    for p in targets {
        let n = p.inner_exclusive_access().nice;
        if n < best {
            best = n;
        }
    }
    (20 - i32::from(best)) as isize
}

pub fn sys_setpriority(which: usize, who: i32, prio: i32) -> isize {
    let targets = match collect_priority_targets(which, who) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let clamped = prio.clamp(-20, 19) as i8;
    let (caller_uid, caller_euid, has_nice_cap) = {
        let c = current_process();
        let ci = c.inner_exclusive_access();
        (ci.uid, ci.euid, ci.euid == 0 || (ci.caps[0][0] & (1 << 23)) != 0)
    };
    for p in &targets {
        let inner = p.inner_exclusive_access();
        if !has_nice_cap {
            if caller_euid != inner.uid && caller_euid != inner.euid && caller_uid != inner.uid {
                return -1; // EPERM
            }
            if clamped < inner.nice {
                return -13; // EACCES when unprivileged tries to lower nice
            }
        }
    }
    for p in targets {
        p.inner_exclusive_access().nice = clamped;
    }
    0
}

pub fn sys_mincore(addr: usize, len: usize, vec: *mut u8) -> isize {
    if addr % PAGE_SIZE != 0 {
        return -22;
    }
    if len == 0 {
        return 0;
    }
    let Some(aligned) = round_mapping_size(len) else {
        return -12;
    };
    let Some(end) = addr.checked_add(aligned) else {
        return -12;
    };
    if end > USER_END {
        return -12;
    }
    if vec.is_null() {
        return -14;
    }
    let range = VirtPageNum(addr / PAGE_SIZE)..VirtPageNum(end / PAGE_SIZE);
    let out = {
        let proc = current_process();
        let inner = proc.inner_exclusive_access();
        match inner.memory_set.mincore_vec(range) {
            Ok(v) => v,
            Err(e) => return e,
        }
    };
    let Some(bufs) = translated_byte_buffer(current_user_token(), vec, out.len(), MapPermission::W) else {
        return -14;
    };
    UserBuffer::new(bufs).copy_from(&out);
    0
}

pub fn sys_mlock2(addr: usize, len: usize, flags: u32) -> isize {
    if flags & !1 != 0 {
        return -22;
    }
    let start = addr & !(PAGE_SIZE - 1);
    let Some(end_raw) = addr.checked_add(len) else {
        return -22;
    };
    if len == 0 {
        return 0;
    }
    let Some(end) = end_raw.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1)) else {
        return -12;
    };
    if end > USER_END {
        return -12;
    }
    let proc = current_process();
    let mut inner = proc.inner_exclusive_access();
    if inner.euid != 0 && (inner.caps[0][0] & (1 << 14)) == 0 {
        let limit = inner.resource_limits[8][0]; // RLIMIT_MEMLOCK
        if limit == 0 {
            return -1; // EPERM
        }
        if end - start > limit {
            return -12; // ENOMEM
        }
    }
    let range = VirtPageNum(start / PAGE_SIZE)..VirtPageNum(end / PAGE_SIZE);
    let res = if flags & 1 != 0 {
        inner.memory_set.check_user_range(&range, MapPermission::empty())
    } else {
        inner.memory_set.populate_range(range)
    };
    res.err().unwrap_or(0)
}

pub fn sys_munlock(addr: usize, len: usize) -> isize {
    let start = addr & !(PAGE_SIZE - 1);
    let Some(end_raw) = addr.checked_add(len) else {
        return -22;
    };
    if len == 0 {
        return 0;
    }
    let Some(end) = end_raw.checked_add(PAGE_SIZE - 1).map(|v| v & !(PAGE_SIZE - 1)) else {
        return -12;
    };
    if end > USER_END {
        return -12;
    }
    let range = VirtPageNum(start / PAGE_SIZE)..VirtPageNum(end / PAGE_SIZE);
    current_process()
        .inner_exclusive_access()
        .memory_set
        .check_user_range(&range, MapPermission::empty())
        .err()
        .unwrap_or(0)
}

pub fn sys_mlockall(flags: u32) -> isize {
    if flags == 0 || flags & !7 != 0 || flags == 4 {
        return -22;
    }
    let proc = current_process();
    let inner = proc.inner_exclusive_access();
    if inner.euid != 0 && (inner.caps[0][0] & (1 << 14)) == 0 && inner.resource_limits[8][0] == 0 {
        return -1;
    }
    0
}

pub fn sys_getrlimit(resource: usize, ptr: *mut u8) -> isize {
    update_rlimit(0, resource, None, Some(ptr)).err().unwrap_or(0)
}

pub fn sys_setrlimit(resource: usize, ptr: *const u8) -> isize {
    update_rlimit(0, resource, Some(ptr), None).err().unwrap_or(0)
}

pub fn sys_prlimit64(pid: i32, resource: usize, new: *const u8, old: *mut u8) -> isize {
    update_rlimit(pid, resource, (!new.is_null()).then_some(new), (!old.is_null()).then_some(old))
        .err().unwrap_or(0)
}

fn update_rlimit(
    pid: i32,
    resource: usize,
    new: Option<*const u8>,
    old: Option<*mut u8>,
) -> Result<(), isize> {
    use crate::task::{MAX_OPEN_FILES, RLIMIT_NOFILE};

    let token = current_user_token();
    let new_limit = if let Some(ptr) = new {
        let buffers = translated_byte_buffer(token, ptr, 16, MapPermission::R).ok_or(-14isize)?;
        let mut values = [[0; 8]; 2];
        for (dst, src) in values.iter_mut().flatten().zip(UserBuffer::new(buffers)) {
            *dst = src.read();
        }
        Some(values.map(usize::from_ne_bytes))
    } else {
        None
    };
    let process = process_by_pid_or_self(pid).or_else(|| {
        let task = crate::task::tid2task(pid as usize)?;
        if task.inner_exclusive_access().exit_code.is_some() {
            return None;
        }
        task.process.upgrade()
    }).ok_or(-3isize)?;
    let caller_euid = current_process().inner_exclusive_access().euid;
    let mut arm_cpu = None;
    let previous = {
        let mut inner = process.inner_exclusive_access();
        if caller_euid != 0 && process.getpid() != current_process().getpid() && caller_euid != inner.uid {
            return Err(-1);
        }
        let limit = inner.resource_limits.get_mut(resource).ok_or(-22isize)?;
        let previous = *limit;
        if let Some(new) = new_limit {
            if new[0] > new[1] {
                return Err(-22);
            }
            if caller_euid != 0 && new[1] > previous[1] {
                return Err(-1);
            }
            if resource == RLIMIT_NOFILE && new[1] > MAX_OPEN_FILES {
                return Err(-1);
            }
            *limit = new;
            if resource == 0 {
                arm_cpu = Some((new[0], new[1]));
            }
        }
        previous
    };
    if let Some((cur, max)) = arm_cpu {
        super::signal::arm_cpu_limit(&process, cur, max);
    }
    if let Some(ptr) = old {
        let buffers = translated_byte_buffer(token, ptr, 16, MapPermission::W).ok_or(-14isize)?;
        let values = previous.into_iter().flat_map(usize::to_ne_bytes);
        for (dst, value) in UserBuffer::new(buffers).into_iter().zip(values) {
            dst.write(value);
        }
    }
    Ok(())
}

pub fn sys_getuid() -> isize { current_process().inner_exclusive_access().uid as isize }
pub fn sys_geteuid() -> isize { current_process().inner_exclusive_access().euid as isize }
pub fn sys_getgid() -> isize { current_process().inner_exclusive_access().gid as isize }
pub fn sys_getegid() -> isize { current_process().inner_exclusive_access().egid as isize }

pub fn sys_setuid(uid: u32) -> isize {
    if uid == u32::MAX {
        return -22;
    }
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid == 0 {
        inner.update_uids(uid, uid, uid);
        0
    } else if uid == inner.uid || uid == inner.suid {
        let (ruid, suid) = (inner.uid, inner.suid);
        inner.update_uids(ruid, uid, suid);
        0
    } else {
        -1
    }
}

pub fn sys_setgid(gid: u32) -> isize {
    if gid == u32::MAX {
        return -22;
    }
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid == 0 {
        inner.gid = gid;
        inner.egid = gid;
        inner.sgid = gid;
        inner.fsgid = gid;
        0
    } else if gid == inner.gid || gid == inner.sgid {
        inner.egid = gid;
        inner.fsgid = gid;
        0
    } else {
        -1
    }
}

pub fn sys_setreuid(ruid: u32, euid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid != 0 {
        if ruid != u32::MAX && ruid != inner.uid && ruid != inner.euid {
            return -1;
        }
        if euid != u32::MAX && euid != inner.uid && euid != inner.euid && euid != inner.suid {
            return -1;
        }
    }
    let old_ruid = inner.uid;
    let new_ruid = if ruid == u32::MAX { inner.uid } else { ruid };
    let new_euid = if euid == u32::MAX { inner.euid } else { euid };
    let new_suid = if ruid != u32::MAX || (euid != u32::MAX && euid != old_ruid) {
        new_euid
    } else {
        inner.suid
    };
    inner.update_uids(new_ruid, new_euid, new_suid);
    0
}

pub fn sys_setregid(rgid: u32, egid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid != 0 {
        if rgid != u32::MAX && rgid != inner.gid && rgid != inner.egid {
            return -1;
        }
        if egid != u32::MAX && egid != inner.gid && egid != inner.egid && egid != inner.sgid {
            return -1;
        }
    }
    let old_rgid = inner.gid;
    let new_rgid = if rgid == u32::MAX { inner.gid } else { rgid };
    let new_egid = if egid == u32::MAX { inner.egid } else { egid };
    if rgid != u32::MAX || (egid != u32::MAX && egid != old_rgid) {
        inner.sgid = new_egid;
    }
    inner.gid = new_rgid;
    inner.egid = new_egid;
    inner.fsgid = new_egid;
    0
}

pub fn sys_setresuid(ruid: u32, euid: u32, suid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid != 0 {
        let allowed = [inner.uid, inner.euid, inner.suid];
        for candidate in [ruid, euid, suid] {
            if candidate != u32::MAX && !allowed.contains(&candidate) {
                return -1;
            }
        }
    }
    let ruid = if ruid == u32::MAX { inner.uid } else { ruid };
    let euid = if euid == u32::MAX { inner.euid } else { euid };
    let suid = if suid == u32::MAX { inner.suid } else { suid };
    inner.update_uids(ruid, euid, suid);
    0
}

pub fn sys_setresgid(rgid: u32, egid: u32, sgid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.euid != 0 {
        let allowed = [inner.gid, inner.egid, inner.sgid];
        for candidate in [rgid, egid, sgid] {
            if candidate != u32::MAX && !allowed.contains(&candidate) {
                return -1;
            }
        }
    }
    if rgid != u32::MAX { inner.gid = rgid; }
    if egid != u32::MAX { inner.egid = egid; }
    if sgid != u32::MAX { inner.sgid = sgid; }
    inner.fsgid = inner.egid;
    0
}

pub fn sys_getresuid(ruid: *mut u32, euid: *mut u32, suid: *mut u32) -> isize {
    let token = current_user_token();
    let (r, e, s) = {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        (inner.uid, inner.euid, inner.suid)
    };
    if ruid.is_null() || euid.is_null() || suid.is_null()
        || try_write_user(token, ruid, r).is_none()
        || try_write_user(token, euid, e).is_none()
        || try_write_user(token, suid, s).is_none()
    {
        return -14;
    }
    0
}

pub fn sys_getresgid(rgid: *mut u32, egid: *mut u32, sgid: *mut u32) -> isize {
    let token = current_user_token();
    let (r, e, s) = {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        (inner.gid, inner.egid, inner.sgid)
    };
    if rgid.is_null() || egid.is_null() || sgid.is_null()
        || try_write_user(token, rgid, r).is_none()
        || try_write_user(token, egid, e).is_none()
        || try_write_user(token, sgid, s).is_none()
    {
        return -14;
    }
    0
}

pub fn sys_setfsuid(uid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let prev = inner.fsuid;
    if uid != u32::MAX && (inner.euid == 0 || uid == inner.uid || uid == inner.euid || uid == inner.suid) {
        inner.fsuid = uid;
    }
    prev as isize
}

pub fn sys_setfsgid(gid: u32) -> isize {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let prev = inner.fsgid;
    if gid != u32::MAX && (inner.euid == 0 || gid == inner.gid || gid == inner.egid || gid == inner.sgid) {
        inner.fsgid = gid;
    }
    prev as isize
}

pub fn sys_getgroups(size: isize, list: *mut u32) -> isize {
    if size < 0 {
        return -22;
    }
    let groups = current_process().inner_exclusive_access().groups.clone();
    if size == 0 {
        return groups.len() as isize;
    }
    if (size as usize) < groups.len() {
        return -22;
    }
    if groups.is_empty() {
        return 0;
    }
    if list.is_null() {
        return -14;
    }
    let token = current_user_token();
    for (idx, &gid) in groups.iter().enumerate() {
        let Some(ptr) = (list as usize).checked_add(idx * 4) else {
            return -14;
        };
        if try_write_user(token, ptr as *mut u32, gid).is_none() {
            return -14;
        }
    }
    groups.len() as isize
}

pub fn sys_setgroups(size: usize, list: *const u32) -> isize {
    if size > 65536 {
        return -22;
    }
    if current_process().inner_exclusive_access().euid != 0 {
        return -1;
    }
    let mut next = Vec::with_capacity(size.min(256));
    let token = current_user_token();
    for idx in 0..size {
        if list.is_null() {
            return -14;
        }
        let Some(ptr) = (list as usize).checked_add(idx * 4) else {
            return -14;
        };
        let Some(gid) = try_read_user(token, ptr as *const u32) else {
            return -14;
        };
        next.push(gid);
    }
    current_process().inner_exclusive_access().groups = next;
    0
}
