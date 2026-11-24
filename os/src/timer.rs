use core::cmp::Ordering;

use crate::arch::{read_ticks, set_timer, timer_frequency};
use crate::sync::UPSafeCell;
use crate::task::{add_task, TaskControlBlock};
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use lazy_static::*;

const TICKS_PER_SEC: usize = 100;
const MSEC_PER_SEC: usize = 1000;

pub fn get_time() -> usize {
    read_ticks()
}

pub fn get_time_ms() -> usize {
    read_ticks() / (timer_frequency() / MSEC_PER_SEC)
}

/// 微秒级时间。基准测试（lmbench/unixbench）需要 us 级分辨率，
/// 原来只给 ms，自适应迭代次数会涨到 1<<27 而卡住。
pub fn get_time_us() -> usize {
    read_ticks() * 1_000_000 / timer_frequency()
}

/// 纳秒级时间（12.5MHz 时基 → 80ns 精度）
pub fn get_time_ns() -> usize {
    read_ticks() * (1_000_000_000 / timer_frequency())
}

pub fn set_next_trigger() {
    set_timer(get_time() + timer_frequency() / TICKS_PER_SEC);
}

pub struct TimerCondVar {
    pub expire_ms: usize,
    pub task: Arc<TaskControlBlock>,
}

impl PartialEq for TimerCondVar {
    fn eq(&self, other: &Self) -> bool {
        self.expire_ms == other.expire_ms
    }
}
impl Eq for TimerCondVar {}
impl PartialOrd for TimerCondVar {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let a = -(self.expire_ms as isize);
        let b = -(other.expire_ms as isize);
        Some(a.cmp(&b))
    }
}

impl Ord for TimerCondVar {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap()
    }
}

lazy_static! {
    static ref TIMERS: UPSafeCell<BinaryHeap<TimerCondVar>> =
        unsafe { UPSafeCell::new(BinaryHeap::<TimerCondVar>::new()) };
}

pub fn add_timer(expire_ms: usize, task: Arc<TaskControlBlock>) {
    let mut timers = TIMERS.exclusive_access();
    timers.push(TimerCondVar { expire_ms, task });
}

pub fn remove_timer(task: Arc<TaskControlBlock>) {
    let mut timers = TIMERS.exclusive_access();
    let mut temp = BinaryHeap::<TimerCondVar>::new();
    for condvar in timers.drain() {
        if Arc::as_ptr(&task) != Arc::as_ptr(&condvar.task) {
            temp.push(condvar); 
        }
    }
    timers.clear();
    timers.append(&mut temp);
}

pub fn check_timer() {
    crate::syscall::futex::check_waiters();
    let current_ms = get_time_ms();
    let mut timers = TIMERS.exclusive_access();
    while let Some(timer) = timers.peek() {
        if timer.expire_ms <= current_ms {
            add_task(Arc::clone(&timer.task));
            timers.pop();
        } else {
            break;
        }
    }
}
