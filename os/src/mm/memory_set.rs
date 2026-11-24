use super::elf::{ElfHeaders, ElfImage};
use super::resident::{alloc_page, ResidentPage};
use super::FrameTracker;
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use super::vma::{PageFault, VmaBacking, VmaSpace, USER_END};
use crate::config::{MMIO, PAGE_SIZE, TRAMPOLINE, USER_STACK_SIZE};
use crate::fs::{open_executable, resolve_path, Executable};
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
        Self::try_new_bare().expect("out of page-table frames")
    }

    fn try_new_bare() -> Option<Self> {
        Some(Self {
            page_table: PageTable::try_new()?,
            areas: Vec::new(),
            vmas: VmaSpace::default(),
        })
    }
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// The address range must be vacant.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        assert!(
            self.try_insert_framed_area(start_va, end_va, permission),
            "out of frames for a framed area"
        );
    }

    pub(crate) fn try_insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) -> bool {
        let mut area = MapArea::new(start_va, end_va, MapType::Framed, permission);
        for vpn in area.vpn_range {
            if !area.try_map_one(&mut self.page_table, vpn) {
                area.unmap_frames(&mut self.page_table);
                return false;
            }
        }
        self.areas.push(area);
        true
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
        if let Some(page) = self.areas.iter_mut().find_map(|area| area.data_frames.get_mut(&vpn)) {
            return page.resident.write_fault(&mut self.page_table, vpn, page.permissions, access);
        }
        self.vmas.fault(&mut self.page_table, vpn, access)
    }

    pub(crate) fn pin_user_page(&self, vpn: VirtPageNum) -> Option<Arc<FrameTracker>> {
        self.vmas.pin(vpn).or_else(|| {
            self.areas.iter().find_map(|area| {
                area.data_frames.get(&vpn).map(|page| page.resident.frame.clone())
            })
        })
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

    pub(crate) fn invalidate_file_pages(&mut self, identity: (u64, u64), first_invalid: usize) {
        let mut tables = Vec::new();
        let retired = self.vmas.invalidate_file_pages(
            &mut self.page_table, identity, first_invalid, &mut tables,
        );
        if !retired.is_empty() {
            unsafe { asm!("sfence.vma"); }
        }
        drop((retired, tables));
    }

    pub(crate) fn check_user_range(
        &self,
        range: &Range<VirtPageNum>,
        permissions: MapPermission,
    ) -> Result<(), isize> {
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
        Ok(())
    }

    pub fn mprotect(&mut self, range: Range<VirtPageNum>, permissions: MapPermission) -> Result<(), isize> {
        self.check_user_range(&range, permissions)?;
        self.vmas.protect(&mut self.page_table, range.clone(), permissions);
        for area in &mut self.areas {
            for (&vpn, page) in area.data_frames.range_mut(range.clone()) {
                page.permissions = permissions;
                self.page_table.set_flags(vpn, page.resident.flags(permissions));
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
        if let Some(idx) = self
            .areas
            .iter()
            .position(|area| area.vpn_range.get_start() == start_vpn)
        {
            self.areas.remove(idx).unmap_frames(&mut self.page_table);
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
        assert!(self.try_map_trampoline(), "out of page-table frames for the trampoline");
    }

    fn try_map_trampoline(&mut self) -> bool {
        self.page_table.try_map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        )
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
        Self::load_elf(source, None)
    }

    pub fn from_exec(source: Executable, path: &str) -> Result<(Self, ElfHeaders), isize> {
        Self::load_elf(source, Some(path))
    }

    fn load_elf(source: Executable, path: Option<&str>) -> Result<(Self, ElfHeaders), isize> {
        const ENOEXEC: isize = -8;
        const ELIBBAD: isize = -80;
        const PIE_BASE: usize = 0x0400_0000;
        const INTERPRETER_BASE: usize = 0x4000_0000;
        const HEAP_GAP: usize = 0x0400_0000;

        let (mut image, mut headers) = ElfImage::parse(source)?;
        let interpreter = match (path, headers.interpreter.as_deref()) {
            (_, None) => None,
            (None, Some(_)) => return Err(ENOEXEC), // Kernel-started init is static.
            (Some(path), Some(interpreter)) => {
                match open_executable(&resolve_path(interpreter)?) {
                    Ok(source) => Some(source),
                    Err(-2) if image.is_official_basic(path, &headers) => None,
                    Err(errno) => return Err(errno),
                }
            }
        };
        image.load_at(&mut headers, if interpreter.is_some() { PIE_BASE } else { 0 })?;
        headers.stack_base = image.stack_base()?;

        let mut memory_set = Self::new_bare();
        memory_set.map_trampoline();
        memory_set.map_elf_image(image);
        if let Some(source) = interpreter {
            if headers.program_addr.is_none() {
                return Err(ENOEXEC);
            }
            let (mut loader, mut loader_headers) = ElfImage::parse(source).map_err(|_| ELIBBAD)?;
            if loader_headers.interpreter.is_some() {
                return Err(ELIBBAD);
            }
            let stack_start = (headers.stack_base - PAGE_SIZE) / PAGE_SIZE;
            let stack_end = headers.stack_base + USER_STACK_SIZE + PAGE_SIZE;
            // Leave room for brk to grow even when the main image is linked high.
            let minimum_bias = INTERPRETER_BASE.max(stack_end + HEAP_GAP);
            headers.interpreter_base = loader
                .load_at(&mut loader_headers, minimum_bias)
                .map_err(|_| ELIBBAD)?;
            if loader.areas().iter().any(|(range, _)| {
                !memory_set.vacant(range)
                    || (range.start.0 < stack_end / PAGE_SIZE && range.end.0 > stack_start)
            }) {
                return Err(ELIBBAD);
            }
            headers.start_entry = loader_headers.entry;
            memory_set.map_elf_image(loader);
        }
        Ok((memory_set, headers))
    }

    fn map_elf_image(&mut self, image: ElfImage) {
        let image = Arc::new(image);
        for (range, permissions) in image.areas() {
            let address = range.start.0 * PAGE_SIZE;
            self.insert_vma(
                range,
                permissions,
                VmaBacking::Executable { image: Arc::clone(&image), address },
            );
        }
    }
    pub fn fork(&mut self) -> Result<Self, isize> {
        const ENOMEM: isize = -12;
        let mut child = Self::try_new_bare().ok_or(ENOMEM)?;
        if !child.try_map_trampoline() {
            return Err(ENOMEM);
        }

        for area in &self.areas {
            debug_assert_eq!(area.map_type, MapType::Framed);
            let mut new_area = MapArea::from_another(area);
            for (&vpn, page) in &area.data_frames {
                let resident = if page.permissions.contains(MapPermission::U) {
                    page.resident.share_private()
                } else {
                    // Trap contexts are written through direct kernel mappings.
                    page.resident.private_copy().map_err(|_| ENOMEM)?
                };
                if !child.page_table.try_map(vpn, resident.frame.ppn, resident.flags(page.permissions)) {
                    return Err(ENOMEM);
                }
                new_area.data_frames.insert(vpn, FramedPage { resident, permissions: page.permissions });
            }
            child.areas.push(new_area);
        }
        child.vmas = self.vmas.fork_child(&mut child.page_table).ok_or(ENOMEM)?;

        // Commit the parent's write protection only after the child's mappings succeeded.
        for area in &mut self.areas {
            for (&vpn, page) in &mut area.data_frames {
                if page.permissions.contains(MapPermission::U) {
                    page.resident.write_protect(&mut self.page_table, vpn, page.permissions);
                }
            }
        }
        self.vmas.write_protect_private(&mut self.page_table);
        unsafe { asm!("sfence.vma"); }
        Ok(child)
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

struct FramedPage {
    resident: ResidentPage,
    permissions: MapPermission,
}

pub struct MapArea {
    vpn_range: VPNRange,
    data_frames: BTreeMap<VirtPageNum, FramedPage>,
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
    fn try_map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) -> bool {
        let flags = PTEFlags::from_bits(self.map_perm.bits()).unwrap();
        if self.map_type == MapType::Identical {
            return page_table.try_map(vpn, PhysPageNum(vpn.0), flags);
        }
        let frame = match alloc_page() {
            Ok(frame) => frame,
            Err(_) => return false,
        };
        if !page_table.try_map(vpn, frame.ppn, flags) {
            return false;
        }
        let resident = ResidentPage::new(frame, false);
        self.data_frames.insert(vpn, FramedPage { resident, permissions: self.map_perm });
        true
    }

    fn unmap_frames(self, page_table: &mut PageTable) {
        debug_assert_eq!(self.map_type, MapType::Framed);
        let mut tables = Vec::new();
        for &vpn in self.data_frames.keys() {
            page_table.unmap_and_reclaim(vpn, &mut tables);
        }
        unsafe { asm!("sfence.vma"); }
        drop((self, tables));
    }

    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            assert!(self.try_map_one(page_table, vpn), "out of frames for a mapped area");
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
