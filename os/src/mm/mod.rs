mod address;
mod elf;
mod frame_allocator;
mod heap_allocator;
mod memory_set;
mod page_table;
mod resident;
mod vma;

use address::VPNRange;
pub use address::{PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum};
pub use frame_allocator::{frame_alloc, frame_dealloc, free_pages, ram_end, set_ram_end, FrameTracker};
pub use memory_set::handle_user_page_fault;
pub use memory_set::remap_test;
pub use memory_set::{kernel_token, MapPermission, MemorySet, KERNEL_SPACE};
pub use vma::{PageFault, VmaBacking, USER_END};
use page_table::PTEFlags;
pub(crate) use page_table::{translated_c_bytes, try_translated_refmut};
pub use page_table::{
    translated_byte_buffer, translated_path, translated_ref, translated_refmut, translated_str,
    PageTable,
    PageTableEntry, UserBuffer, UserBufferIterator,
};

pub fn init() {
    heap_allocator::init_heap();
    frame_allocator::init_frame_allocator();
    KERNEL_SPACE.exclusive_access().activate();
}
