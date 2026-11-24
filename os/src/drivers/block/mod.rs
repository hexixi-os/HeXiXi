#[cfg(target_arch = "riscv64")]
mod virtio_blk;
#[cfg(target_arch = "loongarch64")]
mod virtio_pci;

#[cfg(all(target_arch = "riscv64", feature = "board_k210"))]
mod sdcard;
#[cfg(all(target_arch = "riscv64", feature = "board_k210"))]
pub use sdcard::SDCardWrapper;
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

#[allow(unused)]
pub fn block_device_test() {
    let block_device = BLOCK_DEVICE.clone();
    let mut write_buffer = [0u8; 512];
    let mut read_buffer = [0u8; 512];
    for i in 0..512 {
        for byte in write_buffer.iter_mut() {
            *byte = i as u8;
        }
        block_device.write_block(i as usize, &write_buffer);
        block_device.read_block(i as usize, &mut read_buffer);
        assert_eq!(write_buffer, read_buffer);
    }
    println!("block device test passed!");
}
