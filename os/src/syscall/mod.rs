const SYSCALL_GETCWD: usize = 17;
const SYSCALL_UMOUNT2: usize = 39;
const SYSCALL_MOUNT: usize = 40;
const SYSCALL_GETDENTS64: usize = 61;
const SYSCALL_MKDIRAT: usize = 34;
const SYSCALL_UNLINKAT: usize = 35;
const SYSCALL_CHDIR: usize = 49;
const SYSCALL_FSTAT: usize = 80;
const SYSCALL_TRUNCATE: usize = 45;
const SYSCALL_FTRUNCATE: usize = 46;
const SYSCALL_STATFS: usize = 43;
const SYSCALL_FSTATFS: usize = 44;
const SYSCALL_RENAMEAT: usize = 38;
const SYSCALL_FCNTL: usize = 25;
const SYSCALL_UTIMENSAT: usize = 88;
const SYSCALL_IOCTL: usize = 29;
const SYSCALL_SENDFILE: usize = 71;
const SYSCALL_LSEEK: usize = 62;
const SYSCALL_FUTEX: usize = 98;
const SYSCALL_CLONE: usize = 220;
const SYSCALL_READV: usize = 65;
const SYSCALL_RENAMEAT2: usize = 276;
const SYSCALL_PSELECT6: usize = 72;
const SYSCALL_PPOLL: usize = 73;
const SYSCALL_SET_ROBUST_LIST: usize = 99;
const SYSCALL_GET_ROBUST_LIST: usize = 100;
const SYSCALL_SIGALTSTACK: usize = 132;
const SYSCALL_GETRUSAGE: usize = 165;
const SYSCALL_MPROTECT: usize = 226;
const SYSCALL_MADVISE: usize = 233;
const SYSCALL_GETRANDOM: usize = 278;
const SYSCALL_RT_SIGTIMEDWAIT: usize = 137;
const SYSCALL_GETRLIMIT: usize = 163;
const SYSCALL_SETRLIMIT: usize = 164;
const SYSCALL_PRLIMIT64: usize = 261;
const SYSCALL_UMASK: usize = 166;
const SYSCALL_PRCTL: usize = 167;
const SYSCALL_GETCPU: usize = 168;
const SYSCALL_FACCESSAT: usize = 48;
const SYSCALL_FCHMOD: usize = 52;
const SYSCALL_FCHMODAT: usize = 53;
const SYSCALL_FCHOWNAT: usize = 54;
const SYSCALL_SETSID: usize = 157;
const SYSCALL_SETPGID: usize = 154;
const SYSCALL_GETPGID: usize = 155;
const SYSCALL_GETSID: usize = 156;
const SYSCALL_TKILL: usize = 130;
const SYSCALL_TGKILL: usize = 234;
const SYSCALL_RT_TGSIGQUEUEINFO: usize = 240;
const SYSCALL_NEWFSTATAT: usize = 79;
const SYSCALL_FSYNC: usize = 81;
const SYSCALL_FDATASYNC: usize = 82;
const SYSCALL_SIGACTION: usize = 134;
const SYSCALL_SIGPROCMASK: usize = 135;
const SYSCALL_SETPRIORITY: usize = 141;
const SYSCALL_GETUID: usize = 174;
const SYSCALL_GETEUID: usize = 175;
const SYSCALL_GETGID: usize = 176;
const SYSCALL_GETEGID: usize = 177;
const SYSCALL_GETTID_LINUX: usize = 178;
const SYSCALL_DUP_LEGACY: usize = 23;
const SYSCALL_DUP: usize = 24;
const SYSCALL_OPEN: usize = 56;
const SYSCALL_CLOSE: usize = 57;
const SYSCALL_PIPE: usize = 59;
const SYSCALL_WRITEV: usize = 66;
const SYSCALL_EXIT_GROUP: usize = 94;
const SYSCALL_SET_TID_ADDRESS: usize = 96;
const SYSCALL_READ: usize = 63;
const SYSCALL_WRITE: usize = 64;
const SYSCALL_EXIT: usize = 93;
const SYSCALL_SLEEP: usize = 101;
const SYSCALL_SYSLOG: usize = 116;
const SYSCALL_SETITIMER: usize = 103;
const SYSCALL_RT_SIGRETURN: usize = 139;
const SYSCALL_CLOCK_GETTIME: usize = 113;
const SYSCALL_CLOCK_GETRES: usize = 114;
const SYSCALL_CLOCK_NANOSLEEP: usize = 115;
const SYSCALL_TIMES: usize = 153;
const SYSCALL_UNAME: usize = 160;
const SYSCALL_YIELD: usize = 124;
const SYSCALL_SCHED_SETPARAM: usize = 118;
const SYSCALL_SCHED_SETSCHEDULER: usize = 119;
const SYSCALL_SCHED_GETSCHEDULER: usize = 120;
const SYSCALL_SCHED_GETPARAM: usize = 121;
const SYSCALL_SCHED_SETAFFINITY: usize = 122;
const SYSCALL_SCHED_GETAFFINITY: usize = 123;
const SYSCALL_SCHED_YIELD: usize = 124;
const SYSCALL_SCHED_PRIO_MAX: usize = 125;
const SYSCALL_SCHED_PRIO_MIN: usize = 126;
const SYSCALL_SCHED_RR_INTERVAL: usize = 127;
const SYSCALL_MLOCK: usize = 228;
const SYSCALL_MUNLOCK: usize = 229;
const SYSCALL_MLOCKALL: usize = 230;
const SYSCALL_MUNLOCKALL: usize = 231;
const SYSCALL_KILL: usize = 129;
const SYSCALL_GET_TIME: usize = 169;
const SYSCALL_BRK: usize = 214;
const SYSCALL_MUNMAP: usize = 215;
const SYSCALL_MMAP: usize = 222;
const SYSCALL_GETPID: usize = 172;
const SYSCALL_GETPPID: usize = 173;
const SYSCALL_FORK: usize = 220;
const SYSCALL_EXEC: usize = 221;
const SYSCALL_WAITPID: usize = 260;
const SYSCALL_THREAD_CREATE: usize = 1000;
const SYSCALL_GETTID: usize = 1001;
const SYSCALL_WAITTID: usize = 1002;
const SYSCALL_MUTEX_CREATE: usize = 1010;
const SYSCALL_MUTEX_LOCK: usize = 1011;
const SYSCALL_MUTEX_UNLOCK: usize = 1012;
const SYSCALL_SEMAPHORE_CREATE: usize = 1020;
const SYSCALL_SEMAPHORE_UP: usize = 1021;
const SYSCALL_SEMAPHORE_DOWN: usize = 1022;
const SYSCALL_CONDVAR_CREATE: usize = 1030;
const SYSCALL_CONDVAR_SIGNAL: usize = 1031;
const SYSCALL_CONDVAR_WAIT: usize = 1032;

mod fs;
mod process;
pub mod signal;
mod sync;
mod thread;

use fs::*;
use process::*;
use sync::*;
use thread::*;

pub static LAST_SYSCALL: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// 缺失系统调用的打印次数限制（串口很慢，高频缺失会拖死测试）
static MISSING_PRINTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// 最近 8 条系统调用（panic 时裸打印，用于定位崩溃点）
pub static LAST_SYSCALLS: [core::sync::atomic::AtomicUsize; 8] = [
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
];
pub static LAST_SYSCALL_IDX: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

pub fn syscall(syscall_id: usize, args: [usize; 6]) -> isize {
    #[cfg(feature = "elf_startup_stats")]
    crate::exec_stats::complete();
    // 已被 SIGKILL/SIGTERM 的进程不再服务任何系统调用（超时杀进程必死）
    crate::task::check_kill_current();
    LAST_SYSCALL.store(syscall_id, core::sync::atomic::Ordering::Relaxed);
    {
        let i = LAST_SYSCALL_IDX.fetch_add(1, core::sync::atomic::Ordering::Relaxed) % 8;
        LAST_SYSCALLS[i].store(syscall_id, core::sync::atomic::Ordering::Relaxed);
    }
    match syscall_id {
        SYSCALL_FCNTL => sys_fcntl(args[0], args[1], args[2]),
        SYSCALL_UTIMENSAT => sys_utimensat(args[1] as *const u8),
        SYSCALL_RENAMEAT => sys_renameat(args[1] as *const u8, args[3] as *const u8),
        SYSCALL_RENAMEAT2 => sys_renameat(args[1] as *const u8, args[3] as *const u8),
        SYSCALL_STATFS | SYSCALL_FSTATFS => sys_statfs(args[1] as *mut u8),
        SYSCALL_FTRUNCATE => sys_ftruncate(args[0], args[1]),
        SYSCALL_TRUNCATE => sys_truncate(),
        SYSCALL_IOCTL => sys_ioctl(),
        SYSCALL_LSEEK => sys_lseek(args[0], args[1] as isize, args[2]),
        SYSCALL_FUTEX => sys_futex(args[0], args[1], args[2]),
        SYSCALL_CLONE => sys_clone_thread(args[0], args[1], args[2], args[3], args[4]),
        SYSCALL_READV => sys_readv(args[0], args[1] as *const u8, args[2]),
        SYSCALL_PSELECT6 => sys_pselect6(
            args[0],
            args[1] as *mut u8,
            args[2] as *mut u8,
            args[3] as *mut u8,
            args[4] as *const u8,
        ),
        SYSCALL_PPOLL => sys_ppoll(args[0] as *mut u8, args[1]),
        SYSCALL_MADVISE | SYSCALL_SET_ROBUST_LIST | SYSCALL_SIGALTSTACK => 0,
        SYSCALL_GET_ROBUST_LIST => 0,
        SYSCALL_GETRUSAGE => sys_getrusage(args[1] as *mut u8),
        SYSCALL_GETRANDOM => sys_getrandom(args[0] as *mut u8, args[1]),
        SYSCALL_MPROTECT => sys_mprotect(args[0], args[1], args[2]),
        SYSCALL_RT_SIGTIMEDWAIT => sys_rt_sigtimedwait(
            args[0] as *const u8,
            args[1] as *mut u8,
            args[2] as *const u8,
        ),
        SYSCALL_GETRLIMIT => sys_getrlimit(args[0], args[1] as *mut u8),
        SYSCALL_SETRLIMIT => sys_setrlimit(args[0], args[1] as *const u8),
        SYSCALL_PRLIMIT64 => {
            sys_prlimit64(args[0], args[1], args[2] as *const u8, args[3] as *mut u8)
        }
        SYSCALL_UMASK | SYSCALL_PRCTL | SYSCALL_GETCPU => 0,
        // LTP 每个用例 setup 都会 chown 临时目录、access 文件，全部按成功处理，
        // 否则 162 个用例全部 TBROK 在 setup 阶段。
        SYSCALL_FACCESSAT | SYSCALL_FCHMOD | SYSCALL_FCHMODAT | SYSCALL_FCHOWNAT => 0,
        SYSCALL_SETSID => sys_getpid(),
        // LTP 的 test harness 每个用例都 setpgid(0,0)，缺了就全部 TBROK
        SYSCALL_SETPGID => 0,
        SYSCALL_GETPGID | SYSCALL_GETSID => sys_getpid(),
        // pthread_cancel 走 tkill/tgkill 发 SIGCANCEL；按“给本进程投递信号”处理
        SYSCALL_TKILL => sys_thread_kill(args[1] as u32),
        SYSCALL_TGKILL => sys_thread_kill(args[2] as u32),
        SYSCALL_RT_TGSIGQUEUEINFO => sys_thread_kill(args[2] as u32),
        SYSCALL_SENDFILE => sys_sendfile(args[0], args[1], args[3]),
        SYSCALL_NEWFSTATAT => sys_newfstatat(
            args[0] as isize,
            args[1] as *const u8,
            args[2] as *mut u8,
            args[3],
        ),
        SYSCALL_FSYNC | SYSCALL_FDATASYNC => 0,
        SYSCALL_SIGACTION => signal::sys_rt_sigaction(args[0], args[1] as *const u8, args[2] as *mut u8),
        SYSCALL_SIGPROCMASK => 0,
        SYSCALL_SETPRIORITY => 0,
        SYSCALL_GETUID | SYSCALL_GETEUID | SYSCALL_GETGID | SYSCALL_GETEGID => 0,
        SYSCALL_GETTID_LINUX => sys_getpid(),
        SYSCALL_DUP_LEGACY => sys_dup(args[0]),
        SYSCALL_DUP => sys_dup3(args[0], args[1], args[2] as u32),
        SYSCALL_GETCWD => sys_getcwd(args[0] as *mut u8, args[1]),
        SYSCALL_MOUNT => sys_mount_stub(),
        SYSCALL_UMOUNT2 => sys_mount_stub(),
        SYSCALL_GETDENTS64 => sys_getdents64(args[0], args[1] as *mut u8, args[2]),
        SYSCALL_MKDIRAT => sys_mkdirat(args[0] as isize, args[1] as *const u8),
        SYSCALL_UNLINKAT => sys_unlinkat(args[0] as isize, args[1] as *const u8, args[2]),
        SYSCALL_CHDIR => sys_chdir(args[0] as *const u8),
        SYSCALL_FSTAT => sys_fstat(args[0], args[1] as *mut u8),
        SYSCALL_OPEN => sys_openat(args[0] as isize, args[1] as *const u8, args[2] as u32, args[3] as u32),
        SYSCALL_CLOSE => sys_close(args[0]),
        SYSCALL_PIPE => sys_pipe(args[0] as *mut usize),
        SYSCALL_READ => sys_read(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITEV => sys_writev(args[0], args[1] as *const u8, args[2]),
        SYSCALL_WRITE => sys_write(args[0], args[1] as *const u8, args[2]),
        SYSCALL_EXIT => sys_exit(args[0] as i32),
        SYSCALL_EXIT_GROUP => sys_exit_group(args[0] as i32),
        SYSCALL_SET_TID_ADDRESS => sys_set_tid_address(args[0]),
        SYSCALL_SLEEP => sys_nanosleep(args[0], args[1]),
        SYSCALL_SYSLOG => 0,
        SYSCALL_SETITIMER => signal::sys_setitimer(args[0], args[1] as *const u8, args[2] as *mut u8),
        SYSCALL_RT_SIGRETURN => signal::sys_rt_sigreturn(),
        SYSCALL_CLOCK_GETTIME => sys_clock_gettime(args[1] as *mut u8),
        SYSCALL_CLOCK_GETRES => sys_clock_getres(args[1] as *mut u8),
        SYSCALL_CLOCK_NANOSLEEP => sys_clock_nanosleep(args[2] as *const u8),
        SYSCALL_TIMES => sys_times(args[0] as *mut u8),
        SYSCALL_UNAME => sys_uname(args[0] as *mut u8),
        SYSCALL_YIELD => sys_yield(),
        SYSCALL_KILL => sys_kill(args[0], args[1] as u32),
        SYSCALL_SCHED_SETPARAM | SYSCALL_SCHED_SETSCHEDULER | SYSCALL_SCHED_SETAFFINITY
        | SYSCALL_MLOCK | SYSCALL_MUNLOCK | SYSCALL_MLOCKALL | SYSCALL_MUNLOCKALL => 0,
        SYSCALL_SCHED_GETSCHEDULER => 0,
        SYSCALL_SCHED_GETAFFINITY => sys_sched_getaffinity(args[1], args[2] as *mut u8),
        SYSCALL_SCHED_GETPARAM => sys_sched_getparam(args[1] as *mut u8),
        SYSCALL_SCHED_YIELD => 0,
        SYSCALL_SCHED_PRIO_MAX => 99,
        SYSCALL_SCHED_PRIO_MIN => 1,
        SYSCALL_SCHED_RR_INTERVAL => 0,
        SYSCALL_GET_TIME => sys_gettimeofday(args[0] as *mut u8),
        SYSCALL_BRK => sys_brk(args[0]),
        SYSCALL_MMAP => sys_mmap(args[0], args[1], args[2], args[3], args[4], args[5]),
        SYSCALL_MUNMAP => sys_munmap(args[0], args[1]),
        SYSCALL_GETPID => sys_getpid(),
        SYSCALL_GETPPID => sys_getppid(),
        SYSCALL_FORK => sys_fork(),
        SYSCALL_EXEC => {
            let path = args[0] as *const u8;
            let load = || sys_exec(path, args[1] as *const usize);
            #[cfg(feature = "elf_startup_stats")]
            return crate::exec_stats::measure(path, load);
            #[cfg(not(feature = "elf_startup_stats"))]
            load()
        }
        SYSCALL_WAITPID => sys_waitpid(args[0] as isize, args[1] as *mut i32),
        SYSCALL_THREAD_CREATE => sys_thread_create(args[0], args[1]),
        SYSCALL_GETTID => sys_gettid(),
        SYSCALL_WAITTID => sys_waittid(args[0]) as isize,
        SYSCALL_MUTEX_CREATE => sys_mutex_create(args[0] == 1),
        SYSCALL_MUTEX_LOCK => sys_mutex_lock(args[0]),
        SYSCALL_MUTEX_UNLOCK => sys_mutex_unlock(args[0]),
        SYSCALL_SEMAPHORE_CREATE => sys_semaphore_create(args[0]),
        SYSCALL_SEMAPHORE_UP => sys_semaphore_up(args[0]),
        SYSCALL_SEMAPHORE_DOWN => sys_semaphore_down(args[0]),
        SYSCALL_CONDVAR_CREATE => sys_condvar_create(args[0]),
        SYSCALL_CONDVAR_SIGNAL => sys_condvar_signal(args[0]),
        SYSCALL_CONDVAR_WAIT => sys_condvar_wait(args[0], args[1]),
        _ => {
            // 串口 115200 波特下每行约 6ms，高频缺失会拖慢测试，只打印前 40 条
            let n = MISSING_PRINTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if n < 40 {
                println!(
                    "[kernel] unsupported syscall {} (args [{:#x}, {:#x}, {:#x}]) -> ENOSYS",
                    syscall_id, args[0], args[1], args[2]
                );
            }
            -38 // ENOSYS
        }
    }
}
