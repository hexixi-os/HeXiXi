use buddy_system_allocator::LockedHeap;
use core::sync::atomic::{AtomicUsize, Ordering};

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

static HEAP_END: AtomicUsize = AtomicUsize::new(0);

/// 内核堆末尾（frame allocator 从这之后开始分配物理页）
pub fn heap_end() -> usize {
    HEAP_END.load(Ordering::Relaxed)
}

/// 从物理 RAM 动态划分内核堆（一半 RAM，夹在 [64MB, 512MB]），避免大文件/多文件场景堆爆。
pub fn init_heap() {
    extern "C" {
        fn ekernel();
    }
    let start = (super::PhysAddr::from(ekernel as *const () as usize).0 + 0xFFF) & !0xFFF;
    let ram_end = crate::mm::frame_allocator::ram_end();
    let avail = ram_end.saturating_sub(start);
    let size = ((avail / 2).clamp(0x400_0000, 0x2000_0000)) & !0xFFF;
    // 清零：物理内存可能残留旧数据，避免未初始化读
    unsafe {
        let address = crate::arch::phys_to_virt(start);
        core::ptr::write_bytes(address as *mut u8, 0, size);
        HEAP_ALLOCATOR.lock().init(address, size);
    }
    HEAP_END.store(start + size, Ordering::Relaxed);
    println!(
        "[mm] kernel heap: {:#x}..{:#x} ({} MB)",
        start,
        start + size,
        size >> 20
    );
}
