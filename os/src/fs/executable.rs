use super::{
    ext4::Ext4File, lower_inode,
    mount::{resolve_mount, MountBackend},
    open_file, overlay::FileData, overlay_get, persistent::PersistentExecutable, OpenFlags, OverlayEntry, EXT4,
};
use crate::mm::FrameTracker;
use alloc::{sync::Arc, vec::Vec};

/// File contents pinned to the object opened by exec, independent of its directory entry.
pub enum Executable {
    #[cfg_attr(not(feature = "oscomp_runner"), allow(dead_code))]
    Embedded(&'static [u8]),
    Snapshot(Vec<u8>),
    Overlay(FileData),
    Disk(Ext4File),
    Persistent(PersistentExecutable),
}

impl Executable {
    pub fn len(&self) -> usize {
        match self {
            Self::Embedded(data) => data.len(),
            Self::Snapshot(data) => data.len(),
            Self::Overlay(data) => data.len,
            Self::Disk(file) => file.len(),
            Self::Persistent(file) => file.len(),
        }
    }

    pub fn read_at(&self, offset: usize, buffer: &mut [u8]) -> usize {
        let data = match self {
            Self::Embedded(data) => *data,
            Self::Snapshot(data) => data.as_slice(),
            Self::Overlay(data) => return data.read_at(offset, buffer),
            Self::Disk(file) => return file.read_at(offset, buffer),
            Self::Persistent(file) => return file.read_at(offset, buffer),
        };
        let count = data.len().saturating_sub(offset).min(buffer.len());
        if count != 0 {
            buffer[..count].copy_from_slice(&data[offset..offset + count]);
        }
        count
    }

    pub(crate) fn device_id(&self) -> Option<u64> {
        match self { Self::Persistent(file) => Some(file.dev()), _ => None }
    }

    pub(crate) fn cached_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        match self {
            Self::Disk(file) => file.cached_page(offset),
            Self::Persistent(file) => file.cached_page(offset),
            _ => None,
        }
    }
}

pub fn open_executable(name: &str) -> Result<Executable, isize> {
    if let Some((mount, rel)) = resolve_mount(name) {
        match mount.backend {
            MountBackend::Procfs | MountBackend::Devfs => return Err(-13),
            MountBackend::Memfs(fs) => {
                if rel.is_empty() {
                    return Err(-13);
                }
                return match fs.get(rel) {
                    Some(OverlayEntry::File(data)) => {
                        data.exclusive_access().snapshot().map(Executable::Overlay)
                    }
                    Some(OverlayEntry::Directory(_)) => Err(-13),
                    Some(OverlayEntry::Symlink(_)) => Err(-40),
                    Some(OverlayEntry::Whiteout) | None => Err(-2),
                };
            }
            MountBackend::Persistent(fs) => return fs.executable(rel, name).map(Executable::Persistent),
            MountBackend::RootOverlay => {}
        }
    }

    match overlay_get(name) {
        Some(OverlayEntry::File(data)) => {
            return data.exclusive_access().snapshot().map(Executable::Overlay);
        }
        Some(OverlayEntry::Directory(_)) => return Err(-13),
        Some(OverlayEntry::Symlink(_)) => return Err(-40),
        Some(OverlayEntry::Whiteout) => return Err(-2),
        None => {}
    }

    if let Some(fs) = EXT4.as_ref() {
        let inode = lower_inode(name).ok_or(-2isize)?;
        if !inode.is_file() {
            return Err(-13);
        }
        Ok(Executable::Disk(Ext4File::new(Arc::clone(fs), inode)))
    } else {
        open_file(name, OpenFlags::RDONLY)
            .map(|file| Executable::Snapshot(file.read_all()))
            .ok_or(-2)
    }
}
