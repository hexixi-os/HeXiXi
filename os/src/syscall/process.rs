use crate::config::PAGE_SIZE;
use crate::mm::{MapPermission, VirtPageNum, VmaBacking, USER_END};
use core::ops::Range;
use crate::fs::{open_file, OpenFlags};
use crate::mm::{translated_path, translated_ref, translated_refmut, translated_str};
use crate::task::{
    current_process, current_task, current_user_token, exit_current_and_run_next, pid2process,
    suspend_current_and_run_next, SignalFlags,
};
use crate::timer::get_time_ms;
use alloc::string::String;
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

pub fn sys_get_time() -> isize {
    get_time_ms() as isize
}

pub fn sys_getpid() -> isize {
    current_task().unwrap().process.upgrade().unwrap().getpid() as isize
}

pub fn sys_fork() -> isize {
    current_process().fork(0).getpid() as isize
}

pub fn sys_exec(path: *const u8, mut args: *const usize) -> isize {
    let token = current_user_token();
    let raw_path = match translated_path(token, path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    // exec 的路径同样要做归一化（busybox sh 传入的是 ./prog）
    let path = match crate::fs::resolve_path(&raw_path) {
        Ok(path) => path,
        Err(errno) => return errno,
    };
    let mut args_vec: Vec<String> = Vec::new();
    loop {
        let arg_str_ptr = *translated_ref(token, args);
        if arg_str_ptr == 0 {
            break;
        }
        args_vec.push(translated_str(token, arg_str_ptr as *const u8));
        unsafe {
            args = args.add(1);
        }
    }
    if let Some(all_data) = crate::fs::read_file_all(path.as_str()) {
        let process = current_process();
        // 支持 shebang：把脚本交给解释器运行（Linux 的 binfmt_script 行为）
        if all_data.starts_with(b"#!") {
            let end = all_data.iter().position(|&c| c == b'\n').unwrap_or(all_data.len());
            let line = core::str::from_utf8(&all_data[2..end]).unwrap_or("").trim();
            let mut it = line.split_whitespace();
            let interp = it.next().unwrap_or("");
            let extra = it.next();
            let interp_name = interp.trim_start_matches('/');
            let basename = interp_name.rsplit('/').next().unwrap_or(interp_name);
            // argv = [解释器, 可选解释器参数(缺省用 basename), 脚本, ...原参数]
            let (prog, interp_arg, interp_data) = if let Some(d) = crate::fs::read_file_all(interp_name) {
                (alloc::string::String::from(interp_name), extra.map(|s| alloc::string::String::from(s)), d)
            } else if let Some((prog, data)) = crate::fs::read_file_all("busybox")
                .map(|data| ("busybox", data))
                .or_else(|| {
                    if interp == "/bin/sh" {
                        crate::fs::read_file_all("musl/busybox").map(|data| ("/musl/busybox", data))
                    } else {
                        None
                    }
                })
            {
                // 回退到 busybox 时用 sh 解释（#!/bin/bash 也能跑）
                let arg = match extra {
                    Some(s) => alloc::string::String::from(s),
                    None => alloc::string::String::from("sh"),
                };
                (String::from(prog), Some(arg), data)
            } else {
                return -1;
            };
            let mut new_args: Vec<String> = Vec::new();
            new_args.push(prog);
            if let Some(a) = interp_arg {
                new_args.push(a);
            }
            new_args.push(raw_path.clone());
            // Linux 的 binfmt_script：新 argv = [解释器, 可选参数, 脚本路径, 原 argv[1..]]
            for a in args_vec.iter().skip(1) {
                new_args.push(a.clone());
            }
            process.exec(interp_data.as_slice(), new_args);
            return 0;
        }
        // 不是 ELF 也不是脚本：返回 ENOEXEC(-8) 而不是 EPERM，
        // 这样 busybox sh 的 exec 失败后会按 POSIX 语义把文件当 shell 脚本执行
        if !all_data.starts_with(b"\x7fELF") {
            return -8;
        }
        let argc = args_vec.len();
        process.exec(all_data.as_slice(), args_vec);
        // return argc because cx.x[10] will be covered with it later
        argc as isize
    } else {
        -2 // ENOENT
    }
}

/// If there is not a child process whose pid is same as given, return -1.
/// Else if there is a child process but it is still running, return -2.
pub fn sys_waitpid(pid: isize, exit_code_ptr: *mut i32) -> isize {
    let process = current_process();
    loop {
        let mut inner = process.inner_exclusive_access();
        if !inner
            .children
            .iter()
            .any(|p| pid == -1 || pid as usize == p.getpid())
        {
            return -1;
        }
        let pair = inner.children.iter().enumerate().find(|(_, p)| {
            p.inner_exclusive_access().is_zombie && (pid == -1 || pid as usize == p.getpid())
        });
        if let Some((idx, _)) = pair {
            let child = inner.children.remove(idx);
            let found_pid = child.getpid();
            let status = child.inner_exclusive_access().wait_status;
            let token = inner.memory_set.token();
            drop(inner);
            if !exit_code_ptr.is_null() {
                *translated_refmut(token, exit_code_ptr) = status;
            }
            return found_pid as isize;
        }
        drop(inner);
        // 父进程在内核里忙等，时钟中断不会到来，
        // 必须主动轮询定时器，否则子进程的 sleep 永远醒不了。
        crate::timer::check_timer();
        crate::task::check_kill_current();
        // SIGALRM 到期（LTP / busybox timeout 的超时）：返回 EINTR 让用户态 handler 先跑
        if crate::syscall::signal::alarm_interrupts() {
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

/// uname(2)：填 6 个 65 字节字段
pub fn sys_uname(buf: *mut u8) -> isize {
    let token = current_user_token();
    let fields: [&str; 6] = [
        "Linux",
        "rcore",
        "6.0.0-rcore",
        "#1 rCore-Tutorial",
        "riscv64",
        "localdomain",
    ];
    for (i, f) in fields.iter().enumerate() {
        for j in 0..65usize {
            let c = if j < f.len() { f.as_bytes()[j] } else { 0 };
            *translated_refmut(token, (buf as usize + i * 65 + j) as *mut u8) = c;
        }
    }
    0
}

/// times(2)：返回 100Hz 的时钟滴答数
pub fn sys_times(buf: *mut u8) -> isize {
    let token = current_user_token();
    for i in 0..32usize {
        *translated_refmut(token, (buf as usize + i) as *mut u8) = 0;
    }
    (crate::timer::get_time_ms() / 10) as isize
}

/// kill(2)。注意：**目标不存在（已退出/僵尸进程）时绝不能把信号投给自己**。
/// 之前这里有一个“找不到 pid 就投给当前进程”的兜底（本来是为 pthread_kill 写的），
/// 结果 busybox 脚本 `kill -9 $stale_pid`、lmbench 的 `kill(benchmp_sigalrm_pid, SIGTERM)`
/// 会把调用者自己杀掉 —— 表现为 shell 半路消失 / benchmp 自己收到 SIGTERM。
/// tkill/tgkill 走 sys_thread_kill。
pub fn sys_kill(pid: usize, signal: u32) -> isize {
    // 实时信号/线程取消信号（>=32）用 bitflags 表示不了，直接按成功放过
    if signal >= 32 {
        return 0;
    }
    if signal == 0 {
        return 0;
    }
    let flag = match SignalFlags::from_bits(1u32 << signal) {
        Some(f) => f,
        None => return -22,
    };
    if pid == 0 {
        // kill(0) 语义上是发给自己所在进程组（这里就是自己）
        let this = current_process();
        let mut inner = this.inner_exclusive_access();
        inner.signals |= flag;
        if signal != 2 && signal != 9 && signal != 15 {
            inner.signal_pending |= 1u32 << signal;
        }
        return 0;
    }
    if let Some(process) = pid2process(pid) {
        let mut inner = process.inner_exclusive_access();
        inner.signals |= flag;
        // SIGKILL/SIGTERM/SIGINT 走 check_error 直接杀进程；其它信号置 pending 交给 handler
        if signal != 2 && signal != 9 && signal != 15 {
            inner.signal_pending |= 1u32 << signal;
        }
    }
    // 目标不存在：按成功返回（测试脚本常见“kill 一个已退出的后台进程”）
    0
}

/// tkill / tgkill / rt_tgsigqueueinfo：参数是 tid，不是 pid。
/// 本内核的线程信号统一投递给整个进程（pthread_cancel 依赖这条路径）。
pub fn sys_thread_kill(signal: u32) -> isize {
    if signal >= 32 {
        return 0;
    }
    if signal == 0 {
        return 0;
    }
    let flag = match SignalFlags::from_bits(1u32 << signal) {
        Some(f) => f,
        None => return -22,
    };
    let this = current_process();
    let mut inner = this.inner_exclusive_access();
    inner.signals |= flag;
    if signal != 2 && signal != 9 && signal != 15 {
        inner.signal_pending |= 1u32 << signal;
    }
    0
}

pub fn sys_exit_group(exit_code: i32) -> ! {
    sys_exit(exit_code)
}

/// 单线程进程，tid 用 pid 代替
pub fn sys_set_tid_address(_tidptr: usize) -> isize {
    current_process().getpid() as isize
}

/// gettimeofday(169)：填 struct timeval { long sec; long usec; }，返回 0
pub fn sys_gettimeofday(tv: *mut u8) -> isize {
    let token = current_user_token();
    // 微秒分辨率（基准测试靠它做时间校准）
    let us = crate::timer::get_time_us();
    *translated_refmut(token, tv as *mut usize) = us / 1_000_000;
    *translated_refmut(token, (tv as usize + 8) as *mut usize) = us % 1_000_000;
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
        if file.mapping_size().is_none() {
            return -19;
        }
        VmaBacking::file(file, offset, shared)
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

/// sched_getparam(123)：写一个 4 字节的 sched_priority
pub fn sys_sched_getparam(param: *mut u8) -> isize {
    if param.is_null() {
        return -22;
    }
    let token = crate::task::current_user_token();
    for i in 0..4 {
        *crate::mm::translated_refmut(token, (param as usize + i) as *mut u8) = 0;
    }
    0
}

/// sched_getaffinity(122)：只报告 CPU0
pub fn sys_sched_getaffinity(size: usize, mask: *mut u8) -> isize {
    if mask.is_null() {
        return -22;
    }
    let token = crate::task::current_user_token();
    for i in 0..size {
        let val: u8 = if i == 0 { 1 } else { 0 };
        *crate::mm::translated_refmut(token, (mask as usize + i) as *mut u8) = val;
    }
    size as isize
}

/// pselect6(72)：按真实 fd 就绪状态轮询。
/// - nfds == 0（或三组 fd 集全为 NULL）：按 timeout 睡眠（lmbench 用它做精确延时）；
/// - 有 fd 集：用 File::poll_readable/poll_writable 探测，超时前每 10ms 重扫一次。
///   lmbench 的 benchmp 用 select(0) 轮询管道做父子同步，必须反映真实就绪状态，
///   否则子进程会在 read(尚未写入的 start_signal) 上永久阻塞，父子互等死锁。
fn read_fdset(token: usize, ptr: *mut u8, bytes: usize) -> alloc::vec::Vec<u8> {
    let mut v = alloc::vec![0u8; bytes];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = *translated_ref(token, (ptr as usize + i) as *const u8);
    }
    v
}

fn write_fdset(token: usize, ptr: *mut u8, v: &[u8]) {
    for (i, b) in v.iter().enumerate() {
        *translated_refmut(token, (ptr as usize + i) as *mut u8) = *b;
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
        let sec = unsafe { *translated_ref(token, timeout as *const usize) } as usize;
        let nsec = unsafe { *translated_ref(token, (timeout as usize + 8) as *const usize) } as usize;
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
        *translated_refmut(token, (buf as usize + i) as *mut u8) = 0;
    }
    0
}

/// getrandom(278)：给伪随机字节（测试只当熵源，不要求密码学强度）
pub fn sys_getrandom(buf: *mut u8, len: usize) -> isize {
    if buf.is_null() {
        return -14;
    }
    let token = current_user_token();
    for i in 0..len {
        *translated_refmut(token, (buf as usize + i) as *mut u8) =
            (i as u8).wrapping_mul(73).wrapping_add(41);
    }
    len as isize
}

/// rt_sigtimedwait(137)：简化版——有挂起信号就取走并返回信号号，
/// 否则睡眠等待（每 5ms 查一次），超时返回 -EAGAIN。
/// libc-test 的 runtest 用它等 SIGCHLD（子进程退出），是 5s 看门狗的核心。
pub fn sys_rt_sigtimedwait(set: *const u8, info: *mut u8, timeout: *const u8) -> isize {
    if set.is_null() {
        return -14;
    }
    let token = current_user_token();
    // 内核 sigset_t：bit(n-1) 表示信号 n；SignalFlags：bit(n) 表示信号 n → 左移 1 对齐
    let setbits = (unsafe { *translated_ref(token, set as *const usize) } as u32) << 1;
    let deadline = if timeout.is_null() {
        usize::MAX
    } else {
        let sec = unsafe { *translated_ref(token, timeout as *const usize) } as usize;
        let nsec = unsafe { *translated_ref(token, (timeout as usize + 8) as *const usize) } as usize;
        crate::timer::get_time_ms() + sec * 1000 + (nsec + 999_999) / 1_000_000
    };
    loop {
        let process = current_process();
        let signo = {
            let mut inner = process.inner_exclusive_access();
            let pending = inner.signals.bits() & setbits;
            if pending != 0 {
                let signo = pending.trailing_zeros() as usize;
                inner.signals =
                    SignalFlags::from_bits_truncate(inner.signals.bits() & !(1u32 << signo));
                Some(signo)
            } else {
                None
            }
        };
        if let Some(signo) = signo {
            if !info.is_null() {
                for i in 0..128usize {
                    *translated_refmut(token, (info as usize + i) as *mut u8) = 0;
                }
            }
            return signo as isize;
        }
        if crate::timer::get_time_ms() >= deadline {
            return -11; // EAGAIN
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
    *translated_refmut(token, ptr as *mut usize) = cur;
    *translated_refmut(token, (ptr as usize + 8) as *mut usize) = max;
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
    let cur = unsafe { *translated_ref(token, ptr as *const usize) } as usize;
    if resource == 7 {
        apply_nofile(cur);
    }
    0
}

/// prlimit64(261)：new != NULL 时应用，old != NULL 时回填
pub fn sys_prlimit64(_pid: usize, resource: usize, new: *const u8, old: *mut u8) -> isize {
    let token = current_user_token();
    if !new.is_null() {
        let cur = unsafe { *translated_ref(token, new as *const usize) } as usize;
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
