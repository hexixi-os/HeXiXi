//! CPU operations used by the shared kernel.
#[cfg(target_arch = "loongarch64")]
mod loongarch64;
#[cfg(target_arch = "riscv64")]
mod riscv64;

#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[derive(Debug, Clone, Copy)]
pub enum TrapCause {
    Syscall,
    StorePageFault,
    LoadPageFault,
    InstructionPageFault,
    AccessFault,
    IllegalInstruction,
    #[cfg(target_arch = "loongarch64")]
    Breakpoint,
    #[cfg(target_arch = "loongarch64")]
    FloatingPoint,
    Timer,
    Unknown(usize),
}
