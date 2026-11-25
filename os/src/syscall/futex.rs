use crate::mm::try_read_user;
use crate::sync::UPSafeCell;
use crate::task::{
    add_task, block_current_and_run_next, current_task, current_user_token, TaskControlBlock,
};
use alloc::{collections::VecDeque, sync::Arc};
use lazy_static::lazy_static;

type Key = (usize, usize);

struct Waiter {
    key: Key,
    bits: u32,
    deadline: Option<usize>,
    task: Arc<TaskControlBlock>,
}

lazy_static! {
    static ref WAITERS: UPSafeCell<VecDeque<Waiter>> = unsafe { UPSafeCell::new(VecDeque::new()) };
}

fn resume(waiter: Waiter, result: isize) {
    waiter.task.inner_exclusive_access().futex_result = result;
    add_task(waiter.task);
}

fn wake_matching(waiters: &mut VecDeque<Waiter>, key: Key, count: usize, bits: u32) -> usize {
    let mut woken = 0;
    let mut index = 0;
    while index < waiters.len() && woken < count {
        let waiter = &waiters[index];
        if waiter.key == key && waiter.bits & bits != 0 {
            resume(waiters.remove(index).unwrap(), 0);
            woken += 1;
        } else {
            index += 1;
        }
    }
    woken
}

pub(crate) fn wake_one(token: usize, address: usize) {
    wake_matching(
        &mut WAITERS.exclusive_access(),
        (token, address),
        1,
        u32::MAX,
    );
}

pub(crate) fn remove_task(task: &Arc<TaskControlBlock>) {
    WAITERS
        .exclusive_access()
        .retain(|waiter| !Arc::ptr_eq(&waiter.task, task));
}

pub(crate) fn check_waiters() {
    let now = crate::timer::get_time_ns();
    let mut waiters = WAITERS.exclusive_access();
    let mut index = 0;
    while index < waiters.len() {
        let waiter = &waiters[index];
        let interrupted = super::signal::interrupts_wait(&waiter.task, now / 1_000_000);
        if interrupted {
            resume(waiters.remove(index).unwrap(), -4);
        } else if waiter.deadline.map_or(false, |deadline| now >= deadline) {
            resume(waiters.remove(index).unwrap(), -110);
        } else {
            index += 1;
        }
    }
}

fn deadline(token: usize, pointer: usize, relative: bool) -> Result<Option<usize>, isize> {
    if pointer == 0 {
        return Ok(None);
    }
    let seconds = try_read_user(token, pointer as *const i64).ok_or(-14isize)?;
    let nanos_pointer = pointer.checked_add(8).ok_or(-14isize)?;
    let nanos = try_read_user(token, nanos_pointer as *const i64).ok_or(-14isize)?;
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(-22);
    }
    let duration = (seconds as usize)
        .saturating_mul(1_000_000_000)
        .saturating_add(nanos as usize);
    Ok(Some(if relative {
        crate::timer::get_time_ns().saturating_add(duration)
    } else {
        duration
    }))
}

pub(super) fn sys_futex([address, op, value, arg4, address2, arg6]: [usize; 6]) -> isize {
    const WAIT: usize = 0;
    const WAKE: usize = 1;
    const REQUEUE: usize = 3;
    const CMP_REQUEUE: usize = 4;
    const WAIT_BITSET: usize = 9;
    const WAKE_BITSET: usize = 10;
    const CLOCK_REALTIME: usize = 256;
    let command = op & !384; // Strip FUTEX_PRIVATE_FLAG and FUTEX_CLOCK_REALTIME.
    if op & CLOCK_REALTIME != 0 && command != WAIT_BITSET {
        return -38;
    }
    if address % 4 != 0 {
        return -22;
    }
    let token = current_user_token();
    let key = (token, address);
    let bits = if matches!(command, WAIT_BITSET | WAKE_BITSET) {
        arg6 as u32
    } else {
        u32::MAX
    };
    if bits == 0 {
        return -22;
    }
    match command {
        WAIT | WAIT_BITSET => {
            let word = match try_read_user(token, address as *const u32) {
                Some(word) => word,
                None => return -14,
            };
            if word != value as u32 {
                return -11;
            }
            let deadline = match deadline(token, arg4, command == WAIT) {
                Ok(deadline) => deadline,
                Err(errno) => return errno,
            };
            if deadline.map_or(false, |end| end <= crate::timer::get_time_ns()) {
                return -110;
            }
            if super::signal::alarm_interrupts() {
                return -4;
            }
            // On one hart with interrupts masked in the kernel, no task can
            // modify the word or wake this waiter before it blocks.
            WAITERS.exclusive_access().push_back(Waiter {
                key,
                bits,
                deadline,
                task: current_task().unwrap(),
            });
            block_current_and_run_next();
            current_task()
                .unwrap()
                .inner_exclusive_access()
                .futex_result
        }
        WAKE | WAKE_BITSET | REQUEUE | CMP_REQUEUE => {
            let count = value as i32;
            let requeue = matches!(command, REQUEUE | CMP_REQUEUE);
            if count < 0
                || (requeue && ((arg4 as i32) < 0 || address2 % 4 != 0 || address == address2))
            {
                return -22;
            }
            let word = match try_read_user(token, address as *const u32) {
                Some(word) => word,
                None => return -14,
            };
            if command == CMP_REQUEUE && word != arg6 as u32 {
                return -11;
            }
            if requeue && try_read_user(token, address2 as *const u32).is_none() {
                return -14;
            }
            let mut waiters = WAITERS.exclusive_access();
            let woken = wake_matching(&mut waiters, key, count as usize, bits);
            let mut moved = 0;
            if requeue {
                for waiter in waiters
                    .iter_mut()
                    .filter(|waiter| waiter.key == key)
                    .take(arg4 as u32 as usize)
                {
                    waiter.key = (token, address2);
                    moved += 1;
                }
            }
            (woken + moved) as isize
        }
        _ => -38,
    }
}
