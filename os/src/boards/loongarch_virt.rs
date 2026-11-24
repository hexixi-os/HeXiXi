//! QEMU virt's low RAM, PCI configuration space and 32-bit MMIO window.
pub const MEMORY_END: usize = 0x0ff0_0000;
pub const PCI_ECAM: usize = 0x2000_0000;
pub const PCI_MMIO_START: u64 = 0x4000_0000;
pub const PCI_MMIO_END: u64 = 0x8000_0000;

pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;
