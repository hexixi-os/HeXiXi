use super::{
    overlay::{directory_identity, OverlayDirectory},
    persistent::{PersistentFs, PersistentNode},
    File,
};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::{Arc, Weak};
use alloc::{borrow::Cow, string::String};

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
}

impl DirectoryFile {
    pub(super) fn new(path: &str, identity: DirectoryIdentity) -> Self {
        Self {
            location: DirectoryLocation::new(path, identity),
            position: unsafe { UPSafeCell::new(0) },
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
            DirectoryIdentity::Upper(dir) => dir.mode,
            _ => 0o40755,
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
}
