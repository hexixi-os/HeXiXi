use super::{ext4::Ext4File, File, OpenFlags};
use crate::config::PAGE_SIZE;
use crate::mm::{frame_alloc_charged, ram_end, FrameTracker, PhysAddr, UserBuffer};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub(super) type OverlayData = Arc<UPSafeCell<OverlayContents>>;
const MAX_DATA_PAGES: usize = 128 * 1024 * 1024 / PAGE_SIZE;
const MAX_ENTRIES: usize = 16_384;
const MAX_PATH_BYTES: usize = 2 * 1024 * 1024;
static OVERLAY_INODES: AtomicUsize = AtomicUsize::new(0);
const OVERLAY_DEVICE: u64 = 1 << 32;
static OVERLAY_PAGES: AtomicUsize = AtomicUsize::new(0);
static NEXT_INODE: AtomicUsize = AtomicUsize::new(2);

fn data_page_limit() -> usize {
    let ram_pages = ram_end().saturating_sub(0x8000_0000) / PAGE_SIZE;
    MAX_DATA_PAGES.min(ram_pages / 4)
}

pub(super) struct OverlayDirectory {
    pub(super) path: UPSafeCell<String>,
    pub(super) ino: u32,
    _charge: InodeCharge,
}

#[derive(Clone)]
pub(super) enum OverlayEntry {
    File(OverlayData),
    Directory(Arc<OverlayDirectory>),
    Whiteout,
}

impl OverlayEntry {
    pub(super) fn dirent(&self) -> Option<(u32, u8)> {
        match self {
            Self::File(data) => Some((data.exclusive_access().ino, 1)),
            Self::Directory(dir) => Some((dir.ino, 2)),
            Self::Whiteout => None,
        }
    }
}

pub(super) fn directory_identity(dir: &OverlayDirectory) -> (u64, u64) {
    (OVERLAY_DEVICE, u64::from(dir.ino))
}

fn next_inode() -> u32 {
    NEXT_INODE.fetch_add(1, Ordering::Relaxed) as u32
}

struct InodeCharge;

impl InodeCharge {
    fn new() -> Result<Self, isize> {
        OVERLAY_INODES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (count < MAX_ENTRIES).then(|| count + 1)
            })
            .map_err(|_| -28isize)?;
        Ok(Self)
    }
}

impl Drop for InodeCharge {
    fn drop(&mut self) {
        OVERLAY_INODES.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Default)]
struct Namespace {
    entries: BTreeMap<String, OverlayEntry>,
    path_bytes: usize,
}

impl Namespace {
    fn check_capacity(&self, count: usize, bytes: usize) -> Result<(), isize> {
        if count > MAX_ENTRIES || bytes > MAX_PATH_BYTES {
            Err(-28)
        } else {
            Ok(())
        }
    }

    fn check_insert(&self, path: &str) -> Result<(), isize> {
        let extra = usize::from(!self.entries.contains_key(path));
        self.check_capacity(
            self.entries.len() + extra,
            self.path_bytes + extra * path.len(),
        )
    }

    fn remove(&mut self, path: &str) -> Option<OverlayEntry> {
        let entry = self.entries.remove(path)?;
        self.path_bytes -= path.len();
        Some(entry)
    }

    fn insert(&mut self, path: String, entry: OverlayEntry) -> Option<OverlayEntry> {
        let bytes = path.len();
        let previous = self.entries.insert(path, entry);
        if previous.is_none() {
            self.path_bytes += bytes;
        }
        previous
    }
}

lazy_static! {
    static ref OVERLAY: UPSafeCell<Namespace> = unsafe { UPSafeCell::new(Namespace::default()) };
}

pub(super) fn overlay_get(name: &str) -> Option<OverlayEntry> {
    OVERLAY.exclusive_access().entries.get(name).cloned()
}

pub(super) fn overlay_create(name: &str, lower: Option<Ext4File>) -> Result<OverlayData, isize> {
    let mut entries = OVERLAY.exclusive_access();
    if let Some(OverlayEntry::File(data)) = entries.entries.get(name) {
        return Ok(data.clone());
    }
    entries.check_insert(name)?;
    let data = new_file(lower, true)?;
    entries.insert(String::from(name), OverlayEntry::File(data.clone()));
    Ok(data)
}

pub(super) fn new_file(lower: Option<Ext4File>, linked: bool) -> Result<OverlayData, isize> {
    let charge = InodeCharge::new()?;
    let len = lower.as_ref().map_or(0, Ext4File::len);
    Ok(Arc::new(unsafe {
        UPSafeCell::new(OverlayContents {
            ino: next_inode(),
            linked,
            _charge: charge,
            contents: FileData {
                len,
                lower: lower.map(Arc::new),
                lower_len: len,
                pages: BTreeMap::new(),
            },
        })
    }))
}

pub(super) fn overlay_mkdir(name: &str) -> Result<(), isize> {
    let mut entries = OVERLAY.exclusive_access();
    entries.check_insert(name)?;
    let directory = Arc::new(OverlayDirectory {
        path: unsafe { UPSafeCell::new(String::from(name)) },
        ino: next_inode(),
        _charge: InodeCharge::new()?,
    });
    entries.insert(String::from(name), OverlayEntry::Directory(directory));
    Ok(())
}

fn detach(entry: Option<OverlayEntry>) {
    match entry {
        Some(OverlayEntry::File(data)) => data.exclusive_access().linked = false,
        Some(OverlayEntry::Directory(dir)) => *dir.path.exclusive_access() = String::new(),
        _ => (),
    }
}

fn child_paths(entries: &BTreeMap<String, OverlayEntry>, name: &str) -> Vec<String> {
    let prefix = alloc::format!("{}/", name);
    entries
        .range(prefix.clone()..)
        .take_while(|(path, _)| path.starts_with(&prefix))
        .map(|(path, _)| path.clone())
        .collect()
}

pub(super) fn overlay_unlink(name: &str, hide_lower: bool, directory: bool) -> Result<(), isize> {
    let mut entries = OVERLAY.exclusive_access();
    let children = if directory {
        child_paths(&entries.entries, name)
    } else {
        Vec::new()
    };
    if hide_lower && !entries.entries.contains_key(name) {
        let child_bytes = children.iter().map(String::len).sum::<usize>();
        entries.check_capacity(
            entries.entries.len() - children.len() + 1,
            entries.path_bytes - child_bytes + name.len(),
        )?;
    }
    for child in children {
        detach(entries.remove(&child));
    }
    detach(entries.remove(name));
    if hide_lower {
        entries.insert(String::from(name), OverlayEntry::Whiteout);
    }
    Ok(())
}

pub(super) fn overlay_rename(
    old: &str,
    new: &str,
    source: OverlayEntry,
    hide_lower: bool,
) -> Result<(), isize> {
    let mut entries = OVERLAY.exclusive_access();
    let removes_source = usize::from(entries.entries.contains_key(old));
    let adds_destination = usize::from(!entries.entries.contains_key(new));
    let whiteout = usize::from(hide_lower);
    let final_count = entries.entries.len() - removes_source + adds_destination + whiteout;
    let final_bytes = entries.path_bytes - removes_source * old.len()
        + adds_destination * new.len()
        + whiteout * old.len();
    if let OverlayEntry::Directory(dir) = &source {
        let destination_children = child_paths(&entries.entries, new);
        let paths = child_paths(&entries.entries, old);
        if paths
            .iter()
            .any(|path| new.len() + path.len() - old.len() >= 4096)
        {
            return Err(-36);
        }
        let replaced_bytes = destination_children.iter().map(String::len).sum::<usize>();
        entries.check_capacity(
            final_count - destination_children.len(),
            final_bytes - replaced_bytes - paths.len() * old.len() + paths.len() * new.len(),
        )?;
        for path in destination_children {
            detach(entries.remove(&path));
        }
        *dir.path.exclusive_access() = String::from(new);
        for old_path in paths {
            let new_path = alloc::format!("{}{}", new, &old_path[old.len()..]);
            let child = entries.remove(&old_path).unwrap();
            if let OverlayEntry::Directory(dir) = &child {
                *dir.path.exclusive_access() = new_path.clone();
            }
            entries.insert(new_path, child);
        }
    } else {
        entries.check_capacity(final_count, final_bytes)?;
    }
    entries.remove(old);
    if hide_lower {
        entries.insert(String::from(old), OverlayEntry::Whiteout);
    }
    detach(entries.insert(String::from(new), source));
    Ok(())
}

pub(super) fn overlay_children(directory: &str) -> BTreeMap<String, OverlayEntry> {
    let mut prefix = String::from(directory);
    if !prefix.is_empty() {
        prefix.push('/');
    }
    OVERLAY
        .exclusive_access()
        .entries
        .range(prefix.clone()..)
        .take_while(|(path, _)| path.starts_with(&prefix))
        .filter_map(|(path, entry)| {
            let child = &path[prefix.len()..];
            (!child.contains('/')).then(|| (String::from(child), entry.clone()))
        })
        .collect()
}

/// The in-memory file owns its pages. MAP_SHARED takes another reference to the same frames.
pub(super) struct OverlayContents {
    ino: u32,
    linked: bool,
    _charge: InodeCharge,
    contents: FileData,
}

/// Sparse file bytes shared by open descriptions and writable mappings.
/// Exec takes a separate copy of the resident pages and can still demand-read the lower file.
pub struct FileData {
    pub(super) len: usize,
    lower: Option<Arc<Ext4File>>,
    lower_len: usize,
    pages: BTreeMap<usize, Arc<FrameTracker>>,
}

pub(super) fn resize(data: &OverlayData, len: usize) {
    let (identity, retired) = {
        let mut data = data.exclusive_access();
        let identity = data.identity();
        let contents = &mut data.contents;
        if contents.len == len {
            return;
        }
        contents.zero_tail(len.min(contents.len), PAGE_SIZE);
        let retired = if len < contents.len {
            contents.lower_len = contents.lower_len.min(len);
            Some(
                contents
                    .pages
                    .split_off(&((len + PAGE_SIZE - 1) / PAGE_SIZE)),
            )
        } else {
            None
        };
        contents.len = len;
        (identity, retired)
    };
    if let Some(retired) = retired {
        let first_invalid = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        crate::task::invalidate_file_pages(identity, first_invalid);
        drop(retired);
    }
}

impl OverlayContents {
    fn identity(&self) -> (u64, u64) {
        (OVERLAY_DEVICE, u64::from(self.ino))
    }

    pub(super) fn snapshot(&self) -> Result<FileData, isize> {
        self.contents.snapshot()
    }
}

fn alloc_overlay_page() -> Option<Arc<FrameTracker>> {
    frame_alloc_charged(&OVERLAY_PAGES, data_page_limit()).map(Arc::new)
}

impl FileData {
    fn read_lower(&self, offset: usize, buffer: &mut [u8]) {
        let count = self.lower_len.saturating_sub(offset).min(buffer.len());
        if count > 0 {
            self.lower
                .as_ref()
                .unwrap()
                .read_at(offset, &mut buffer[..count]);
        }
        buffer[count..].fill(0);
    }

    fn zero_tail(&self, offset: usize, end_in_page: usize) {
        let tail = offset % PAGE_SIZE;
        if tail != 0 {
            if let Some(page) = self.pages.get(&(offset / PAGE_SIZE)) {
                page.ppn.get_bytes_array()[tail..end_in_page].fill(0);
            }
        }
    }

    fn page(&mut self, index: usize) -> Option<Arc<FrameTracker>> {
        if let Some(page) = self.pages.get(&index) {
            return Some(page.clone());
        }
        let page = alloc_overlay_page()?;
        if index * PAGE_SIZE < self.lower_len {
            self.read_lower(index * PAGE_SIZE, page.ppn.get_bytes_array());
        }
        self.pages.insert(index, page.clone());
        Some(page)
    }

    fn snapshot(&self) -> Result<Self, isize> {
        let mut pages = BTreeMap::new();
        for (&index, source) in &self.pages {
            let page = alloc_overlay_page().ok_or(-12isize)?;
            page.ppn
                .get_bytes_array()
                .copy_from_slice(source.ppn.get_bytes_array());
            pages.insert(index, page);
        }
        Ok(Self {
            pages,
            len: self.len,
            lower: self.lower.clone(),
            lower_len: self.lower_len,
        })
    }

    pub(super) fn read_at(&self, offset: usize, buffer: &mut [u8]) -> usize {
        unsafe { self.copy_out(offset, buffer.as_mut_ptr(), buffer.len()) }
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
            } else if pos < self.lower_len {
                self.read_lower(pos, core::slice::from_raw_parts_mut(dst.add(copied), count));
            } else {
                dst.add(copied).write_bytes(0, count);
            }
            copied += count;
        }
        len
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
            if self.len < pos {
                self.zero_tail(
                    self.len,
                    (self.len % PAGE_SIZE + pos - self.len).min(PAGE_SIZE),
                );
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

    fn stat_identity(&self) -> Option<(u64, u64)> {
        Some(self.data.exclusive_access().identity())
    }

    fn link_count(&self) -> u32 {
        u32::from(self.data.exclusive_access().linked)
    }

    fn read(&self, buf: UserBuffer) -> usize {
        let data = self.data.exclusive_access();
        let mut offset = self.offset.exclusive_access();
        let start = *offset;
        for chunk in buf.buffers {
            let n = unsafe {
                data.contents
                    .copy_out(*offset, chunk.as_mut_ptr(), chunk.len())
            };
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
        let data = &mut data.contents;
        let mut offset = self.offset.exclusive_access();
        let start = *offset;
        let mut position = if self.flags.contains(OpenFlags::APPEND) {
            data.len
        } else {
            start
        };
        let first = position;
        for chunk in buf.buffers {
            let len = chunk.len();
            if len == 0 {
                continue;
            }
            let count = len.min((isize::MAX as usize).saturating_sub(position));
            if count == 0 {
                if position == first {
                    return EFBIG;
                }
                break;
            }
            let n = unsafe { data.copy_in(position, chunk.as_ptr(), count) };
            if n == 0 && position == first {
                return ENOSPC;
            }
            position += n;
            if n < len {
                break;
            }
        }
        if position != first {
            *offset = position;
        }
        (position - first) as isize
    }

    fn size(&self) -> usize {
        self.data.exclusive_access().contents.len
    }

    fn set_len(&self, len: usize) -> isize {
        if !self.writable() {
            return -22;
        }
        resize(&self.data, len);
        0
    }

    fn seek_to(&self, pos: isize) -> isize {
        if pos < 0 {
            return -22;
        }
        *self.offset.exclusive_access() = pos as usize;
        pos
    }

    fn seek(&self, delta: isize) -> isize {
        let position = *self.offset.exclusive_access() as isize;
        position
            .checked_add(delta)
            .map_or(-75, |pos| self.seek_to(pos))
    }

    fn mapping_size(&self) -> Option<usize> {
        Some(self.size())
    }

    fn supports_mapped_writes(&self) -> bool {
        true
    }

    fn mapping_page(&self, offset: usize, shared: bool) -> Option<Arc<FrameTracker>> {
        let mut inode = self.data.exclusive_access();
        let data = &mut inode.contents;
        if offset >= data.len || offset % PAGE_SIZE != 0 {
            return None;
        }
        let index = offset / PAGE_SIZE;
        if shared {
            return data.page(index);
        }
        if let Some(page) = data.pages.get(&index) {
            return Some(page.clone());
        }
        let page = Arc::new(crate::mm::frame_alloc()?);
        data.read_lower(offset, page.ppn.get_bytes_array());
        Some(page)
    }
}
