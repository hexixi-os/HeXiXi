use core::cell::{RefCell, RefMut};

/// Wrap a static data structure inside it so that we are
/// able to access it without any `unsafe`.
///
/// On SMP, access is serialized by the global kernel lock.
///
/// In order to get mutable reference of inner data, call
/// `exclusive_access`.
pub struct UPSafeCell<T> {
    /// inner data
    inner: RefCell<T>,
}

unsafe impl<T> Sync for UPSafeCell<T> {}

impl<T> UPSafeCell<T> {
    /// Caller must guarantee that all borrows are serialized.
    pub const unsafe fn new(value: T) -> Self {
        Self {
            inner: RefCell::new(value),
        }
    }
    /// Panic if the data has been borrowed.
    pub fn exclusive_access(&self) -> RefMut<'_, T> {
        self.inner.borrow_mut()
    }
    /// 若数据已被借用则返回 None。缺页补页路径用它，避免在持有锁时补页造成死锁。
    pub fn try_exclusive_access(&self) -> Option<RefMut<'_, T>> {
        self.inner.try_borrow_mut().ok()
    }
}
