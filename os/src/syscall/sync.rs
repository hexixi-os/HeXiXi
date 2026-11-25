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

/// Linux nanosleep(101)：参数是 struct timespec { i64 sec; i64 nsec; }
/// 兼容旧写法：参数很小（< 4096）时按毫秒处理
pub fn sys_nanosleep(req: usize, _rem: usize) -> isize {
    let ms = if req < 4096 {
        req
    } else {
        let token = crate::task::current_user_token();
        let sec = crate::mm::read_user(token, req as *const usize) as i64;
        let nsec = crate::mm::read_user(token, (req + 8) as *const usize) as i64;
        (sec * 1000 + nsec / 1_000_000) as usize
    };
    sys_sleep(ms);
    0
}

/// clock_gettime(113)：只填 timespec 的秒/纳秒
pub fn sys_clock_gettime(tp: *mut u8) -> isize {
    let token = crate::task::current_user_token();
    // 用 SBI time CSR（12.5MHz）换算，纳秒精度
    let ns = crate::timer::get_time_ns();
    let sec = ns / 1_000_000_000;
    let nsec = ns % 1_000_000_000;
    crate::mm::write_user(token, tp as *mut usize, sec);
    crate::mm::write_user(token, (tp as usize + 8) as *mut usize, nsec);
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

/// clock_nanosleep(115)：用轮询定时器实现
pub fn sys_clock_nanosleep(req: *const u8) -> isize {
    if req.is_null() {
        return -22;
    }
    let token = crate::task::current_user_token();
    let sec = crate::mm::read_user(token, req as *const u64) as usize;
    let nsec = crate::mm::read_user(token, (req as usize + 8) as *const u64) as usize;
    let deadline = crate::timer::get_time_ms() + sec * 1000 + (nsec + 999_999) / 1_000_000;
    while crate::timer::get_time_ms() < deadline {
        crate::task::suspend_current_and_run_next();
    }
    0
}
