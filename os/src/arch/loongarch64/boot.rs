use super::mmio_address;
use crate::fdt::BootInfo;

// QEMU 8.2 does not pass a command line to a directly loaded LoongArch ELF.
// The harness supplies test options as named fw_cfg entries.
const FW_CFG: usize = 0x1e02_0000;
const FW_CFG_FILE_DIR: u16 = 0x19;
const GROUP_FILE: &[u8] = b"opt/hexixi/oscomp.group\0";
const LEGACY_FILE: &[u8] = b"opt/hexixi/oscomp.legacy\0";
const MAX_FILES: usize = 256;

unsafe fn select(key: u16) {
    (mmio_address(FW_CFG + 8) as *mut u16).write_volatile(key.to_be());
}

unsafe fn read<const N: usize>() -> [u8; N] {
    core::array::from_fn(|_| (mmio_address(FW_CFG) as *const u8).read_volatile())
}

unsafe fn read_file(key: u16, len: usize, buffer: &mut [u8; 32]) -> &[u8] {
    if len == 0 || len > buffer.len() {
        return &[];
    }
    select(key);
    let bytes = &mut buffer[..len];
    for byte in bytes.iter_mut() {
        *byte = read::<1>()[0];
    }
    bytes.split(|&byte| byte == 0).next().unwrap_or_default()
}

pub fn boot_info() -> BootInfo {
    let mut boot = BootInfo::default();
    unsafe {
        select(0);
        if &read::<4>() != b"QEMU" {
            return boot;
        }
        select(FW_CFG_FILE_DIR);
        let files = u32::from_be_bytes(read::<4>()) as usize;
        let mut group = None;
        let mut legacy = None;
        for _ in 0..files.min(MAX_FILES) {
            let len = u32::from_be_bytes(read::<4>()) as usize;
            let key = u16::from_be_bytes(read::<2>());
            read::<2>(); // Reserved
            let name = read::<56>();
            if name.starts_with(GROUP_FILE) {
                group = Some((key, len));
            } else if name.starts_with(LEGACY_FILE) {
                legacy = Some((key, len));
            }
        }
        let mut bytes = [0; 32];
        if let Some((key, len)) = group {
            boot.set_oscomp_group(read_file(key, len, &mut bytes));
        }
        if let Some((key, len)) = legacy {
            boot.legacy_oscomp_image = read_file(key, len, &mut bytes) == b"1";
        }
    }
    boot
}
