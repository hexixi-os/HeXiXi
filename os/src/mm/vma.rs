use super::elf::ElfImage;
use super::{frame_alloc, FrameTracker, MapPermission, PTEFlags, PageTable, VirtPageNum};
use crate::{config::PAGE_SIZE, fs::File, sync::UPSafeCell};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::{arch::asm, ops::Range};

pub const USER_END: usize = 1 << 38;

#[derive(PartialEq, Eq)]
pub enum PageFault {
    Resolved,
    Invalid,
    BeyondEof,
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
        _lease: Option<Arc<()>>,
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
        let lease = file.mapping_lease();
        Self::File {
            file,
            offset,
            shared,
            _lease: lease,
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
                    return Err(PageFault::BeyondEof);
                }
                let page = file.mapping_page(offset).ok_or(PageFault::Invalid)?;
                (page, !shared)
            }
            Self::Executable { image, address } => return load_elf_page(image, address + bytes),
        };
        Ok(ResidentPage {
            frame,
            copy_on_write,
        })
    }
}

fn load_elf_page(image: &ElfImage, address: usize) -> Result<ResidentPage, PageFault> {
    let mut parts = image.page_parts(address).peekable();
    // A complete file page can be shared. Leave its writable mappings protected until a store.
    if let Some(&(0, offset, PAGE_SIZE)) = parts.peek() {
        if let Some(frame) = image.source().cached_page(offset) {
            return Ok(ResidentPage { frame, copy_on_write: true });
        }
    }

    let frame = alloc_page()?;
    let page = frame.ppn.get_bytes_array();
    for (start, offset, len) in parts {
        if image.source().read_at(offset, &mut page[start..start + len]) != len {
            return Err(PageFault::Invalid);
        }
    }
    Ok(ResidentPage { frame, copy_on_write: false })
}

fn alloc_page() -> Result<Arc<FrameTracker>, PageFault> {
    frame_alloc().map(Arc::new).ok_or(PageFault::Invalid)
}

#[derive(Clone)]
struct ResidentPage {
    frame: Arc<FrameTracker>,
    copy_on_write: bool,
}

impl ResidentPage {
    fn flags(&self, mut permissions: MapPermission) -> PTEFlags {
        if self.copy_on_write {
            permissions.remove(MapPermission::W);
        }
        PTEFlags::from_bits(permissions.bits()).unwrap()
    }

    fn private_copy(&self) -> Result<Self, PageFault> {
        let frame = alloc_page()?;
        frame
            .ppn
            .get_bytes_array()
            .copy_from_slice(self.frame.ppn.get_bytes_array());
        Ok(Self {
            frame,
            copy_on_write: false,
        })
    }
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
            if !page.copy_on_write || access != MapPermission::W {
                return PageFault::Invalid;
            }
            let new = match page.private_copy() {
                Ok(page) => page,
                Err(fault) => return fault,
            };
            table.remap(vpn, new.frame.ppn, new.flags(permissions));
            let old = core::mem::replace(page, new);
            unsafe {
                asm!("sfence.vma");
            }
            drop(old);
            return PageFault::Resolved;
        }
        let mut page = match area.backing.load((vpn.0 - start.0) * PAGE_SIZE) {
            Ok(page) => page,
            Err(fault) => return fault,
        };
        if page.copy_on_write && access == MapPermission::W {
            page = match page.private_copy() {
                Ok(page) => page,
                Err(fault) => return fault,
            };
        }
        if !table.try_map(vpn, page.frame.ppn, page.flags(permissions)) {
            return PageFault::Invalid;
        }
        self.pages.insert(vpn, page);
        PageFault::Resolved
    }

    pub(super) fn fork(&self, table: &mut PageTable) -> Self {
        let mut child = Self {
            areas: self.areas.clone(),
            pages: BTreeMap::new(),
        };
        for (&vpn, page) in &self.pages {
            let (_, area) = Self::covering(&self.areas, vpn).unwrap();
            let new_page = if area.backing.is_shared() || page.copy_on_write {
                page.clone()
            } else {
                match page.private_copy() {
                    Ok(page) => page,
                    Err(_) => panic!("out of frames copying a resident page during fork"),
                }
            };
            table.map(vpn, new_page.frame.ppn, new_page.flags(area.permissions));
            child.pages.insert(vpn, new_page);
        }
        child
    }
}
