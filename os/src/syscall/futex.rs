use crate::mm::{futex_phys_addr, try_read_user, UserValue, VirtAddr};
use crate::sync::UPSafeCell;
use crate::task::{
    block_current_and_run_next, current_process, current_task, current_user_token,
    TaskControlBlock,
};
use alloc::{collections::VecDeque, sync::Arc};
use lazy_static::lazy_static;

#[derive(Copy, Clone, Eq, PartialEq)]
enum Key {
    Private { token: usize, address: usize },
    Shared { phys: usize },
}

struct Waiter {
    key: Key,
    bits: u32,
    deadline: Option<usize>,
    task: Arc<TaskControlBlock>,
}

lazy_static! {
    static ref WAITERS: UPSafeCell<VecDeque<Waiter>> = unsafe { UPSafeCell::new(VecDeque::new()) };
}

fn is_shared_mapping(token: usize, address: usize) -> bool {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    inner.memory_set.token() == token && inner.memory_set.is_shared_at(VirtAddr::from(address).floor())
}

fn resolve_futex_word(token: usize, address: usize, private: bool) -> Option<(Key, u32)> {
    let pa = futex_phys_addr(token, address)?;
    let word = unsafe { u32::load(pa.kernel_address() as *const u32) };
    let key = if !private && is_shared_mapping(token, address) {
        Key::Shared { phys: pa.0 }
    } else {
        Key::Private { token, address }
    };
    Some((key, word))
}

fn resume(waiter: Waiter, result: isize) {
    waiter.task.inner_exclusive_access().futex_result = result;
    crate::task::wake_blocked_task(waiter.task);
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
    let shared_key = (!is_shared_mapping(token, address))
        .then_some(())
        .map_or_else(
            || futex_phys_addr(token, address).map(|pa| Key::Shared { phys: pa.0 }),
            |()| None,
        );
    let private_key = Key::Private { token, address };
    let mut waiters = WAITERS.exclusive_access();
    if let Some(shared) = shared_key {
        if wake_matching(&mut waiters, shared, 1, u32::MAX) > 0 {
            return;
        }
    }
    wake_matching(&mut waiters, private_key, 1, u32::MAX);
}

pub(crate) fn remove_task(task: &Arc<TaskControlBlock>) {
    WAITERS
        .exclusive_access()
        .retain(|waiter| !Arc::ptr_eq(&waiter.task, task));
}

pub(crate) fn sys_set_robust_list(head: usize, len: usize) -> isize {
    if len != 3 * core::mem::size_of::<usize>() {
        return -22;
    }
    current_task().unwrap().inner_exclusive_access().robust_list = head;
    0
}

pub(crate) fn sys_get_robust_list(pid: i32, head_ptr: usize, len_ptr: usize) -> isize {
    use crate::mm::try_write_user;
    if pid < 0 {
        return -22;
    }
    let caller = current_process();
    let (caller_pid, caller_euid) = {
        let inner = caller.inner_exclusive_access();
        (caller.getpid(), inner.euid)
    };
    let task = if pid == 0 {
        current_task().unwrap()
    } else if let Some(task) = crate::task::tid2task(pid as usize) {
        if task.inner_exclusive_access().exit_code.is_some() {
            return -3;
        }
        task
    } else if let Some(proc) = crate::task::pid2process(pid as usize) {
        let inner = proc.inner_exclusive_access();
        if inner.is_zombie {
            return -3;
        }
        inner.get_task(0)
    } else {
        return -3;
    };
    if let Some(target_proc) = task.process.upgrade() {
        let target_inner = target_proc.inner_exclusive_access();
        if caller_euid != 0
            && target_proc.getpid() != caller_pid
            && caller_euid != target_inner.uid
            && caller_euid != target_inner.euid
        {
            return -1;
        }
    } else {
        return -3;
    }
    let head = task.inner_exclusive_access().robust_list;
    let token = current_user_token();
    let size = 3 * core::mem::size_of::<usize>();
    if head_ptr == 0
        || len_ptr == 0
        || try_write_user(token, head_ptr as *mut usize, head).is_none()
        || try_write_user(token, len_ptr as *mut usize, size).is_none()
    {
        return -14;
    }
    0
}

pub(crate) fn exit_robust_list(task: &Arc<TaskControlBlock>, token: usize) {
    const FUTEX_TID_MASK: u32 = 0x3fff_ffff;
    const FUTEX_OWNER_DIED: u32 = 0x4000_0000;
    const FUTEX_WAITERS: u32 = 0x8000_0000;
    const ROBUST_LIST_LIMIT: usize = 1024;

    let head = {
        let mut inner = task.inner_exclusive_access();
        core::mem::replace(&mut inner.robust_list, 0)
    };
    if head == 0 {
        return;
    }
    let tid = task.linux_tid() as u32;
    let word_size = core::mem::size_of::<usize>();
    let Some(mut entry) = try_read_user(token, head as *const usize) else { return };
    let Some(futex_offset) = head
        .checked_add(word_size)
        .and_then(|p| try_read_user(token, p as *const usize).map(|v| v as isize))
    else {
        return;
    };
    let pending = head
        .checked_add(2 * word_size)
        .and_then(|p| try_read_user(token, p as *const usize))
        .unwrap_or(0);

    let handle_entry = |node: usize| {
        let futex_addr = (node as isize).wrapping_add(futex_offset) as usize;
        if futex_addr % 4 != 0 {
            return;
        }
        let Some(pa) = futex_phys_addr(token, futex_addr) else { return };
        let ptr = pa.kernel_address() as *mut u32;
        let current = unsafe { u32::load(ptr) };
        if (current & FUTEX_TID_MASK) == tid {
            let next_val = (current & FUTEX_WAITERS) | FUTEX_OWNER_DIED;
            let _ = crate::mm::try_write_user(token, futex_addr as *mut u32, next_val);
            if current & FUTEX_WAITERS != 0 {
                wake_one(token, futex_addr);
            }
        }
    };

    let mut count = 0;
    while entry != 0 && entry != head && count < ROBUST_LIST_LIMIT {
        let next = try_read_user(token, entry as *const usize);
        if entry != pending {
            handle_entry(entry);
        }
        let Some(next_entry) = next else { break };
        entry = next_entry;
        count += 1;
    }
    if pending != 0 && pending != head {
        handle_entry(pending);
    }
}

pub(crate) fn interrupt_waiter(task: &Arc<TaskControlBlock>) {
    let mut waiters = WAITERS.exclusive_access();
    if let Some(index) = waiters.iter().position(|waiter| Arc::ptr_eq(&waiter.task, task)) {
        resume(waiters.remove(index).unwrap(), -4);
    }
}

pub(crate) fn next_waiter_ms() -> Option<usize> {
    WAITERS
        .exclusive_access()
        .iter()
        .filter_map(|w| w.deadline.map(|ns| (ns + 999_999) / 1_000_000))
        .min()
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
    const PRIVATE_FLAG: usize = 128;
    const CLOCK_REALTIME: usize = 256;
    let private = op & PRIVATE_FLAG != 0;
    let command = op & !(PRIVATE_FLAG | CLOCK_REALTIME);
    if op & CLOCK_REALTIME != 0 && command != WAIT_BITSET {
        return -38;
    }
    if address % 4 != 0 {
        return -22;
    }
    let token = current_user_token();
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
            let (key, word) = match resolve_futex_word(token, address, private) {
                Some(resolved) => resolved,
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
            if super::signal::current_wait_interrupted() {
                return -4;
            }
            // Kernel entry is serialized and interrupts stay masked, so no wake can slip in before blocking.
            WAITERS.exclusive_access().push_back(Waiter {
                key,
                bits,
                deadline,
                task: current_task().unwrap(),
            });
            if let Some(end) = deadline {
                let delta_ms = end.saturating_sub(crate::timer::get_time_ns()) / 1_000_000;
                crate::timer::arm_within_ms(delta_ms.max(1));
            }
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
            let (key, word) = match resolve_futex_word(token, address, private) {
                Some(resolved) => resolved,
                None => return -14,
            };
            if command == CMP_REQUEUE && word != arg6 as u32 {
                return -11;
            }
            let target_key = if requeue {
                match resolve_futex_word(token, address2, private) {
                    Some((key2, _)) if key2 == key => return -22,
                    Some((key2, _)) => Some(key2),
                    None => return -14,
                }
            } else {
                None
            };
            let mut waiters = WAITERS.exclusive_access();
            let woken = wake_matching(&mut waiters, key, count as usize, bits);
            let mut moved = 0;
            if let Some(key2) = target_key {
                for waiter in waiters
                    .iter_mut()
                    .filter(|waiter| waiter.key == key)
                    .take(arg4 as u32 as usize)
                {
                    waiter.key = key2;
                    moved += 1;
                }
            }
            (woken + moved) as isize
        }
        _ => -38,
    }
}
