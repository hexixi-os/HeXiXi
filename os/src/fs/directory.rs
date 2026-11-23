use super::File;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::string::String;
use alloc::sync::Arc;

#[derive(Clone)]
pub(super) enum DirectoryIdentity {
    Permanent,
    Lower,
    Upper(Arc<()>),
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
            path: String::from(path),
            identity,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
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

    fn size(&self) -> usize {
        4096
    }

    fn seek_to(&self, position: isize) -> isize {
        if position < 0 {
            return -22;
        }
        self.set_position(position as usize);
        position
    }
}
