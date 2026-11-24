use super::elf::{ElfHeaders, ElfImage};
use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use super::vma::{PageFault, VmaBacking, VmaSpace, USER_END};
use crate::config::{MMIO, PAGE_SIZE, TRAMPOLINE};
use crate::fs::Executable;
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::{arch::asm, ops::Range};
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}

pub fn kernel_token() -> usize {
    KERNEL_SPACE.exclusive_access().token()
}

pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
    vmas: VmaSpace,
}

impl MemorySet {
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            vmas: VmaSpace::default(),
        }
    }
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    fn framed_overlap_end(&self, range: &Range<VirtPageNum>) -> Option<VirtPageNum> {
        self.areas.iter().filter_map(|area| {
            let end = area.vpn_range.get_end();
            (area.vpn_range.get_start() < range.end && range.start < end).then_some(end)
        }).max()
    }

    fn overlap_end(&self, range: &Range<VirtPageNum>) -> Option<VirtPageNum> {
        self.framed_overlap_end(range).into_iter().chain(self.vmas.overlap_end(range)).max()
    }

    pub fn vacant(&self, range: &Range<VirtPageNum>) -> bool {
        self.overlap_end(range).is_none()
    }

    pub fn find_vacant(&self, mut start: VirtPageNum, pages: usize) -> Option<Range<VirtPageNum>> {
        loop {
            let end = VirtPageNum(start.0.checked_add(pages)?);
            if end.0 > USER_END / PAGE_SIZE {
                return None;
            }
            let range = start..end;
            match self.overlap_end(&range) {
                Some(next) => start = next,
                None => return Some(range),
            }
        }
    }

    pub fn insert_vma(&mut self, range: Range<VirtPageNum>, permissions: MapPermission, backing: VmaBacking) {
        self.vmas.insert(range, permissions, backing);
    }

    pub fn handle_page_fault(&mut self, vpn: VirtPageNum, access: MapPermission) -> PageFault {
        self.vmas.fault(&mut self.page_table, vpn, access)
    }

    pub(crate) fn pin_vma_page(&self, vpn: VirtPageNum) -> Option<Arc<FrameTracker>> {
        self.vmas.pin(vpn)
    }

    pub fn munmap(&mut self, range: Range<VirtPageNum>) -> Result<(), isize> {
        // ELF segments and task resources follow their existing allocation lifecycle.
        if self.framed_overlap_end(&range).is_some() || self.vmas.overlaps_executable(&range) {
            return Err(-22);
        }
        let mut tables = Vec::new();
        let retired = self.vmas.unmap(&mut self.page_table, range, &mut tables);
        unsafe { asm!("sfence.vma"); }
        drop((retired, tables));
        Ok(())
    }

    pub fn mprotect(&mut self, range: Range<VirtPageNum>, permissions: MapPermission) -> Result<(), isize> {
        // Check the entire interval before changing any resident translation or VMA.
        let mut cursor = range.start;
        while cursor < range.end {
            cursor = if let Some(end) = self.vmas.covered_until(cursor, permissions) {
                end?
            } else {
                self.areas.iter().find(|area| {
                    area.map_perm.contains(MapPermission::U)
                        && area.vpn_range.get_start() <= cursor
                        && cursor < area.vpn_range.get_end()
                }).map(|area| area.vpn_range.get_end()).ok_or(-12isize)?
            };
        }
        self.vmas.protect(&mut self.page_table, range.clone(), permissions);
        let flags = PTEFlags::from_bits(permissions.bits()).unwrap();
        for area in &self.areas {
            for (&vpn, _) in area.data_frames.range(range.clone()) {
                self.page_table.set_flags(vpn, flags);
            }
        }
        unsafe { asm!("sfence.vma"); }
        Ok(())
    }

    /// 映射一个页并写入数据（用于信号 stub 等内核注入的用户页）
    pub fn insert_data_page(
        &mut self,
        start_va: VirtAddr,
        permission: MapPermission,
        data: &[u8],
    ) -> Option<PhysPageNum> {
        let end_va = VirtAddr::from(start_va.0 + PAGE_SIZE);
        let area = MapArea::new(start_va, end_va, MapType::Framed, permission);
        let data_opt = if data.is_empty() { None } else { Some(data) };
        self.push(area, data_opt);
        self.translate(start_va.floor()).map(|e| e.ppn())
    }

    pub fn remove_area_with_start_vpn(&mut self, start_vpn: VirtPageNum) {
        if let Some((idx, area)) = self
            .areas
            .iter_mut()
            .enumerate()
            .find(|(_, area)| area.vpn_range.get_start() == start_vpn)
        {
            area.unmap(&mut self.page_table);
            self.areas.remove(idx);
        }
    }
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data, 0);
        }
        self.areas.push(map_area);
    }

    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        println!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        println!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        println!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        println!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        println!("mapping .text section");
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        println!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        println!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        println!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        println!("mapping physical memory");
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                crate::mm::frame_allocator::ram_end().into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        println!("mapping memory-mapped registers");
        for pair in MMIO {
            memory_set.push(
                MapArea::new(
                    (*pair).0.into(),
                    ((*pair).0 + (*pair).1).into(),
                    MapType::Identical,
                    MapPermission::R | MapPermission::W,
                ),
                None,
            );
        }
        memory_set
    }
    pub fn from_elf(source: Executable) -> Result<(Self, ElfHeaders), isize> {
        let (image, headers) = ElfImage::parse(source)?;
        let image = Arc::new(image);
        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        for (range, permissions) in image.areas() {
            let address = range.start.0 * PAGE_SIZE;
            memory_set.insert_vma(
                range,
                permissions,
                VmaBacking::Executable { image: Arc::clone(&image), address },
            );
        }
        Ok((memory_set, headers))
    }
    pub fn from_existed_user(user_space: &MemorySet) -> MemorySet {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // copy data sections/trap_context/user_stack
        for area in user_space.areas.iter() {
            let mut new_area = MapArea::from_another(area);
            for vpn in area.vpn_range {
                if let Some(pte) = user_space.translate(vpn) {
                    if !pte.is_valid() {
                        continue;
                    }
                    let src_ppn = pte.ppn();
                    new_area.map_one(&mut memory_set.page_table, vpn);
                    memory_set.page_table.set_flags(vpn, pte.flags());
                    let dst_ppn = memory_set.page_table.translate(vpn).unwrap().ppn();
                    dst_ppn
                        .get_bytes_array()
                        .copy_from_slice(src_ppn.get_bytes_array());
                    #[cfg(feature = "elf_startup_stats")]
                    crate::fork_stats::on_frame_copy();
                }
            }
            memory_set.areas.push(new_area);
        }
        memory_set.vmas = user_space.vmas.fork(&mut memory_set.page_table);
        memory_set
    }
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            asm!("sfence.vma");
        }
    }
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    pub fn recycle_data_pages(&mut self) {
        self.vmas = VmaSpace::default();
        self.areas.clear();
    }
}

pub struct MapArea {
    vpn_range: VPNRange,
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    pub fn from_another(another: &MapArea) -> Self {
        Self {
            vpn_range: VPNRange::new(another.vpn_range.get_start(), another.vpn_range.get_end()),
            data_frames: BTreeMap::new(),
            map_type: another.map_type,
            map_perm: another.map_perm,
        }
    }
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        // lazy 区里可能有从未缺页访问过的页，此时页表里没有有效 PTE，不能 unmap
        let valid = page_table
            .translate(vpn)
            .map(|pte| pte.is_valid())
            .unwrap_or(false);
        if valid {
            page_table.unmap(vpn);
        }
    }
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8], offset: usize) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut written = 0usize;
        let mut current_vpn = self.vpn_range.get_start();
        let mut page_off = offset % PAGE_SIZE;
        while written < data.len() {
            let n = (PAGE_SIZE - page_off).min(data.len() - written);
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[page_off..page_off + n];
            dst.copy_from_slice(&data[written..written + n]);
            written += n;
            page_off = 0;
            if written < data.len() {
                current_vpn.step();
            }
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MapType {
    Identical,
    Framed,
}

bitflags! {
    pub struct MapPermission: u8 {
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
    }
}

/// 用户态缺页入口：找到当前进程，交给它的 MemorySet 处理。
pub fn handle_user_page_fault(stval: usize, access: MapPermission) -> PageFault {
    if stval >= USER_END {
        return PageFault::Invalid;
    }
    let vpn = VirtPageNum(stval / PAGE_SIZE);
    let process = crate::task::current_process();
    let mut inner = process.inner_exclusive_access();
    inner.memory_set.handle_page_fault(vpn, access)
}

#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
