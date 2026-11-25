use super::{ProcessControlBlock, TaskControlBlock};
use crate::sync::UPSafeCell;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use lazy_static::*;

pub struct TaskManager {
    ready_queue: VecDeque<Arc<TaskControlBlock>>,
}

/// A simple FIFO scheduler.
impl TaskManager {
    pub fn new() -> Self {
        Self {
            ready_queue: VecDeque::new(),
        }
    }
    pub fn add(&mut self, task: Arc<TaskControlBlock>) {
        self.ready_queue.push_back(task);
    }
    pub fn fetch(&mut self) -> Option<Arc<TaskControlBlock>> {
        #[cfg(feature = "smp")]
        {
            let hart = crate::arch::hart_id();
            // A process's threads share its affinity, including across exec.
            let index = self.ready_queue.iter().position(|task| {
                task.process.upgrade().expect("ready task without process").affinity_hart() == hart
            })?;
            self.ready_queue.remove(index)
        }
        #[cfg(not(feature = "smp"))]
        { self.ready_queue.pop_front() }
    }
    pub fn remove(&mut self, task: Arc<TaskControlBlock>) {
        if let Some((id, _)) = self.ready_queue
            .iter()
            .enumerate()
            .find(|(_, t)| Arc::as_ptr(t) == Arc::as_ptr(&task)) {
                self.ready_queue.remove(id);
            }
    }
}

lazy_static! {
    pub static ref TASK_MANAGER: UPSafeCell<TaskManager> =
        unsafe { UPSafeCell::new(TaskManager::new()) };
    pub static ref PID2PCB: UPSafeCell<BTreeMap<usize, Arc<ProcessControlBlock>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

pub fn add_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().add(task);
}

pub fn remove_task(task: Arc<TaskControlBlock>) {
    TASK_MANAGER.exclusive_access().remove(task);
}

pub fn fetch_task() -> Option<Arc<TaskControlBlock>> {
    TASK_MANAGER.exclusive_access().fetch()
}

pub fn pid2process(pid: usize) -> Option<Arc<ProcessControlBlock>> {
    let map = PID2PCB.exclusive_access();
    map.get(&pid).map(Arc::clone)
}

pub fn tid2task(tid: usize) -> Option<Arc<TaskControlBlock>> {
    PID2PCB.exclusive_access().values().find_map(|process| {
        process.inner_exclusive_access().tasks.iter().flatten()
            .find(|task| task.linux_tid() == tid)
            .cloned()
    })
}

pub fn insert_into_pid2process(pid: usize, process: Arc<ProcessControlBlock>) {
    PID2PCB.exclusive_access().insert(pid, process);
}

pub fn remove_from_pid2process(pid: usize) {
    let mut map = PID2PCB.exclusive_access();
    if map.remove(&pid).is_none() {
        panic!("cannot find pid {} in pid2task!", pid);
    }
}

/// Truncate holds the kernel lock and has released its file borrow.
pub(crate) fn invalidate_file_pages(identity: (u64, u64), first_invalid: usize) {
    for process in PID2PCB.exclusive_access().values() {
        process.inner_exclusive_access().memory_set.invalidate_file_pages(identity, first_invalid);
    }
}
