use crate::mm::{
    try_read_user, try_write_user, MapPermission, MemorySet, UserValue, VirtAddr,
};
use crate::sync::UPSafeCell;
use crate::task::{
    current_process, current_task, current_user_token, ProcessControlBlock,
    ProcessControlBlockInner, WaitQueue,
};
use alloc::{collections::{BTreeMap, VecDeque}, sync::{Arc, Weak}};
use lazy_static::lazy_static;

mod frame;
pub(crate) use frame::AltStack;
pub use frame::{sys_rt_sigreturn, sys_sigaltstack};
use crate::task::{TaskControlBlock, TaskControlBlockInner};

const SA_NOCLDSTOP: usize = 1;
const SA_SIGINFO: usize = 4;
const SA_ONSTACK: usize = 0x08000000;
const SA_RESTART: usize = 0x10000000;
const SA_NODEFER: usize = 0x40000000;
const SA_RESETHAND: usize = 0x80000000;

/// 可执行的信号返回 stub 页（li a7,139; ecall; j .）
pub const SIGSTUB_VA: usize = 0x00FF_E000;
/// Upper end of the signal VA reservation in the ELF loader.
pub const SIGCTX_VA: usize = 0x00FF_F000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct SignalAction {
    pub handler: usize,
    flags: usize,
    mask: u64,
}

// The ABI is three aligned integer fields. A shared mapping may change between fields.
unsafe impl UserValue for SignalAction {
    unsafe fn load(src: *const Self) -> Self {
        Self {
            handler: usize::load(core::ptr::addr_of!((*src).handler)),
            flags: usize::load(core::ptr::addr_of!((*src).flags)),
            mask: u64::load(core::ptr::addr_of!((*src).mask)),
        }
    }

    unsafe fn store(dst: *mut Self, value: Self) {
        usize::store(core::ptr::addr_of_mut!((*dst).handler), value.handler);
        usize::store(core::ptr::addr_of_mut!((*dst).flags), value.flags);
        u64::store(core::ptr::addr_of_mut!((*dst).mask), value.mask);
    }
}

fn signal_bit(signo: usize) -> u64 {
    1u64 << (signo - 1)
}

const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));
const ITIMER_SIGNALS: [usize; 3] = [14, 26, 27]; // SIGALRM, SIGVTALRM, SIGPROF
const CPU_LIMIT_SOFT_TIMER: usize = 3;
const CPU_LIMIT_HARD_TIMER: usize = 4;

lazy_static! {
    // Keyed by (deadline_us, pid, timer_kind) so wakeups and periodic timers stay ordered.
    static ref ALARMS: UPSafeCell<BTreeMap<(u64, usize, usize), Weak<ProcessControlBlock>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

/// Install the user-mode rt_sigreturn trampoline and legacy signal-context VMA.
pub fn setup_signal_pages(memory_set: &mut MemorySet) {
    if memory_set.translate(SIGSTUB_VA.into()).is_none() {
        memory_set.insert_data_page(
            VirtAddr::from(SIGSTUB_VA),
            MapPermission::U | MapPermission::R | MapPermission::X,
            &crate::arch::SIGRETURN_CODE,
        );
    }
    let ctx_vpn = VirtAddr::from(SIGCTX_VA).floor();
    let ctx_range = ctx_vpn..VirtAddr::from(SIGCTX_VA + crate::config::PAGE_SIZE).floor();
    if memory_set.vacant(&ctx_range) {
        memory_set.insert_vma(
            ctx_range,
            MapPermission::U | MapPermission::R | MapPermission::W,
            crate::mm::VmaBacking::Anonymous,
        );
    }
}

fn encode_itimer(interval_us: u64, deadline_us: u64, now_us: u64) -> [u8; 32] {
    let rem_us = if deadline_us == 0 {
        0
    } else {
        deadline_us.saturating_sub(now_us).max(1)
    };
    let fields = [
        (interval_us / 1_000_000) as i64,
        (interval_us % 1_000_000) as i64,
        (rem_us / 1_000_000) as i64,
        (rem_us % 1_000_000) as i64,
    ];
    let mut out = [0u8; 32];
    for (slot, val) in out.chunks_exact_mut(8).zip(fields) {
        slot.copy_from_slice(&val.to_ne_bytes());
    }
    out
}

pub fn sys_getitimer(which: usize, value: *mut u8) -> isize {
    if which > 2 {
        return -22;
    }
    if value.is_null() {
        return -14;
    }
    check_alarm();
    let now_us = crate::timer::get_time_us() as u64;
    let [interval_us, deadline_us] = current_process().inner_exclusive_access().itimers[which];
    let bytes = encode_itimer(interval_us, deadline_us, now_us);
    frame::copy_out(current_user_token(), value as usize, &bytes).map_or(-14, |_| 0)
}

pub fn sys_setitimer(which: usize, new: *const u8, old: *mut u8) -> isize {
    if which > 2 {
        return -22;
    }
    if new.is_null() {
        return -14;
    }
    check_alarm();
    let token = current_user_token();
    let Some(raw_new) = frame::copy_in(token, new as usize, 32) else {
        return -14;
    };
    let fields: [i64; 4] = core::array::from_fn(|i| {
        i64::from_ne_bytes(raw_new[i * 8..(i + 1) * 8].try_into().unwrap())
    });
    if fields[0] < 0
        || !(0..1_000_000).contains(&fields[1])
        || fields[2] < 0
        || !(0..1_000_000).contains(&fields[3])
    {
        return -22;
    }
    let now_us = crate::timer::get_time_us() as u64;
    let process = current_process();
    let pid = process.getpid();
    if !old.is_null() {
        let [prev_interval, prev_deadline] = process.inner_exclusive_access().itimers[which];
        let bytes = encode_itimer(prev_interval, prev_deadline, now_us);
        if frame::copy_out(token, old as usize, &bytes).is_none() {
            return -14;
        }
    }
    let interval_us = (fields[0] as u64)
        .saturating_mul(1_000_000)
        .saturating_add(fields[1] as u64);
    let value_us = (fields[2] as u64)
        .saturating_mul(1_000_000)
        .saturating_add(fields[3] as u64);
    let mut inner = process.inner_exclusive_access();
    let prev_deadline = inner.itimers[which][1];
    if prev_deadline != 0 {
        ALARMS.exclusive_access().remove(&(prev_deadline, pid, which));
    }
    if value_us == 0 {
        inner.itimers[which] = [0, 0];
    } else {
        let deadline_us = now_us.saturating_add(value_us);
        inner.itimers[which] = [interval_us, deadline_us];
        ALARMS
            .exclusive_access()
            .insert((deadline_us, pid, which), Arc::downgrade(&process));
        drop(inner);
        crate::timer::arm_within_ms(((value_us + 999) / 1000).max(1) as usize);
    }
    0
}

pub(crate) fn arm_cpu_limit(process: &Arc<ProcessControlBlock>, cur_sec: usize, max_sec: usize) {
    let pid = process.getpid();
    let now_us = crate::timer::get_time_us() as u64;
    let mut inner = process.inner_exclusive_access();
    let mut alarms = ALARMS.exclusive_access();
    if inner.cpu_soft_deadline_us != 0 {
        alarms.remove(&(inner.cpu_soft_deadline_us, pid, CPU_LIMIT_SOFT_TIMER));
        inner.cpu_soft_deadline_us = 0;
    }
    if inner.cpu_hard_deadline_us != 0 {
        alarms.remove(&(inner.cpu_hard_deadline_us, pid, CPU_LIMIT_HARD_TIMER));
        inner.cpu_hard_deadline_us = 0;
    }
    let mut wake_ms = None;
    if cur_sec != usize::MAX {
        let delta_us = (cur_sec as u64).saturating_mul(1_000_000).max(1_000);
        let soft = now_us.saturating_add(delta_us);
        inner.cpu_soft_deadline_us = soft;
        alarms.insert((soft, pid, CPU_LIMIT_SOFT_TIMER), Arc::downgrade(process));
        wake_ms = Some(((delta_us + 999) / 1000) as usize);
    }
    if max_sec != usize::MAX {
        let delta_us = (max_sec as u64).saturating_mul(1_000_000).max(1_000);
        let hard = now_us.saturating_add(delta_us);
        inner.cpu_hard_deadline_us = hard;
        alarms.insert((hard, pid, CPU_LIMIT_HARD_TIMER), Arc::downgrade(process));
        let ms = ((delta_us + 999) / 1000) as usize;
        wake_ms = Some(wake_ms.map_or(ms, |w: usize| w.min(ms)));
    }
    drop(alarms);
    drop(inner);
    if let Some(ms) = wake_ms {
        crate::timer::arm_within_ms(ms.max(1));
    }
}

pub(crate) fn next_alarm_ms() -> Option<usize> {
    ALARMS
        .exclusive_access()
        .keys()
        .next()
        .map(|&(deadline_us, _, _)| ((deadline_us + 999) / 1000) as usize)
}

pub(crate) fn cancel_alarm(pid: usize, inner: &mut ProcessControlBlockInner) {
    let mut alarms = ALARMS.exclusive_access();
    for which in 0..3 {
        let deadline = inner.itimers[which][1];
        if deadline != 0 {
            alarms.remove(&(deadline, pid, which));
        }
        inner.itimers[which] = [0, 0];
    }
    if inner.cpu_soft_deadline_us != 0 {
        alarms.remove(&(inner.cpu_soft_deadline_us, pid, CPU_LIMIT_SOFT_TIMER));
        inner.cpu_soft_deadline_us = 0;
    }
    if inner.cpu_hard_deadline_us != 0 {
        alarms.remove(&(inner.cpu_hard_deadline_us, pid, CPU_LIMIT_HARD_TIMER));
        inner.cpu_hard_deadline_us = 0;
    }
}

pub fn sys_rt_sigaction(signo: usize, act: usize, old: usize, size: usize) -> isize {
    if !(1..=64).contains(&signo) || size != 8 || (act != 0 && matches!(signo, 9 | 19)) {
        return -22;
    }
    let token = current_user_token();
    let action = if act == 0 {
        None
    } else {
        match try_read_user(token, act as *const SignalAction) {
            Some(action) => Some(action),
            None => return -14,
        }
    };
    let process = current_process();
    let previous = process.inner_exclusive_access().signal_actions[signo - 1];
    if old != 0 && try_write_user(token, old as *mut SignalAction, previous).is_none() {
        return -14;
    }
    if let Some(action) = action {
        let mut inner = process.inner_exclusive_access();
        inner.signal_actions[signo - 1] = action;
        if action.handler == 1 || (action.handler == 0 && matches!(signo, 17 | 23 | 28)) {
            discard_pending(&mut inner, signo);
        }
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
        match try_read_user(token, set as *const u64) {
            Some(mask) => match how {
                0 => previous | mask,
                1 => previous & !mask,
                _ => mask,
            },
            None => return -14,
        }
    };
    if old != 0 && try_write_user(token, old as *mut u64, previous).is_none() {
        return -14;
    }
    task.inner_exclusive_access().signal_mask = requested & !UNBLOCKABLE;
    0
}

pub(super) fn sys_rt_sigpending(set: usize, size: usize) -> isize {
    if size > core::mem::size_of::<u64>() {
        return -22;
    }
    check_alarm();
    let pending = {
        let process = current_process();
        let inner = process.inner_exclusive_access();
        let task = current_task().unwrap();
        let thread = task.inner_exclusive_access();
        (inner.signal_pending | thread.pending_signals) & thread.signal_mask
    };
    frame::copy_out(current_user_token(), set, &pending.to_ne_bytes()[..size]).map_or(-14, |_| 0)
}

pub(super) fn sys_rt_sigsuspend(set: usize, size: usize) -> isize {
    if size != core::mem::size_of::<u64>() {
        return -22;
    }
    let Some(next) = frame::copy_in(current_user_token(), set, size) else { return -14 };
    let next = u64::from_ne_bytes(next.as_slice().try_into().unwrap());
    {
        let task = current_task().unwrap();
        let mut thread = task.inner_exclusive_access();
        thread.saved_signal_mask = Some(thread.signal_mask);
        thread.signal_mask = next & !UNBLOCKABLE;
    }
    // Signal delivery saves the original mask in the handler's sigreturn frame.
    while crate::task::poll_waiters().register_current_interruptible() {
        crate::task::block_current_and_run_next();
        WaitQueue::cancel(&current_task().unwrap());
    }
    -4
}

// Standard signals coalesce, while each real-time occurrence retains its own siginfo.
pub(crate) type PendingInfo = BTreeMap<usize, VecDeque<SignalInfo>>;

#[derive(Clone, Copy)]
enum SignalSource {
    Sender { pid: u32, uid: u32 },
    Child { pid: u32, status: i32 },
    Fault(usize),
    Kernel,
}

#[derive(Clone, Copy)]
pub(crate) struct SignalInfo {
    pub(crate) signo: usize,
    code: i32,
    source: SignalSource,
}

impl SignalInfo {
    fn kernel(signo: usize) -> Self {
        Self { signo, code: 128, source: SignalSource::Kernel }
    }

    fn sender(signo: usize, code: i32, pid: usize, uid: u32) -> Self {
        Self { signo, code, source: SignalSource::Sender { pid: pid as u32, uid } }
    }

    pub(crate) fn child(pid: usize, code: i32, status: i32) -> Self {
        Self { signo: 17, code, source: SignalSource::Child { pid: pid as u32, status } }
    }

    fn encode(self, bytes: &mut [u8]) {
        bytes[..4].copy_from_slice(&(self.signo as i32).to_ne_bytes());
        bytes[8..12].copy_from_slice(&self.code.to_ne_bytes());
        match self.source {
            SignalSource::Sender { pid, uid } => {
                bytes[16..20].copy_from_slice(&pid.to_ne_bytes());
                bytes[20..24].copy_from_slice(&uid.to_ne_bytes());
            }
            SignalSource::Child { pid, status } => {
                bytes[16..20].copy_from_slice(&pid.to_ne_bytes());
                bytes[24..28].copy_from_slice(&status.to_ne_bytes());
            }
            SignalSource::Fault(address) => bytes[16..24].copy_from_slice(&address.to_ne_bytes()),
            SignalSource::Kernel => {}
        }
    }

    pub(crate) fn write_to(self, token: usize, address: usize) -> Option<()> {
        let mut bytes = [0; frame::INFO_SIZE];
        self.encode(&mut bytes);
        frame::copy_out(token, address, &bytes)
    }
}

fn enqueue(signals: &mut u64, details: &mut PendingInfo, info: SignalInfo) {
    let bit = signal_bit(info.signo);
    if *signals & bit == 0 || info.signo >= 32 {
        details.entry(info.signo).or_default().push_back(info);
        *signals |= bit;
    }
}

fn dequeue(signals: &mut u64, details: &mut PendingInfo, signo: usize) -> SignalInfo {
    let entries = details.get_mut(&signo).expect("pending signal has info");
    let info = entries.pop_front().unwrap();
    if entries.is_empty() {
        details.remove(&signo);
        *signals &= !signal_bit(signo);
    }
    info
}

fn discard_pending(inner: &mut ProcessControlBlockInner, signo: usize) {
    let bit = signal_bit(signo);
    inner.signal_pending &= !bit;
    inner.signal_info.remove(&signo);
    for task in inner.tasks.iter().flatten() {
        let mut thread = task.inner_exclusive_access();
        thread.pending_signals &= !bit;
        thread.signal_info.remove(&signo);
    }
}

fn interrupt_sleep(task: &Arc<TaskControlBlock>) {
    WaitQueue::interrupt(task);
    crate::timer::interrupt_sleep(task);
    super::futex::interrupt_waiter(task);
}

pub(crate) fn queue_process_signal(inner: &mut ProcessControlBlockInner, info: SignalInfo) {
    let action = inner.signal_actions[info.signo - 1];
    if action.handler == 1 {
        return;
    }
    enqueue(&mut inner.signal_pending, &mut inner.signal_info, info);
    if interrupts_for_action(action, info.signo) {
        let bit = signal_bit(info.signo);
        for task in inner.tasks.iter().flatten() {
            if task.inner_exclusive_access().signal_mask & bit == 0 {
                interrupt_sleep(task);
            }
        }
    }
}

fn set_job_state(process: &ProcessControlBlock, stopped: bool, signo: usize) {
    let parent = {
        let mut inner = process.inner_exclusive_access();
        if inner.job_stopped == stopped {
            return;
        }
        inner.job_stopped = stopped;
        if !stopped {
            for task in inner.tasks.iter().flatten() {
                if core::mem::take(&mut task.inner_exclusive_access().job_parked) {
                    crate::task::wake_blocked_task(Arc::clone(task));
                }
            }
        }
        if signo == 9 {
            return; // SIGKILL wakes the group; its exit is the reportable transition.
        }
        inner.job_wait_status = Some(if stopped { (signo as i32) << 8 | 0x7f } else { 0xffff });
        inner.parent.as_ref().and_then(Weak::upgrade)
    };
    if let Some(parent) = parent {
        let waiters = {
            let mut inner = parent.inner_exclusive_access();
            if inner.signal_actions[16].flags & SA_NOCLDSTOP == 0 {
                let code = if stopped { 5 } else { 6 }; // CLD_STOPPED / CLD_CONTINUED
                queue_process_signal(&mut inner, SignalInfo::child(process.getpid(), code, signo as i32));
            }
            inner.child_waiters.clone()
        };
        waiters.wake_all();
    }
}

fn generate(process: &ProcessControlBlock, target: Option<&Arc<TaskControlBlock>>, info: SignalInfo) {
    let signo = info.signo;
    if signo == 0 {
        return;
    }
    match signo {
        18 => {
            let mut inner = process.inner_exclusive_access();
            for stop in 19..=22 {
                discard_pending(&mut inner, stop);
            }
            drop(inner);
            set_job_state(process, false, signo);
        }
        19..=22 => {
            discard_pending(&mut process.inner_exclusive_access(), 18);
            if signo == 19 {
                set_job_state(process, true, signo);
                return;
            }
        }
        9 => set_job_state(process, false, signo),
        _ => {}
    }
    let mut inner = process.inner_exclusive_access();
    if let Some(task) = target.filter(|_| signo != 9) {
        let action = inner.signal_actions[signo - 1];
        if action.handler == 1 {
            return;
        }
        let interrupt = {
            let mut inner = task.inner_exclusive_access();
            let thread = &mut *inner;
            enqueue(&mut thread.pending_signals, &mut thread.signal_info, info);
            thread.signal_mask & signal_bit(signo) == 0 && interrupts_for_action(action, signo)
        };
        if interrupt {
            interrupt_sleep(task);
        }
    } else {
        queue_process_signal(&mut inner, info);
    }
}

pub(crate) fn send_process_signal(process: &ProcessControlBlock, signo: usize, pid: usize, uid: u32) {
    if process.getpid() == crate::task::IDLE_PID && signo > 0 && signo <= 64 {
        if process.inner_exclusive_access().signal_actions[signo - 1].handler == 0 {
            return;
        }
    }
    generate(process, None, SignalInfo::sender(signo, 0, pid, uid));
}

pub fn sys_thread_kill(tgid: Option<usize>, tid: usize, signo: usize) -> isize {
    if tid == 0 || tgid == Some(0) || signo > 64 {
        return -22;
    }
    let Some(task) = crate::task::tid2task(tid) else { return -3 };
    let Some(process) = task.process.upgrade() else { return -3 };
    if tgid.map_or(false, |pid| pid != process.getpid()) || task.inner_exclusive_access().exit_code.is_some() {
        return -3;
    }
    generate(&process, Some(&task), SignalInfo::sender(signo, -6, current_process().getpid(), 0));
    0
}

fn take_signal(process: &mut ProcessControlBlockInner, thread: &mut TaskControlBlockInner, set: u64) -> Option<SignalInfo> {
    let pending = (thread.pending_signals | process.signal_pending) & set;
    if pending == 0 {
        return None;
    }
    let signo = pending.trailing_zeros() as usize + 1;
    if thread.pending_signals & signal_bit(signo) != 0 {
        Some(dequeue(&mut thread.pending_signals, &mut thread.signal_info, signo))
    } else {
        Some(dequeue(&mut process.signal_pending, &mut process.signal_info, signo))
    }
}

pub(super) fn take_pending_info(set: u64) -> Option<SignalInfo> {
    check_alarm();
    let process = current_process();
    let task = current_task().unwrap();
    let mut process_inner = process.inner_exclusive_access();
    let mut thread = task.inner_exclusive_access();
    take_signal(&mut process_inner, &mut thread, set & !UNBLOCKABLE)
}

/// Deliver due alarms even if the recipient has no runnable thread.
pub fn check_alarm() {
    let now_us = crate::timer::get_time_us() as u64;
    loop {
        let (key, process) = {
            let mut alarms = ALARMS.exclusive_access();
            let key = match alarms.keys().next() {
                Some(&key) if key.0 <= now_us => key,
                _ => return,
            };
            let proc = alarms.remove(&key).unwrap();
            (key, proc)
        };
        if let Some(process) = process.upgrade() {
            let (_, pid, which) = key;
            let mut rearm_ms = None;
            if which < 3 {
                let mut inner = process.inner_exclusive_access();
                let interval_us = inner.itimers[which][0];
                if interval_us > 0 {
                    let next = now_us.saturating_add(interval_us);
                    inner.itimers[which][1] = next;
                    ALARMS
                        .exclusive_access()
                        .insert((next, pid, which), Arc::downgrade(&process));
                    rearm_ms = Some(((interval_us + 999) / 1000).max(1) as usize);
                } else {
                    inner.itimers[which][1] = 0;
                }
                queue_process_signal(&mut inner, SignalInfo::kernel(ITIMER_SIGNALS[which]));
            } else if which == CPU_LIMIT_SOFT_TIMER {
                let mut inner = process.inner_exclusive_access();
                let next = now_us.saturating_add(1_000_000);
                inner.cpu_soft_deadline_us = next;
                ALARMS
                    .exclusive_access()
                    .insert((next, pid, CPU_LIMIT_SOFT_TIMER), Arc::downgrade(&process));
                rearm_ms = Some(1000);
                queue_process_signal(&mut inner, SignalInfo::kernel(24)); // SIGXCPU
            } else if which == CPU_LIMIT_HARD_TIMER {
                process.inner_exclusive_access().cpu_hard_deadline_us = 0;
                generate(&process, None, SignalInfo::kernel(9)); // SIGKILL
            }
            if let Some(ms) = rearm_ms {
                crate::timer::arm_within_ms(ms);
            }
        }
    }
}

pub(crate) fn current_wait_interrupted() -> bool {
    check_alarm();
    interrupts_wait(&current_task().unwrap(), crate::timer::get_time_ms())
}

fn queue_thread(info: SignalInfo) {
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    let thread = &mut *inner;
    enqueue(&mut thread.pending_signals, &mut thread.signal_info, info);
}

pub(crate) fn queue_current(signo: usize) {
    queue_thread(SignalInfo::kernel(signo));
}

pub(crate) fn queue_fault(signo: usize, code: i32, address: usize) {
    let process = current_process();
    let task = current_task().unwrap();
    let mut process_inner = process.inner_exclusive_access();
    let mut inner = task.inner_exclusive_access();
    let thread = &mut *inner;
    let bit = signal_bit(signo);
    if process_inner.signal_actions[signo - 1].handler == 1 || thread.signal_mask & bit != 0 {
        process_inner.signal_actions[signo - 1] = SignalAction::default();
        thread.signal_mask &= !bit;
    }
    enqueue(
        &mut thread.pending_signals,
        &mut thread.signal_info,
        SignalInfo { signo, code, source: SignalSource::Fault(address) },
    );
}

pub(crate) fn interrupts_wait(task: &Arc<TaskControlBlock>, now_ms: usize) -> bool {
    let Some(process) = task.process.upgrade() else { return true };
    let process_inner = process.inner_exclusive_access();
    if process_inner.signals.check_error().is_some() {
        return true;
    }
    let inner = task.inner_exclusive_access();
    let now_us = (now_ms as u64).saturating_mul(1000);
    let mut pending = inner.pending_signals | process_inner.signal_pending;
    for which in 0..3 {
        let d = process_inner.itimers[which][1];
        if d != 0 && d <= now_us {
            pending |= signal_bit(ITIMER_SIGNALS[which]);
        }
    }
    if process_inner.cpu_soft_deadline_us != 0 && process_inner.cpu_soft_deadline_us <= now_us {
        pending |= signal_bit(24);
    }
    if process_inner.cpu_hard_deadline_us != 0 && process_inner.cpu_hard_deadline_us <= now_us {
        pending |= signal_bit(9);
    }
    let mut ready = pending & !inner.signal_mask;
    while ready != 0 {
        let index = ready.trailing_zeros() as usize;
        ready &= ready - 1;
        if interrupts_for_action(process_inner.signal_actions[index], index + 1) {
            return true;
        }
    }
    false
}

fn interrupts_for_action(action: SignalAction, signo: usize) -> bool {
    action.handler > 1 || (action.handler == 0 && (terminating_default(signo) || matches!(signo, 20..=22)))
}

pub(crate) fn check_fatal_current() {
    let fatal = {
        let process = current_process();
        let task = current_task().unwrap();
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
        crate::task::exit_current_by_signal(signo as i32);
    }
}

fn terminating_default(signo: usize) -> bool {
    !matches!(signo, 17..=23 | 28)
}

pub(crate) fn check_stop_or_fatal_current() {
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let fast_clear = {
        let inner = process.inner_exclusive_access();
        let thread = task.inner_exclusive_access();
        !inner.job_stopped
            && ((thread.pending_signals | inner.signal_pending) & !thread.signal_mask) == 0
    };
    drop((process, task));
    if !fast_clear {
        park_while_stopped();
        check_fatal_current();
    }
}

pub(crate) fn park_while_stopped() {
    while current_process().inner_exclusive_access().job_stopped {
        current_task().unwrap().inner_exclusive_access().job_parked = true;
        crate::task::block_current_and_run_next();
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Restart {
    pub pc: usize,
    pub args: [usize; 6],
    pub when_caught: bool,
}

impl Restart {
    fn apply(self) {
        let cx = crate::task::current_trap_cx();
        cx.sepc = self.pc;
        cx.x[crate::arch::REG_A0..crate::arch::REG_A0 + 6].copy_from_slice(&self.args);
    }
}

pub(crate) fn with_signal_mask<T>(
    mask_ptr: usize,
    mask_size: usize,
    f: impl FnOnce() -> Result<T, isize>,
) -> Result<T, isize> {
    if mask_ptr == 0 {
        return f();
    }
    if mask_size != core::mem::size_of::<u64>() {
        return Err(-22);
    }
    let token = current_user_token();
    let new_mask = try_read_user(token, mask_ptr as *const u64).ok_or(-14isize)? & !UNBLOCKABLE;
    let task = current_task().unwrap();
    let old_mask = {
        let mut thread = task.inner_exclusive_access();
        let old = thread.signal_mask;
        thread.saved_signal_mask = Some(old);
        thread.signal_mask = new_mask;
        old
    };
    let result = f();
    if result.as_ref().err() != Some(&-4) {
        let mut thread = task.inner_exclusive_access();
        thread.signal_mask = thread.saved_signal_mask.take().unwrap_or(old_mask);
    }
    result
}

pub(crate) fn deliver_signal(restart: Option<Restart>) -> bool {
    if restart.is_none() {
        let task = current_task().unwrap();
        let process = task.process.upgrade().unwrap();
        let process_inner = process.inner_exclusive_access();
        let thread = task.inner_exclusive_access();
        if !process_inner.job_stopped
            && process_inner.signal_pending == 0
            && thread.pending_signals == 0
            && thread.saved_signal_mask.is_none()
        {
            return !process_inner.signals.is_empty();
        }
    }
    loop {
        park_while_stopped();
        check_fatal_current();
        let process = current_process();
        let task = current_task().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        let mut thread = task.inner_exclusive_access();
        let active_mask = thread.signal_mask;
        let Some(info) = take_signal(&mut process_inner, &mut thread, !active_mask) else {
            if let Some(saved) = thread.saved_signal_mask.take() {
                thread.signal_mask = saved;
            }
            // A different thread may have accepted the process-directed signal.
            if let Some(restart) = restart { restart.apply(); }
            return !process_inner.signals.is_empty();
        };
        let action = process_inner.signal_actions[info.signo - 1];
        let restore_mask = if action.handler > 1 {
            let base_mask = thread.saved_signal_mask.take().unwrap_or(active_mask);
            let self_mask = if action.flags & SA_NODEFER == 0 { signal_bit(info.signo) } else { 0 };
            thread.signal_mask = (active_mask | action.mask | self_mask) & !UNBLOCKABLE;
            if action.flags & SA_RESETHAND != 0 {
                process_inner.signal_actions[info.signo - 1] = SignalAction::default();
            }
            base_mask
        } else {
            active_mask
        };
        drop(thread);
        drop(process_inner);
        drop(task);
        if action.handler == 0 && matches!(info.signo, 19..=22) {
            set_job_state(&process, true, info.signo);
            continue;
        }
        if action.handler <= 1 {
            continue;
        }
        drop(process);
        if action.flags & SA_RESTART != 0 {
            if let Some(restart) = restart.filter(|restart| restart.when_caught) {
                restart.apply();
            }
        }
        if frame::install(action, info, restore_mask).is_none() {
            crate::task::exit_current_by_signal(11);
        }
        return true;
    }
}
