mod boot;

pub use boot::boot_info;

use super::TrapCause;
use crate::trap::TrapContext;
use core::arch::{asm, global_asm};

global_asm!(include_str!("entry.S"));
global_asm!(include_str!("trap.S"));
global_asm!(include_str!("switch.S"));

pub const NAME: &str = "loongarch64";
pub const ELF_MACHINE: u16 = 258;
pub const OFFICIAL_BASIC_INTERPRETER: &str = "/lib64/ld-linux-loongarch-lp64d.so.1";
pub const CLONE_CHILD_TID_FIRST: bool = true;
pub const RAM_BASE: usize = 0;
pub const PA_BITS: usize = 48;
pub const fn hart_id() -> usize { 0 }

pub const REG_SP: usize = 3;
pub const REG_TP: usize = 2;
pub const REG_FP: usize = 22;
pub const REG_A0: usize = 4;
pub const REG_SYSCALL: usize = 11;
pub const SIGRETURN_CODE: [u8; 12] = [
    0x0b, 0x2c, 0x82, 0x02, // addi.w a7, zero, 139
    0x00, 0x00, 0x2b, 0x00, // syscall 0
    0x00, 0x00, 0x00, 0x50, // b .
];

const DMW_CACHED: usize = 0x9000_0000_0000_0000;
const DMW_UNCACHED: usize = 0x8000_0000_0000_0000;
const UART: usize = 0x1fe0_01e0;
const PA_MASK: usize = (1 << PA_BITS) - 1;

pub const fn phys_to_virt(pa: usize) -> usize {
    DMW_CACHED | pa
}

pub const fn mmio_address(pa: usize) -> usize {
    DMW_UNCACHED | pa
}

pub fn console_putchar(ch: usize) {
    unsafe {
        let uart = mmio_address(UART) as *mut u8;
        while uart.add(5).read_volatile() & 0x20 == 0 {}
        uart.write_volatile(ch as u8);
    }
}

pub fn console_getchar() -> usize {
    unsafe {
        let uart = mmio_address(UART) as *const u8;
        if uart.add(5).read_volatile() & 1 == 0 {
            usize::MAX
        } else {
            uart.read_volatile() as usize
        }
    }
}

pub fn shutdown() -> ! {
    // QEMU virt ACPI GED, S5 with the sleep enable bit set.
    unsafe {
        (mmio_address(0x100e_001c) as *mut u8).write_volatile(0x34);
    }
    loop {
        unsafe {
            asm!("idle 0");
        }
    }
}

pub const fn root_to_token(ppn: usize) -> usize {
    ppn << 12
}

pub const fn token_to_root(token: usize) -> usize {
    token >> 12
}

pub fn directory_entry(ppn: usize) -> usize {
    ppn << 12
}

// Common V/R/W/X/U/G/A/D are software flags shared with the VM. Only
// LoongArch's paired-page entry format is present in the physical table.
pub fn pte(ppn: usize, flags: u8) -> usize {
    let mut entry = (ppn << 12) | 1 | (1 << 4) | (1 << 7); // V, coherent MAT, P
    if flags & (1 << 1) == 0 {
        entry |= 1 << 61;
    } // NR
    if flags & (1 << 2) != 0 {
        entry |= (1 << 1) | (1 << 8);
    } // D, W
    if flags & (1 << 3) == 0 {
        entry |= 1 << 62;
    } // NX
    if flags & (1 << 4) != 0 {
        entry |= 3 << 2;
    } // PLV3
    if flags & (1 << 5) != 0 {
        entry |= 1 << 6;
    } // G
    entry
}

pub fn pte_ppn(entry: usize) -> usize {
    (entry & PA_MASK) >> 12
}

pub fn pte_flags(entry: usize) -> u8 {
    if entry == 0 {
        return 0;
    }
    if entry & 1 == 0 {
        return 1; // A nonempty directory contains a physical address only.
    }
    let mut flags = 1;
    if entry & (1 << 61) == 0 {
        flags |= 1 << 1;
    }
    if entry & (1 << 1) != 0 {
        flags |= 1 << 2;
    }
    if entry & (1 << 62) == 0 {
        flags |= 1 << 3;
    }
    if entry & (3 << 2) == 3 << 2 {
        flags |= 1 << 4;
    }
    if entry & (1 << 6) != 0 {
        flags |= 1 << 5;
    }
    flags
}

pub fn flush_tlb() {
    unsafe {
        asm!("dbar 0", "invtlb 0, $zero, $zero");
    }
}

pub use self::flush_tlb as local_flush_tlb;

pub fn local_flush_tlb_page(va: usize) {
    // User mappings are non-global and use ASID 0.
    unsafe {
        asm!("dbar 0", "invtlb 0x5, $zero, {va}", va = in(reg) va);
    }
}

pub fn activate_kernel_table(token: usize) {
    unsafe {
        // 4 KiB pages and three 9-bit directory/page indexes.
        const PWCL: usize = 12 | (9 << 5) | (21 << 10) | (9 << 15) | (30 << 20) | (9 << 25);
        asm!(
            "csrwr {low}, 0x19", // PGDL
            "csrwr {high}, 0x1a", // PGDH
            "csrwr {walk}, 0x1c", // PWCL
            "csrwr $zero, 0x1d", // PWCH
            "csrwr {size}, 0x1e", // STLBPS
            "csrwr {refill}, 0x88", // TLBRENTRY, physical address
            low = inout(reg) token => _,
            high = inout(reg) token => _,
            walk = inout(reg) PWCL => _,
            size = inout(reg) 12usize => _,
            refill = inout(reg) (__loongarch_refill as *const () as usize & PA_MASK) => _,
        );
    }
    flush_tlb();
}

#[inline(always)]
pub fn frame_pointer() -> usize {
    let fp: usize;
    unsafe {
        asm!("move {}, $fp", out(reg) fp);
    }
    fp
}

pub fn read_ticks() -> usize {
    let ticks: usize;
    unsafe {
        asm!("rdtime.d {}, $zero", out(reg) ticks);
    }
    ticks
}

pub fn timer_frequency() -> usize {
    let crystal: usize;
    let scale: usize;
    unsafe {
        asm!("cpucfg {crystal}, {cfg4}", "cpucfg {scale}, {cfg5}",
             crystal = out(reg) crystal, scale = out(reg) scale,
             cfg4 = in(reg) 4usize, cfg5 = in(reg) 5usize);
    }
    crystal * (scale & 0xffff) / ((scale >> 16) & 0xffff)
}

pub fn set_timer(deadline: usize) {
    let ticks = deadline.saturating_sub(read_ticks()).max(4);
    unsafe {
        asm!("csrwr {clear}, 0x44", "csrwr {config}, 0x41",
             clear = inout(reg) 1usize => _,
             config = inout(reg) (ticks & !3 | 1) => _);
    }
}

pub fn enable_timer_interrupt() {
    unsafe {
        asm!("csrxchg {bits}, {mask}, 0x4",
             bits = inout(reg) (1usize << 11) => _,
             mask = in(reg) (1usize << 11));
    }
}

pub fn user_status() -> usize {
    3 | (1 << 2) // PRMD.PPLV=3, PRMD.PIE=1
}

extern "C" {
    fn __loongarch_refill();
    fn __loongarch_kernel_trap();
    fn __loongarch_restore(cx: *mut TrapContext, token: usize) -> !;
}

pub fn install_kernel_trap() {
    unsafe {
        asm!("csrwr {}, 0xc", inout(reg) __loongarch_kernel_trap as *const () as usize => _);
    }
}

pub fn trap_info() -> (TrapCause, usize, usize) {
    let estat: usize;
    let badv: usize;
    let era: usize;
    unsafe {
        asm!("csrrd {}, 0x5", "csrrd {}, 0x7", "csrrd {}, 0x6",
             out(reg) estat, out(reg) badv, out(reg) era);
    }
    let cause = match (estat >> 16) & 0x3f {
        0 if estat & (1 << 11) != 0 => TrapCause::Timer,
        1 | 5 => TrapCause::LoadPageFault,
        2 | 4 => TrapCause::StorePageFault,
        3 | 6 => TrapCause::InstructionPageFault,
        7..=10 => TrapCause::AccessFault,
        11 => TrapCause::Syscall,
        12 => TrapCause::Breakpoint,
        18 => TrapCause::FloatingPoint,
        13..=17 | 19 => TrapCause::IllegalInstruction,
        _ => TrapCause::Unknown(estat),
    };
    (cause, badv, era)
}

pub unsafe fn return_to_user(cx: *mut TrapContext, _: usize, token: usize) -> ! {
    __loongarch_restore(cx, token)
}
