use super::{File, OpenFlags};
use crate::config::PAGE_SIZE;
use crate::mm::{frame_alloc, FrameTracker, PhysAddr, UserBuffer};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lazy_static::lazy_static;

pub(super) type OverlayData = Arc<UPSafeCell<OverlayContents>>;
const MAX_OVERLAY: usize = 0x10_0000;

#[derive(Clone)]
pub(super) enum OverlayEntry {
    File(OverlayData),
    Directory(Arc<()>),
    Whiteout,
}

impl OverlayEntry {
    pub(super) fn dirent_type(&self) -> Option<u8> {
        match self {
            Self::File(_) => Some(1),
            Self::Directory(_) => Some(2),
            Self::Whiteout => None,
        }
    }
}

lazy_static! {
    static ref OVERLAY: UPSafeCell<BTreeMap<String, OverlayEntry>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

pub(super) fn overlay_get(name: &str) -> Option<OverlayEntry> {
    OVERLAY.exclusive_access().get(name).cloned()
}

pub(super) fn overlay_create(name: &str) -> OverlayData {
    let mut entries = OVERLAY.exclusive_access();
    if let Some(OverlayEntry::File(data)) = entries.get(name) {
        return data.clone();
    }
    let data = Arc::new(unsafe { UPSafeCell::new(OverlayContents::default()) });
    entries.insert(String::from(name), OverlayEntry::File(data.clone()));
    data
}

pub(super) fn overlay_mkdir(name: &str) {
    OVERLAY
        .exclusive_access()
        .insert(String::from(name), OverlayEntry::Directory(Arc::new(())));
}

pub(super) fn overlay_unlink(name: &str, hide_lower: bool) {
    let mut entries = OVERLAY.exclusive_access();
    if hide_lower {
        entries.insert(String::from(name), OverlayEntry::Whiteout);
    } else {
        entries.remove(name);
    }
}

pub(super) fn overlay_rename(old: &str, new: &str, hide_lower: bool) -> bool {
    let mut entries = OVERLAY.exclusive_access();
    let file = match entries.get(old) {
        Some(OverlayEntry::File(data)) => OverlayEntry::File(data.clone()),
        _ => return false,
    };
    if old != new {
        if hide_lower {
            entries.insert(String::from(old), OverlayEntry::Whiteout);
        } else {
            entries.remove(old);
        }
        entries.insert(String::from(new), file);
    }
    true
}

pub(super) fn overlay_children(directory: &str) -> BTreeMap<String, OverlayEntry> {
    let mut prefix = String::from(directory);
    if !prefix.is_empty() {
        prefix.push('/');
    }
    OVERLAY
        .exclusive_access()
        .range(prefix.clone()..)
        .take_while(|(path, _)| path.starts_with(&prefix))
        .filter_map(|(path, entry)| {
            let child = &path[prefix.len()..];
            (!child.contains('/')).then(|| (String::from(child), entry.clone()))
        })
        .collect()
}

/// The in-memory file owns its pages. MAP_SHARED takes another reference to the same frames.
#[derive(Default)]
pub struct OverlayContents {
    len: usize,
    pages: BTreeMap<usize, Arc<FrameTracker>>,
    mapping_lease: Arc<()>,
}

impl OverlayContents {
    fn page(&mut self, index: usize) -> Option<Arc<FrameTracker>> {
        if let Some(page) = self.pages.get(&index) {
            return Some(page.clone());
        }
        let page = Arc::new(frame_alloc()?);
        self.pages.insert(index, page.clone());
        Some(page)
    }

    pub(super) fn read_all(&self) -> Vec<u8> {
        let mut data = vec![0; self.len];
        self.read_at(0, &mut data);
        data
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        unsafe { self.copy_out(offset, buf.as_mut_ptr(), buf.len()) }
    }

    // The destination can be another virtual mapping of the file's own frames.
    unsafe fn copy_out(&self, offset: usize, dst: *mut u8, capacity: usize) -> usize {
        let len = self.len.saturating_sub(offset).min(capacity);
        let mut copied = 0;
        while copied < len {
            let pos = offset + copied;
            let start = pos % PAGE_SIZE;
            let count = (len - copied).min(PAGE_SIZE - start);
            if let Some(page) = self.pages.get(&(pos / PAGE_SIZE)) {
                let src = usize::from(PhysAddr::from(page.ppn)) as *const u8;
                core::ptr::copy(src.add(start), dst.add(copied), count);
            } else {
                dst.add(copied).write_bytes(0, count);
            }
            copied += count;
        }
        len
    }

    pub(super) fn resize(&mut self, len: usize) -> bool {
        if len == self.len {
            return true;
        }
        if len < self.len {
            // Any mapped view or in-flight pin can still reference the previous size.
            if Arc::strong_count(&self.mapping_lease) > 1
                || self
                    .pages
                    .range(len / PAGE_SIZE..)
                    .any(|(_, page)| Arc::strong_count(page) > 1)
            {
                return false;
            }
            self.pages
                .split_off(&(len / PAGE_SIZE + usize::from(len % PAGE_SIZE != 0)));
        }
        let tail = len.min(self.len);
        if tail % PAGE_SIZE != 0 {
            if let Some(page) = self.pages.get(&(tail / PAGE_SIZE)) {
                page.ppn.get_bytes_array()[tail % PAGE_SIZE..].fill(0);
            }
        }
        self.len = len;
        true
    }

    pub(super) fn write_at(&mut self, offset: usize, data: &[u8]) -> usize {
        unsafe { self.copy_in(offset, data.as_ptr(), data.len()) }
    }

    unsafe fn copy_in(&mut self, offset: usize, src: *const u8, len: usize) -> usize {
        let mut written = 0;
        while written < len {
            let pos = offset + written;
            let start = pos % PAGE_SIZE;
            let count = (len - written).min(PAGE_SIZE - start);
            let page = match self.page(pos / PAGE_SIZE) {
                Some(page) => page,
                None => break,
            };
            let dst = usize::from(PhysAddr::from(page.ppn)) as *mut u8;
            core::ptr::copy(src.add(written), dst.add(start), count);
            // The source may be in the shared tail of this file. Copy before zeroing any gap.
            let tail = self.len % PAGE_SIZE;
            if self.len < pos && tail != 0 {
                if let Some(last) = self.pages.get(&(self.len / PAGE_SIZE)) {
                    let gap = (pos - self.len).min(PAGE_SIZE - tail);
                    last.ppn.get_bytes_array()[tail..tail + gap].fill(0);
                }
            }
            self.len = self.len.max(pos + count);
            written += count;
        }
        written
    }
}

pub(super) struct OverlayFile {
    data: OverlayData,
    offset: UPSafeCell<usize>,
    flags: OpenFlags,
}

impl OverlayFile {
    pub(super) fn new(data: OverlayData, flags: OpenFlags) -> Self {
        Self {
            data,
            offset: unsafe { UPSafeCell::new(0) },
            flags,
        }
    }
}

impl File for OverlayFile {
    fn readable(&self) -> bool {
        !self.flags.contains(OpenFlags::WRONLY)
    }

    fn writable(&self) -> bool {
        self.flags.intersects(OpenFlags::WRONLY | OpenFlags::RDWR)
    }

    fn read(&self, buf: UserBuffer) -> usize {
        let data = self.data.exclusive_access();
        let mut offset = self.offset.exclusive_access();
        let start = *offset;
        for chunk in buf.buffers {
            let n = unsafe { data.copy_out(*offset, chunk.as_mut_ptr(), chunk.len()) };
            *offset += n;
            if n < chunk.len() {
                break;
            }
        }
        *offset - start
    }

    fn write(&self, buf: UserBuffer) -> isize {
        const EFBIG: isize = -27;
        const ENOSPC: isize = -28;

        let mut data = self.data.exclusive_access();
        let mut offset = self.offset.exclusive_access();
        let start = *offset;
        for chunk in buf.buffers {
            let len = chunk.len();
            if len == 0 {
                continue;
            }
            let count = len.min(MAX_OVERLAY.saturating_sub(*offset));
            if count == 0 {
                if *offset == start {
                    return EFBIG;
                }
                break;
            }
            let n = unsafe { data.copy_in(*offset, chunk.as_ptr(), count) };
            if n == 0 && *offset == start {
                return ENOSPC;
            }
            *offset += n;
            if n < len {
                break;
            }
        }
        (*offset - start) as isize
    }

    fn size(&self) -> usize {
        self.data.exclusive_access().len
    }

    fn set_len(&self, len: usize) -> isize {
        if !self.writable() {
            return -22;
        }
        if len > MAX_OVERLAY {
            return -27;
        }
        if self.data.exclusive_access().resize(len) {
            0
        } else {
            -16
        }
    }

    fn seek_to(&self, pos: isize) -> isize {
        if pos < 0 {
            return -1;
        }
        let pos = (pos as usize).min(self.size());
        *self.offset.exclusive_access() = pos;
        pos as isize
    }

    fn seek(&self, delta: isize) -> isize {
        let pos = (*self.offset.exclusive_access() as isize).saturating_add(delta);
        self.seek_to(pos)
    }

    fn mapping_size(&self) -> Option<usize> {
        Some(self.size())
    }

    fn mapping_lease(&self) -> Option<Arc<()>> {
        Some(self.data.exclusive_access().mapping_lease.clone())
    }

    fn supports_mapped_writes(&self) -> bool {
        true
    }

    fn mapping_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        let mut data = self.data.exclusive_access();
        if offset >= data.len || offset % PAGE_SIZE != 0 {
            return None;
        }
        data.page(offset / PAGE_SIZE)
    }
}
