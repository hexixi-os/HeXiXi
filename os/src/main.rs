#![no_std]
#![no_main]
#![cfg_attr(target_arch = "riscv64", feature(panic_info_message))]
#![feature(alloc_error_handler)]
#![cfg_attr(feature = "smp", feature(asm_const))]

#[cfg(all(feature = "smp", not(all(target_arch = "riscv64", feature = "board_qemu")),))]
compile_error!("SMP is supported on RISC-V QEMU virt");

extern crate alloc;

#[macro_use]
extern crate bitflags;

#[cfg(all(target_arch = "riscv64", feature = "board_k210"))]
#[path = "boards/k210.rs"]
mod board;
#[cfg(all(target_arch = "riscv64", not(feature = "board_k210")))]
#[path = "boards/qemu.rs"]
mod board;
#[cfg(target_arch = "loongarch64")]
#[path = "boards/loongarch_virt.rs"]
mod board;

mod arch;

#[macro_use]
mod console;
mod config;
mod drivers;
#[cfg(feature = "elf_startup_stats")]
mod exec_stats;
mod fdt;
#[cfg(feature = "elf_startup_stats")]
mod fork_stats;
mod fs;
mod lang_items;
mod mm;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;

fn clear_bss() {
    extern "C" {
        fn sbss();
        fn ebss();
    }
    unsafe {
        core::slice::from_raw_parts_mut(sbss as usize as *mut u8, ebss as usize - sbss as usize)
            .fill(0);
    }
}

#[no_mangle]
pub fn rust_main() -> ! {
    clear_bss();
    sync::enter_kernel();
    println!("[kernel] Hello, world!");
    let boot = arch::boot_info();
    match boot.ram_end {
        Some(end) => {
            // 末尾留 1MB 余量，避免越过 RAM
            let end = end.saturating_sub(0x10_0000);
            mm::set_ram_end(end);
            println!("[kernel] DTB memory end: {:#x}", end);
        }
        None => println!("[kernel] Using default RAM end: {:#x}", config::MEMORY_END),
    }
    mm::init();
    #[cfg(target_arch = "riscv64")]
    mm::remap_test();
    trap::init();
    trap::enable_timer_interrupt();
    timer::set_next_trigger();
    #[cfg(feature = "smp")]
    arch::boot_smp(boot.hart_mask);
    let is_ext4 = fs::ext4::probe();
    if !is_ext4 {
        fs::list_apps();
    }
    task::add_initproc(boot.oscomp_group());
    task::run_tasks()
}
