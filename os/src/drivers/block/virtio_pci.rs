use super::BlockDevice;
use crate::arch::{mmio_address, phys_to_virt};
use crate::board::{PCI_ECAM, PCI_MMIO_END, PCI_MMIO_START};
use crate::config::PAGE_SIZE;
use crate::mm::PhysAddr;
use crate::sync::UPSafeCell;
use alloc::alloc::{alloc_zeroed, dealloc, handle_alloc_error, Layout};
use core::ptr::{copy_nonoverlapping, NonNull};
use virtio_pci::{
    device::blk::VirtIOBlk,
    transport::{
        pci::{
            bus::{Cam, Command, PciRoot},
            virtio_device_type, PciTransport,
        },
        DeviceType,
    },
    BufferDirection, Hal,
};

pub struct VirtIOBlock(UPSafeCell<VirtIOBlk<VirtioHal, PciTransport>>);

// The transport and queue pointers refer to global MMIO or owned DMA
// memory. The single-hart kernel serializes access through UPSafeCell.
unsafe impl Send for VirtIOBlock {}

impl VirtIOBlock {
    pub fn new() -> Self {
        let mut root = unsafe { PciRoot::new(mmio_address(PCI_ECAM) as *mut u8, Cam::Ecam) };
        let (function, _) = root
            .enumerate_bus(0)
            .find(|(_, info)| virtio_device_type(info) == Some(DeviceType::Block))
            .expect("virtio PCI block device not found on bus 0");

        // Direct boot has no PCI firmware. Allocate this device's memory
        // BARs with decoding off and leave legacy I/O BARs disabled.
        root.set_command(function, Command::empty());
        let mut address = PCI_MMIO_START;
        let mut index = 0;
        while index < 6 {
            let bar = root.bar_info(function, index).expect("invalid PCI BAR");
            if let Some((_, size)) = bar.memory_address_size().filter(|&(_, size)| size != 0) {
                assert!(size.is_power_of_two());
                let size = u64::from(size);
                address = (address + size - 1) & !(size - 1);
                assert!(address + size <= PCI_MMIO_END, "PCI MMIO window exhausted");
                if bar.takes_two_entries() {
                    root.set_bar_64(function, index, address);
                } else {
                    root.set_bar_32(function, index, address as u32);
                }
                address += size;
            }
            index += if bar.takes_two_entries() { 2 } else { 1 };
        }
        root.set_command(
            function,
            Command::MEMORY_SPACE | Command::BUS_MASTER | Command::INTERRUPT_DISABLE,
        );
        let transport = PciTransport::new::<VirtioHal>(&mut root, function)
            .expect("virtio block requires modern PCI capabilities");
        let blk = VirtIOBlk::new(transport).expect("virtio block initialization failed");
        Self(unsafe { UPSafeCell::new(blk) })
    }
}

impl BlockDevice for VirtIOBlock {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.0
            .exclusive_access()
            .read_blocks(block_id, buf)
            .expect("virtio block read");
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.0
            .exclusive_access()
            .write_blocks(block_id, buf)
            .expect("virtio block write");
    }
}

struct VirtioHal;

fn dma_buffer(layout: Layout) -> NonNull<u8> {
    NonNull::new(unsafe { alloc_zeroed(layout) }).unwrap_or_else(|| handle_alloc_error(layout))
}

fn queue_layout(pages: usize) -> Layout {
    Layout::from_size_align(pages * PAGE_SIZE, PAGE_SIZE).unwrap()
}

// The kernel heap lies entirely inside the cached direct map and is
// physically contiguous. Request buffers may live on a framed stack.
unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _: BufferDirection) -> (usize, NonNull<u8>) {
        let buffer = dma_buffer(queue_layout(pages));
        (PhysAddr::from(buffer.as_ptr() as usize).0, buffer)
    }

    unsafe fn dma_dealloc(_: usize, vaddr: NonNull<u8>, pages: usize) -> i32 {
        dealloc(vaddr.as_ptr(), queue_layout(pages));
        0
    }

    unsafe fn mmio_phys_to_virt(paddr: usize, size: usize) -> NonNull<u8> {
        assert!(
            paddr >= PCI_MMIO_START as usize
                && paddr
                    .checked_add(size)
                    .is_some_and(|end| end <= PCI_MMIO_END as usize)
        );
        NonNull::new(mmio_address(paddr) as *mut u8).unwrap()
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
