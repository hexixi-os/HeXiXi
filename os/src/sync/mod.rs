mod condvar;
#[cfg(feature = "smp")]
mod kernel_lock;
mod mutex;
mod semaphore;
mod up;

pub use condvar::Condvar;
pub use mutex::{Mutex, MutexBlocking, MutexSpin};
pub use semaphore::Semaphore;
pub use up::UPSafeCell;


#[cfg(feature = "smp")]
pub(crate) use kernel_lock::{enter_kernel, leave_kernel};

#[cfg(not(feature = "smp"))]
pub(crate) fn enter_kernel() {}

#[cfg(not(feature = "smp"))]
pub(crate) fn leave_kernel() {}
