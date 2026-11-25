use core::sync::atomic::{AtomicUsize, Ordering};

static NEXT: AtomicUsize = AtomicUsize::new(0);
static SERVING: AtomicUsize = AtomicUsize::new(0);

// Supervisor interrupts stay masked in the kernel. A hart acquires at trap
// entry and retains ownership across __switch until it returns to userspace
// or reaches the scheduler's yield point. No kernel borrows cross a release.
pub(crate) fn enter_kernel() {
    let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
    while SERVING.load(Ordering::Acquire) != ticket {
        core::hint::spin_loop();
    }
}

pub(crate) fn leave_kernel() {
    // Only the holder writes SERVING.
    let next = SERVING.load(Ordering::Relaxed).wrapping_add(1);
    SERVING.store(next, Ordering::Release);
}
