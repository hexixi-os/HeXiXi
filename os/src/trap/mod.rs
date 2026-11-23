mod context;

use crate::config::TRAMPOLINE;
use crate::syscall::syscall;
use crate::task::{
    check_signals_of_current, current_add_signal, current_trap_cx, current_trap_cx_user_va,
    current_user_token, SignalFlags,
};
use crate::timer::{check_timer, set_next_trigger};
use core::arch::{asm, global_asm};
use riscv::register::{
    mtvec::TrapMode,
    scause::{self, Exception, Interrupt, Trap},
    sie, stval, stvec,
};

global_asm!(include_str!("trap.S"));

pub fn init() {
    set_kernel_trap_entry();
}

fn set_kernel_trap_entry() {
    unsafe {
        stvec::write(trap_from_kernel as usize, TrapMode::Direct);
    }
}

fn set_user_trap_entry() {
    unsafe {
        stvec::write(TRAMPOLINE as usize, TrapMode::Direct);
    }
}

pub fn enable_timer_interrupt() {
    unsafe {
        sie::set_stimer();
    }
}

/// 用户态非法访问诊断。是否杀进程由调用方决定：
/// 用户注册了 SIGSEGV handler 时应该投递 handler（lat_sig 的 prot 用例就靠这个）。
fn report_user_fault(cause: Trap, stval: usize) {
    let cx = current_trap_cx();
    println!(
        "[fault] pid={} {:?} sepc={:#x} stval={:#x} sp={:#x} ra={:#x} s0={:#x}",
        crate::task::current_process().getpid(),
        cause,
        cx.sepc,
        stval,
        cx.x[2],
        cx.x[1],
        cx.x[8]
    );
}

fn handle_user_fault(cause: Trap, stval: usize, signal: SignalFlags) {
    report_user_fault(cause, stval);
    let signo = signal.bits().trailing_zeros() as usize;
    if crate::task::current_has_handler(signo) {
        // 用户 handler：投递后由 deliver_signal 跳到 handler，返回后指令重试
        crate::task::current_set_pending(signo);
    } else {
        current_add_signal(signal);
    }
}

#[no_mangle]
pub fn trap_handler() -> ! {
    set_kernel_trap_entry();
    // 在别人的内核栈上析构已退出线程（不能在自己的栈上析构）
    crate::task::drain_recycled_tasks();
    let scause = scause::read();
    let stval = stval::read();
    match scause.cause() {
        Trap::Exception(Exception::UserEnvCall) => {
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            cx.sepc += 4;
            // get system call return value
            let id = cx.x[17];
            let result = syscall(
                id,
                [cx.x[10], cx.x[11], cx.x[12], cx.x[13], cx.x[14], cx.x[15]],
            );
            crate::task::current_task()
                .unwrap()
                .inner_exclusive_access()
                .user_io_frames
                .clear();
            // cx is changed during sys_exec, so we have to call it again
            cx = current_trap_cx();
            if id != 139 {
                // 139 = rt_sigreturn，已恢复完整上下文，不能覆盖 a0
                cx.x[10] = result as usize;
            }
        }
        Trap::Exception(Exception::StorePageFault)
        | Trap::Exception(Exception::LoadPageFault)
        | Trap::Exception(Exception::InstructionPageFault) => {
            use crate::mm::{MapPermission, PageFault};
            let access = match scause.cause() {
                Trap::Exception(Exception::StorePageFault) => MapPermission::W,
                Trap::Exception(Exception::InstructionPageFault) => MapPermission::X,
                _ => MapPermission::R,
            };
            match crate::mm::handle_user_page_fault(stval, access) {
                PageFault::Resolved => {
                    unsafe { asm!("sfence.vma"); }
                    trap_return();
                }
                PageFault::BeyondEof => handle_user_fault(scause.cause(), stval, SignalFlags::SIGBUS),
                PageFault::Invalid => handle_user_fault(scause.cause(), stval, SignalFlags::SIGSEGV),
            }
        }
        Trap::Exception(Exception::StoreFault)
        | Trap::Exception(Exception::InstructionFault)
        | Trap::Exception(Exception::LoadFault) => {
            handle_user_fault(scause.cause(), stval, SignalFlags::SIGSEGV);
        }
        Trap::Exception(Exception::IllegalInstruction) => {
            current_add_signal(SignalFlags::SIGILL);
        }
        Trap::Interrupt(Interrupt::SupervisorTimer) => {
            set_next_trigger();
            crate::syscall::signal::check_alarm();
            check_timer();
            // 时间片轮转：每次时钟中断让出 CPU。
            // 之前只在系统调用/阻塞点调度，纯用户态死循环（lmbench 的 benchmp
            // select(0) 轮询、hackbench 压测）会把其它进程饿死。
            crate::task::suspend_current_and_run_next();
        }
        _ => {
            panic!(
                "Unsupported trap {:?}, stval = {:#x}!",
                scause.cause(),
                stval
            );
        }
    }
    crate::syscall::signal::deliver_signal();
    // check signals
    if let Some((errno, msg)) = check_signals_of_current() {
        println!("[kernel] {}", msg);
        crate::task::exit_current_by_signal(-errno);
    }
    trap_return();
}

#[no_mangle]
pub fn trap_return() -> ! {
    set_user_trap_entry();
    let trap_cx_user_va = current_trap_cx_user_va();
    let user_satp = current_user_token();
    extern "C" {
        fn __alltraps();
        fn __restore();
    }
    let restore_va = __restore as usize - __alltraps as usize + TRAMPOLINE;
    unsafe {
        asm!(
            "fence.i",
            "jr {restore_va}",
            restore_va = in(reg) restore_va,
            in("a0") trap_cx_user_va,
            in("a1") user_satp,
            options(noreturn)
        );
    }
}

#[no_mangle]
pub fn trap_from_kernel() -> ! {
    use core::fmt::Write;
    use riscv::register::sepc;

    /// 裸串口 writer：panic 时堆可能已损坏，不能用 println!
    struct RawConsole;
    impl Write for RawConsole {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for b in s.bytes() {
                crate::sbi::console_putchar(b as usize);
            }
            Ok(())
        }
    }

    let mut c = RawConsole;
    let pid = crate::task::current_task()
        .map(|t| t.process.upgrade().map(|p| p.getpid()).unwrap_or(0))
        .unwrap_or(0);
    let _ = write!(
        c,
        "\n[kfault] scause={:?} stval={:#x} sepc={:#x} pid={} last_sys={}\n",
        scause::read().cause(),
        stval::read(),
        sepc::read(),
        pid,
        crate::syscall::LAST_SYSCALL.load(core::sync::atomic::Ordering::Relaxed)
    );
    let _ = write!(c, "[kfault] last syscalls:");
    for i in 0..8 {
        let v = crate::syscall::LAST_SYSCALLS[i].load(core::sync::atomic::Ordering::Relaxed);
        let _ = write!(c, " {}", v);
    }
    let _ = write!(c, "\n");
    panic!("a trap {:?} from kernel!", scause::read().cause());
}

pub use context::TrapContext;
