use super::id::{try_kstack_alloc, RecycleAllocator};
use super::manager::insert_into_pid2process;
use super::TaskControlBlock;
use super::{add_task, SignalFlags};
use super::{pid_alloc, PidHandle};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::{DirectoryLocation, Executable, File, Stdin, Stdout};
use crate::mm::{translated_refmut, MemorySet, KERNEL_SPACE};
use crate::sync::{Condvar, Mutex, Semaphore, UPSafeCell};
use crate::trap::{trap_handler, TrapContext};
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefMut;

pub struct ProcessControlBlock {
    // immutable
    pub pid: PidHandle,
    // mutable
    inner: UPSafeCell<ProcessControlBlockInner>,
}

pub struct ProcessControlBlockInner {
    pub is_zombie: bool,
    pub memory_set: MemorySet,
    pub parent: Option<Weak<ProcessControlBlock>>,
    pub children: Vec<Arc<ProcessControlBlock>>,
    pub wait_status: i32,
    pub fd_table: Vec<Option<Arc<dyn File + Send + Sync>>>,
    pub signals: SignalFlags,
    pub tasks: Vec<Option<Arc<TaskControlBlock>>>,
    pub task_res_allocator: RecycleAllocator,
    pub mutex_list: Vec<Option<Arc<dyn Mutex>>>,
    pub semaphore_list: Vec<Option<Arc<Semaphore>>>,
    pub condvar_list: Vec<Option<Arc<Condvar>>>,
    /// 当前工作目录
    pub cwd: DirectoryLocation,
    /// 堆起始（可增长下界）
    pub brk_base: usize,
    /// 当前 program break
    pub brk: usize,
    /// mmap 分配起点（向上增长）
    pub mmap_top: usize,
    /// SIGALRM 到期时间（ms，0 表示未设置）
    pub alarm_deadline: usize,
    /// 每个信号号的用户 handler（0 = SIG_DFL，1 = SIG_IGN）
    pub signal_handlers: [usize; 32],
    /// 待投递信号位图（bit = signo）
    pub signal_pending: u32,
    /// 正在执行 SIGALRM handler（避免重入）
    pub in_sigalrm: bool,
    /// RLIMIT_NOFILE 的当前上限（fd 表硬上限；setrlimit/prlimit64 会改）
    pub nofile_limit: usize,
    #[cfg(feature = "elf_startup_stats")]
    pub(crate) pending_exec: Option<crate::exec_stats::PendingExec>,
}

impl ProcessControlBlockInner {
    #[allow(unused)]
    pub fn get_user_token(&self) -> usize {
        self.memory_set.token()
    }

    /// 分配一个 fd；已到 RLIMIT_NOFILE 上限时返回 None（调用方返回 EMFILE）
    pub fn alloc_fd(&mut self) -> Option<usize> {
        if let Some(fd) = (0..self.fd_table.len()).find(|fd| self.fd_table[*fd].is_none()) {
            Some(fd)
        } else if self.fd_table.len() < self.nofile_limit {
            self.fd_table.push(None);
            Some(self.fd_table.len() - 1)
        } else {
            None
        }
    }

    pub fn alloc_tid(&mut self) -> usize {
        self.task_res_allocator.alloc()
    }

    pub fn dealloc_tid(&mut self, tid: usize) {
        self.task_res_allocator.dealloc(tid)
    }

    pub fn thread_count(&self) -> usize {
        self.tasks.iter().filter(|task| task.is_some()).count()
    }

    pub fn get_task(&self, tid: usize) -> Arc<TaskControlBlock> {
        self.tasks[tid].as_ref().unwrap().clone()
    }
}

impl ProcessControlBlock {
    pub fn inner_exclusive_access(&self) -> RefMut<'_, ProcessControlBlockInner> {
        self.inner.exclusive_access()
    }

    /// 拿不到（已被借用）时返回 None，供内核代用户访问内存/补缺页时探测使用
    pub fn try_inner_exclusive_access(&self) -> Option<RefMut<'_, ProcessControlBlockInner>> {
        self.inner.try_exclusive_access()
    }

    pub fn new(source: Executable) -> Arc<Self> {
        let (memory_set, elf) = MemorySet::from_elf(source).expect("invalid initial executable");
        // allocate a pid
        let pid_handle = pid_alloc();
        let process = Arc::new(Self {
            pid: pid_handle,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: None,
                    children: Vec::new(),
                    wait_status: 0,
                    fd_table: vec![
                        // 0 -> stdin
                        Some(Arc::new(Stdin)),
                        // 1 -> stdout
                        Some(Arc::new(Stdout)),
                        // 2 -> stderr
                        Some(Arc::new(Stdout)),
                    ],
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    cwd: DirectoryLocation::root(),
                    brk_base: 0,
                    brk: 0,
                    mmap_top: 0x1000_0000,
                    alarm_deadline: 0,
                    signal_handlers: [0usize; 32],
                    signal_pending: 0,
                    in_sigalrm: false,
                    nofile_limit: 1024,
                    #[cfg(feature = "elf_startup_stats")]
                    pending_exec: None,
                })
            },
        });
        // create a main thread, we should allocate ustack and trap_cx here
        let task = Arc::new(TaskControlBlock::new(
            Arc::clone(&process),
            elf.stack_base,
            true,
        ));
        // prepare trap_cx of main thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        let ustack_top = task_inner.res.as_ref().unwrap().ustack_top();
        let kstack_top = task.kstack.get_top();
        drop(task_inner);
        *trap_cx = TrapContext::app_init_context(
            elf.entry,
            ustack_top,
            KERNEL_SPACE.exclusive_access().token(),
            kstack_top,
            trap_handler as usize,
        );
        // add main thread to the process
        let mut process_inner = process.inner_exclusive_access();
        process_inner.tasks.push(Some(Arc::clone(&task)));
        drop(process_inner);
        insert_into_pid2process(process.getpid(), Arc::clone(&process));
        // add main thread to scheduler
        add_task(task);
        process
    }

    #[cfg(feature = "oscomp_runner")]
    pub(super) fn set_initial_args(&self, args: &[&str]) {
        let inner = self.inner_exclusive_access();
        let token = inner.get_user_token();
        let task = inner.get_task(0);
        drop(inner);
        let mut inner = task.inner_exclusive_access();
        let mut sp = inner.res.as_ref().unwrap().ustack_top();
        let mut pointers = Vec::with_capacity(args.len());
        for arg in args {
            sp -= arg.len() + 1;
            for (offset, byte) in arg.bytes().chain(core::iter::once(0)).enumerate() {
                *translated_refmut(token, (sp + offset) as *mut u8) = byte;
            }
            pointers.push(sp);
        }
        let word = core::mem::size_of::<usize>();
        sp = (sp - (pointers.len() + 1) * word) & !15;
        for (index, pointer) in pointers.into_iter().chain(core::iter::once(0)).enumerate() {
            *translated_refmut(token, (sp + index * word) as *mut usize) = pointer;
        }
        let trap = inner.get_trap_cx();
        trap.set_sp(sp);
        trap.x[10] = args.len();
        trap.x[11] = sp;
    }

    /// Only support processes with a single thread.
    pub fn exec(
        self: &Arc<Self>,
        source: Executable,
        args: Vec<Vec<u8>>,
        envs: Vec<Vec<u8>>,
    ) -> Result<(), isize> {
        const AUXV_ENTRIES: usize = 15;
        if self.inner_exclusive_access().thread_count() != 1 {
            return Err(-16); // EBUSY
        }
        let (mut memory_set, elf) = MemorySet::from_elf(source)?;
        let new_token = memory_set.token();
        let heap_base = 0x2000_0000.max(elf.stack_base + USER_STACK_SIZE + PAGE_SIZE);
        let string_bytes = args.iter().chain(&envs).try_fold(0usize, |total, s| {
            total.checked_add(s.len() + 1)
        }).ok_or(-7isize)?;
        let stack_words = 3 + args.len() + envs.len() + AUXV_ENTRIES * 2;
        // Leave room for AT_RANDOM and the two alignment adjustments.
        let fixed_bytes = elf.programs.len() + stack_words * core::mem::size_of::<usize>() + 16 + 7 + 15;
        if fixed_bytes > USER_STACK_SIZE || string_bytes > USER_STACK_SIZE - fixed_bytes {
            return Err(-7); // E2BIG
        }
        // 信号投递用的用户页：可执行 stub + 保存的 trap context
        crate::syscall::signal::setup_signal_pages(&mut memory_set);
        // substitute memory_set
        self.inner_exclusive_access().memory_set = memory_set;
        // then we alloc user resource for main thread again
        // since memory_set has been changed
        let task = self.inner_exclusive_access().get_task(0);
        let mut task_inner = task.inner_exclusive_access();
        task_inner.res.as_mut().unwrap().ustack_base = elf.stack_base;
        task_inner.res.as_mut().unwrap().alloc_user_res();
        task_inner.trap_cx_ppn = task_inner.res.as_mut().unwrap().trap_cx_ppn();
        {
            let mut pinner = self.inner_exclusive_access();
            pinner.brk_base = heap_base;
            pinner.brk = heap_base;
            pinner.mmap_top = 0x1000_0000;
        }
        // ---- 构造 Linux 进程启动栈 ----
        // 低地址 -> 高地址：argc | argv[..] | NULL | envp[..] | NULL | auxv | 字符串/程序头/随机数
        let mut sp = task_inner.res.as_mut().unwrap().ustack_top();
        let argc = args.len();

        // 环境变量字符串（和参数字符串一样放高地址）
        let mut env_ptrs: Vec<usize> = Vec::with_capacity(envs.len());
        for env in envs.iter().rev() {
            sp -= env.len() + 1;
            let mut ep = sp;
            for c in env {
                *translated_refmut(new_token, ep as *mut u8) = *c;
                ep += 1;
            }
            *translated_refmut(new_token, ep as *mut u8) = 0;
            env_ptrs.push(sp);
        }
        env_ptrs.reverse();

        // 1. 参数字符串（先放到高地址区）
        let mut arg_ptrs: Vec<usize> = Vec::with_capacity(argc);
        for arg in args.iter().rev() {
            sp -= arg.len() + 1;
            let mut p = sp;
            for c in arg {
                *translated_refmut(new_token, p as *mut u8) = *c;
                p += 1;
            }
            *translated_refmut(new_token, p as *mut u8) = 0;
            arg_ptrs.push(sp);
        }
        arg_ptrs.reverse();

        // 2. 程序头表副本（AT_PHDR 指向它）
        sp -= elf.programs.len();
        sp -= sp % 8;
        let phdr_addr = sp;
        for (i, &byte) in elf.programs.iter().enumerate() {
            *translated_refmut(new_token, (phdr_addr + i) as *mut u8) = byte;
        }

        // 3. AT_RANDOM 的 16 字节
        sp -= 16;
        let random_addr = sp;
        for i in 0..16usize {
            *translated_refmut(new_token, (random_addr + i) as *mut u8) =
                (i as u8).wrapping_mul(37).wrapping_add(11);
        }

        // 4. 数组区：argc / argv / envp / auxv 必须连续
        let auxv: [(usize, usize); AUXV_ENTRIES] = [
            (3, phdr_addr),
            (4, elf.program_entry_size()),
            (5, elf.program_count()),
            (6, 4096),
            (9, elf.entry),
            (11, 0),
            (12, 0),
            (13, 0),
            (14, 0),
            (23, 0),
            (25, random_addr),
            (31, arg_ptrs.first().copied().unwrap_or(0)),
            (17, 100),
            (16, 0),
            (0, 0),
        ];
        sp -= stack_words * core::mem::size_of::<usize>();
        sp -= sp % 16;
        let base = sp;
        let sz = core::mem::size_of::<usize>();
        *translated_refmut(new_token, base as *mut usize) = argc;
        for i in 0..argc {
            *translated_refmut(new_token, (base + (1 + i) * sz) as *mut usize) = arg_ptrs[i];
        }
        *translated_refmut(new_token, (base + (1 + argc) * sz) as *mut usize) = 0;
        for (i, p) in env_ptrs.iter().enumerate() {
            *translated_refmut(new_token, (base + (2 + argc + i) * sz) as *mut usize) = *p;
        }
        let envc = env_ptrs.len();
        *translated_refmut(new_token, (base + (2 + argc + envc) * sz) as *mut usize) = 0;
        for (i, (t, v)) in auxv.iter().enumerate() {
            *translated_refmut(new_token, (base + (3 + argc + envc + i * 2) * sz) as *mut usize) = *t;
            *translated_refmut(new_token, (base + (4 + argc + envc + i * 2) * sz) as *mut usize) = *v;
        }
        let user_sp = base;
        // initialize trap_cx
        let mut trap_cx = TrapContext::app_init_context(
            elf.entry,
            user_sp,
            KERNEL_SPACE.exclusive_access().token(),
            task.kstack.get_top(),
            trap_handler as usize,
        );
        trap_cx.x[10] = args.len();
        trap_cx.x[11] = user_sp + core::mem::size_of::<usize>();
        *task_inner.get_trap_cx() = trap_cx;
        Ok(())
    }

    /// Only support processes with a single thread.
    pub fn fork(self: &Arc<Self>, stack: usize) -> Result<usize, isize> {
        #[cfg(feature = "elf_startup_stats")]
        crate::fork_stats::begin();
        let mut parent = self.inner_exclusive_access();
        if parent.thread_count() != 1 {
            return Err(-11); // EAGAIN
        }
        let kstack = try_kstack_alloc().ok_or(-12isize)?;
        let memory_set = parent.memory_set.fork()?;
        // alloc a pid
        let pid = pid_alloc();
        // copy fd table
        let mut new_fd_table: Vec<Option<Arc<dyn File + Send + Sync>>> = Vec::new();
        for fd in parent.fd_table.iter() {
            if let Some(file) = fd {
                new_fd_table.push(Some(file.clone()));
            } else {
                new_fd_table.push(None);
            }
        }
        // create child process pcb
        let child = Arc::new(Self {
            pid,
            inner: unsafe {
                UPSafeCell::new(ProcessControlBlockInner {
                    is_zombie: false,
                    memory_set,
                    parent: Some(Arc::downgrade(self)),
                    children: Vec::new(),
                    wait_status: 0,
                    fd_table: new_fd_table,
                    signals: SignalFlags::empty(),
                    tasks: Vec::new(),
                    task_res_allocator: RecycleAllocator::new(),
                    mutex_list: Vec::new(),
                    semaphore_list: Vec::new(),
                    condvar_list: Vec::new(),
                    cwd: parent.cwd.clone(),
                    brk_base: 0,
                    brk: 0,
                    mmap_top: 0x1000_0000,
                    alarm_deadline: 0,
                    signal_handlers: [0usize; 32],
                    signal_pending: 0,
                    in_sigalrm: false,
                    nofile_limit: 1024,
                    #[cfg(feature = "elf_startup_stats")]
                    pending_exec: None,
                })
            },
        });
        // 子进程继承父进程的工作目录与堆/mmap 指针
        {
            let mut ci = child.inner_exclusive_access();
            ci.brk_base = parent.brk_base;
            ci.brk = parent.brk;
            ci.mmap_top = parent.mmap_top;
            ci.nofile_limit = parent.nofile_limit;
        }
        // add child
        parent.children.push(Arc::clone(&child));
        // create main thread of child process
        let task = Arc::new(TaskControlBlock::for_fork(
            Arc::clone(&child),
            parent
                .get_task(0)
                .inner_exclusive_access()
                .res
                .as_ref()
                .unwrap()
                .ustack_base(),
            kstack,
        ));
        // attach task to child process
        let mut child_inner = child.inner_exclusive_access();
        child_inner.tasks.push(Some(Arc::clone(&task)));
        drop(child_inner);
        // modify kstack_top in trap_cx of this thread
        let task_inner = task.inner_exclusive_access();
        let trap_cx = task_inner.get_trap_cx();
        trap_cx.kernel_sp = task.kstack.get_top();
        if stack != 0 {
            trap_cx.x[2] = stack;
        }
        trap_cx.x[10] = 0;
        drop(task_inner);
        insert_into_pid2process(child.getpid(), Arc::clone(&child));
        // add this thread to scheduler
        add_task(task);
        #[cfg(feature = "elf_startup_stats")]
        crate::fork_stats::prepared(child.getpid());
        Ok(child.getpid())
    }

    pub fn getpid(&self) -> usize {
        self.pid.0
    }
}
