use crate::sync::{Condvar, Mutex, MutexBlocking, MutexSpin, Semaphore};
use crate::task::{block_current_and_run_next, current_process, current_task};
use crate::timer::{add_timer, get_time_ms};
use alloc::sync::Arc;

pub fn sys_sleep(ms: usize) -> isize {
    let expire_ms = get_time_ms() + ms;
    let task = current_task().unwrap();
    add_timer(expire_ms, task);
    block_current_and_run_next();
    0
}

pub fn sys_mutex_create(blocking: bool) -> isize {
    let process = current_process();
    let mutex: Option<Arc<dyn Mutex>> = if !blocking {
        Some(Arc::new(MutexSpin::new()))
    } else {
        Some(Arc::new(MutexBlocking::new()))
    };
    let mut process_inner = process.inner_exclusive_access();
    if let Some(id) = process_inner
        .mutex_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.mutex_list[id] = mutex;
        id as isize
    } else {
        process_inner.mutex_list.push(mutex);
        process_inner.mutex_list.len() as isize - 1
    }
}

pub fn sys_mutex_lock(mutex_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    mutex.lock();
    0
}

pub fn sys_mutex_unlock(mutex_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    drop(process);
    mutex.unlock();
    0
}

pub fn sys_semaphore_create(res_count: usize) -> isize {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .semaphore_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.semaphore_list[id] = Some(Arc::new(Semaphore::new(res_count)));
        id
    } else {
        process_inner
            .semaphore_list
            .push(Some(Arc::new(Semaphore::new(res_count))));
        process_inner.semaphore_list.len() - 1
    };
    id as isize
}

pub fn sys_semaphore_up(sem_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.up();
    0
}

pub fn sys_semaphore_down(sem_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let sem = Arc::clone(process_inner.semaphore_list[sem_id].as_ref().unwrap());
    drop(process_inner);
    sem.down();
    0
}

pub fn sys_condvar_create(_arg: usize) -> isize {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let id = if let Some(id) = process_inner
        .condvar_list
        .iter()
        .enumerate()
        .find(|(_, item)| item.is_none())
        .map(|(id, _)| id)
    {
        process_inner.condvar_list[id] = Some(Arc::new(Condvar::new()));
        id
    } else {
        process_inner
            .condvar_list
            .push(Some(Arc::new(Condvar::new())));
        process_inner.condvar_list.len() - 1
    };
    id as isize
}

pub fn sys_condvar_signal(condvar_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    drop(process_inner);
    condvar.signal();
    0
}

pub fn sys_condvar_wait(condvar_id: usize, mutex_id: usize) -> isize {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let condvar = Arc::clone(process_inner.condvar_list[condvar_id].as_ref().unwrap());
    let mutex = Arc::clone(process_inner.mutex_list[mutex_id].as_ref().unwrap());
    drop(process_inner);
    condvar.wait(mutex);
    0
}

fn sleep_until_ns(expire_ns: usize, rem: usize) -> isize {
    loop {
        crate::task::check_kill_current();
        let now_ns = crate::timer::get_time_ns();
        if now_ns >= expire_ns {
            return 0;
        }
        if super::signal::current_wait_interrupted() {
            if rem != 0 {
                let left_ns = expire_ns.saturating_sub(crate::timer::get_time_ns());
                let token = crate::task::current_user_token();
                let _ = crate::mm::try_write_user(token, rem as *mut u64, (left_ns / 1_000_000_000) as u64);
                let _ = crate::mm::try_write_user(
                    token,
                    (rem + 8) as *mut u64,
                    (left_ns % 1_000_000_000) as u64,
                );
            }
            return -4;
        }
        let wake_ms = (expire_ns + 999_999) / 1_000_000;
        let task = current_task().unwrap();
        add_timer(wake_ms.max(get_time_ms() + 1), task);
        block_current_and_run_next();
    }
}

/// Linux nanosleep(101)：参数是 struct timespec { i64 sec; i64 nsec; }
/// 兼容旧写法：参数很小（< 4096）时按毫秒处理
pub fn sys_nanosleep(req: usize, rem: usize) -> isize {
    if (1..4096).contains(&req) {
        sys_sleep(req);
        return 0;
    }
    let token = crate::task::current_user_token();
    let Some(sec) = crate::mm::try_read_user(token, req as *const i64) else {
        return -14;
    };
    let Some(nsec) = req.checked_add(8).and_then(|p| crate::mm::try_read_user(token, p as *const i64)) else {
        return -14;
    };
    if sec < 0 || sec >= i64::MAX / 1_000_000_000 || !(0..1_000_000_000).contains(&nsec) {
        return -22;
    }
    let dur_ns = (sec as usize)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as usize);
    sleep_until_ns(crate::timer::get_time_ns().saturating_add(dur_ns), rem)
}

fn resolve_clock_ns(clockid: i32) -> Result<u64, isize> {
    match clockid {
        0 | 5 | 8 | 11 => Ok(crate::timer::get_realtime_ns()),
        1 | 4 | 6 | 7 | 9 => Ok(crate::timer::get_time_ns() as u64),
        2 | 3 => Ok(crate::timer::get_time_ns() as u64),
        _ if clockid < 0 => {
            // Dynamic process/thread CPU clock encoding: (~clockid) >> 3 == pid/tid, low 3 bits < 6
            let low = clockid & 7;
            if low >= 6 {
                return Err(-22);
            }
            let id = ((!clockid) >> 3) as usize;
            if id != 0 {
                let per_thread = (low & 4) != 0;
                let exists = if per_thread {
                    crate::task::tid2task(id).is_some()
                } else {
                    crate::task::pid2process(id).map_or(false, |p| !p.inner_exclusive_access().is_zombie)
                };
                if !exists {
                    return Err(-22);
                }
            }
            Ok(crate::timer::get_time_ns() as u64)
        }
        _ => Err(-22),
    }
}

/// clock_settime(112)：支持 CLOCK_REALTIME(0)
pub fn sys_clock_settime(clockid: i32, tp: *const u8) -> isize {
    if clockid != 0 {
        return -22;
    }
    if tp.is_null() {
        return -14;
    }
    let token = crate::task::current_user_token();
    let Some(sec) = crate::mm::try_read_user(token, tp as *const i64) else {
        return -14;
    };
    let Some(nsec) = (tp as usize)
        .checked_add(8)
        .and_then(|p| crate::mm::try_read_user(token, p as *const i64))
    else {
        return -14;
    };
    if sec < 0 || sec >= i64::MAX / 1_000_000_000 || !(0..1_000_000_000).contains(&nsec) {
        return -22;
    }
    if current_process().inner_exclusive_access().euid != 0 {
        return -1; // EPERM
    }
    let target_ns = (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64);
    crate::timer::set_realtime_ns(target_ns);
    0
}

/// clock_gettime(113)：填 timespec 的秒/纳秒
pub fn sys_clock_gettime(clockid: i32, tp: *mut u8) -> isize {
    let ns = match resolve_clock_ns(clockid) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if tp.is_null() {
        return -14;
    }
    let token = crate::task::current_user_token();
    let Some(bufs) =
        crate::mm::translated_byte_buffer(token, tp, 16, crate::mm::MapPermission::W)
    else {
        return -14;
    };
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&((ns / 1_000_000_000) as i64).to_ne_bytes());
    raw[8..16].copy_from_slice(&((ns % 1_000_000_000) as i64).to_ne_bytes());
    crate::mm::UserBuffer::new(bufs).copy_from(&raw);
    0
}

/// clock_getres(114)：报告 1ns 分辨率
pub fn sys_clock_getres(clockid: i32, ts: *mut u8) -> isize {
    if let Err(e) = resolve_clock_ns(clockid) {
        return e;
    }
    if ts.is_null() {
        return 0;
    }
    let token = crate::task::current_user_token();
    let Some(bufs) =
        crate::mm::translated_byte_buffer(token, ts, 16, crate::mm::MapPermission::W)
    else {
        return -14;
    };
    let mut raw = [0u8; 16];
    raw[8..16].copy_from_slice(&1i64.to_ne_bytes());
    crate::mm::UserBuffer::new(bufs).copy_from(&raw);
    0
}

/// clock_nanosleep(115)：支持相对睡眠与 TIMER_ABSTIME，可被信号唤醒中断
pub fn sys_clock_nanosleep(clockid: i32, flags: usize, req: *const u8, rem: usize) -> isize {
    const TIMER_ABSTIME: usize = 1;
    // POSIX/Linux: CLOCK_THREAD_CPUTIME_ID(3) returns EINVAL; COARSE/MONOTONIC_RAW return EOPNOTSUPP(-95)
    match clockid {
        0 | 1 | 2 | 7 | 8 | 9 | 11 => {}
        4 | 5 | 6 => return -95,
        _ => return -22,
    }
    if flags & !TIMER_ABSTIME != 0 {
        return -22;
    }
    if req.is_null() {
        return -14;
    }
    let token = crate::task::current_user_token();
    let Some(sec) = crate::mm::try_read_user(token, req as *const i64) else {
        return -14;
    };
    let Some(nsec) = (req as usize)
        .checked_add(8)
        .and_then(|p| crate::mm::try_read_user(token, p as *const i64))
    else {
        return -14;
    };
    if sec < 0 || sec >= i64::MAX / 1_000_000_000 || !(0..1_000_000_000).contains(&nsec) {
        return -22;
    }
    let req_ns = (sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(nsec as u64);
    let (deadline_ns, out_rem) = if flags & TIMER_ABSTIME != 0 {
        let now_clock = if matches!(clockid, 0 | 8 | 11) {
            crate::timer::get_realtime_ns()
        } else {
            crate::timer::get_time_ns() as u64
        };
        if req_ns <= now_clock {
            return 0;
        }
        let delta_ns = (req_ns - now_clock) as usize;
        (crate::timer::get_time_ns().saturating_add(delta_ns), 0)
    } else {
        (crate::timer::get_time_ns().saturating_add(req_ns as usize), rem)
    };
    sleep_until_ns(deadline_ns, out_rem)
}
