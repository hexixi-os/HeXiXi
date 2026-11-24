use super::mmio_address;
use crate::fdt::BootInfo;

// QEMU 8.2 does not pass a command line to a directly loaded LoongArch ELF.
// The harness supplies the optional group as a named fw_cfg entry.
const FW_CFG: usize = 0x1e02_0000;
const FW_CFG_FILE_DIR: u16 = 0x19;
const GROUP_FILE: &[u8] = b"opt/hexixi/oscomp.group\0";
const MAX_FILES: usize = 256;

unsafe fn select(key: u16) {
    (mmio_address(FW_CFG + 8) as *mut u16).write_volatile(key.to_be());
}

unsafe fn read<const N: usize>() -> [u8; N] {
    core::array::from_fn(|_| (mmio_address(FW_CFG) as *const u8).read_volatile())
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
        for _ in 0..files.min(MAX_FILES) {
            let len = u32::from_be_bytes(read::<4>()) as usize;
            let key = u16::from_be_bytes(read::<2>());
            read::<2>(); // Reserved
            let name = read::<56>();
            if !name.starts_with(GROUP_FILE) {
                continue;
            }
            if len == 0 || len > 32 {
                boot.set_oscomp_group(b"");
                break;
            }
            select(key);
            let mut group = [0; 32];
            for byte in &mut group[..len] {
                *byte = read::<1>()[0];
            }
            let end = group[..len]
                .iter()
                .position(|&byte| byte == 0)
                .unwrap_or(len);
            boot.set_oscomp_group(&group[..end]);
            break;
        }
    }
    boot
}
