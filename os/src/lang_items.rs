use crate::arch::shutdown;
use crate::task::current_kstack_top;
use core::fmt::{self, Write};
use core::panic::PanicInfo;

/// 裸串口输出：panic 时堆/格式化链路可能已损坏，这里完全不分配、不用 print!。
struct RawConsole;

impl Write for RawConsole {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            crate::arch::console_putchar(b as usize);
        }
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut c = RawConsole;
    let pid = crate::task::current_task()
        .map(|t| t.process.upgrade().map(|p| p.getpid()).unwrap_or(0))
        .unwrap_or(0);
    let _ = write!(c, "\n[kernel] PANIC pid={}", pid);
    if let Some(location) = info.location() {
        let _ = write!(c, " at {}:{}", location.file(), location.line());
    }
    #[cfg(target_arch = "riscv64")]
    if let Some(message) = info.message() {
        let _ = write!(c, " msg={}", message);
    }
    #[cfg(target_arch = "loongarch64")]
    let _ = write!(c, " msg={}", info.message());
    let _ = write!(c, "\n[kernel] last syscalls:");
    for i in 0..8 {
        let v = crate::syscall::LAST_SYSCALLS[i].load(core::sync::atomic::Ordering::Relaxed);
        let _ = write!(c, " {}", v);
    }
    let _ = write!(c, "\n");
    unsafe {
        backtrace(&mut c);
    }
    shutdown()
}

#[inline(never)]
unsafe fn backtrace(c: &mut RawConsole) {
    let mut fp = crate::arch::frame_pointer();
    let stop = current_kstack_top();
    let _ = write!(c, "---START BACKTRACE---\n");
    for i in 0..10 {
        if fp == stop || fp == 0 {
            break;
        }
        let ra = *((fp - 8) as *const usize);
        let _ = write!(c, "#{}:ra={:#x}\n", i, ra);
        fp = *((fp - 16) as *const usize);
    }
    let _ = write!(c, "---END   BACKTRACE---\n");
}
