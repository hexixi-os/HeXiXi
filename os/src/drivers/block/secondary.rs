use super::BlockDevice;
use crate::{drivers::virtio_hal::VirtioHal, sync::UPSafeCell};
use alloc::sync::Arc;
use lazy_static::lazy_static;
use virtio_pci::{
    device::blk::{VirtIOBlk, SECTOR_SIZE},
    transport::{DeviceType, Transport},
    Error,
};

#[cfg(target_arch = "riscv64")]
use virtio_pci::transport::mmio::MmioTransport as BlockTransport;
#[cfg(target_arch = "loongarch64")]
use virtio_pci::transport::pci::PciTransport as BlockTransport;

const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const EIO: isize = -5;
const EINVAL: isize = -22;
const EROFS: isize = -30;
const EOPNOTSUPP: isize = -95;

pub struct SecondaryDisk {
    block: UPSafeCell<VirtIOBlk<VirtioHal, BlockTransport>>,
    supports_flush: bool,
}

// The transport refers to global MMIO and the driver owns its DMA memory.
// The kernel serializes access through UPSafeCell.
unsafe impl Send for SecondaryDisk {}

lazy_static! {
    static ref SECONDARY_DISK: Option<Arc<SecondaryDisk>> = SecondaryDisk::probe().map(Arc::new);
}

pub fn secondary_disk() -> Option<Arc<SecondaryDisk>> {
    SECONDARY_DISK.as_ref().map(Arc::clone)
}

impl SecondaryDisk {
    fn probe() -> Option<Self> {
        let mut transport = probe_transport()?;
        // The library would report success for an unnegotiated FLUSH.
        let supports_flush = transport.read_device_features() & VIRTIO_BLK_F_FLUSH != 0;
        match VirtIOBlk::new(transport) {
            Ok(block) => Some(Self {
                block: unsafe { UPSafeCell::new(block) },
                supports_flush,
            }),
            Err(error) => {
                log::warn!("VirtIO secondary disk initialization failed: {:?}", error);
                None
            }
        }
    }

    /// The capacity is measured in 512-byte sectors.
    pub fn capacity(&self) -> u64 {
        self.block.exclusive_access().capacity()
    }

    pub fn readonly(&self) -> bool {
        self.block.exclusive_access().readonly()
    }

    pub fn flush(&self) -> Result<(), isize> {
        if !self.supports_flush {
            return Err(EOPNOTSUPP);
        }
        self.block.exclusive_access().flush().map_err(block_error)
    }

    pub fn read_sector(&self, sector: usize, buf: &mut [u8]) -> Result<(), isize> {
        self.check_sector(sector, buf.len())?;
        self.block
            .exclusive_access()
            .read_blocks(sector, buf)
            .map_err(block_error)
    }

    pub fn write_sector(&self, sector: usize, buf: &[u8]) -> Result<(), isize> {
        self.check_sector(sector, buf.len())?;
        let mut block = self.block.exclusive_access();
        if block.readonly() {
            return Err(EROFS);
        }
        block.write_blocks(sector, buf).map_err(block_error)
    }

    fn check_sector(&self, sector: usize, len: usize) -> Result<(), isize> {
        if len != SECTOR_SIZE {
            return Err(EINVAL);
        }
        if sector as u64 >= self.capacity() {
            return Err(EIO);
        }
        Ok(())
    }
}

impl BlockDevice for SecondaryDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        self.read_sector(block_id, buf)
            .expect("VirtIO secondary disk read failed");
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.write_sector(block_id, buf)
            .expect("VirtIO secondary disk write failed");
    }
}

fn block_error(error: Error) -> isize {
    log::warn!("VirtIO secondary disk request failed: {:?}", error);
    match error {
        Error::Unsupported => EOPNOTSUPP,
        _ => EIO,
    }
}

#[cfg(target_arch = "riscv64")]
fn probe_transport() -> Option<BlockTransport> {
    use crate::board::{VIRTIO_MMIO_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_SLOT_SIZE};
    use core::ptr::NonNull;
    use virtio_pci::transport::mmio::VirtIOHeader;

    const DEVICE_ID_OFFSET: usize = 0x008;
    const STATUS_OFFSET: usize = 0x070;

    // Slot zero is reserved for the root disk. Dropping an MMIO transport
    // resets its device, so only construct one for an unclaimed block device.
    (1..VIRTIO_MMIO_SLOTS).find_map(|slot| {
        let address = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_SLOT_SIZE;
        let device_id = unsafe { ((address + DEVICE_ID_OFFSET) as *const u32).read_volatile() };
        if device_id != DeviceType::Block as u32 {
            return None;
        }
        let status = unsafe { ((address + STATUS_OFFSET) as *const u32).read_volatile() };
        if status != 0 {
            return None;
        }
        let header = NonNull::new(address as *mut VirtIOHeader)?;
        unsafe { BlockTransport::new(header) }.ok()
    })
}

#[cfg(target_arch = "loongarch64")]
fn probe_transport() -> Option<BlockTransport> {
    crate::drivers::pci::probe_virtio_nth(DeviceType::Block, 1)
}
