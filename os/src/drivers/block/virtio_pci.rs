use super::BlockDevice;
use crate::drivers::{pci::probe_virtio, virtio_hal::VirtioHal};
use crate::sync::UPSafeCell;
use virtio_pci::{
    device::blk::VirtIOBlk,
    transport::{pci::PciTransport, DeviceType},
};

pub struct VirtIOBlock(UPSafeCell<VirtIOBlk<VirtioHal, PciTransport>>);

// The transport and queue pointers refer to global MMIO or owned DMA
// memory. The single-hart kernel serializes access through UPSafeCell.
unsafe impl Send for VirtIOBlock {}

impl VirtIOBlock {
    pub fn new() -> Self {
        let transport = probe_virtio(DeviceType::Block).expect("virtio PCI block device not found");
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
