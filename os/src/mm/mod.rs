mod address;
mod elf;
mod frame_allocator;
mod heap_allocator;
mod memory_set;
mod page_table;
mod resident;
mod user_access;
mod vma;

use address::VPNRange;
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
pub(crate) use frame_allocator::frame_alloc_charged;
#[cfg(target_arch = "riscv64")]
pub use frame_allocator::frame_dealloc;
pub use frame_allocator::{frame_alloc, ram_end, set_ram_end, FrameTracker};
pub use memory_set::handle_user_page_fault;
#[cfg(target_arch = "riscv64")]
pub use memory_set::remap_test;
pub use memory_set::{kernel_token, MapPermission, MemorySet, KERNEL_SPACE};
pub use vma::{PageFault, VmaBacking, USER_END};
use page_table::PTEFlags;
pub(crate) use page_table::{
    prepare_user_write, read_user, translated_c_bytes, try_read_user, try_write_user, write_user,
};
pub use page_table::{
    translated_byte_buffer, translated_path, PageTable, PageTableEntry, UserBuffer,
};
pub(crate) use user_access::{copy_live_bytes, zero_live_bytes, UserValue};

pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    KERNEL_SPACE.exclusive_access().activate();
}
