use buddy_system_allocator::LockedHeap;
use core::sync::atomic::{AtomicUsize, Ordering};

#[global_allocator]
static HEAP_ALLOCATOR: LockedHeap = LockedHeap::empty();

#[alloc_error_handler]
pub fn handle_alloc_error(layout: core::alloc::Layout) -> ! {
    panic!("Heap allocation error, layout = {:?}", layout);
}

static HEAP_START: AtomicUsize = AtomicUsize::new(0);
static HEAP_END: AtomicUsize = AtomicUsize::new(0);

/// 内核堆末尾（frame allocator 从这之后开始分配物理页）
pub fn heap_end() -> usize {
    HEAP_END.load(Ordering::Relaxed)
}

/// 从物理 RAM 里划一块当内核堆（不再用固定大小的 .bss 数组，避免大文件/多文件场景堆爆）。
/// 大小随内存自适应：RAM 的三分之一，夹在 [64MB, 256MB]。
/// 为什么：lmbench 的 lat_fs 会按 1 秒的时间预算创建上万个文件（overlay 数据在内核堆里），
/// 64MB 固定堆在大内存机器上会被撑爆（`Heap allocation error, layout = size 4096`）。
pub fn init_heap() {
    extern "C" {
        fn ekernel();
    }
    let start = (super::PhysAddr::from(ekernel as usize).0 + 0xFFF) & !0xFFF;
    let ram_end = crate::mm::frame_allocator::ram_end();
    let avail = ram_end.saturating_sub(start);
    // 用一半 RAM、上限 512MB：
    // lmbench 的 lat_fs 会按“1 秒的时间预算”创建上万个 10KB 文件（overlay 数据在内核堆里），
    // 实测在 1G 内存的 QEMU 上峰值要 200~300MB，256MB 堆会在 10k 那轮 OOM。
    let size = ((avail / 2).clamp(0x400_0000, 0x2000_0000)) & !0xFFF;
    // 清零：物理内存可能残留旧数据，避免未初始化读
    unsafe {
        let address = crate::arch::phys_to_virt(start);
        core::ptr::write_bytes(address as *mut u8, 0, size);
        HEAP_ALLOCATOR.lock().init(address, size);
    }
    HEAP_START.store(start, Ordering::Relaxed);
    HEAP_END.store(start + size, Ordering::Relaxed);
    println!(
        "[mm] kernel heap: {:#x}..{:#x} ({} MB)",
        start,
        start + size,
        size >> 20
    );
}

#[allow(unused)]
pub fn heap_test() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    let start = HEAP_START.load(Ordering::Relaxed);
    let end = HEAP_END.load(Ordering::Relaxed);
    let a = Box::new(5);
    assert_eq!(*a, 5);
    let addr = a.as_ref() as *const _ as usize;
    assert!(start <= addr && addr < end);
    drop(a);
    let mut v: Vec<usize> = Vec::new();
    for i in 0..500 {
        v.push(i);
    }
    for (i, val) in v.iter().take(500).enumerate() {
        assert_eq!(*val, i);
    }
    drop(v);
    println!("heap_test passed!");
}
