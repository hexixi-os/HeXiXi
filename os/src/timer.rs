use core::cmp::Ordering;
use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering as AtomicOrdering};

use crate::arch::{read_ticks, set_timer, timer_frequency};
use crate::sync::UPSafeCell;
use crate::task::TaskControlBlock;
use alloc::collections::BinaryHeap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

const TICKS_PER_SEC: usize = 100;
const MSEC_PER_SEC: usize = 1000;
#[cfg(feature = "smp")]
const IDLE_TICKS_MS: usize = 100;
#[cfg(not(feature = "smp"))]
const IDLE_TICKS_MS: usize = 1000;

#[cfg(not(feature = "smp"))]
static NEXT_TRIGGER: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "smp")]
static NEXT_TRIGGER: [AtomicUsize; crate::arch::MAX_HARTS] = {
    const ZERO: AtomicUsize = AtomicUsize::new(0);
    [ZERO; crate::arch::MAX_HARTS]
};

fn local_next_trigger() -> &'static AtomicUsize {
    #[cfg(feature = "smp")]
    {
        &NEXT_TRIGGER[crate::arch::hart_id()]
    }
    #[cfg(not(feature = "smp"))]
    {
        &NEXT_TRIGGER
    }
}

pub fn get_time() -> usize {
    read_ticks()
}

pub fn get_time_ms() -> usize {
    let ticks = read_ticks();
    let freq = timer_frequency();
    (ticks / freq) * MSEC_PER_SEC + (ticks % freq) * MSEC_PER_SEC / freq
}

/// 微秒级时间。基准测试（lmbench/unixbench）需要 us 级分辨率，
/// 原来只给 ms，自适应迭代次数会涨到 1<<27 而卡住。
pub fn get_time_us() -> usize {
    let ticks = read_ticks();
    let freq = timer_frequency();
    (ticks / freq) * 1_000_000 + (ticks % freq) * 1_000_000 / freq
}

static REALTIME_OFFSET_NS: AtomicI64 = AtomicI64::new(0);

/// 纳秒级单调时间（按实际时基换算，避免固定假设 12.5MHz 或乘法溢出）
pub fn get_time_ns() -> usize {
    let ticks = read_ticks();
    let freq = timer_frequency();
    (ticks / freq) * 1_000_000_000 + (ticks % freq) * 1_000_000_000 / freq
}

pub fn get_realtime_ns() -> u64 {
    let mono = get_time_ns() as i128;
    let off = REALTIME_OFFSET_NS.load(AtomicOrdering::Relaxed) as i128;
    (mono + off).max(0) as u64
}

pub fn set_realtime_ns(target_ns: u64) {
    let mono = get_time_ns() as i128;
    let diff = (target_ns as i128 - mono).clamp(i64::MIN as i128, i64::MAX as i128) as i64;
    REALTIME_OFFSET_NS.store(diff, AtomicOrdering::Relaxed);
    arm_within_ms(1);
}

fn next_timer_interval_ticks() -> usize {
    let freq = timer_frequency();
    if crate::task::has_ready_task() || crate::net::has_active_sockets() {
        return freq / TICKS_PER_SEC;
    }
    let mut earliest_ms = TIMERS
        .exclusive_access()
        .peek()
        .map(|timer| timer.expire_ms);
    for candidate in [
        crate::syscall::signal::next_alarm_ms(),
        crate::syscall::futex::next_waiter_ms(),
        crate::syscall::eventfd::next_timerfd_ms(),
    ] {
        if let Some(ms) = candidate {
            earliest_ms = Some(earliest_ms.map_or(ms, |cur| cur.min(ms)));
        }
    }
    let delta_ms = match earliest_ms {
        Some(deadline) => deadline.saturating_sub(get_time_ms()).clamp(1, IDLE_TICKS_MS),
        None => IDLE_TICKS_MS,
    };
    (freq / MSEC_PER_SEC).max(1) * delta_ms
}

pub fn set_next_trigger() {
    let deadline = get_time() + next_timer_interval_ticks();
    local_next_trigger().store(deadline, AtomicOrdering::Relaxed);
    set_timer(deadline);
}

pub fn arm_within_ms(max_ms: usize) {
    let slot = local_next_trigger();
    let current = slot.load(AtomicOrdering::Relaxed);
    if current == 0 {
        return;
    }
    let interval = (timer_frequency() / MSEC_PER_SEC).max(1) * max_ms.max(1);
    let deadline = get_time() + interval;
    if deadline < current {
        slot.store(deadline, AtomicOrdering::Relaxed);
        set_timer(deadline);
    }
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
    {
        let mut timers = TIMERS.exclusive_access();
        timers.push(TimerCondVar { expire_ms, task });
    }
    arm_within_ms(expire_ms.saturating_sub(get_time_ms()).max(1));
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

pub(crate) fn interrupt_sleep(task: &Arc<TaskControlBlock>) {
    let mut woken = false;
    {
        let mut timers = TIMERS.exclusive_access();
        let mut temp = BinaryHeap::with_capacity(timers.len());
        for condvar in timers.drain() {
            if Arc::ptr_eq(&condvar.task, task) {
                woken = true;
            } else {
                temp.push(condvar);
            }
        }
        *timers = temp;
    }
    if woken {
        crate::task::wake_blocked_task(Arc::clone(task));
    }
}

pub fn check_timer() {
    crate::net::poll();
    crate::syscall::signal::check_alarm();
    crate::syscall::futex::check_waiters();
    crate::syscall::eventfd::check_timerfds();
    let current_ms = get_time_ms();
    let mut expired = Vec::new();
    {
        let mut timers = TIMERS.exclusive_access();
        while let Some(timer) = timers.peek() {
            if timer.expire_ms <= current_ms {
                expired.push(timers.pop().unwrap().task);
            } else {
                break;
            }
        }
    }
    for task in expired {
        crate::task::WaitQueue::cancel(&task);
        crate::task::wake_blocked_task(task);
    }
}
