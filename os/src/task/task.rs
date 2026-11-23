use super::id::TaskUserRes;
use super::{kstack_alloc, KernelStack, ProcessControlBlock, TaskContext};
use crate::trap::TrapContext;
use crate::{mm::{FrameTracker, PhysPageNum}, sync::UPSafeCell};
use alloc::vec::Vec;
use alloc::sync::{Arc, Weak};
use core::cell::RefMut;

pub struct TaskControlBlock {
    // immutable
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    // mutable
    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    pub fn inner_exclusive_access(&self) -> RefMut<'_, TaskControlBlockInner> {
        self.inner.exclusive_access()
    }

    pub fn get_user_token(&self) -> usize {
        let process = self.process.upgrade().unwrap();
        let inner = process.inner_exclusive_access();
        inner.memory_set.token()
    }
}

pub struct TaskControlBlockInner {
    pub res: Option<TaskUserRes>,
    pub trap_cx_ppn: PhysPageNum,
    pub task_cx: TaskContext,
    pub task_status: TaskStatus,
    pub exit_code: Option<i32>,
    /// CLONE_CHILD_CLEARTID 的地址：线程退出时把它清零
    pub clear_child_tid: usize,
    pub(crate) user_io_frames: Vec<Arc<FrameTracker>>,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        if self.trap_cx_ppn.0 == 0 {
            println!("[tcb] get_trap_cx with ppn=0!");
        }
        self.trap_cx_ppn.get_mut()
    }

    #[allow(unused)]
    fn get_status(&self) -> TaskStatus {
        self.task_status
    }
}

impl TaskControlBlock {
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc();
        let kstack_top = kstack.get_top();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    res: Some(res),
                    trap_cx_ppn,
                    task_cx: TaskContext::goto_trap_return(kstack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: None,
                    clear_child_tid: 0,
                    user_io_frames: Vec::new(),
                })
            },
        }
    }

    /// clone 线程专用：只分配 trap_cx，用户栈由调用者（musl）提供
    pub fn new_thread(process: Arc<ProcessControlBlock>, ustack_base: usize) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, false);
        res.alloc_trap_cx_only();
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack = kstack_alloc();
        let kstack_top = kstack.get_top();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    res: Some(res),
                    trap_cx_ppn,
                    task_cx: TaskContext::goto_trap_return(kstack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: None,
                    clear_child_tid: 0,
                    user_io_frames: Vec::new(),
                })
            },
        }
    }
}

#[derive(Copy, Clone, PartialEq)]
pub enum TaskStatus {
    Ready,
    Running,
    Blocking,
}
