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
        if errno == -2 && crate::config::legacy_oscomp_image()
            && raw_path.rsplit('/').next() == Some("busybox") {
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
        // 父进程在内核里忙等，时钟中断不会到来，
        // 必须主动轮询定时器，否则子进程的 sleep 永远醒不了。
        crate::timer::check_timer();
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4; // EINTR
        }
        suspend_current_and_run_next();
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

/// uname(2)：填 6 个 65 字节字段
pub fn sys_uname(buf: *mut u8) -> isize {
    let token = current_user_token();
    let fields: [&str; 6] = [
        "Linux",
        "rcore",
        "6.0.0-rcore",
        "#1 rCore-Tutorial",
        crate::arch::NAME,
        "localdomain",
    ];
    for (i, f) in fields.iter().enumerate() {
        for j in 0..65usize {
            let c = if j < f.len() { f.as_bytes()[j] } else { 0 };
            write_user(token, (buf as usize + i * 65 + j) as *mut u8, c);
        }
    }
    0
}

/// times(2)：返回 100Hz 的时钟滴答数
pub fn sys_times(buf: *mut u8) -> isize {
    let token = current_user_token();
    for i in 0..32usize {
        write_user(token, (buf as usize + i) as *mut u8, 0);
    }
    (crate::timer::get_time_ms() / 10) as isize
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
    let send = |target: &Arc<ProcessControlBlock>| {
        if signal != 0 && !target.inner_exclusive_access().is_zombie {
            // All tasks currently run with real and effective UID 0.
            super::signal::send_process_signal(target, signal as usize, sender, 0);
        }
    };
    if selector > 0 {
        return pid2process(selector as usize).map_or(-3, |target| { send(&target); 0 });
    }
    let group = if selector == 0 {
        caller.inner_exclusive_access().pgid.0
    } else {
        selector.unsigned_abs() as usize
    };
    let mut found = false;
    for target in process_ids().into_iter().filter_map(pid2process) {
        if selector == -1 {
            if target.getpid() == crate::task::IDLE_PID || target.getpid() == sender {
                continue;
            }
        } else if target.inner_exclusive_access().pgid.0 != group {
            continue;
        }
        found = true;
        send(&target);
    }
    if found { 0 } else { -3 }
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
pub fn sys_gettimeofday(tv: *mut u8) -> isize {
    let token = current_user_token();
    // 微秒分辨率（基准测试靠它做时间校准）
    let us = crate::timer::get_time_us();
    write_user(token, tv as *mut usize, us / 1_000_000);
    write_user(token, (tv as usize + 8) as *mut usize, us % 1_000_000);
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
        } else if let Err(error) = inner.memory_set.munmap(range.clone()) {
            return error;
        }
        range
    } else {
        let hint = addr & !(PAGE_SIZE - 1);
        if let Some(range) = page_range(hint, size)
            .filter(|range| hint >= PAGE_SIZE && inner.memory_set.vacant(range))
        {
            range
        } else {
            let start = VirtPageNum(inner.mmap_top / PAGE_SIZE);
            match inner.memory_set.find_vacant(start, size / PAGE_SIZE) {
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
    let range = match round_mapping_size(len).and_then(|size| page_range(addr, size)) {
        Some(range) => range,
        None => return -22,
    };
    let process = current_process();
    let result = process.inner_exclusive_access().memory_set.munmap(range);
    result.err().unwrap_or(0)
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

/// sched_getparam(123)：写一个 4 字节的 sched_priority
pub fn sys_sched_getparam(param: *mut u8) -> isize {
    if param.is_null() {
        return -22;
    }
    let token = crate::task::current_user_token();
    for i in 0..4 {
        write_user(token, (param as usize + i) as *mut u8, 0);
    }
    0
}

pub fn sys_sched_getaffinity(tid: i32, size: usize, mask: *mut u8) -> isize {
    let word = core::mem::size_of::<usize>();
    if size < word || size % word != 0 {
        return -22;
    }
    let process = if tid == 0 {
        current_process()
    } else {
        match crate::task::tid2task(tid as usize).and_then(|task| task.process.upgrade()) {
            Some(process) => process,
            None => return -3, // ESRCH
        }
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
) -> isize {
    let token = current_user_token();
    let bytes = (nfds + 7) / 8;
    let sleep_only =
        bytes == 0 || (readfds.is_null() && writefds.is_null() && exceptfds.is_null());
    let (deadline, sleep_ms) = if timeout.is_null() {
        (None, 0usize)
    } else {
        let sec = read_user(token, timeout as *const usize) as usize;
        let nsec = read_user(token, (timeout as usize + 8) as *const usize) as usize;
        let ms = sec * 1000 + (nsec + 999_999) / 1_000_000;
        (Some(crate::timer::get_time_ms() + ms), ms)
    };
    if sleep_only {
        if !timeout.is_null() && sleep_ms > 0 {
            crate::syscall::sync::sys_sleep(sleep_ms);
        }
        return 0;
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
    loop {
        // 先取 fd 表快照，再逐个 poll（避免持有进程锁去锁管道）
        let files: alloc::vec::Vec<Option<alloc::sync::Arc<dyn crate::fs::File + Send + Sync>>> = {
            let process = current_process();
            let inner = process.inner_exclusive_access();
            (0..nfds)
                .map(|fd| inner.fd_table.get(fd).and_then(|f| f.clone()))
                .collect()
        };
        let mut out_r = alloc::vec![0u8; bytes];
        let mut out_w = alloc::vec![0u8; bytes];
        let out_e = alloc::vec![0u8; bytes];
        let mut nready = 0usize;
        for fd in 0..nfds {
            let file = match &files[fd] {
                Some(f) => f,
                None => continue,
            };
            let mut ready = false;
            if let Some(req) = &req_r {
                if fdset_get(req, fd) && file.poll_readable() {
                    out_r[fd / 8] |= 1 << (fd % 8);
                    ready = true;
                }
            }
            if let Some(req) = &req_w {
                if fdset_get(req, fd) && file.poll_writable() {
                    out_w[fd / 8] |= 1 << (fd % 8);
                    ready = true;
                }
            }
            if ready {
                nready += 1;
            }
        }
        if nready > 0 || (deadline.is_some() && crate::timer::get_time_ms() >= deadline.unwrap()) {
            if !readfds.is_null() {
                write_fdset(token, readfds, &out_r);
            }
            if !writefds.is_null() {
                write_fdset(token, writefds, &out_w);
            }
            if !exceptfds.is_null() {
                write_fdset(token, exceptfds, &out_e);
            }
            return nready as isize;
        }
        drop(files);
        drop((out_r, out_w, out_e));
        match deadline {
            None => {
                crate::syscall::sync::sys_sleep(5);
            }
            Some(d) => {
                let now = crate::timer::get_time_ms();
                crate::syscall::sync::sys_sleep((d.saturating_sub(now)).clamp(1, 10));
            }
        }
    }
}

/// getrusage(165)：返回全 0 的 struct rusage（144 字节）。
/// lmbench 用它做时间校准，全 0 只影响校准精度，不会让测试崩溃。
pub fn sys_getrusage(buf: *mut u8) -> isize {
    if buf.is_null() {
        return -14;
    }
    let token = current_user_token();
    for i in 0..144usize {
        write_user(token, (buf as usize + i) as *mut u8, 0);
    }
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

/// 当前进程对某个 resource 的 (rlim_cur, rlim_max)
fn current_rlimit(resource: usize) -> (usize, usize) {
    match resource {
        3 => (8 * 1024 * 1024, 8 * 1024 * 1024), // RLIMIT_STACK
        4 => (8 * 1024 * 1024, 8 * 1024 * 1024), // RLIMIT_CORE
        7 => {
            // RLIMIT_NOFILE：真正生效，限制 fd 表长度
            let limit = current_process().inner_exclusive_access().nofile_limit;
            (limit, limit)
        }
        _ => (usize::MAX, usize::MAX),
    }
}

fn write_rlimit(token: usize, ptr: *mut u8, cur: usize, max: usize) {
    write_user(token, ptr as *mut usize, cur);
    write_user(token, (ptr as usize + 8) as *mut usize, max);
}

/// 应用一个 RLIMIT_NOFILE 设置（夹到 [16, 65536]，避免把 busybox 之类直接卡死）
fn apply_nofile(cur: usize) {
    let v = cur.clamp(16, 65536);
    current_process().inner_exclusive_access().nofile_limit = v;
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

/// getrlimit(163)
pub fn sys_getrlimit(resource: usize, ptr: *mut u8) -> isize {
    if ptr.is_null() {
        return -14;
    }
    let token = current_user_token();
    let (cur, max) = current_rlimit(resource);
    write_rlimit(token, ptr, cur, max);
    0
}

/// setrlimit(164)：目前只真正实现 RLIMIT_NOFILE，其它 resource 接受即成功
pub fn sys_setrlimit(resource: usize, ptr: *const u8) -> isize {
    if ptr.is_null() {
        return -14;
    }
    let token = current_user_token();
    let cur = read_user(token, ptr as *const usize) as usize;
    if resource == 7 {
        apply_nofile(cur);
    }
    0
}

/// prlimit64(261)：new != NULL 时应用，old != NULL 时回填
pub fn sys_prlimit64(_pid: usize, resource: usize, new: *const u8, old: *mut u8) -> isize {
    let token = current_user_token();
    if !new.is_null() {
        let cur = read_user(token, new as *const usize) as usize;
        if resource == 7 {
            apply_nofile(cur);
        }
    }
    if !old.is_null() {
        let (cur, max) = current_rlimit(resource);
        write_rlimit(token, old, cur, max);
    }
    0
}
