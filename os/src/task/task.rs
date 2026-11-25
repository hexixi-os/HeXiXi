use super::id::{try_kstack_alloc, TaskUserRes};
use super::{kstack_alloc, pid_alloc, KernelStack, PidHandle, ProcessControlBlock, TaskContext};
use crate::trap::TrapContext;
use crate::{
    mm::{FrameTracker, PhysPageNum},
    sync::UPSafeCell,
};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::RefMut;

pub struct TaskControlBlock {
    // immutable
    pub process: Weak<ProcessControlBlock>,
    pub kstack: KernelStack,
    linux_tid: Option<PidHandle>,
    pub(super) linux_thread: bool,
    // mutable
    inner: UPSafeCell<TaskControlBlockInner>,
}

impl TaskControlBlock {
    pub fn linux_tid(&self) -> usize {
        self.linux_tid
            .as_ref()
            .map_or_else(|| self.process.upgrade().unwrap().getpid(), |tid| tid.0)
    }

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
    pub(crate) set_child_tid: Option<usize>,
    pub(crate) futex_result: isize,
    pub(crate) signal_mask: u64,
    pub(crate) pending_signals: u64,
    pub(crate) signal_context: Option<(TrapContext, u64)>,
    pub(crate) user_io_frames: Vec<Arc<FrameTracker>>,
    pub(crate) user_io_file: Option<Arc<dyn crate::fs::File>>,
}

impl TaskControlBlockInner {
    pub fn get_trap_cx(&self) -> &'static mut TrapContext {
        if self.trap_cx_ppn.0 == 0 {
            println!("[tcb] get_trap_cx with ppn=0!");
        }
        self.trap_cx_ppn.get_mut()
    }
}

impl TaskControlBlock {
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, alloc_user_res);
        Self::with_resources(process, res, kstack_alloc(), None, false)
    }

    pub(super) fn for_fork(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        kstack: KernelStack,
    ) -> Self {
        let res = TaskUserRes::new(Arc::clone(&process), ustack_base, false);
        Self::with_resources(process, res, kstack, None, false)
    }

    /// Linux clone supplies its own stack. Teaching threads pass a stack hint.
    pub fn try_thread(
        process: Arc<ProcessControlBlock>,
        stack_hint: Option<usize>,
    ) -> Option<Self> {
        let kstack = try_kstack_alloc()?;
        let res = TaskUserRes::try_for_thread(&process, stack_hint)?;
        Some(Self::with_resources(
            process,
            res,
            kstack,
            Some(pid_alloc()),
            stack_hint.is_none(),
        ))
    }

    fn with_resources(
        process: Arc<ProcessControlBlock>,
        res: TaskUserRes,
        kstack: KernelStack,
        linux_tid: Option<PidHandle>,
        linux_thread: bool,
    ) -> Self {
        let trap_cx_ppn = res.trap_cx_ppn();
        let kstack_top = kstack.get_top();
        Self {
            process: Arc::downgrade(&process),
            kstack,
            linux_tid,
            linux_thread,
            inner: unsafe {
                UPSafeCell::new(TaskControlBlockInner {
                    res: Some(res),
                    trap_cx_ppn,
                    task_cx: TaskContext::goto_trap_return(kstack_top),
                    task_status: TaskStatus::Ready,
                    exit_code: None,
                    clear_child_tid: 0,
                    set_child_tid: None,
                    futex_result: 0,
                    signal_mask: 0,
                    pending_signals: 0,
                    signal_context: None,
                    user_io_frames: Vec::new(),
                    user_io_file: None,
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
