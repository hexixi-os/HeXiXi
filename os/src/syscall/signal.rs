use crate::mm::{MapPermission, MemorySet, VirtAddr};
use crate::task::{current_add_signal, current_process, current_user_token, SignalFlags};
use crate::trap::TrapContext;
use crate::mm::{translated_ref, try_translated_refmut};

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

/// 在用户地址空间建立信号所需的两页（exec 时调用）
pub fn setup_signal_pages(memory_set: &mut MemorySet) {
    if memory_set.translate(SIGSTUB_VA.into()).is_none() {
        // addi a7, x0, 139 ; ecall ; j .
        let code: [u8; 12] = [0x93, 0x08, 0xB0, 0x08, 0x73, 0x00, 0x00, 0x00, 0x6F, 0x00, 0x00, 0x00];
        memory_set.insert_data_page(
            VirtAddr::from(SIGSTUB_VA),
            MapPermission::U | MapPermission::R | MapPermission::X,
            &code,
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

/// rt_sigaction(134)：记录任意信号号的用户 handler（0=SIG_DFL, 1=SIG_IGN）。
/// 我们不做信号屏蔽/flag 语义，只保存 handler 地址并在投递时调用。
pub fn sys_rt_sigaction(signo: usize, act: *const u8, _old: *mut u8) -> isize {
    if act.is_null() || signo >= 32 {
        return 0;
    }
    let token = current_user_token();
    // struct sigaction 第一个字段就是 handler 地址
    let handler = *translated_ref(token, act as *const usize);
    current_process().inner_exclusive_access().signal_handlers[signo] = handler;
    0
}

/// rt_sigreturn(139)：恢复被打断的上下文
pub fn sys_rt_sigreturn() -> isize {
    let token = current_user_token();
    let cx = crate::task::current_trap_cx();
    let dst = cx as *mut TrapContext as *mut u8;
    for i in 0..core::mem::size_of::<TrapContext>() {
        let b = *translated_ref(token, (SIGCTX_VA + i) as *const u8);
        unsafe { *dst.add(i) = b; }
    }
    let process = current_process();
    process.inner_exclusive_access().in_sigalrm = false;
    0
}

/// 定时器中断里调用：到期则标记 SIGALRM 待投递
pub fn check_alarm() {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    if inner.alarm_deadline != 0 && crate::timer::get_time_ms() >= inner.alarm_deadline {
        inner.alarm_deadline = 0;
        inner.signal_pending |= 1u32 << SIGALRM;
    }
}

/// 内核忙等循环（waitpid / futex …）里调用：SIGALRM 已到期则返回 true。
/// 调用者应立刻返回 -EINTR，这样回到用户态后 deliver_signal 才能执行超时 handler。
pub fn alarm_interrupts() -> bool {
    check_alarm();
    current_process().inner_exclusive_access().signal_pending & (1u32 << SIGALRM) != 0
}

/// 返回用户态前调用：把待投递、且注册了用户 handler 的信号变成“跳到 handler”。
/// 一次只投一个（不嵌套）；handler 里的 rt_sigreturn 恢复现场后，下一次 trap 再投剩下的。
pub fn deliver_signal() {
    let process = current_process();
    let (pending, handlers, in_handler) = {
        let inner = process.inner_exclusive_access();
        (inner.signal_pending, inner.signal_handlers, inner.in_sigalrm)
    };
    if in_handler || pending == 0 {
        return;
    }
    // 选一个“有用户 handler”的最低 pending 信号；没有 handler 的直接清掉
    let mut mask = pending;
    let mut deliver: Option<usize> = None;
    while mask != 0 {
        let signo = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        if handlers[signo] > 1 {
            deliver = Some(signo);
            break;
        } else {
            process.inner_exclusive_access().signal_pending &= !(1u32 << signo);
        }
    }
    let signo = match deliver {
        Some(s) => s,
        None => return,
    };
    let handler = handlers[signo];
    let token = current_user_token();
    let saved = match try_translated_refmut(token, SIGCTX_VA as *mut TrapContext) {
        Some(saved) => saved,
        None => {
            current_add_signal(SignalFlags::SIGSEGV);
            return;
        }
    };
    let cx = crate::task::current_trap_cx();
    *saved = *cx;
    // 构造调用 handler 的上下文
    cx.sepc = handler;
    cx.x[1] = SIGSTUB_VA;
    cx.x[10] = signo;
    cx.x[2] = cx.x[2].saturating_sub(512) & !0xf;
    let mut inner = process.inner_exclusive_access();
    inner.signal_pending &= !(1u32 << signo);
    inner.in_sigalrm = true;
}
