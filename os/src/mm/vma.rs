use super::{frame_alloc, FrameTracker, MapPermission, PTEFlags, PageTable, VirtPageNum};
use crate::{config::PAGE_SIZE, fs::File, sync::UPSafeCell};
use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::ops::Range;

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
                || (file.writable() && file.shared_mappable());
        }
        true
    }

    fn load(&self, bytes: usize) -> Result<Arc<FrameTracker>, PageFault> {
        fn alloc_page() -> Result<Arc<FrameTracker>, PageFault> {
            frame_alloc().map(Arc::new).ok_or(PageFault::Invalid)
        }

        match self {
            Self::Anonymous => alloc_page(),
            Self::SharedAnonymous { pages, offset } => {
                let mut pages = pages.exclusive_access();
                let index = (offset + bytes) / PAGE_SIZE;
                if let Some(page) = pages.get(&index) {
                    return Ok(page.clone());
                }
                let page = alloc_page()?;
                pages.insert(index, page.clone());
                Ok(page)
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
                if *shared && file.shared_mappable() {
                    return file.shared_page(offset).ok_or(PageFault::Invalid);
                }
                let page = alloc_page()?;
                file.read_mapping(offset, page.ppn.get_bytes_array());
                Ok(page)
            }
        }
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
    pages: BTreeMap<VirtPageNum, Arc<FrameTracker>>,
}

fn pte_flags(permissions: MapPermission) -> PTEFlags {
    PTEFlags::from_bits(permissions.bits()).unwrap()
}

impl VmaSpace {
    pub(super) fn pin(&self, vpn: VirtPageNum) -> Option<Arc<FrameTracker>> {
        self.pages.get(&vpn).cloned()
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

    fn covering(&self, vpn: VirtPageNum) -> Option<(VirtPageNum, &Vma)> {
        self.areas
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
        self.covering(vpn).map(|(_, area)| {
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

    fn split(&mut self, vpn: VirtPageNum) {
        if let Some((start, area)) = self.covering(vpn) {
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
        for (&vpn, _) in self.pages.range(range) {
            table.set_flags(vpn, pte_flags(permissions));
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
            retired.push(self.pages.remove(&vpn).unwrap());
        }
        retired
    }

    pub(super) fn fault(
        &mut self,
        table: &mut PageTable,
        vpn: VirtPageNum,
        access: MapPermission,
    ) -> PageFault {
        let (start, area) = match self.covering(vpn) {
            Some(mapping) if mapping.1.permissions.contains(access) => mapping,
            _ => return PageFault::Invalid,
        };
        if self.pages.contains_key(&vpn) {
            return PageFault::Invalid;
        }
        let page = match area.backing.load((vpn.0 - start.0) * PAGE_SIZE) {
            Ok(page) => page,
            Err(fault) => return fault,
        };
        if !table.try_map(vpn, page.ppn, pte_flags(area.permissions)) {
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
            let (_, area) = self.covering(vpn).unwrap();
            let new_page = if area.backing.is_shared() {
                page.clone()
            } else {
                let copy = Arc::new(frame_alloc().unwrap());
                copy.ppn
                    .get_bytes_array()
                    .copy_from_slice(page.ppn.get_bytes_array());
                copy
            };
            table.map(vpn, new_page.ppn, pte_flags(area.permissions));
            child.pages.insert(vpn, new_page);
        }
        child
    }
}
