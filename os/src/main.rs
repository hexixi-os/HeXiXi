#![no_std]
#![no_main]
#![feature(panic_info_message)]
#![feature(alloc_error_handler)]

extern crate alloc;

#[macro_use]
extern crate bitflags;

#[cfg(feature = "board_k210")]
#[path = "boards/k210.rs"]
mod board;
#[cfg(not(any(feature = "board_k210")))]
#[path = "boards/qemu.rs"]
mod board;

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
mod sbi;
mod sync;
mod syscall;
mod task;
mod timer;
mod trap;

use core::arch::global_asm;

global_asm!(include_str!("entry.asm"));

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
    println!("[kernel] Hello, world!");
    match fdt::detect_ram_end() {
        Some(end) => {
            // 末尾留 1MB 余量，避免越过 RAM
            let end = end.saturating_sub(0x10_0000);
            mm::set_ram_end(end);
            println!("[kernel] DTB memory end: {:#x}", end);
        }
        None => println!("[kernel] DTB memory: not found, using default"),
    }
    mm::init();
    mm::remap_test();
    trap::init();
    trap::enable_timer_interrupt();
    timer::set_next_trigger();
    let is_ext4 = fs::ext4::probe();
    if !is_ext4 {
        fs::list_apps();
    }
    task::add_initproc();
    task::run_tasks();
    panic!("Unreachable in rust_main!");
}
