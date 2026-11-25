use super::{current_task, manager::wake_blocked_task, TaskControlBlock};
use crate::sync::UPSafeCell;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use lazy_static::*;

/// Condition checks, registration, blocking and wakeups are serialized by the kernel lock.
pub(crate) struct WaitQueue {
    waiters: UPSafeCell<VecDeque<Weak<TaskControlBlock>>>,
}

lazy_static! {
    static ref POLL_WAITERS: Arc<WaitQueue> = WaitQueue::new();
}

pub(crate) fn poll_waiters() -> &'static Arc<WaitQueue> {
    &POLL_WAITERS
}

pub(crate) fn wake_poll_waiters() {
    POLL_WAITERS.wake_all();
}

impl WaitQueue {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            waiters: unsafe { UPSafeCell::new(VecDeque::new()) },
        })
    }

    /// Call while checking the condition, then drop all borrows before switching away.
    /// A pending signal that can interrupt the syscall takes precedence over going to sleep.
    pub(crate) fn register_current_interruptible(self: &Arc<Self>) -> bool {
        let task = current_task().unwrap();
        if crate::syscall::signal::interrupts_wait(&task, crate::timer::get_time_ms()) {
            // If the alarm just became due, queue its signal before returning EINTR.
            crate::syscall::signal::check_alarm();
            return false;
        }
        let mut inner = task.inner_exclusive_access();
        assert!(
            inner.wait_queue.is_none(),
            "task already registered in a wait queue"
        );
        inner.wait_queue = Some(Arc::downgrade(self));
        self.waiters
            .exclusive_access()
            .push_back(Arc::downgrade(&task));
        true
    }

    pub(crate) fn wake_all(&self) {
        let mut waiters = self.waiters.exclusive_access();
        while let Some(waiter) = waiters.pop_front() {
            if let Some(task) = waiter.upgrade() {
                task.inner_exclusive_access().wait_queue = None;
                wake_blocked_task(task);
            }
        }
    }

    pub(crate) fn cancel(task: &TaskControlBlock) -> bool {
        let registration = task.inner_exclusive_access().wait_queue.take();
        match registration {
            Some(queue) => {
                if let Some(queue) = queue.upgrade() {
                    queue
                        .waiters
                        .exclusive_access()
                        .retain(|waiter| !core::ptr::eq(waiter.as_ptr(), task));
                }
                true
            }
            None => false,
        }
    }

    pub(crate) fn interrupt(task: &Arc<TaskControlBlock>) {
        if Self::cancel(task) {
            wake_blocked_task(Arc::clone(task));
        }
    }
}
