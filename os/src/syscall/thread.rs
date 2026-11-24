use crate::{
    mm::kernel_token,
    task::{add_task, current_task, TaskControlBlock},
    trap::{trap_handler, TrapContext},
};
use alloc::sync::Arc;

pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let ustack_base = task
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .ustack_base;
    let new_task = match TaskControlBlock::try_thread(Arc::clone(&process), Some(ustack_base)) {
        Some(task) => Arc::new(task),
        None => return -12,
    };
    // add new task to scheduler
    add_task(Arc::clone(&new_task));
    let mut new_task_inner = new_task.inner_exclusive_access();
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_task_tid = new_task_res.tid;
    let mut process_inner = process.inner_exclusive_access();
    // add new thread to current process
    let tasks = &mut process_inner.tasks;
    while tasks.len() < new_task_tid + 1 {
        tasks.push(None);
    }
    tasks[new_task_tid] = Some(Arc::clone(&new_task));
    let new_task_trap_cx = new_task_inner.get_trap_cx();
    *new_task_trap_cx = TrapContext::app_init_context(
        entry,
        new_task_res.ustack_top(),
        kernel_token(),
        new_task.kstack.get_top(),
        trap_handler as usize,
    );
    (*new_task_trap_cx).x[crate::arch::REG_A0] = arg;
    new_task_tid as isize
}
pub fn sys_gettid() -> isize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .tid as isize
}

/// thread does not exist, return -1
/// thread has not exited yet, return -2
/// otherwise, return thread's exit code
pub fn sys_waittid(tid: usize) -> i32 {
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let task_inner = task.inner_exclusive_access();
    let mut process_inner = process.inner_exclusive_access();
    // a thread cannot wait for itself
    if task_inner.res.as_ref().unwrap().tid == tid {
        return -1;
    }
    let exited = match process_inner.tasks.get(tid).and_then(Option::as_ref) {
        Some(waited) => waited.inner_exclusive_access().exit_code,
        None => return -1,
    };
    let code = match exited {
        Some(code) => code,
        None => return -2,
    };
    let reaped = process_inner.tasks[tid].take();
    drop(process_inner);
    drop(reaped);
    code
}

/// clone(220) 的线程语义：CLONE_VM|CLONE_THREAD 时创建同进程内的新线程。
/// musl 的 __clone 在子线程里需要：sp = stack 参数、tp = tls 参数、a0 = 0。
pub fn sys_clone_thread(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> isize {
    const CLONE_VM: usize = 0x100;
    const CLONE_FILES: usize = 0x400;
    const CLONE_FS: usize = 0x200;
    const CLONE_SIGHAND: usize = 0x800;
    const CLONE_THREAD: usize = 0x10000;
    const CLONE_SYSVSEM: usize = 0x40000;
    const CLONE_SETTLS: usize = 0x80000;
    const CLONE_PARENT_SETTID: usize = 0x100000;
    const CLONE_CHILD_CLEARTID: usize = 0x200000;
    const CLONE_DETACHED: usize = 0x400000;
    const CLONE_CHILD_SETTID: usize = 0x1000000;
    const GROUP: usize = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
    const THREAD_FLAGS: usize = GROUP
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID;
    if flags & CLONE_VM == 0 {
        return if matches!(flags & 0xff, 0 | 17)
            && flags & !(0xff | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID) == 0
        {
            super::process::fork_with_child_tid(
                stack,
                (flags & CLONE_CHILD_SETTID != 0).then_some(ctid),
                (flags & CLONE_CHILD_CLEARTID != 0).then_some(ctid),
            )
        } else {
            -22
        };
    }
    if flags & GROUP != GROUP
        || flags & !THREAD_FLAGS != 0
        || stack == 0
        || stack >= crate::mm::USER_END
    {
        return -22;
    }
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let token = task.get_user_token();
    let parent_tid = if flags & CLONE_PARENT_SETTID != 0 {
        match crate::mm::try_translated_refmut(token, ptid as *mut i32) {
            Some(word) => Some(word),
            None => return -14,
        }
    } else {
        None
    };
    // 复制父线程的用户上下文
    let (parent_cx, signal_mask) = {
        let parent_inner = task.inner_exclusive_access();
        (*parent_inner.get_trap_cx(), parent_inner.signal_mask)
    };
    let new_task = match TaskControlBlock::try_thread(Arc::clone(&process), None) {
        Some(task) => Arc::new(task),
        None => return -12,
    };
    let linux_tid = new_task.linux_tid();
    if let Some(word) = parent_tid {
        *word = linux_tid as i32;
    }
    let mut new_task_inner = new_task.inner_exclusive_access();
    new_task_inner.signal_mask = signal_mask;
    new_task_inner.set_child_tid = (flags & CLONE_CHILD_SETTID != 0).then_some(ctid);
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_tid = new_task_res.tid;
    {
        let mut process_inner = process.inner_exclusive_access();
        process_inner.legacy_thread_exit = false;
        let tasks = &mut process_inner.tasks;
        while tasks.len() < new_tid + 1 {
            tasks.push(None);
        }
        tasks[new_tid] = Some(Arc::clone(&new_task));
    }
    // CLONE_CHILD_CLEARTID：线程退出时由内核清零该地址（musl 的 pthread_join 依赖它）
    if flags & CLONE_CHILD_CLEARTID != 0 {
        new_task_inner.clear_child_tid = ctid;
    }
    let new_cx = new_task_inner.get_trap_cx();
    *new_cx = parent_cx;
    // 关键：内核栈必须是新线程自己的，否则切换过去会踩父线程的栈
    new_cx.kernel_sp = new_task.kstack.get_top();
    // 子线程从 ecall 之后继续，返回 0；栈/线程指针由调用者给出
    new_cx.x[crate::arch::REG_SP] = stack;
    if flags & CLONE_SETTLS != 0 {
        new_cx.x[crate::arch::REG_TP] = tls;
    }
    new_cx.x[crate::arch::REG_A0] = 0;
    drop(new_task_inner);
    add_task(new_task);
    linux_tid as isize
}
