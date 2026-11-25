pub const USER_STACK_SIZE: usize = 4096 * 256;
pub const KERNEL_STACK_SIZE: usize = 4096 * 2;
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
