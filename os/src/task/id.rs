use super::ProcessControlBlock;
use crate::config::{KERNEL_STACK_SIZE, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE, USER_STACK_SIZE};
use crate::mm::{MapPermission, PhysPageNum, VirtAddr, KERNEL_SPACE};
use crate::sync::UPSafeCell;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::*;

pub struct RecycleAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl RecycleAllocator {
    pub fn new() -> Self {
        RecycleAllocator {
            current: 0,
            recycled: Vec::new(),
        }
    }
    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            self.current += 1;
            self.current - 1
        }
    }
    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.iter().any(|i| *i == id),
            "id {} has been deallocated!",
            id
        );
        self.recycled.push(id);
    }
}

lazy_static! {
    static ref PID_ALLOCATOR: UPSafeCell<RecycleAllocator> =
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
    static ref KSTACK_ALLOCATOR: UPSafeCell<RecycleAllocator> =
        unsafe { UPSafeCell::new(RecycleAllocator::new()) };
}

pub const IDLE_PID: usize = 0;

pub struct PidHandle(pub usize);

pub fn pid_alloc() -> PidHandle {
    PidHandle(PID_ALLOCATOR.exclusive_access().alloc())
}

impl Drop for PidHandle {
    fn drop(&mut self) {
        PID_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(kstack_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - kstack_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

pub struct KernelStack(pub usize);

pub fn kstack_alloc() -> KernelStack {
    try_kstack_alloc().expect("out of kernel stack frames")
}

pub(super) fn try_kstack_alloc() -> Option<KernelStack> {
    let kstack_id = KSTACK_ALLOCATOR.exclusive_access().alloc();
    let (kstack_bottom, kstack_top) = kernel_stack_position(kstack_id);
    let mapped = KERNEL_SPACE.exclusive_access().try_insert_framed_area(
        kstack_bottom.into(),
        kstack_top.into(),
        MapPermission::R | MapPermission::W,
    );
    if mapped {
        // Harts may have cached an invalid translation before this stack existed.
        #[cfg(feature = "smp")]
        crate::arch::flush_tlb();
        Some(KernelStack(kstack_id))
    } else {
        KSTACK_ALLOCATOR.exclusive_access().dealloc(kstack_id);
        None
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let (kernel_stack_bottom, _) = kernel_stack_position(self.0);
        let kernel_stack_bottom_va: VirtAddr = kernel_stack_bottom.into();
        KERNEL_SPACE
            .exclusive_access()
            .remove_area_with_start_vpn(kernel_stack_bottom_va.into());
        KSTACK_ALLOCATOR.exclusive_access().dealloc(self.0);
    }
}

impl KernelStack {
    pub fn get_top(&self) -> usize {
        let (_, kernel_stack_top) = kernel_stack_position(self.0);
        kernel_stack_top
    }
}

pub struct TaskUserRes {
    pub tid: usize,
    pub ustack_base: usize,
    pub process: Weak<ProcessControlBlock>,
    owns_stack: bool,
    owns_trap: bool,
}

fn trap_cx_bottom_from_tid(tid: usize) -> usize {
    TRAP_CONTEXT_BASE - tid * PAGE_SIZE
}

impl TaskUserRes {
    pub fn new(
        process: Arc<ProcessControlBlock>,
        ustack_base: usize,
        alloc_user_res: bool,
    ) -> Self {
        let tid = process.inner_exclusive_access().alloc_tid();
        let task_user_res = Self {
            tid,
            ustack_base,
            process: Arc::downgrade(&process),
            owns_stack: true,
            owns_trap: true,
        };
        if alloc_user_res {
            task_user_res.alloc_user_res();
        }
        task_user_res
    }

    pub(super) fn try_for_thread(process: &Arc<ProcessControlBlock>, stack_hint: Option<usize>) -> Option<Self> {
        let tid = process.inner_exclusive_access().alloc_tid();
        let mut res = Self {
            tid,
            ustack_base: 0,
            process: Arc::downgrade(process),
            owns_stack: false,
            owns_trap: false,
        };
        let mut inner = process.inner_exclusive_access();
        let trap = trap_cx_bottom_from_tid(tid);
        res.owns_trap = inner.memory_set.try_insert_framed_area(
            trap.into(),
            (trap + PAGE_SIZE).into(),
            MapPermission::R | MapPermission::W,
        );
        if !res.owns_trap {
            return None;
        }
        if let Some(hint) = stack_hint {
            // Skip existing mappings and leave space for a guard page.
            let pages = USER_STACK_SIZE / PAGE_SIZE;
            let range = inner.memory_set.find_vacant((hint / PAGE_SIZE).into(), pages + 2)?;
            res.ustack_base = (range.start.0 + 1) * PAGE_SIZE;
            res.owns_stack = inner.memory_set.try_insert_framed_area(
                res.ustack_base.into(),
                res.ustack_top().into(),
                MapPermission::R | MapPermission::W | MapPermission::U,
            );
            if !res.owns_stack {
                return None;
            }
        }
        drop(inner);
        Some(res)
    }

    pub fn alloc_user_res(&self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        // alloc user stack
        let ustack_bottom = self.ustack_base;
        let ustack_top = ustack_bottom + USER_STACK_SIZE;
        process_inner.memory_set.insert_framed_area(
            ustack_bottom.into(),
            ustack_top.into(),
            MapPermission::R | MapPermission::W | MapPermission::U,
        );
        // alloc trap_cx
        let trap_cx_bottom = trap_cx_bottom_from_tid(self.tid);
        let trap_cx_top = trap_cx_bottom + PAGE_SIZE;
        process_inner.memory_set.insert_framed_area(
            trap_cx_bottom.into(),
            trap_cx_top.into(),
            MapPermission::R | MapPermission::W,
        );
    }

    pub(super) fn release_mappings(&mut self) {
        let process = self.process.upgrade().unwrap();
        let mut process_inner = process.inner_exclusive_access();
        if self.owns_stack {
            process_inner.memory_set.remove_area_with_start_vpn(VirtAddr::from(self.ustack_base).into());
            self.owns_stack = false;
        }
        if self.owns_trap {
            process_inner.memory_set.remove_area_with_start_vpn(VirtAddr::from(self.trap_cx_user_va()).into());
            self.owns_trap = false;
        }
    }

    pub fn trap_cx_user_va(&self) -> usize {
        trap_cx_bottom_from_tid(self.tid)
    }

    pub fn trap_cx_ppn(&self) -> PhysPageNum {
        let process = self.process.upgrade().unwrap();
        let process_inner = process.inner_exclusive_access();
        let trap_cx_bottom_va: VirtAddr = trap_cx_bottom_from_tid(self.tid).into();
        process_inner
            .memory_set
            .translate(trap_cx_bottom_va.into())
            .unwrap()
            .ppn()
    }

    pub fn ustack_base(&self) -> usize {
        self.ustack_base
    }
    pub fn ustack_top(&self) -> usize {
        self.ustack_base + USER_STACK_SIZE
    }
}

impl Drop for TaskUserRes {
    fn drop(&mut self) {
        // 延迟回收 TCB 时进程可能已经析构（Weak 升级失败），此时直接跳过
        if let Some(process) = self.process.upgrade() {
            self.release_mappings();
            process.inner_exclusive_access().dealloc_tid(self.tid);
        }
    }
}
