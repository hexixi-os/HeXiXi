use super::{
    overlay::{directory_identity, OverlayDirectory},
    persistent::{PersistentFs, PersistentNode},
    File,
};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::{Arc, Weak};
use alloc::{borrow::Cow, string::String};
use core::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone)]
pub(super) enum DirectoryIdentity {
    Permanent,
    Lower,
    Upper(Arc<OverlayDirectory>),
    Proc(Weak<crate::task::ProcessControlBlock>),
    Persistent(Arc<PersistentFs>, Arc<PersistentNode>),
}

#[derive(Clone)]
pub struct DirectoryLocation {
    path: String,
    pub(super) identity: DirectoryIdentity,
}

impl DirectoryLocation {
    pub fn root() -> Self {
        Self::new("", DirectoryIdentity::Permanent)
    }

    pub(super) fn new(path: &str, identity: DirectoryIdentity) -> Self {
        Self {
            path: if matches!(identity, DirectoryIdentity::Upper(_) | DirectoryIdentity::Persistent(_, _)) {
                String::new()
            } else {
                String::from(path)
            },
            identity,
        }
    }

    pub fn path(&self) -> Cow<'_, str> {
        match &self.identity {
            DirectoryIdentity::Upper(dir) => Cow::Owned(dir.full_path()),
            DirectoryIdentity::Persistent(_, node) => Cow::Owned(node.path()),
            _ => Cow::Borrowed(&self.path),
        }
    }
}

/// Directory location and cursor shared by descriptors for the same open instance.
pub struct DirectoryFile {
    location: DirectoryLocation,
    position: UPSafeCell<usize>,
    extra_flags: AtomicU32,
}

impl DirectoryFile {
    pub(super) fn new(path: &str, identity: DirectoryIdentity) -> Self {
        Self {
            location: DirectoryLocation::new(path, identity),
            position: unsafe { UPSafeCell::new(0) },
            extra_flags: AtomicU32::new(0o200000), // O_DIRECTORY
        }
    }

    pub fn location(&self) -> &DirectoryLocation {
        &self.location
    }

    pub fn position(&self) -> usize {
        *self.position.exclusive_access()
    }

    pub fn set_position(&self, position: usize) {
        *self.position.exclusive_access() = position;
    }
}

impl File for DirectoryFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        false
    }

    fn status_flags(&self) -> u32 {
        self.extra_flags.load(Ordering::Relaxed)
    }

    fn set_status_flags(&self, flags: u32) {
        const MUTABLE: u32 = 0o2000 | 0o4000 | 0o20000 | 0o40000 | 0o1000000;
        self.extra_flags
            .store(0o200000 | (flags & MUTABLE), Ordering::Relaxed);
    }

    fn timestamps(&self) -> ((i64, i64), (i64, i64), (i64, i64)) {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => dir.timestamps(),
            _ => ((0, 0), (0, 0), (0, 0)),
        }
    }

    fn set_timestamps(&self, atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> isize {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => {
                dir.set_timestamps(atime, mtime);
                0
            }
            DirectoryIdentity::Persistent(fs, _) => {
                if fs.readonly() { -30 } else { 0 }
            }
            _ => 0,
        }
    }

    fn xattr_op(
        &self,
        name: &str,
        set_val: Option<(&[u8], usize)>,
        remove: bool,
    ) -> Result<alloc::vec::Vec<u8>, isize> {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => {
                super::overlay::apply_xattr_map(&mut dir.xattrs.exclusive_access(), name, set_val, remove)
            }
            DirectoryIdentity::Lower | DirectoryIdentity::Permanent => {
                if set_val.is_some() || remove {
                    Err(-95)
                } else if name.is_empty() {
                    Ok(alloc::vec::Vec::new())
                } else {
                    Err(-61)
                }
            }
            _ => Err(-95),
        }
    }

    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }

    fn write(&self, _buf: UserBuffer) -> isize {
        -21 // EISDIR
    }

    fn directory(&self) -> Option<&DirectoryFile> {
        Some(self)
    }

    fn mode(&self) -> u16 {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => dir.mode(),
            _ => 0o40755,
        }
    }

    fn owner(&self) -> (u32, u32) {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => dir.owner(),
            _ => (0, 0),
        }
    }

    fn set_mode(&self, perm: u16) -> isize {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => {
                dir.set_mode(perm);
                0
            }
            DirectoryIdentity::Persistent(fs, _) => {
                if fs.readonly() { -30 } else { 0 }
            }
            _ => -30,
        }
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> isize {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => {
                dir.set_owner(uid, gid);
                0
            }
            DirectoryIdentity::Persistent(fs, _) => {
                if fs.readonly() { -30 } else { 0 }
            }
            _ => -30,
        }
    }

    fn stat_identity(&self) -> Option<(u64, u64)> {
        match &self.location.identity {
            DirectoryIdentity::Upper(dir) => Some(directory_identity(dir)),
            DirectoryIdentity::Persistent(_, node) => Some(node.identity()),
            _ => None,
        }
    }

    fn link_count(&self) -> u32 {
        u32::from(super::directory_is_live(&self.location))
    }

    fn size(&self) -> usize {
        4096
    }

    fn space_usage(&self) -> Option<(easy_fs::SpaceUsage, bool)> {
        match &self.location.identity {
            DirectoryIdentity::Persistent(fs, _) => Some((fs.space_usage(), fs.readonly())),
            _ => None,
        }
    }

    fn sync_range(&self, _offset: usize, _length: usize) -> Result<(), isize> {
        if let DirectoryIdentity::Persistent(fs, _) = &self.location.identity { fs.sync()?; }
        Ok(())
    }

    fn seek_to(&self, position: isize) -> isize {
        if position < 0 {
            return -22;
        }
        self.set_position(position as usize);
        position
    }

    fn seek(&self, delta: isize) -> isize {
        let cur = self.position() as isize;
        cur.checked_add(delta).map_or(-75, |pos| self.seek_to(pos))
    }
}
