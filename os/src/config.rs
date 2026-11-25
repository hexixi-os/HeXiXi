use core::sync::atomic::{AtomicBool, Ordering};

pub const USER_STACK_SIZE: usize = 4096 * 256;
// Exec path walks can hold VFS, EXT4, and VirtIO frames together on both architectures.
pub const KERNEL_STACK_SIZE: usize = 4096 * 4;
#[cfg(target_arch = "riscv64")]
pub const MEMORY_END: usize = 0x8FF0_0000;
#[cfg(target_arch = "loongarch64")]
pub use crate::board::MEMORY_END;
pub const PAGE_SIZE: usize = 0x1000;
pub const PAGE_SIZE_BITS: usize = 0xc;

pub const TRAMPOLINE: usize = usize::MAX - PAGE_SIZE + 1;
pub const TRAP_CONTEXT_BASE: usize = TRAMPOLINE - PAGE_SIZE;

#[cfg(target_arch = "riscv64")]
pub use crate::board::MMIO;

static LEGACY_OSCOMP_IMAGE: AtomicBool = AtomicBool::new(false);

pub(crate) fn configure_legacy_oscomp_image(enabled: bool) {
    LEGACY_OSCOMP_IMAGE.store(enabled, Ordering::Relaxed);
}

pub(crate) fn legacy_oscomp_image() -> bool {
    LEGACY_OSCOMP_IMAGE.load(Ordering::Relaxed)
}
