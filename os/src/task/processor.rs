use super::__switch;
use super::{fetch_task, TaskStatus};
use super::{ProcessControlBlock, TaskContext, TaskControlBlock};
use crate::sync::UPSafeCell;
use crate::trap::TrapContext;
use alloc::sync::Arc;
use lazy_static::*;

#[cfg(feature = "sched_switch_stats")]
use core::sync::atomic::{AtomicUsize, Ordering};
#[cfg(feature = "sched_switch_stats")]
static TASK_DISPATCHES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "sched_switch_stats")]
static READY_YIELDS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "sched_switch_stats")]
pub(super) fn record_ready_yield() {
    READY_YIELDS.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "sched_switch_stats")]
pub(super) fn report_switch_stats() {
    println!(
        "[sched] task_dispatches={} ready_yields={}",
        TASK_DISPATCHES.load(Ordering::Relaxed),
        READY_YIELDS.load(Ordering::Relaxed)
    );
}

pub struct Processor {
    current: Option<Arc<TaskControlBlock>>,
    idle_task_cx: TaskContext,
}

impl Processor {
    pub fn new() -> Self {
        Self {
            current: None,
            idle_task_cx: TaskContext::zero_init(),
        }
    }
    fn get_idle_task_cx_ptr(&mut self) -> *mut TaskContext {
        &mut self.idle_task_cx as *mut _
    }
    pub fn take_current(&mut self) -> Option<Arc<TaskControlBlock>> {
        self.current.take()
    }
    pub fn current(&self) -> Option<Arc<TaskControlBlock>> {
        self.current.as_ref().map(Arc::clone)
    }
}

#[cfg(not(feature = "smp"))]
lazy_static! {
    static ref PROCESSOR: UPSafeCell<Processor> = unsafe { UPSafeCell::new(Processor::new()) };
}
#[cfg(feature = "smp")]
lazy_static! {
    static ref PROCESSORS: [UPSafeCell<Processor>; crate::arch::MAX_HARTS] =
        core::array::from_fn(|_| unsafe { UPSafeCell::new(Processor::new()) });
}

fn local_processor() -> &'static UPSafeCell<Processor> {
    #[cfg(feature = "smp")]
    {
        &PROCESSORS[crate::arch::hart_id()]
    }
    #[cfg(not(feature = "smp"))]
    {
        &PROCESSOR
    }
}

pub fn run_tasks() -> ! {
    loop {
        super::drain_recycled_tasks();
        let mut processor = local_processor().exclusive_access();
        if let Some(task) = fetch_task() {
            let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
            // access coming task TCB exclusively
            let mut task_inner = task.inner_exclusive_access();
            let next_task_cx_ptr = &task_inner.task_cx as *const TaskContext;
            task_inner.task_status = TaskStatus::Running;
            drop(task_inner);
            // release coming task TCB manually
            processor.current = Some(task);
            // release processor manually
            drop(processor);
            #[cfg(feature = "sched_switch_stats")]
            TASK_DISPATCHES.fetch_add(1, Ordering::Relaxed);
            unsafe {
                __switch(idle_task_cx_ptr, next_task_cx_ptr);
            }
        } else {
            drop(processor);
            // 内核态中断被屏蔽，不能依赖时钟中断唤醒休眠任务，
            // 空闲时主动轮询定时器，否则所有任务都阻塞时系统死锁。
            crate::timer::check_timer();
        }
        // Allow other harts into the kernel after every completed switch.
        // The previous task's context has been saved and no RefCell is borrowed.
        crate::sync::leave_kernel();
        core::hint::spin_loop();
        crate::sync::enter_kernel();
    }
}

pub fn take_current_task() -> Option<Arc<TaskControlBlock>> {
    local_processor().exclusive_access().take_current()
}

pub fn current_task() -> Option<Arc<TaskControlBlock>> {
    local_processor().exclusive_access().current()
}

pub fn current_process() -> Arc<ProcessControlBlock> {
    current_task().unwrap().process.upgrade().unwrap()
}

pub fn current_user_token() -> usize {
    let task = current_task().unwrap();
    task.get_user_token()
}

pub fn current_trap_cx() -> &'static mut TrapContext {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .get_trap_cx()
}

pub fn current_trap_cx_user_va() -> usize {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .res
        .as_ref()
        .unwrap()
        .trap_cx_user_va()
}

pub fn current_kstack_top() -> usize {
    // panic 处理器会调用这里；没有当前任务时返回 0，避免二次 panic
    current_task().map(|t| t.kstack.get_top()).unwrap_or(0)
}

pub fn schedule(switched_task_cx_ptr: *mut TaskContext) {
    let mut processor = local_processor().exclusive_access();
    let idle_task_cx_ptr = processor.get_idle_task_cx_ptr();
    drop(processor);
    unsafe {
        __switch(switched_task_cx_ptr, idle_task_cx_ptr);
    }
}
