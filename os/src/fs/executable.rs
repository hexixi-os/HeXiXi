use super::{ext4::Ext4File, lower_inode, open_file, overlay_get, OpenFlags, OverlayEntry, EXT4};
use crate::mm::FrameTracker;
use alloc::{sync::Arc, vec::Vec};

/// File contents pinned to the object opened by exec, independent of its directory entry.
pub enum Executable {
    Embedded(&'static [u8]),
    Snapshot(Vec<u8>),
    Disk(Ext4File),
}

impl Executable {
    pub fn len(&self) -> usize {
        match self {
            Self::Embedded(data) => data.len(),
            Self::Snapshot(data) => data.len(),
            Self::Disk(file) => file.len(),
        }
    }

    pub fn read_at(&self, offset: usize, buffer: &mut [u8]) -> usize {
        let data = match self {
            Self::Embedded(data) => *data,
            Self::Snapshot(data) => data.as_slice(),
            Self::Disk(file) => return file.read_at(offset, buffer),
        };
        let count = data.len().saturating_sub(offset).min(buffer.len());
        if count != 0 {
            buffer[..count].copy_from_slice(&data[offset..offset + count]);
        }
        count
    }

    pub(crate) fn cached_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        match self {
            Self::Disk(file) => file.cached_page(offset),
            _ => None,
        }
    }
}

pub fn open_executable(name: &str) -> Result<Executable, isize> {
    match overlay_get(name) {
        Some(OverlayEntry::File(data)) => {
            return Ok(Executable::Snapshot(data.exclusive_access().read_all()));
        }
        Some(OverlayEntry::Directory(_)) => return Err(-13),
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
