use crate::{
    mm::kernel_token,
    task::{add_task, current_task, TaskControlBlock, TaskUserRes},
    trap::{trap_handler, TrapContext},
};
use alloc::sync::Arc;

pub fn sys_thread_create(entry: usize, arg: usize) -> isize {
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let ustack_base = task.inner_exclusive_access().res.as_ref().unwrap().ustack_base;
    if !TaskUserRes::next_stack_available(&process, ustack_base) {
        return -12;
    }
    let new_task = Arc::new(TaskControlBlock::new(Arc::clone(&process), ustack_base, true));
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
    (*new_task_trap_cx).x[10] = arg;
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
    let mut exit_code: Option<i32> = None;
    let waited_task = process_inner.tasks[tid].as_ref();
    if let Some(waited_task) = waited_task {
        if let Some(waited_exit_code) = waited_task.inner_exclusive_access().exit_code {
            exit_code = Some(waited_exit_code);
        }
    } else {
        // waited thread does not exist
        return -1;
    }
    if let Some(exit_code) = exit_code {
        // dealloc the exited thread
        process_inner.tasks[tid] = None;
        exit_code
    } else {
        // waited thread has not exited
        -2
    }
}

/// clone(220) 的线程语义：CLONE_VM|CLONE_THREAD 时创建同进程内的新线程。
/// musl 的 __clone 在子线程里需要：sp = stack 参数、tp = tls 参数、a0 = 0。
pub fn sys_clone_thread(flags: usize, stack: usize, ptid: usize, tls: usize, ctid: usize) -> isize {
    const CLONE_VM: usize = 0x100;
    if flags & CLONE_VM == 0 {
        return super::process::sys_fork();
    }
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let ustack_base = task
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .ustack_base;
    // 复制父线程的用户上下文
    let parent_cx = {
        let mut parent_inner = task.inner_exclusive_access();
        let cx = parent_inner.get_trap_cx();
        *cx
    };
    let new_task = Arc::new(TaskControlBlock::new_thread(Arc::clone(&process), ustack_base));
    add_task(Arc::clone(&new_task));
    let mut new_task_inner = new_task.inner_exclusive_access();
    let new_task_res = new_task_inner.res.as_ref().unwrap();
    let new_tid = new_task_res.tid;
    {
        let mut process_inner = process.inner_exclusive_access();
        let tasks = &mut process_inner.tasks;
        while tasks.len() < new_tid + 1 {
            tasks.push(None);
        }
        tasks[new_tid] = Some(Arc::clone(&new_task));
    }
    // CLONE_PARENT_SETTID：把子线程 tid 写给父线程
    if flags & 0x0010_0000 != 0 && ptid != 0 {
        let token = crate::task::current_user_token();
        *crate::mm::translated_refmut(token, ptid as *mut i32) = new_tid as i32;
    }
    if flags & 0x0100_0000 != 0 && ctid != 0 {
        // CLONE_CHILD_SETTID：在子线程内存里写入自己的 tid
        let child_token = process.inner_exclusive_access().memory_set.token();
        *crate::mm::translated_refmut(child_token, ctid as *mut i32) = new_tid as i32;
    }
    // CLONE_CHILD_CLEARTID：线程退出时由内核清零该地址（musl 的 pthread_join 依赖它）
    if flags & 0x0020_0000 != 0 && ctid != 0 {
        new_task_inner.clear_child_tid = ctid;
    }
    let new_cx = new_task_inner.get_trap_cx();
    *new_cx = parent_cx;
    // 关键：内核栈必须是新线程自己的，否则切换过去会踩父线程的栈
    new_cx.kernel_sp = new_task.kstack.get_top();
    // 子线程从 ecall 之后继续，返回 0；栈/线程指针由调用者给出
    new_cx.x[2] = stack;
    new_cx.x[4] = tls;
    new_cx.x[10] = 0;
    new_tid as isize
}

/// futex(98)：用让出 CPU 的方式实现 WAIT，WAKE 直接返回
pub fn sys_futex(uaddr: usize, op: usize, val: usize) -> isize {
    const FUTEX_WAIT: usize = 0;
    const FUTEX_WAKE: usize = 1;
    let token = crate::mm::kernel_token() as usize;
    let _ = token;
    let token = crate::task::current_user_token();
    match op & 0x7f {
        FUTEX_WAIT => {
            if uaddr == 0 {
                return -22;
            }
            loop {
                let cur = unsafe {
                    let ptr = crate::mm::translated_ref(token, uaddr as *const u32);
                    *ptr as usize
                };
                if cur != val {
                    return 0;
                }
                crate::task::check_kill_current();
                if crate::syscall::signal::alarm_interrupts() {
                    return -4; // EINTR
                }
                crate::task::suspend_current_and_run_next();
            }
        }
        FUTEX_WAKE => 1,
        _ => 0,
    }
}
