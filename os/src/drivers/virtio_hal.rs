use crate::arch::phys_to_virt;
use crate::config::PAGE_SIZE;
use crate::mm::PhysAddr;
use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use core::ptr::{copy_nonoverlapping, NonNull};
use virtio_pci::{BufferDirection, Hal};

pub(super) struct VirtioHal;

fn dma_buffer(layout: Layout) -> NonNull<u8> {
    NonNull::new(unsafe { alloc_zeroed(layout) }).unwrap_or_else(|| handle_alloc_error(layout))
}

fn queue_layout(pages: usize) -> Layout {
    Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).unwrap()
}

// The kernel heap is physically contiguous and directly mapped on both architectures.
// Request buffers can also live on framed stacks, so use heap-backed bounce buffers.
unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _: BufferDirection) -> (usize, NonNull<u8>) {
        let buffer = dma_buffer(queue_layout(pages));
        (PhysAddr::from(buffer.as_ptr() as usize).0, buffer)
    }

    unsafe fn dma_dealloc(_: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
        dealloc(vaddr.as_ptr(), queue_layout(pages));
        0
    }

    #[cfg(target_arch = "loongarch64")]
    unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
        use crate::arch::mmio_address;
        use crate::board::{PCI_MMIO_END, PCI_MMIO_START};

        assert!(
            paddr >= PCI_MMIO_START as usize
                && paddr
                    .checked_add(size)
                    .is_some_and(|end| end <= PCI_MMIO_END as usize)
        );
        NonNull::new(mmio_address(paddr) as *mut u8).unwrap()
    }

    #[cfg(target_arch = "riscv64")]
    unsafe fn mmio_phys_to_virt(paddr: usize, _: usize) -> NonNull<u8> {
        NonNull::new(paddr as *mut u8).unwrap()
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> usize {
        let layout = Layout::array::<u8>(buffer.len()).unwrap();
        let bounce = dma_buffer(layout);
        if direction != BufferDirection::DeviceToDriver {
            copy_nonoverlapping(buffer.as_ptr() as *const u8, bounce.as_ptr(), buffer.len());
        }
        PhysAddr::from(bounce.as_ptr() as usize).0
    }

    unsafe fn unshare(paddr: usize, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let bounce = phys_to_virt(paddr) as *mut u8;
        if direction != BufferDirection::DriverToDevice {
            copy_nonoverlapping(bounce, buffer.as_ptr() as *mut u8, buffer.len());
        }
        dealloc(bounce, Layout::array::<u8>(buffer.len()).unwrap());
    }
}
