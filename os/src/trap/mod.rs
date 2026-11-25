mod context;

use crate::arch::{self, TrapCause, REG_A0, REG_FP, REG_SP, REG_SYSCALL};
use crate::syscall::syscall;
use crate::task::{check_signals_of_current, current_trap_cx};
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
            let (id, args, syscall_pc) = {
                let cx = current_trap_cx();
                let syscall_pc = cx.sepc;
                cx.sepc += 4;
                // get system call return value
                (
                    cx.x[REG_SYSCALL],
                    cx.x[REG_A0..REG_A0 + 6].try_into().unwrap(),
                    syscall_pc,
                )
            };
            let result = syscall(id, args);
            // cx is changed during sys_exec, so we have to fetch it again
            let cx = {
                let task = crate::task::current_task().unwrap();
                let mut inner = task.inner_exclusive_access();
                if !inner.user_io_frames.is_empty() {
                    inner.user_io_frames.clear();
                }
                inner.user_io_file = None;
                inner.get_trap_cx()
            };
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
            check_timer();
            set_next_trigger();
            // 时间片轮转：仅在就绪队列有其它可运行任务且当前任务已运行足够时间片时让出 CPU，
            // 避免单任务基准测试空切产生抖动，也避免刚切入的子任务在残余 tick 上立刻被抢占。
            if crate::task::has_ready_task() && crate::task::current_slice_elapsed() {
                crate::task::suspend_current_and_run_next();
            }
        }
        TrapCause::Unknown(raw) => {
            panic!("Unsupported trap {:#x}, stval = {:#x}!", raw, address);
        }
    }
    if crate::syscall::signal::deliver_signal(restart) {
        if let Some((errno, msg)) = check_signals_of_current() {
            println!("[kernel] {}", msg);
            crate::task::exit_current_by_signal(-errno);
        }
    }
    trap_return();
}

#[no_mangle]
pub fn trap_return() -> ! {
    let task = crate::task::current_task().unwrap();
    let user_token = task.get_user_token();
    let (trap_cx, trap_cx_user_va, child_tid) = {
        let mut inner = task.inner_exclusive_access();
        let trap_cx = inner.get_trap_cx();
        let trap_cx_user_va = inner.res.as_ref().unwrap().trap_cx_user_va();
        let child_tid = inner.set_child_tid.take();
        (trap_cx, trap_cx_user_va, child_tid)
    };
    if let Some(address) = child_tid {
        let _ = crate::mm::try_write_user(user_token, address as *mut i32, task.linux_tid() as i32);
    }
    drop(task);
    unsafe { arch::return_to_user(trap_cx, trap_cx_user_va, user_token) }
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
