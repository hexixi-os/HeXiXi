use super::{PhysAddr, PhysPageNum};
use crate::config::MEMORY_END;
use core::sync::atomic::{AtomicUsize, Ordering};

/// 实际可用的物理内存末尾（由 DTB 动态探测，缺省用配置值）
static RAM_END: AtomicUsize = AtomicUsize::new(MEMORY_END);

pub fn set_ram_end(v: usize) {
    RAM_END.store(v, Ordering::Relaxed);
}

pub fn ram_end() -> usize {
    RAM_END.load(Ordering::Relaxed)
}
use crate::sync::UPSafeCell;
use alloc::{vec, vec::Vec};
use core::fmt::{self, Debug, Formatter};
use lazy_static::*;

pub struct FrameTracker {
    pub ppn: PhysPageNum,
    charge: Option<&'static AtomicUsize>,
}

impl FrameTracker {
    pub fn new(ppn: PhysPageNum) -> Self {
        // page cleaning
        let bytes_array = ppn.get_bytes_array();
        for i in bytes_array {
            *i = 0;
        }
        Self { ppn, charge: None }
    }
}

impl Debug for FrameTracker {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("FrameTracker:PPN={:#x}", self.ppn.0))
    }
}

impl Drop for FrameTracker {
    fn drop(&mut self) {
        frame_dealloc(self.ppn);
        if let Some(charge) = self.charge {
            charge.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

const BITS_PER_WORD: usize = usize::BITS as usize;

#[derive(Default)]
struct BitmapFrameAllocator {
    start: usize,
    end: usize,
    allocated: Vec<usize>,
    free: Vec<PhysPageNum>,
}

impl BitmapFrameAllocator {
    fn init(&mut self, start: PhysPageNum, end: PhysPageNum) {
        self.start = start.0;
        self.end = end.0;
        let count = self.end - self.start;
        self.allocated = vec![0; (count + BITS_PER_WORD - 1) / BITS_PER_WORD];
        // Start with space for all frames so deallocation never grows the stack.
        self.free = (self.start..self.end).rev().map(PhysPageNum).collect();
        println!("last {} Physical Frames.", count);
    }

    fn allocation_bit(&mut self, ppn: PhysPageNum) -> (&mut usize, usize) {
        let offset = ppn.0 - self.start;
        let mask = 1 << (offset % BITS_PER_WORD);
        (&mut self.allocated[offset / BITS_PER_WORD], mask)
    }

    fn alloc(&mut self) -> Option<PhysPageNum> {
        let ppn = self.free.pop()?;
        let (word, mask) = self.allocation_bit(ppn);
        *word |= mask;
        Some(ppn)
    }

    fn dealloc(&mut self, ppn: PhysPageNum) {
        assert!(
            (self.start..self.end).contains(&ppn.0),
            "Frame ppn={:#x} is outside the managed range!",
            ppn.0
        );
        let (word, mask) = self.allocation_bit(ppn);
        assert_ne!(
            *word & mask,
            0,
            "Frame ppn={:#x} has not been allocated!",
            ppn.0
        );
        *word &= !mask;
        self.free.push(ppn);
    }
}

lazy_static! {
    static ref FRAME_ALLOCATOR: UPSafeCell<BitmapFrameAllocator> =
        unsafe { UPSafeCell::new(BitmapFrameAllocator::default()) };
}

pub fn init_frame_allocator() {
    extern "C" {
        fn ekernel();
    }
    // 内核堆是动态划出来的（在 ekernel 之后），帧分配器必须从堆末尾开始，
    // 否则会把内核堆的物理页当空闲帧发出去。
    let heap_end = super::heap_allocator::heap_end();
    let start = PhysAddr::from(heap_end.max(PhysAddr::from(ekernel as *const () as usize).0)).ceil();
    let end = PhysAddr::from(ram_end()).floor();
    FRAME_ALLOCATOR.exclusive_access().init(start, end);
    println!(
        "[mm] frame allocator: {:#x}..{:#x} = {} pages",
        usize::from(PhysAddr::from(start)),
        usize::from(PhysAddr::from(end)),
        end.0 - start.0
    );
}

pub fn frame_alloc() -> Option<FrameTracker> {
    let mut ppn = FRAME_ALLOCATOR.exclusive_access().alloc();
    if ppn.is_none() && crate::fs::page_cache::reclaim_one() {
        ppn = FRAME_ALLOCATOR.exclusive_access().alloc();
    }
    if ppn.is_none() {
        println!("[mm] OUT OF FRAMES");
    }
    ppn.map(|ppn| {
        #[cfg(feature = "elf_startup_stats")]
        {
            crate::exec_stats::on_frame_alloc();
            crate::fork_stats::on_frame_alloc();
        }
        FrameTracker::new(ppn)
    })
}

pub(crate) fn frame_alloc_charged(used: &'static AtomicUsize, limit: usize) -> Option<FrameTracker> {
    used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
        (count < limit).then(|| count + 1)
    }).ok()?;
    match frame_alloc() {
        Some(mut frame) => {
            frame.charge = Some(used);
            Some(frame)
        }
        None => {
            used.fetch_sub(1, Ordering::Relaxed);
            None
        }
    }
}

pub fn frame_dealloc(ppn: PhysPageNum) {
    FRAME_ALLOCATOR.exclusive_access().dealloc(ppn);
    #[cfg(feature = "elf_startup_stats")]
    crate::fork_stats::on_frame_free();
}

pub(crate) fn free_frame_count() -> usize {
    FRAME_ALLOCATOR.exclusive_access().free.len()
}
