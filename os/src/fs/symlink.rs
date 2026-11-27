use super::{overlay::SymlinkCharge, File};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use lazy_static::lazy_static;

pub(super) struct Symlink {
    target: Vec<u8>,
    identity: Option<(u64, u64)>,
    links: AtomicU32,
    uid: AtomicU32,
    gid: AtomicU32,
    xattrs: UPSafeCell<BTreeMap<String, Vec<u8>>>,
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
        let (fsuid, fsgid) = crate::task::current_task()
            .and_then(|t| t.process.upgrade())
            .map_or((0, 0), |p| {
                let inner = p.inner_exclusive_access();
                (inner.fsuid, inner.fsgid)
            });
        Self {
            target,
            identity,
            links: AtomicU32::new(1),
            uid: AtomicU32::new(fsuid),
            gid: AtomicU32::new(fsgid),
            xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
            charge: unsafe { UPSafeCell::new(charge) },
        }
    }

    pub fn set_owner_ids(&self, uid: Option<u32>, gid: Option<u32>) {
        if let Some(u) = uid {
            self.uid.store(u, Ordering::Relaxed);
        }
        if let Some(g) = gid {
            self.gid.store(g, Ordering::Relaxed);
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

    pub fn add_link(&self) {
        self.links.fetch_add(1, Ordering::Relaxed);
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
    fn owner(&self) -> (u32, u32) {
        (self.uid.load(Ordering::Relaxed), self.gid.load(Ordering::Relaxed))
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
    fn xattr_op(
        &self,
        name: &str,
        set_val: Option<(&[u8], usize)>,
        remove: bool,
    ) -> Result<Vec<u8>, isize> {
        super::overlay::apply_xattr_map(&mut self.xattrs.exclusive_access(), name, set_val, remove)
    }
}
