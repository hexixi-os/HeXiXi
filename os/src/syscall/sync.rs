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

fn sleep_until_ms(expire_ms: usize, rem: usize) -> isize {
    loop {
        crate::task::check_kill_current();
        let now = get_time_ms();
        if now >= expire_ms {
            return 0;
        }
        if super::signal::current_wait_interrupted() {
            if rem != 0 {
                let left_ms = expire_ms.saturating_sub(get_time_ms());
                let token = crate::task::current_user_token();
                let _ = crate::mm::try_write_user(token, rem as *mut u64, (left_ms / 1000) as u64);
                let _ = crate::mm::try_write_user(
                    token,
                    (rem + 8) as *mut u64,
                    ((left_ms % 1000) * 1_000_000) as u64,
                );
            }
            return -4;
        }
        let task = current_task().unwrap();
        add_timer(expire_ms, task);
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
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return -22;
    }
    let ms = (sec as usize)
        .saturating_mul(1000)
        .saturating_add((nsec as usize + 999_999) / 1_000_000);
    sleep_until_ms(get_time_ms().saturating_add(ms), rem)
}

/// clock_gettime(113)：只填 timespec 的秒/纳秒
pub fn sys_clock_gettime(tp: *mut u8) -> isize {
    let token = crate::task::current_user_token();
    let ns = crate::timer::get_time_ns();
    let pair = [ns / 1_000_000_000, ns % 1_000_000_000];
    if crate::mm::try_write_user(token, tp as *mut [usize; 2], pair).is_none() {
        crate::mm::write_user(token, tp as *mut usize, pair[0]);
        crate::mm::write_user(token, (tp as usize + 8) as *mut usize, pair[1]);
    }
    0
}

/// clock_getres(114)：报告 1ns 分辨率
pub fn sys_clock_getres(ts: *mut u8) -> isize {
    if ts.is_null() {
        return 0;
    }
    let token = crate::task::current_user_token();
    for i in 0..8 {
        crate::mm::write_user(token, (ts as usize + i) as *mut u8, 0);
    }
    crate::mm::write_user(token, (ts as usize + 8) as *mut u64, 1);
    0
}

/// clock_nanosleep(115)：支持相对睡眠与 TIMER_ABSTIME，可被信号唤醒中断
pub fn sys_clock_nanosleep(flags: usize, req: *const u8, rem: usize) -> isize {
    const TIMER_ABSTIME: usize = 1;
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
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return -22;
    }
    let target_ms = (sec as usize)
        .saturating_mul(1000)
        .saturating_add((nsec as usize + 999_999) / 1_000_000);
    let (deadline, out_rem) = if flags & TIMER_ABSTIME != 0 {
        (target_ms, 0)
    } else {
        (get_time_ms().saturating_add(target_ms), rem)
    };
    sleep_until_ms(deadline, out_rem)
}
