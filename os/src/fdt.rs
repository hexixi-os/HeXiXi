//! 从 boot DTB 读取真实物理内存大小（QEMU -m 参数）
use core::sync::atomic::{AtomicUsize, Ordering};

static DETECTED_RAM_END: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub struct BootInfo {
    pub ram_end: Option<usize>,
    oscomp_group: [u8; 32],
}

impl BootInfo {
    pub fn oscomp_group(&self) -> Option<&str> {
        let end = self.oscomp_group.iter().position(|&byte| byte == 0)?;
        (end != 0).then(|| core::str::from_utf8(&self.oscomp_group[..end]).unwrap())
    }

    pub fn set_oscomp_group(&mut self, group: &[u8]) {
        let group = if group.is_empty() || group.len() >= self.oscomp_group.len() || !group.is_ascii() {
            b"<invalid>"
        } else {
            group
        };
        self.oscomp_group[..group.len()].copy_from_slice(group);
    }

    #[cfg(target_arch = "riscv64")]
    fn read_bootargs(&mut self, args: &[u8]) {
        let args = args.split(|&byte| byte == 0).next().unwrap_or_default();
        for arg in args.split(|byte| byte.is_ascii_whitespace()) {
            if let Some(group) = arg.strip_prefix(b"oscomp.group=") {
                self.set_oscomp_group(group);
                break;
            }
        }
    }
}

#[cfg(target_arch = "riscv64")]
fn read_be32(addr: usize) -> u32 {
    unsafe { u32::from_be(core::ptr::read_volatile(addr as *const u32)) }
}

#[cfg(target_arch = "riscv64")]
fn read_be64(addr: usize) -> u64 {
    let hi = read_be32(addr) as u64;
    let lo = read_be32(addr + 4) as u64;
    (hi << 32) | lo
}

/// 解析 FDT 中的 RAM 结束地址和评测启动参数。
#[cfg(target_arch = "riscv64")]
pub fn detect_boot_info() -> BootInfo {
    extern "C" {
        static BOOT_DTB: usize;
    }
    let dtb = unsafe { BOOT_DTB };
    let mut info = BootInfo::default();
    if dtb < 0x8000_0000 || dtb >= 0x1_0000_0000 {
        return info;
    }
    if read_be32(dtb) != 0xd00d_feed {
        return info;
    }
    let off_struct = read_be32(dtb + 8) as usize;
    let off_strings = read_be32(dtb + 12) as usize;
    let mut p = dtb + off_struct;

    let mut depth = 0usize;
    // 根节点的 cells
    let mut addr_cells = 2usize;
    let mut size_cells = 2usize;
    // 正在处理的节点名（深度 1 时是 memory 节点）
    let mut node_is_memory = false;
    let mut node_is_chosen = false;
    let mut max_end = 0usize;

    loop {
        let token = read_be32(p);
        p += 4;
        match token {
            1 => {
                // FDT_BEGIN_NODE
                let name_start = p;
                let mut n = 0;
                while unsafe { *(name_start as *const u8).add(n) } != 0 {
                    n += 1;
                }
                let name = unsafe {
                    core::str::from_utf8(core::slice::from_raw_parts(name_start as *const u8, n))
                        .unwrap_or("")
                };
                p = (name_start + n + 1 + 3) & !3;
                depth += 1;
                if depth == 2 && (name.starts_with("memory") || name.contains("memory@")) {
                    node_is_memory = true;
                }
                if depth == 2 {
                    node_is_chosen = name == "chosen";
                }
            }
            2 => {
                // FDT_END_NODE
                if depth == 2 {
                    node_is_memory = false;
                    node_is_chosen = false;
                }
                depth = depth.saturating_sub(1);
            }
            3 => {
                // FDT_PROP
                let len = read_be32(p) as usize;
                let nameoff = read_be32(p + 4) as usize;
                let value = p + 8;
                p = (value + len + 3) & !3;
                let mut q = dtb + off_strings + nameoff;
                let mut nlen = 0;
                while unsafe { *(q as *const u8).add(nlen) } != 0 {
                    nlen += 1;
                }
                let pname = unsafe {
                    core::str::from_utf8(core::slice::from_raw_parts(q as *const u8, nlen))
                        .unwrap_or("")
                };
                if depth == 1 {
                    match pname {
                        "#address-cells" if len >= 4 => addr_cells = read_be32(value) as usize,
                        "#size-cells" if len >= 4 => size_cells = read_be32(value) as usize,
                        _ => {}
                    }
                }
                if depth == 2 && node_is_chosen && pname == "bootargs" && len <= 4096 {
                    let args = unsafe { core::slice::from_raw_parts(value as *const u8, len) };
                    info.read_bootargs(args);
                }
                if node_is_memory && pname == "reg" {
                    let entry = addr_cells + size_cells;
                    if entry > 0 {
                        let mut i = 0;
                        while i + entry <= len / 4 {
                            let base_off = value + i * 4;
                            let base = if addr_cells == 2 {
                                read_be64(base_off) as usize
                            } else {
                                read_be32(base_off) as usize
                            };
                            let size_off = base_off + addr_cells * 4;
                            let size = if size_cells == 2 {
                                read_be64(size_off) as usize
                            } else {
                                read_be32(size_off) as usize
                            };
                            let end = base.saturating_add(size);
                            if base <= 0x8000_0000 && end > 0x8000_0000 && end > max_end {
                                max_end = end;
                            }
                            i += entry;
                        }
                    }
                }
            }
            4 => {} // FDT_NOP
            9 => break,
            _ => break,
        }
    }
    if max_end > 0 {
        DETECTED_RAM_END.store(max_end, Ordering::Relaxed);
        info.ram_end = Some(max_end);
    }
    info
}

pub fn ram_end() -> Option<usize> {
    let v = DETECTED_RAM_END.load(Ordering::Relaxed);
    if v == 0 { None } else { Some(v) }
}
