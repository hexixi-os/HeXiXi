mod context;
mod id;
mod manager;
mod process;
mod processor;
mod signal;
mod switch;
#[allow(clippy::module_inception)]
mod task;

use self::id::TaskUserRes;
use crate::fs::{open_file, OpenFlags};
use alloc::{sync::Arc, vec::Vec};
use lazy_static::*;
use manager::fetch_task;
use process::ProcessControlBlock;
use switch::__switch;
use crate::timer::remove_timer;

pub use context::TaskContext;
pub use id::{kstack_alloc, pid_alloc, KernelStack, PidHandle, IDLE_PID};
pub(crate) use manager::invalidate_file_pages;
pub use manager::{add_task, remove_task, pid2process, remove_from_pid2process, tid2task};
pub use processor::{
    current_kstack_top, current_process, current_task, current_trap_cx, current_trap_cx_user_va,
    current_user_token, run_tasks, schedule, take_current_task,
};
pub use signal::SignalFlags;
pub use task::{TaskControlBlock, TaskStatus};

pub fn suspend_current_and_run_next() {
    // There must be an application running.
    let task = take_current_task().unwrap();

    // ---- access current TCB exclusively
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    // Change status to Ready
    task_inner.task_status = TaskStatus::Ready;
    drop(task_inner);
    // ---- release current TCB

    // push back to ready queue.
    add_task(task);
    // jump to scheduling cycle
    schedule(task_cx_ptr);
}

pub fn block_current_and_run_next() {
    let task = take_current_task().unwrap();
    let mut task_inner = task.inner_exclusive_access();
    let task_cx_ptr = &mut task_inner.task_cx as *mut TaskContext;
    task_inner.task_status = TaskStatus::Blocking;
    drop(task_inner);
    drop(task); // The process and the wait queue hold the task while it is blocked.
    schedule(task_cx_ptr);
}
#[cfg(all(feature = "board_qemu", target_arch = "riscv64"))]
use crate::board::QEMUExit;

pub fn exit_current_and_run_next(exit_code: i32) {
    exit_current_with_status(exit_code, exit_code << 8, false);
}

pub fn exit_current_group(exit_code: i32) {
    exit_current_with_status(exit_code, exit_code << 8, true);
}

pub fn exit_current_by_signal(signal: i32) {
    exit_current_with_status(-signal, signal, true);
}

fn exit_current_with_status(exit_code: i32, wait_status: i32, group: bool) {
    let task = current_task().unwrap();
    let process = task.process.upgrade().unwrap();
    let (tid, clear_child_tid) = {
        let inner = task.inner_exclusive_access();
        (inner.res.as_ref().unwrap().tid, inner.clear_child_tid)
    };
    let (legacy_exit, last_thread) = {
        let inner = process.inner_exclusive_access();
        (inner.legacy_thread_exit, inner.thread_count() == 1)
    };
    // A private file mapping may need a COW fault while the task is still current.
    if clear_child_tid != 0 && !last_thread {
        let token = process.inner_exclusive_access().memory_set.token();
        if crate::mm::try_write_user(token, clear_child_tid as *mut i32, 0).is_some() {
            crate::syscall::futex::wake_one(token, clear_child_tid);
        }
    }
    let linux_thread = task.linux_thread;
    drop(take_current_task());
    let mut task_inner = task.inner_exclusive_access();
    // record exit code
    task_inner.exit_code = Some(exit_code);
    if let Some(res) = task_inner.res.as_mut() {
        res.release_mappings();
    }
    task_inner.user_io_frames.clear();
    task_inner.user_io_file = None;
    drop(task_inner);
    drop(task);
    // however, if this is the main thread of current process
    // the process should terminate at once
    if group || last_thread || (tid == 0 && legacy_exit) {
        let pid = process.getpid();
        #[cfg(any(feature = "board_qemu", target_arch = "loongarch64"))]
        if pid == IDLE_PID {
            println!(
                "[kernel] Idle process exit with exit_code {} ...",
                exit_code
            );
            #[cfg(target_arch = "loongarch64")]
            crate::arch::shutdown();
            #[cfg(target_arch = "riscv64")]
            if exit_code != 0 {
                crate::board::QEMU_EXIT_HANDLE.exit_failure();
            } else {
                crate::board::QEMU_EXIT_HANDLE.exit_success();
            }
        }
        remove_from_pid2process(pid);
        let mut process_inner = process.inner_exclusive_access();
        // mark this process as a zombie process
        process_inner.is_zombie = true;
        // record exit code of main process
        process_inner.wait_status = wait_status;
        // 子进程退出 → 给父进程投递 SIGCHLD。
        // libc-test 的 runtest 在 fork 之后用 sigtimedwait(SIGCHLD) 等子进程结束，
        // 不投递的话每次测试都要空等 5s 超时。
        if let Some(parent) = process_inner.parent.as_ref().and_then(|p| p.upgrade()) {
            let mut pin = parent.inner_exclusive_access();
            pin.signal_pending |= 1 << (17 - 1); // SIGCHLD
        }

        {
            // move all child processes under init process
            let mut initproc_inner = INITPROC.inner_exclusive_access();
            for child in process_inner.children.iter() {
                child.inner_exclusive_access().parent = Some(Arc::downgrade(&INITPROC));
                initproc_inner.children.push(child.clone());
            }
        }

        // deallocate user res (including tid/trap_cx/ustack) of all threads
        // it has to be done before we dealloc the whole memory_set
        // otherwise they will be deallocated twice
        let mut recycle_res = Vec::<TaskUserRes>::new();
        for task in process_inner.tasks.iter().filter(|t| t.is_some()) {
            let task = task.as_ref().unwrap();
            // if other tasks are Ready in TaskManager or waiting for a timer to be
            // expired, we should remove them.
            //
            // Mention that we do not need to consider Mutex/Semaphore since they
            // are limited in a single process. Therefore, the blocked tasks are
            // removed when the PCB is deallocated.
            remove_inactive_task(Arc::clone(&task));
            let mut task_inner = task.inner_exclusive_access();
            task_inner.user_io_frames.clear();
            task_inner.user_io_file = None;
            if let Some(res) = task_inner.res.take() {
                recycle_res.push(res);
            }
        }
        // dealloc_tid and dealloc_user_res require access to PCB inner, so we
        // need to collect those user res first, then release process_inner
        // for now to avoid deadlock/double borrow problem.
        drop(process_inner);
        recycle_res.clear();

        let mut process_inner = process.inner_exclusive_access();
        process_inner.children.clear();
        // 主线程此刻还在自己的内核栈上跑：如果随 tasks.clear() 一起 drop，
        // KernelStack::drop 会把当前内核栈从内核页表 unmap，接着的栈访问就缺页
        //（libcbench/ltp 的 kernel StorePageFault 就是这个原因：TLB 没失效时侥幸不炸）。
        let exiting_task = process_inner.tasks.get_mut(tid).and_then(|t| t.take());
        // deallocate other data in user space i.e. program code/data section
        process_inner.memory_set.recycle_data_pages();
        // drop file descriptors
        process_inner.fd_table.clear();
        // remove all tasks
        process_inner.tasks.clear();
        drop(process_inner);
        // 交给延迟回收队列，下次在别的内核栈上再真正析构
        if let Some(t) = exiting_task {
            recycle_task(t);
        }
    } else if linux_thread {
        // 从进程线程表摘除，TCB 交给延迟回收队列
        let mut process_inner = process.inner_exclusive_access();
        if tid < process_inner.tasks.len() {
            if let Some(task_arc) = process_inner.tasks[tid].take() {
                recycle_task(task_arc);
            }
        }
    }
    drop(process);
    // we do not have to save task context
    let mut _unused = TaskContext::zero_init();
    schedule(&mut _unused as *mut _);
}

lazy_static! {
    pub static ref INITPROC: Arc<ProcessControlBlock> = {
        #[cfg(feature = "oscomp_runner")]
        let runner = include_bytes!(env!("OSCOMP_RUNNER_ELF"));
        #[cfg(feature = "oscomp_runner")]
        if crate::fs::has_ext4() && crate::fs::exists("musl/basic_testcode.sh") {
            return ProcessControlBlock::new(crate::fs::Executable::Embedded(runner));
        }
        // 测试盘上有 run_tests 标记时，改为启动自动跑测的 autorun
        // 盘里有 oscomp_runner 就交给它去跑 *_testcode.sh
        let name = if crate::fs::exists("oscomp_runner") {
            "oscomp_runner"
        } else if crate::fs::exists("run_tests") {
            "autorun"
        } else if crate::fs::exists("initproc") {
            "initproc"
        } else {
            #[cfg(feature = "oscomp_runner")]
            if crate::fs::has_ext4() {
                if let Ok(cwd) = crate::fs::working_directory("musl") {
                    let process = ProcessControlBlock::new(crate::fs::Executable::Embedded(runner));
                    process.inner_exclusive_access().cwd = cwd;
                    return process;
                }
            }
            panic!("missing init executable");
        };
        ProcessControlBlock::new(crate::fs::open_executable(name).expect("missing init executable"))
    };
}

pub fn add_initproc(oscomp_group: Option<&str>) {
    let initproc = INITPROC.clone();
    #[cfg(feature = "oscomp_runner")]
    if let Some(group) = oscomp_group {
        if crate::fs::has_ext4() && crate::fs::exists("musl/basic_testcode.sh") {
            initproc.set_initial_args(&["oscomp_runner", group]);
        }
    }
    #[cfg(not(feature = "oscomp_runner"))]
    let _ = (initproc, oscomp_group);
}

/// 进程被 kill(SIGKILL) 后，在内核里的轮询/等待循环中尽快退出。
/// （用户态循环靠时钟中断里的信号检查退出；内核态等待循环必须主动检查）
pub fn check_kill_current() {
    crate::syscall::signal::check_fatal_current();
}

/// 当前进程是否给 signo 注册了真的用户 handler（不是 SIG_DFL/SIG_IGN）
pub fn current_has_handler(signo: usize) -> bool {
    current_process()
        .inner_exclusive_access()
        .signal_actions
        .get(signo.wrapping_sub(1))
        .map_or(false, |action| action.handler > 1)
}

/// 给当前进程置“待投递信号”位（用于 SIGSEGV handler 投递）
pub fn current_set_pending(signo: usize) -> bool {
    if (1..=64).contains(&signo) {
        crate::syscall::signal::queue_current(signo);
        true
    } else {
        false
    }
}

pub fn check_signals_of_current() -> Option<(i32, &'static str)> {
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    process_inner.signals.check_error()
}

pub fn current_add_signal(signal: SignalFlags) {
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    process_inner.signals |= signal;
}

pub fn remove_inactive_task(task: Arc<TaskControlBlock>) {
    crate::syscall::futex::remove_task(&task);
    remove_task(Arc::clone(&task));
    remove_timer(Arc::clone(&task));
}

lazy_static! {
    /// 已退出线程的 TCB 延迟回收（不能在自己的内核栈上析构）
    static ref RECYCLE_QUEUE: crate::sync::UPSafeCell<Vec<Arc<TaskControlBlock>>> =
        unsafe { crate::sync::UPSafeCell::new(Vec::new()) };
}

pub fn recycle_task(task: Arc<TaskControlBlock>) {
    RECYCLE_QUEUE.exclusive_access().push(task);
}

/// 在“别的内核栈”上真正析构已退出线程的 TCB。
/// 析构会 unmap 它的内核栈，所以绝不能析构正在运行的那个任务（那等于拆自己的梯子）。
/// 调用点：trap_handler 入口（此时 sp 属于即将运行的任务，不是队列里的任何 TCB）。
pub fn drain_recycled_tasks() {
    let cur = current_task();
    let tasks: Vec<Arc<TaskControlBlock>> = {
        let mut q = RECYCLE_QUEUE.exclusive_access();
        if q.is_empty() {
            return;
        }
        core::mem::take(&mut *q)
    };
    let mut requeue: Vec<Arc<TaskControlBlock>> = Vec::new();
    for t in tasks {
        let is_current = match &cur {
            Some(c) => Arc::ptr_eq(c, &t),
            None => false,
        };
        if is_current {
            requeue.push(t);
        } else {
            drop(t);
        }
    }
    if !requeue.is_empty() {
        let mut q = RECYCLE_QUEUE.exclusive_access();
        for t in requeue {
            q.push(t);
        }
    }
}
