pub const CLOCK_FREQ: usize = 12500000;
pub const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
pub const VIRTIO_MMIO_SLOT_SIZE: usize = 0x1000;
pub const VIRTIO_MMIO_SLOTS: usize = 8;

pub const MMIO: &[(usize, usize)] = &[
    (0x0010_0000, 0x00_2000), // VIRT_TEST/RTC in virt machine
    (VIRTIO_MMIO_BASE, VIRTIO_MMIO_SLOT_SIZE * VIRTIO_MMIO_SLOTS),
];

pub type BlockDeviceImpl = crate::drivers::block::VirtIOBlock;

const VIRT_TEST: u64 = 0x100000;
const EXIT_SUCCESS: u32 = 0x5555;
const EXIT_FAILURE: u32 = (1 << 16) | 0x3333;

pub fn qemu_exit(success: bool) -> ! {
    let code = if success { EXIT_SUCCESS } else { EXIT_FAILURE };
    unsafe {
        core::arch::asm!("sw {0}, 0({1})", in(reg) code, in(reg) VIRT_TEST);
        loop {
            core::arch::asm!("wfi", options(nomem, nostack));
        }
    }
}
