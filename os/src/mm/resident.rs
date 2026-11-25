use super::{
    copy_live_bytes, frame_alloc, FrameTracker, MapPermission, PTEFlags, PageFault, PageTable,
    VirtPageNum,
};
use crate::config::PAGE_SIZE;
use alloc::sync::Arc;

pub(super) fn alloc_page() -> Result<Arc<FrameTracker>, PageFault> {
    frame_alloc().map(Arc::new).ok_or(PageFault::Invalid)
}

#[derive(Clone)]
pub(super) struct ResidentPage {
    pub(super) frame: Arc<FrameTracker>,
    copy_on_write: bool,
}

impl ResidentPage {
    pub(super) fn new(frame: Arc<FrameTracker>, copy_on_write: bool) -> Self {
        Self {
            frame,
            copy_on_write,
        }
    }

    pub(super) fn flags(&self, mut permissions: MapPermission) -> PTEFlags {
        if self.copy_on_write {
            permissions.remove(MapPermission::W);
        }
        PTEFlags::from_bits(permissions.bits()).unwrap()
    }

    pub(super) fn share_private(&self) -> Self {
        Self::new(self.frame.clone(), true)
    }

    pub(super) fn write_protect(
        &mut self,
        table: &mut PageTable,
        vpn: VirtPageNum,
        permissions: MapPermission,
    ) {
        if !self.copy_on_write {
            self.copy_on_write = true;
            if permissions.contains(MapPermission::W) {
                table.set_flags(vpn, self.flags(permissions));
            }
        }
    }

    pub(super) fn private_copy(&self) -> Result<Self, PageFault> {
        let frame = alloc_page()?;
        // A private file mapping can still source its COW bytes from an active shared mapping.
        unsafe { copy_live_bytes(self.frame.ppn.bytes_ptr(), frame.ppn.bytes_ptr(), PAGE_SIZE) };
        #[cfg(feature = "elf_startup_stats")]
        crate::fork_stats::on_frame_copy();
        Ok(Self::new(frame, false))
    }

    /// Give a private mapping a writable frame, holding any old frame until the caller's TLB flush.
    pub(super) fn make_writable(&mut self) -> Result<Option<Self>, PageFault> {
        if !self.copy_on_write {
            return Ok(None);
        }
        if Arc::strong_count(&self.frame) == 1 {
            self.copy_on_write = false;
            return Ok(None);
        }
        let new = self.private_copy()?;
        Ok(Some(core::mem::replace(self, new)))
    }

    pub(super) fn write_fault(
        &mut self,
        table: &mut PageTable,
        vpn: VirtPageNum,
        permissions: MapPermission,
        access: MapPermission,
    ) -> PageFault {
        if !self.copy_on_write
            || access != MapPermission::W
            || !permissions.contains(MapPermission::W)
        {
            if permissions.contains(access)
                && (access != MapPermission::W || !self.copy_on_write)
                && table.translate(vpn).map_or(false, |entry| {
                    entry.is_valid()
                        && entry
                            .flags()
                            .contains(PTEFlags::U | PTEFlags::from_bits(access.bits()).unwrap())
                })
            {
                return PageFault::Resolved;
            }
            return PageFault::Invalid;
        }
        let retired = match self.make_writable() {
            Ok(retired) => retired,
            Err(fault) => return fault,
        };
        table.remap(vpn, self.frame.ppn, self.flags(permissions));
        crate::arch::local_flush_tlb();
        drop(retired);
        PageFault::Resolved
    }
}
