mod secondary;
#[cfg(target_arch = "riscv64")]
mod virtio_blk;
#[cfg(target_arch = "loongarch64")]
mod virtio_pci;

pub use secondary::{secondary_disk, SecondaryDisk};
#[cfg(target_arch = "riscv64")]
pub use virtio_blk::VirtIOBlock;
#[cfg(target_arch = "loongarch64")]
pub use virtio_pci::VirtIOBlock;

use crate::board::BlockDeviceImpl;
use alloc::sync::Arc;
use easy_fs::BlockDevice;
use lazy_static::*;

lazy_static! {
    pub static ref BLOCK_DEVICE: Arc<dyn BlockDevice> = Arc::new(BlockDeviceImpl::new());
}
