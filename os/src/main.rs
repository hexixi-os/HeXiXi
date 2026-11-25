#![no_std]
#![no_main]
#![cfg_attr(target_arch = "riscv64", feature(panic_info_message))]
#![feature(alloc_error_handler)]
#![cfg_attr(feature = "smp", feature(asm_const))]

#[cfg(all(feature = "smp", not(target_arch = "riscv64")))]
compile_error!("SMP is supported on RISC-V QEMU virt");

extern crate alloc;

#[macro_use]
extern crate bitflags;

#[cfg(target_arch = "riscv64")]
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
mod net;
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
        let start = sbss as *const () as usize;
        let end = ebss as *const () as usize;
        core::slice::from_raw_parts_mut(start as *mut u8, end - start).fill(0);
    }
}

#[no_mangle]
pub fn rust_main() -> ! {
    clear_bss();
    sync::enter_kernel();
    println!("[kernel] Hello, world!");
    let boot = arch::boot_info();
    #[cfg(target_arch = "riscv64")]
    if let Some(freq) = boot.timebase_freq {
        arch::set_timer_frequency(freq);
    }
    config::configure_legacy_oscomp_image(boot.legacy_oscomp_image);
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
    fs::init_runtime_dirs();
    net::init();
    task::add_initproc(boot.oscomp_group());
    task::run_tasks()
}
