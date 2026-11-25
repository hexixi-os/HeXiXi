mod context;

use crate::arch::{self, TrapCause, REG_A0, REG_FP, REG_SP, REG_SYSCALL};
use crate::syscall::syscall;
use crate::task::{
    check_signals_of_current, current_trap_cx, current_trap_cx_user_va, current_user_token,
};
use crate::timer::{check_timer, set_next_trigger};

pub fn init() {
    arch::install_kernel_trap();
}

pub use arch::enable_timer_interrupt;

/// 用户态非法访问诊断。是否杀进程由调用方决定：
/// 用户注册了 SIGSEGV handler 时应该投递 handler（lat_sig 的 prot 用例就靠这个）。
fn report_user_fault(cause: TrapCause, stval: usize) {
    let cx = current_trap_cx();
    println!(
        "[fault] pid={} {:?} sepc={:#x} stval={:#x} sp={:#x} ra={:#x} s0={:#x}",
        crate::task::current_process().getpid(),
        cause,
        cx.sepc,
        stval,
        cx.x[REG_SP],
        cx.x[1],
        cx.x[REG_FP]
    );
}

fn handle_user_fault(cause: TrapCause, stval: usize, signo: usize, code: i32) {
    if !crate::task::current_has_handler(signo) {
        report_user_fault(cause, stval);
    }
    crate::syscall::signal::queue_fault(signo, code, stval);
}

#[no_mangle]
pub fn trap_handler() -> ! {
    arch::install_kernel_trap();
    crate::sync::enter_kernel();
    // 在别人的内核栈上析构已退出线程（不能在自己的栈上析构）
    crate::task::drain_recycled_tasks();
    let (cause, address, pc) = arch::trap_info();
    let mut restart = None;
    match cause {
        TrapCause::Syscall => {
            // jump to next instruction anyway
            let mut cx = current_trap_cx();
            let syscall_pc = cx.sepc;
            cx.sepc += 4;
            // get system call return value
            let id = cx.x[REG_SYSCALL];
            let args = cx.x[REG_A0..REG_A0 + 6].try_into().unwrap();
            let result = syscall(id, args);
            {
                let task = crate::task::current_task().unwrap();
                let mut inner = task.inner_exclusive_access();
                inner.user_io_frames.clear();
                inner.user_io_file = None;
            }
            // cx is changed during sys_exec, so we have to call it again
            cx = current_trap_cx();
            let replaced_context = matches!(
                id,
                crate::syscall::SYSCALL_RT_SIGRETURN | crate::syscall::SYSCALL_EXEC
            );
            if !replaced_context || result < 0 {
                // Successful execve and rt_sigreturn have installed the full context.
                cx.x[REG_A0] = result as usize;
            }
            if result == -4 && crate::syscall::restartable_syscall(id, args) {
                restart = Some(crate::syscall::signal::Restart {
                    pc: syscall_pc,
                    args,
                });
            }
        }
        TrapCause::StorePageFault | TrapCause::LoadPageFault | TrapCause::InstructionPageFault => {
            use crate::mm::{MapPermission, PageFault};
            let access = match cause {
                TrapCause::StorePageFault => MapPermission::W,
                TrapCause::InstructionPageFault => MapPermission::X,
                _ => MapPermission::R,
            };
            match crate::mm::handle_user_page_fault(address, access) {
                PageFault::Resolved => {
                    arch::local_flush_tlb_page(address);
                    trap_return();
                }
                PageFault::FileUnavailable => handle_user_fault(cause, address, 7, 2),
                PageFault::Invalid => handle_user_fault(cause, address, 11, 1),
            }
        }
        TrapCause::AccessFault => {
            handle_user_fault(cause, address, 11, 1);
        }
        TrapCause::IllegalInstruction => {
            handle_user_fault(cause, pc, 4, 1);
        }
        #[cfg(target_arch = "loongarch64")]
        TrapCause::Breakpoint => handle_user_fault(cause, pc, 5, 1),
        #[cfg(target_arch = "loongarch64")]
        TrapCause::FloatingPoint => handle_user_fault(cause, pc, 8, 1),
        TrapCause::Timer => {
            set_next_trigger();
            check_timer();
            // 时间片轮转：每次时钟中断让出 CPU。
            // 之前只在系统调用/阻塞点调度，纯用户态死循环（lmbench 的 benchmp
            // select(0) 轮询、hackbench 压测）会把其它进程饿死。
            crate::task::suspend_current_and_run_next();
        }
        TrapCause::Unknown(raw) => {
            panic!("Unsupported trap {:#x}, stval = {:#x}!", raw, address);
        }
    }
    crate::syscall::signal::deliver_signal(restart);
    // check signals
    if let Some((errno, msg)) = check_signals_of_current() {
        println!("[kernel] {}", msg);
        crate::task::exit_current_by_signal(-errno);
    }
    trap_return();
}

#[no_mangle]
pub fn trap_return() -> ! {
    let trap_cx_user_va = current_trap_cx_user_va();
    let user_token = current_user_token();
    let task = crate::task::current_task().unwrap();
    let child_tid = task.inner_exclusive_access().set_child_tid.take();
    if let Some(address) = child_tid {
        let _ = crate::mm::try_write_user(user_token, address as *mut i32, task.linux_tid() as i32);
    }
    drop(task);
    unsafe { arch::return_to_user(current_trap_cx(), trap_cx_user_va, user_token) }
}

#[no_mangle]
pub fn trap_from_kernel() -> ! {
    use core::fmt::Write;
    let (cause, address, pc) = arch::trap_info();

    /// 裸串口 writer：panic 时堆可能已损坏，不能用 println!
    struct RawConsole;
    impl Write for RawConsole {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for b in s.bytes() {
                crate::arch::console_putchar(b as usize);
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
        cause,
        address,
        pc,
        pid,
        crate::syscall::LAST_SYSCALL.load(core::sync::atomic::Ordering::Relaxed)
    );
    let _ = write!(c, "[kfault] last syscalls:");
    for i in 0..8 {
        let v = crate::syscall::LAST_SYSCALLS[i].load(core::sync::atomic::Ordering::Relaxed);
        let _ = write!(c, " {}", v);
    }
    let _ = write!(c, "\n");
    panic!("a trap {:?} from kernel!", cause);
}

pub use context::TrapContext;
