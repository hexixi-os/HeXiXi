use super::{activate_kernel_table, read_ticks, sbi, timer_frequency};
use crate::println;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const MAX_HARTS: usize = 8;
const SUPPORTED_HARTS: usize = (1 << MAX_HARTS) - 1;

static ONLINE: AtomicUsize = AtomicUsize::new(0);
static NEXT_HART: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn boot(configured: usize) {
    extern "C" {
        fn _start_secondary();
    }
    let boot_hart = super::hart_id();
    let local = 1 << boot_hart;
    let requested = (configured & SUPPORTED_HARTS) | local;
    ONLINE.store(local, Ordering::Release);
    println!("[smp] hart {} online (boot)", boot_hart);
    let token = crate::mm::kernel_token();
    for hart in 0..MAX_HARTS {
        if (requested & !local) & (1 << hart) != 0 {
            sbi::hart_start(hart, _start_secondary as usize, token)
                .unwrap_or_else(|error| panic!("SBI hart_start({}) returned {}", hart, error));
        }
    }
    let start = read_ticks();
    while ONLINE.load(Ordering::Acquire) != requested {
        assert!(
            read_ticks().wrapping_sub(start) < timer_frequency() * 2,
            "SMP boot timed out, expected harts {:#x}, online {:#x}",
            requested,
            ONLINE.load(Ordering::Relaxed)
        );
        core::hint::spin_loop();
    }
    // Assign the first fork to the hart after the boot hart in the online set.
    NEXT_HART.store(
        (requested & (local - 1)).count_ones() as usize + 1,
        Ordering::Relaxed,
    );
}

#[no_mangle]
extern "C" fn rust_secondary(kernel_token: usize) -> ! {
    activate_kernel_table(kernel_token);
    crate::trap::init();
    crate::trap::enable_timer_interrupt();
    crate::timer::set_next_trigger();
    let hart = super::hart_id();
    ONLINE.fetch_or(1 << hart, Ordering::Release);
    crate::sync::enter_kernel();
    println!("[smp] hart {} online", hart);
    crate::task::run_tasks()
}

pub(crate) fn next_hart() -> usize {
    let mut mask = ONLINE.load(Ordering::Acquire);
    let slot = NEXT_HART.fetch_add(1, Ordering::Relaxed) % mask.count_ones() as usize;
    for _ in 0..slot {
        mask &= mask - 1;
    }
    mask.trailing_zeros() as usize
}

pub(super) fn flush_remote() {
    let remote = ONLINE.load(Ordering::Acquire) & !(1 << super::hart_id());
    if remote != 0 {
        // OpenSBI acknowledges completion on the target harts before returning.
        sbi::remote_sfence_vma(remote)
            .unwrap_or_else(|error| panic!("SBI remote_sfence_vma returned {}", error));
    }
}
