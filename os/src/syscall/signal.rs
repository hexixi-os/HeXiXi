use crate::mm::{translated_ref, try_translated_ref, try_translated_refmut};
use crate::mm::{MapPermission, MemorySet, VirtAddr};
use crate::task::{current_process, current_task, current_user_token, pid2process};
use alloc::sync::Arc;

/// 可执行的信号返回 stub 页（li a7,139; ecall; j .）
pub const SIGSTUB_VA: usize = 0x00FF_E000;
/// 保存被打断的 trap context 的页
pub const SIGCTX_VA: usize = 0x00FF_F000;
/// 信号编号：SIGALRM（定时器）
pub const SIGALRM: usize = 14;
/// 信号编号：SIGCHLD（子进程退出）
pub const SIGCHLD: usize = 17;
/// 信号编号：SIGSEGV（非法访问）
pub const SIGSEGV: usize = 11;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SignalAction {
    pub handler: usize,
    flags: usize,
    mask: u64,
}

fn signal_bit(signo: usize) -> u64 {
    1u64 << (signo - 1)
}

const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

/// 在用户地址空间建立信号所需的两页（exec 时调用）
pub fn setup_signal_pages(memory_set: &mut MemorySet) {
    if memory_set.translate(SIGSTUB_VA.into()).is_none() {
        memory_set.insert_data_page(
            VirtAddr::from(SIGSTUB_VA),
            MapPermission::U | MapPermission::R | MapPermission::X,
            &crate::arch::SIGRETURN_CODE,
        );
    }
    if memory_set.translate(SIGCTX_VA.into()).is_none() {
        memory_set.insert_data_page(
            VirtAddr::from(SIGCTX_VA),
            MapPermission::U | MapPermission::R | MapPermission::W,
            &[],
        );
    }
}

/// setitimer(103)：只关心 ITIMER_REAL 的 it_value
pub fn sys_setitimer(which: usize, new: *const u8, _old: *mut u8) -> isize {
    if which != 0 || new.is_null() {
        return 0;
    }
    let token = current_user_token();
    // struct itimerval { it_interval{sec,usec}, it_value{sec,usec} }：取 it_value
    let sec = *translated_ref(token, (new as usize + 16) as *const u64) as usize;
    let usec = *translated_ref(token, (new as usize + 24) as *const u64) as usize;
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if sec == 0 && usec == 0 {
        inner.alarm_deadline = 0;
    } else {
        inner.alarm_deadline = crate::timer::get_time_ms() + sec * 1000 + usec / 1000;
    }
    0
}

pub fn sys_rt_sigaction(signo: usize, act: usize, old: usize, size: usize) -> isize {
    if !(1..=64).contains(&signo) || size != 8 || (act != 0 && matches!(signo, 9 | 19)) {
        return -22;
    }
    let token = current_user_token();
    let action = if act == 0 {
        None
    } else {
        match try_translated_ref(token, act as *const SignalAction) {
            Some(action) => Some(*action),
            None => return -14,
        }
    };
    let process = current_process();
    let previous = process.inner_exclusive_access().signal_actions[signo - 1];
    if old != 0 {
        match try_translated_refmut(token, old as *mut SignalAction) {
            Some(output) => *output = previous,
            None => return -14,
        }
    }
    if let Some(action) = action {
        process.inner_exclusive_access().signal_actions[signo - 1] = action;
    }
    0
}

pub fn sys_rt_sigprocmask(how: usize, set: usize, old: usize, size: usize) -> isize {
    if size != 8 || (set != 0 && how > 2) {
        return -22;
    }
    let token = current_user_token();
    let task = current_task().unwrap();
    let previous = task.inner_exclusive_access().signal_mask;
    let requested = if set == 0 {
        previous
    } else {
        match try_translated_ref(token, set as *const u64) {
            Some(mask) => match how {
                0 => previous | mask,
                1 => previous & !mask,
                _ => *mask,
            },
            None => return -14,
        }
    };
    if old != 0 {
        match try_translated_refmut(token, old as *mut u64) {
            Some(output) => *output = previous,
            None => return -14,
        }
    }
    task.inner_exclusive_access().signal_mask = requested & !UNBLOCKABLE;
    0
}

pub fn sys_rt_sigreturn() -> isize {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let (context, mask) = match inner.signal_context.take() {
        Some(saved) => saved,
        None => return -22,
    };
    *inner.get_trap_cx() = context;
    inner.signal_mask = mask;
    0
}

pub fn sys_thread_kill(tgid: Option<usize>, tid: usize, signo: usize) -> isize {
    if tid == 0 || tgid == Some(0) || signo > 64 {
        return -22;
    }
    let task = match tgid {
        Some(pid) => pid2process(pid).and_then(|process| {
            process
                .inner_exclusive_access()
                .tasks
                .iter()
                .flatten()
                .find(|task| task.linux_tid() == tid)
                .cloned()
        }),
        None => crate::task::tid2task(tid),
    };
    if let Some(task) = task {
        let mut inner = task.inner_exclusive_access();
        if inner.exit_code.is_none() {
            if signo != 0 {
                inner.pending_signals |= signal_bit(signo);
            }
            return 0;
        }
    }
    -3
}

pub fn kill_process(pid: usize, signo: usize) -> isize {
    if signo > 64 {
        return -22;
    }
    let process = if pid == 0 {
        Some(current_process())
    } else {
        pid2process(pid)
    };
    if let Some(process) = process {
        if signo != 0 {
            process.inner_exclusive_access().signal_pending |= signal_bit(signo);
        }
    }
    // Keep the existing success result for a PID that has already exited.
    0
}

pub(super) fn take_pending(set: u64) -> Option<usize> {
    check_alarm();
    let process = current_process();
    let task = current_task().unwrap();
    let mut process_inner = process.inner_exclusive_access();
    let mut thread = task.inner_exclusive_access();
    let pending = (thread.pending_signals | process_inner.signal_pending) & set;
    if pending == 0 {
        return None;
    }
    let index = pending.trailing_zeros() as usize;
    let bit = 1u64 << index;
    if thread.pending_signals & bit != 0 {
        thread.pending_signals &= !bit;
    } else {
        process_inner.signal_pending &= !bit;
    }
    Some(index + 1)
}

/// 定时器中断里调用：到期则标记 SIGALRM 待投递
pub fn check_alarm() {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.alarm_deadline != 0 && crate::timer::get_time_ms() >= inner.alarm_deadline {
        inner.alarm_deadline = 0;
        inner.signal_pending |= signal_bit(SIGALRM);
    }
}

/// 内核忙等循环（waitpid / futex …）里调用：SIGALRM 已到期则返回 true。
/// 调用者应立刻返回 -EINTR，这样回到用户态后 deliver_signal 才能执行超时 handler。
pub fn alarm_interrupts() -> bool {
    check_alarm();
    let task = current_task().unwrap();
    let mask = task.inner_exclusive_access().signal_mask;
    current_process().inner_exclusive_access().signal_pending & !mask & signal_bit(SIGALRM) != 0
}

pub(crate) fn queue_current(signo: usize) {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .pending_signals |= signal_bit(signo);
}

pub(crate) fn interrupts_wait(task: &Arc<crate::task::TaskControlBlock>, now_ms: usize) -> bool {
    let process = match task.process.upgrade() {
        Some(process) => process,
        None => return true,
    };
    let process_inner = process.inner_exclusive_access();
    if process_inner.signals.check_error().is_some() {
        return true;
    }
    let inner = task.inner_exclusive_access();
    let alarm_due = process_inner.alarm_deadline != 0 && process_inner.alarm_deadline <= now_ms;
    let pending = inner.pending_signals
        | process_inner.signal_pending
        | if alarm_due { signal_bit(SIGALRM) } else { 0 };
    let mut ready = pending & !inner.signal_mask;
    while ready != 0 {
        let bit = ready.trailing_zeros() as usize;
        ready &= ready - 1;
        let action = process_inner.signal_actions[bit];
        if action.handler > 1 || (action.handler == 0 && terminating_default(bit + 1)) {
            return true;
        }
    }
    false
}

pub(crate) fn check_fatal_current() {
    let process = current_process();
    let task = current_task().unwrap();
    let fatal = {
        let inner = process.inner_exclusive_access();
        let thread = task.inner_exclusive_access();
        let mut pending = (thread.pending_signals | inner.signal_pending) & !thread.signal_mask;
        let mut fatal = None;
        while pending != 0 {
            let index = pending.trailing_zeros() as usize;
            pending &= pending - 1;
            if inner.signal_actions[index].handler == 0 && terminating_default(index + 1) {
                fatal = Some(index + 1);
                break;
            }
        }
        fatal
    };
    if let Some(signo) = fatal {
        drop(task);
        drop(process);
        crate::task::exit_current_by_signal(signo as i32);
    }
}

fn terminating_default(signo: usize) -> bool {
    !matches!(signo, 17..=23 | 28)
}

pub fn deliver_signal() {
    const SA_NODEFER: usize = 0x40000000;
    const SA_RESETHAND: usize = 0x80000000;
    check_fatal_current();
    let process = current_process();
    let task = current_task().unwrap();
    let mut process_inner = process.inner_exclusive_access();
    let mut thread = task.inner_exclusive_access();
    if thread.signal_context.is_some() {
        return;
    }
    let mut pending = (thread.pending_signals | process_inner.signal_pending) & !thread.signal_mask;
    while pending != 0 {
        let index = pending.trailing_zeros() as usize;
        let bit = 1u64 << index;
        pending &= !bit;
        if thread.pending_signals & bit != 0 {
            thread.pending_signals &= !bit;
        } else {
            process_inner.signal_pending &= !bit;
        }
        let action = process_inner.signal_actions[index];
        let signo = index + 1;
        if action.handler <= 1 {
            continue;
        }
        let cx = thread.get_trap_cx();
        thread.signal_context = Some((*cx, thread.signal_mask));
        thread.signal_mask |= action.mask;
        if action.flags & SA_NODEFER == 0 {
            thread.signal_mask |= bit;
        }
        thread.signal_mask &= !UNBLOCKABLE;
        if action.flags & SA_RESETHAND != 0 {
            process_inner.signal_actions[index] = SignalAction::default();
        }
        cx.sepc = action.handler;
        cx.x[1] = SIGSTUB_VA;
        cx.x[crate::arch::REG_A0] = signo;
        cx.x[crate::arch::REG_SP] = cx.x[crate::arch::REG_SP].saturating_sub(512) & !0xf;
        return;
    }
}
