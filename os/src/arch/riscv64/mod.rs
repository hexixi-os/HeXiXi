mod sbi;

pub use sbi::{console_getchar, console_putchar, set_timer, shutdown};

use super::TrapCause;
use crate::board::CLOCK_FREQ;
use crate::config::TRAMPOLINE;
use crate::trap::TrapContext;
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    satp,
    scause::{self, Exception, Interrupt, Trap},
    sepc, sie, sstatus, stval, stvec, time,
};

global_asm!(include_str!("entry.S"));
global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));

pub const NAME: &str = "riscv64";
pub const ELF_MACHINE: u16 = 243;
pub const OFFICIAL_BASIC_INTERPRETER: &str = "/lib/ld-linux-riscv64-lp64d.so.1";
pub const CLONE_CHILD_TID_FIRST: bool = false;
pub const RAM_BASE: usize = 0x8000_0000;
pub const PA_BITS: usize = 56;
pub const REG_SP: usize = 2;
pub const REG_TP: usize = 4;
pub const REG_FP: usize = 8;
pub const REG_A0: usize = 10;
pub const REG_SYSCALL: usize = 17;
pub const SIGRETURN_CODE: [u8; 12] = [
    0x93, 0x08, 0xb0, 0x08, // li a7, 139
    0x73, 0x00, 0x00, 0x00, // ecall
    0x6f, 0x00, 0x00, 0x00, // j .
];

pub fn boot_info() -> crate::fdt::BootInfo {
    crate::fdt::detect_boot_info()
}

pub const fn phys_to_virt(pa: usize) -> usize {
    pa
}

pub const fn root_to_token(ppn: usize) -> usize {
    8usize << 60 | ppn
}

pub const fn token_to_root(token: usize) -> usize {
    token & ((1usize << 44) - 1)
}

pub fn directory_entry(ppn: usize) -> usize {
    pte(ppn, 1)
}

pub fn pte(ppn: usize, flags: u8) -> usize {
    ppn << 10 | usize::from(flags)
}

pub fn pte_ppn(pte: usize) -> usize {
    (pte >> 10) & ((1usize << 44) - 1)
}

pub fn pte_flags(pte: usize) -> u8 {
    pte as u8
}

pub fn flush_tlb() {
    unsafe {
        asm!("sfence.vma");
    }
}

pub fn activate_kernel_table(token: usize) {
    unsafe {
        satp::write(token);
    }
    flush_tlb();
}

#[inline(always)]
pub fn frame_pointer() -> usize {
    let fp: usize;
    unsafe {
        asm!("mv {}, s0", out(reg) fp);
    }
    fp
}

pub fn read_ticks() -> usize {
    time::read()
}

pub fn timer_frequency() -> usize {
    CLOCK_FREQ
}

pub fn enable_timer_interrupt() {
    unsafe {
        sie::set_stimer();
    }
}

pub fn user_status() -> usize {
    // Clear SPP to return to U mode.
    sstatus::read().bits() & !(1 << 8)
}

pub fn install_kernel_trap() {
    unsafe {
        stvec::write(crate::trap::trap_from_kernel as usize, TrapMode::Direct);
    }
}

pub fn trap_info() -> (TrapCause, usize, usize) {
    let raw = scause::read();
    let cause = match raw.cause() {
        Trap::Exception(Exception::UserEnvCall) => TrapCause::Syscall,
        Trap::Exception(Exception::StorePageFault) => TrapCause::StorePageFault,
        Trap::Exception(Exception::LoadPageFault) => TrapCause::LoadPageFault,
        Trap::Exception(Exception::InstructionPageFault) => TrapCause::InstructionPageFault,
        Trap::Exception(
            Exception::StoreFault | Exception::InstructionFault | Exception::LoadFault,
        ) => TrapCause::AccessFault,
        Trap::Exception(Exception::IllegalInstruction) => TrapCause::IllegalInstruction,
        Trap::Interrupt(Interrupt::SupervisorTimer) => TrapCause::Timer,
        _ => TrapCause::Unknown(raw.bits()),
    };
    (cause, stval::read(), sepc::read())
}

pub unsafe fn return_to_user(_: *mut TrapContext, context_va: usize, token: usize) -> ! {
    extern "C" {
        fn __alltraps();
        fn __restore();
    }
    stvec::write(TRAMPOLINE, TrapMode::Direct);
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    asm!(
        "fence.i",
        "jr {restore_va}",
        restore_va = in(reg) restore_va,
        in("a0") context_va,
        in("a1") token,
        options(noreturn)
    );
}
