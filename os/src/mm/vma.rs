use super::elf::ElfImage;
use super::resident::{alloc_page, ResidentPage};
use super::{FrameTracker, MapPermission, PageTable, VirtPageNum};
use crate::{config::PAGE_SIZE, fs::File, sync::UPSafeCell};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::ops::Range;

pub const USER_END: usize = 1 << 38;

#[derive(PartialEq, Eq)]
pub enum PageFault {
    Resolved,
    Invalid,
    FileUnavailable,
}

#[derive(Clone)]
pub enum VmaBacking {
    Anonymous,
    SharedAnonymous {
        pages: Arc<UPSafeCell<BTreeMap<usize, Arc<FrameTracker>>>>,
        offset: usize,
    },
    File {
        file: Arc<dyn File + Send + Sync>,
        offset: usize,
        shared: bool,
    },
    Executable {
        image: Arc<ElfImage>,
        address: usize,
    },
}

impl VmaBacking {
    pub fn anonymous(shared: bool) -> Self {
        if shared {
            Self::SharedAnonymous {
                pages: Arc::new(unsafe { UPSafeCell::new(BTreeMap::new()) }),
                offset: 0,
            }
        } else {
            Self::Anonymous
        }
    }

    pub fn file(file: Arc<dyn File + Send + Sync>, offset: usize, shared: bool) -> Self {
        Self::File {
            file,
            offset,
            shared,
        }
    }

    fn advance(&mut self, bytes: usize) {
        match self {
            Self::File { offset, .. } | Self::SharedAnonymous { offset, .. } => *offset += bytes,
            Self::Executable { address, .. } => *address += bytes,
            Self::Anonymous => (),
        }
    }

    fn is_shared(&self) -> bool {
        matches!(
            self,
            Self::SharedAnonymous { .. } | Self::File { shared: true, .. }
        )
    }

    pub fn allows(&self, permissions: MapPermission) -> bool {
        if let Self::File {
            file, shared: true, ..
        } = self
        {
            return !permissions.contains(MapPermission::W)
                || (file.writable() && file.supports_mapped_writes());
        }
        true
    }

    fn load(&self, bytes: usize) -> Result<ResidentPage, PageFault> {
        let (frame, copy_on_write) = match self {
            Self::Anonymous => (alloc_page()?, false),
            Self::SharedAnonymous { pages, offset } => {
                let mut pages = pages.exclusive_access();
                let index = (offset + bytes) / PAGE_SIZE;
                let page = if let Some(page) = pages.get(&index) {
                    page.clone()
                } else {
                    let page = alloc_page()?;
                    pages.insert(index, page.clone());
                    page
                };
                (page, false)
            }
            Self::File {
                file,
                offset,
                shared,
                ..
            } => {
                let offset = offset + bytes;
                if offset >= file.mapping_size().ok_or(PageFault::Invalid)? {
                    return Err(PageFault::FileUnavailable);
                }
                let page = file.mapping_page(offset, *shared).ok_or(PageFault::FileUnavailable)?;
                (page, !shared)
            }
            Self::Executable { image, address } => return load_elf_page(image, address + bytes),
        };
        Ok(ResidentPage::new(frame, copy_on_write))
    }
}

fn load_elf_page(image: &ElfImage, address: usize) -> Result<ResidentPage, PageFault> {
    let mut parts = image.page_parts(address).peekable();
    // A complete file page can be shared. Leave its writable mappings protected until a store.
    if let Some(&(0, offset, PAGE_SIZE)) = parts.peek() {
        if let Some(frame) = image.source().cached_page(offset) {
            return Ok(ResidentPage::new(frame, true));
        }
    }

    let frame = alloc_page()?;
    let page = frame.ppn.get_bytes_array();
    for (start, offset, len) in parts {
        if image.source().read_at(offset, &mut page[start..start + len]) != len {
            return Err(PageFault::Invalid);
        }
    }
    Ok(ResidentPage::new(frame, false))
}

#[derive(Clone)]
struct Vma {
    end: VirtPageNum,
    permissions: MapPermission,
    backing: VmaBacking,
}

#[derive(Default)]
pub(super) struct VmaSpace {
    areas: BTreeMap<VirtPageNum, Vma>,
    pages: BTreeMap<VirtPageNum, ResidentPage>,
}

impl VmaSpace {
    pub(super) fn pin(&self, vpn: VirtPageNum) -> Option<Arc<FrameTracker>> {
        self.pages.get(&vpn).map(|page| page.frame.clone())
    }

    pub(super) fn insert(
        &mut self,
        range: Range<VirtPageNum>,
        permissions: MapPermission,
        backing: VmaBacking,
    ) {
        self.areas.insert(
            range.start,
            Vma {
                end: range.end,
                permissions,
                backing,
            },
        );
    }

    fn covering(
        areas: &BTreeMap<VirtPageNum, Vma>,
        vpn: VirtPageNum,
    ) -> Option<(VirtPageNum, &Vma)> {
        areas
            .range(..=vpn)
            .next_back()
            .filter(|(_, area)| vpn < area.end)
            .map(|(start, area)| (*start, area))
    }

    pub(super) fn covered_until(
        &self,
        vpn: VirtPageNum,
        permissions: MapPermission,
    ) -> Option<Result<VirtPageNum, isize>> {
        Self::covering(&self.areas, vpn).map(|(_, area)| {
            if area.backing.allows(permissions) {
                Ok(area.end)
            } else {
                Err(-13)
            }
        })
    }

    pub(super) fn overlap_end(&self, range: &Range<VirtPageNum>) -> Option<VirtPageNum> {
        self.areas
            .range(..range.end)
            .next_back()
            .map(|(_, area)| area.end)
            .filter(|&end| range.start < end)
    }

    pub(super) fn overlaps_executable(&self, range: &Range<VirtPageNum>) -> bool {
        self.areas
            .range(..range.end)
            .rev()
            .take_while(|(_, area)| range.start < area.end)
            .any(|(_, area)| matches!(area.backing, VmaBacking::Executable { .. }))
    }

    pub(super) fn has_device_mapping(&self, dev: u64) -> bool {
        self.areas.values().any(|area| match &area.backing {
            VmaBacking::File { file, .. } => {
                file.stat_identity().map_or(false, |(file_dev, _)| file_dev == dev)
            }
            VmaBacking::Executable { image, .. } => image.source().device_id() == Some(dev),
            _ => false,
        })
    }

    pub(super) fn sync_files(&self, range: &Range<VirtPageNum>) -> Result<(), isize> {
        for (&start, area) in self.areas.range(..range.end) {
            if area.end <= range.start { continue; }
            if let VmaBacking::File { file, offset, shared: true } = &area.backing {
                let low = start.max(range.start);
                let high = area.end.min(range.end);
                file.sync_range(offset + (low.0 - start.0) * PAGE_SIZE, (high.0 - low.0) * PAGE_SIZE)?;
            }
        }
        Ok(())
    }

    pub(super) fn is_shared_at(&self, vpn: VirtPageNum) -> bool {
        Self::covering(&self.areas, vpn).map_or(false, |(_, area)| area.backing.is_shared())
    }

    fn split(&mut self, vpn: VirtPageNum) {
        if let Some((start, area)) = Self::covering(&self.areas, vpn) {
            if start < vpn {
                let mut right = area.clone();
                right.backing.advance((vpn.0 - start.0) * PAGE_SIZE);
                self.areas.get_mut(&start).unwrap().end = vpn;
                self.areas.insert(vpn, right);
            }
        }
    }

    pub(super) fn protect(
        &mut self,
        table: &mut PageTable,
        range: Range<VirtPageNum>,
        permissions: MapPermission,
    ) {
        self.split(range.start);
        self.split(range.end);
        for (_, area) in self.areas.range_mut(range.clone()) {
            area.permissions = permissions;
        }
        for (&vpn, page) in self.pages.range(range) {
            table.set_flags(vpn, page.flags(permissions));
        }
    }

    pub(super) fn unmap(
        &mut self,
        table: &mut PageTable,
        range: Range<VirtPageNum>,
        tables: &mut Vec<FrameTracker>,
    ) -> Vec<Arc<FrameTracker>> {
        self.split(range.start);
        self.split(range.end);
        let starts: Vec<_> = self
            .areas
            .range(range.clone())
            .map(|(&start, _)| start)
            .collect();
        for start in starts {
            self.areas.remove(&start);
        }
        let vpns: Vec<_> = self.pages.range(range).map(|(&vpn, _)| vpn).collect();
        let mut retired = Vec::with_capacity(vpns.len());
        for vpn in vpns {
            table.unmap_and_reclaim(vpn, tables);
            retired.push(self.pages.remove(&vpn).unwrap().frame);
        }
        retired
    }

    pub(super) fn invalidate_file_pages(
        &mut self,
        table: &mut PageTable,
        identity: (u64, u64),
        first_invalid: usize,
        tables: &mut Vec<FrameTracker>,
    ) -> Vec<Arc<FrameTracker>> {
        let mut vpns = Vec::new();
        for (&start, area) in &self.areas {
            if let VmaBacking::File { file, offset, .. } = &area.backing {
                if file.stat_identity() == Some(identity) {
                    let skip = first_invalid.saturating_sub(*offset) / PAGE_SIZE;
                    let first = VirtPageNum(start.0 + skip);
                    if first < area.end {
                        vpns.extend(self.pages.range(first..area.end).map(|(&vpn, _)| vpn));
                    }
                }
            }
        }
        let mut retired = Vec::with_capacity(vpns.len());
        for vpn in vpns {
            table.unmap_and_reclaim(vpn, tables);
            retired.push(self.pages.remove(&vpn).unwrap().frame);
        }
        retired
    }

    pub(super) fn fault(
        &mut self,
        table: &mut PageTable,
        vpn: VirtPageNum,
        access: MapPermission,
    ) -> PageFault {
        let (start, area) = match Self::covering(&self.areas, vpn) {
            Some(mapping) if mapping.1.permissions.contains(access) => mapping,
            _ => return PageFault::Invalid,
        };
        let permissions = area.permissions;
        if let Some(page) = self.pages.get_mut(&vpn) {
            return page.write_fault(table, vpn, permissions, access);
        }
        let mut page = match area.backing.load((vpn.0 - start.0) * PAGE_SIZE) {
            Ok(page) => page,
            Err(fault) => return fault,
        };
        if access == MapPermission::W {
            if let Err(fault) = page.make_writable() {
                return fault;
            }
        }
        if !table.try_map(vpn, page.frame.ppn, page.flags(permissions)) {
            return PageFault::Invalid;
        }
        self.pages.insert(vpn, page);
        if permissions.contains(MapPermission::X) {
            crate::arch::fence_icache();
        }
        PageFault::Resolved
    }

    pub(super) fn fork_child(&self, table: &mut PageTable) -> Option<Self> {
        let mut child = Self {
            areas: self.areas.clone(),
            pages: BTreeMap::new(),
        };
        for (&vpn, page) in &self.pages {
            let (_, area) = Self::covering(&self.areas, vpn).unwrap();
            let page = if area.backing.is_shared() {
                page.clone()
            } else {
                page.share_private()
            };
            if !table.try_map(vpn, page.frame.ppn, page.flags(area.permissions)) {
                return None;
            }
            child.pages.insert(vpn, page);
        }
        Some(child)
    }

    pub(super) fn write_protect_private(&mut self, table: &mut PageTable) {
        for (&vpn, page) in &mut self.pages {
            let (_, area) = Self::covering(&self.areas, vpn).unwrap();
            if !area.backing.is_shared() {
                page.write_protect(table, vpn, area.permissions);
            }
        }
    }

    pub(super) fn mremap(
        &mut self,
        table: &mut PageTable,
        old_range: Range<VirtPageNum>,
        new_pages: usize,
        fixed_dest: Option<VirtPageNum>,
        may_move: bool,
        vacant_at: impl FnOnce(VirtPageNum, usize) -> Option<Range<VirtPageNum>>,
        check_vacant: impl Fn(&Range<VirtPageNum>) -> bool,
        tables: &mut Vec<FrameTracker>,
    ) -> Result<(VirtPageNum, Vec<Arc<FrameTracker>>), isize> {
        let old_pages = old_range.end.0 - old_range.start.0;
        let check_end = VirtPageNum(old_range.start.0 + old_pages.max(1));
        // Verify [old_range.start, check_end) is contiguously mapped with uniform VMA properties.
        let (first_start, first_area) = Self::covering(&self.areas, old_range.start).ok_or(-14isize)?;
        let perms = first_area.permissions;
        let mut base_backing = first_area.backing.clone();
        base_backing.advance((old_range.start.0 - first_start.0) * PAGE_SIZE);
        if matches!(base_backing, VmaBacking::Executable { .. }) {
            return Err(-22);
        }
        let mut cursor = first_area.end;
        while cursor < check_end {
            let (_, next) = Self::covering(&self.areas, cursor).ok_or(-14isize)?;
            if next.permissions != perms {
                return Err(-14);
            }
            cursor = next.end;
        }
        if old_pages == 0 {
            if !base_backing.is_shared() || (!may_move && fixed_dest.is_none()) {
                return Err(-22);
            }
            let dest = if let Some(dest) = fixed_dest {
                let target = dest..VirtPageNum(dest.0 + new_pages);
                let retired = self.unmap(table, target.clone(), tables);
                self.insert(target, perms, base_backing);
                return Ok((dest, retired));
            } else {
                vacant_at(old_range.start, new_pages).ok_or(-12isize)?
            };
            let start = dest.start;
            self.insert(dest, perms, base_backing);
            return Ok((start, Vec::new()));
        }

        if fixed_dest.is_none() {
            if new_pages <= old_pages {
                let unmap_range = VirtPageNum(old_range.start.0 + new_pages)..old_range.end;
                let retired = if unmap_range.start < unmap_range.end {
                    self.unmap(table, unmap_range, tables)
                } else {
                    Vec::new()
                };
                return Ok((old_range.start, retired));
            }
            let extra = old_range.end..VirtPageNum(old_range.start.0 + new_pages);
            if extra.end.0 <= USER_END / PAGE_SIZE && check_vacant(&extra) {
                let mut ext_backing = base_backing;
                ext_backing.advance(old_pages * PAGE_SIZE);
                self.insert(extra, perms, ext_backing);
                return Ok((old_range.start, Vec::new()));
            }
            if !may_move {
                return Err(-12);
            }
        }

        let target = if let Some(dest) = fixed_dest {
            dest..VirtPageNum(dest.0 + new_pages)
        } else {
            vacant_at(old_range.end, new_pages).ok_or(-12isize)?
        };
        let copy_pages = old_pages.min(new_pages);
        let old_vpns: Vec<_> = self.pages.range(old_range.clone()).map(|(&vpn, _)| vpn).collect();
        let dest_for = |vpn: VirtPageNum| VirtPageNum(target.start.0 + (vpn.0 - old_range.start.0));
        let resident_to_move = old_vpns.partition_point(|vpn| vpn.0 - old_range.start.0 < copy_pages);
        let move_vpns = &old_vpns[..resident_to_move];
        // Prepare the destination page tables before altering either mapping.
        for (index, &vpn) in move_vpns.iter().enumerate() {
            if !table.prepare_mapping(dest_for(vpn)) {
                for &prepared in &move_vpns[..index] {
                    table.reclaim_empty_path(dest_for(prepared), tables);
                }
                return Err(-12);
            }
        }

        self.split(old_range.start);
        self.split(old_range.end);
        let old_starts: Vec<_> = self.areas.range(old_range.clone()).map(|(&s, _)| s).collect();
        for s in old_starts {
            self.areas.remove(&s);
        }
        let mut retired = Vec::new();
        let mut moved_pages = Vec::with_capacity(resident_to_move);
        for &vpn in move_vpns {
            let page = self.pages.remove(&vpn).unwrap();
            let dest = dest_for(vpn);
            table.install_prepared(dest, page.frame.ppn, page.flags(perms));
            if let Some(previous) = self.pages.remove(&dest) {
                retired.push(previous.frame);
            }
            moved_pages.push((dest, page));
        }
        // The new PTEs keep their page tables alive during teardown of either range.
        for vpn in old_vpns {
            table.unmap_and_reclaim(vpn, tables);
            if let Some(page) = self.pages.remove(&vpn) {
                retired.push(page.frame);
            }
        }
        if fixed_dest.is_some() {
            retired.extend(self.unmap(table, target.clone(), tables));
        }
        for (new_vpn, page) in moved_pages {
            self.pages.insert(new_vpn, page);
        }
        let dest_start = target.start;
        self.insert(target, perms, base_backing);
        Ok((dest_start, retired))
    }

    pub(super) fn format_maps(&self, out: &mut alloc::string::String) {
        use core::fmt::Write;
        for (&start, area) in &self.areas {
            let r = if area.permissions.contains(MapPermission::R) { 'r' } else { '-' };
            let w = if area.permissions.contains(MapPermission::W) { 'w' } else { '-' };
            let x = if area.permissions.contains(MapPermission::X) { 'x' } else { '-' };
            let s = if area.backing.is_shared() { 's' } else { 'p' };
            let offset = match &area.backing {
                VmaBacking::File { offset, .. } | VmaBacking::SharedAnonymous { offset, .. } => *offset,
                VmaBacking::Executable { address, .. } => *address,
                VmaBacking::Anonymous => 0,
            };
            let (dev, ino) = match &area.backing {
                VmaBacking::File { file, .. } => file.stat_identity().unwrap_or((0, 0)),
                _ => (0, 0),
            };
            let _ = writeln!(
                out,
                "{:012x}-{:012x} {}{}{}{} {:08x} {:02x}:{:02x} {}",
                start.0 * PAGE_SIZE,
                area.end.0 * PAGE_SIZE,
                r, w, x, s,
                offset,
                (dev >> 8) & 0xff,
                dev & 0xff,
                ino,
            );
        }
    }

    pub(super) fn madvise_check(&self, range: Range<VirtPageNum>, advice: usize) -> Result<(), isize> {
        let mut cursor = range.start;
        while cursor < range.end {
            let Some((_, area)) = self.areas.range(..=cursor).next_back() else {
                return Err(-12); // ENOMEM
            };
            if area.end <= cursor {
                return Err(-12); // ENOMEM
            }
            if advice == 9 {
                if !area.backing.is_shared() {
                    return Err(-22);
                }
                if !area.permissions.contains(MapPermission::W) {
                    return Err(-13);
                }
            } else if (advice == 18 || advice == 19)
                && (area.backing.is_shared() || !matches!(area.backing, VmaBacking::Anonymous))
            {
                return Err(-22);
            }
            cursor = area.end;
        }
        Ok(())
    }
}
