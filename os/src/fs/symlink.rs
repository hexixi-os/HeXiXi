use super::{overlay::SymlinkCharge, File};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use lazy_static::lazy_static;

pub(super) struct Symlink {
    target: Vec<u8>,
    identity: Option<(u64, u64)>,
    links: AtomicU32,
    charge: UPSafeCell<Option<SymlinkCharge>>,
}

lazy_static! {
    static ref LOWER_LINKS: UPSafeCell<BTreeMap<(u64, u64), Weak<Symlink>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

impl Symlink {
    pub fn new(
        target: Vec<u8>,
        identity: Option<(u64, u64)>,
        charge: Option<SymlinkCharge>,
    ) -> Self {
        Self {
            target,
            identity,
            links: AtomicU32::new(1),
            charge: unsafe { UPSafeCell::new(charge) },
        }
    }

    pub fn lower(fs: &super::ext4::Ext4Fs, inode: &super::ext4::Inode) -> Result<Arc<Self>, isize> {
        let identity = fs.inode_identity(inode);
        let mut links = LOWER_LINKS.exclusive_access();
        if let Some(link) = links.get(&identity).and_then(Weak::upgrade) {
            return Ok(link);
        }
        links.retain(|_, link| link.strong_count() != 0);
        let link = Arc::new(Self::new(fs.read_symlink(inode)?, Some(identity), None));
        link.links
            .store(u32::from(inode.link_count), Ordering::Relaxed);
        links.insert(identity, Arc::downgrade(&link));
        Ok(link)
    }

    pub fn reserve_copy_up(&self) -> Result<Option<SymlinkCharge>, isize> {
        if self.charge.exclusive_access().is_some() {
            Ok(None)
        } else {
            SymlinkCharge::new(self.target.len()).map(Some)
        }
    }

    pub fn commit_copy_up(&self, charge: SymlinkCharge) {
        *self.charge.exclusive_access() = Some(charge);
    }

    pub fn detach(&self) {
        let _ = self
            .links
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                count.checked_sub(1)
            });
    }
}

impl File for Symlink {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, _: UserBuffer) -> usize {
        0
    }
    fn write(&self, _: UserBuffer) -> isize {
        -9
    }
    fn link_target(&self) -> Option<&[u8]> {
        Some(&self.target)
    }
    fn mode(&self) -> u16 {
        0o120777
    }
    fn size(&self) -> usize {
        self.target.len()
    }
    fn stat_identity(&self) -> Option<(u64, u64)> {
        self.identity
    }
    fn link_count(&self) -> u32 {
        self.links.load(Ordering::Relaxed)
    }
}
