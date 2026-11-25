use super::{ext4::Ext4File, symlink::Symlink, File, OpenFlags};
use crate::config::PAGE_SIZE;
use crate::mm::{
    copy_live_bytes, frame_alloc_charged, ram_end, zero_live_bytes, FrameTracker, UserBuffer,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub(super) type OverlayData = Arc<UPSafeCell<OverlayContents>>;
const MAX_DATA_PAGES: usize = 128 * 1024 * 1024 / PAGE_SIZE;
const MAX_ENTRIES: usize = 16_384;
const MAX_PATH_BYTES: usize = 2 * 1024 * 1024;
static OVERLAY_INODES: AtomicUsize = AtomicUsize::new(0);
static SYMLINK_BYTES: AtomicUsize = AtomicUsize::new(0);
static TOTAL_PATH_BYTES: AtomicUsize = AtomicUsize::new(0);
pub(super) const OVERLAY_DEVICE: u64 = 1 << 32;
static NEXT_DEVICE: AtomicU64 = AtomicU64::new(2);
static OVERLAY_PAGES: AtomicUsize = AtomicUsize::new(0);
static NEXT_INODE: AtomicUsize = AtomicUsize::new(2);

fn data_page_limit() -> usize {
    let ram_pages = ram_end().saturating_sub(crate::arch::RAM_BASE) / PAGE_SIZE;
    MAX_DATA_PAGES.min(ram_pages / 4)
}

fn next_device() -> u64 {
    NEXT_DEVICE.fetch_add(1, Ordering::Relaxed) << 32
}

pub(super) struct OverlayDirectory {
    dev: u64,
    mount_prefix: String,
    mounted: Arc<AtomicBool>,
    detached: AtomicBool,
    pub(super) path: UPSafeCell<String>,
    pub(super) ino: u32,
    _charge: Option<InodeCharge>,
}

impl OverlayDirectory {
    pub(super) fn full_path(&self) -> String {
        let rel = self.path.exclusive_access();
        if self.mount_prefix.is_empty() {
            rel.clone()
        } else if rel.is_empty() {
            self.mount_prefix.clone()
        } else {
            alloc::format!("{}/{}", self.mount_prefix, rel.as_str())
        }
    }

    pub(super) fn is_live(&self) -> bool {
        !self.detached.load(Ordering::Relaxed) && self.mounted.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
pub(super) enum OverlayEntry {
    File(OverlayData),
    Directory(Arc<OverlayDirectory>),
    Symlink(Arc<Symlink>),
    Whiteout,
}

impl OverlayEntry {
    pub(super) fn dirent(&self) -> Option<(u32, u8)> {
        match self {
            Self::File(data) => Some((data.exclusive_access().ino, 1)),
            Self::Directory(dir) => Some((dir.ino, 2)),
            Self::Symlink(link) => link.stat_identity().map(|(_, ino)| (ino as u32, 7)),
            Self::Whiteout => None,
        }
    }
}

pub(super) fn directory_identity(dir: &OverlayDirectory) -> (u64, u64) {
    (dir.dev, u64::from(dir.ino))
}

fn next_inode() -> u32 {
    NEXT_INODE.fetch_add(1, Ordering::Relaxed) as u32
}

struct InodeCharge;

pub(super) struct SymlinkCharge {
    bytes: usize,
    _inode: InodeCharge,
}

impl SymlinkCharge {
    pub(super) fn new(bytes: usize) -> Result<Self, isize> {
        let inode = InodeCharge::new()?;
        SYMLINK_BYTES
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
                used.checked_add(bytes)
                    .filter(|&total| total <= MAX_PATH_BYTES)
            })
            .map_err(|_| -28isize)?;
        Ok(Self {
            bytes,
            _inode: inode,
        })
    }
}

impl Drop for SymlinkCharge {
    fn drop(&mut self) {
        SYMLINK_BYTES.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

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
        let other_bytes = TOTAL_PATH_BYTES
            .load(Ordering::Relaxed)
            .saturating_sub(self.path_bytes);
        if count > MAX_ENTRIES || other_bytes.saturating_add(bytes) > MAX_PATH_BYTES {
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
        TOTAL_PATH_BYTES.fetch_sub(path.len(), Ordering::Relaxed);
        Some(entry)
    }

    fn insert(&mut self, path: String, entry: OverlayEntry) -> Option<OverlayEntry> {
        let bytes = path.len();
        let previous = self.entries.insert(path, entry);
        if previous.is_none() {
            self.path_bytes += bytes;
            TOTAL_PATH_BYTES.fetch_add(bytes, Ordering::Relaxed);
        }
        previous
    }

    fn clear(&mut self) -> Vec<OverlayEntry> {
        let bytes = core::mem::replace(&mut self.path_bytes, 0);
        if bytes != 0 {
            TOTAL_PATH_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        }
        core::mem::take(&mut self.entries)
            .into_values()
            .collect()
    }
}

impl Drop for Namespace {
    fn drop(&mut self) {
        let bytes = core::mem::replace(&mut self.path_bytes, 0);
        if bytes != 0 {
            TOTAL_PATH_BYTES.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

pub(super) struct MemFs {
    dev: u64,
    magic: u64,
    mount_prefix: String,
    mounted: Arc<AtomicBool>,
    root: Arc<OverlayDirectory>,
    entries: UPSafeCell<Namespace>,
}

impl MemFs {
    fn new_root_overlay() -> Self {
        let mounted = Arc::new(AtomicBool::new(true));
        let root = Arc::new(OverlayDirectory {
            dev: OVERLAY_DEVICE,
            mount_prefix: String::new(),
            mounted: Arc::clone(&mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::new()) },
            ino: 1,
            _charge: None,
        });
        Self {
            dev: OVERLAY_DEVICE,
            magic: 0xef53,
            mount_prefix: String::new(),
            mounted,
            root,
            entries: unsafe { UPSafeCell::new(Namespace::default()) },
        }
    }

    pub(super) fn new_mountable(mount_prefix: &str, magic: u64) -> Result<Arc<Self>, isize> {
        let charge = InodeCharge::new()?;
        let dev = next_device();
        let mounted = Arc::new(AtomicBool::new(true));
        let root = Arc::new(OverlayDirectory {
            dev,
            mount_prefix: String::from(mount_prefix),
            mounted: Arc::clone(&mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::new()) },
            ino: next_inode(),
            _charge: Some(charge),
        });
        Ok(Arc::new(Self {
            dev,
            magic,
            mount_prefix: String::from(mount_prefix),
            mounted,
            root,
            entries: unsafe { UPSafeCell::new(Namespace::default()) },
        }))
    }

    pub(super) fn dev(&self) -> u64 {
        self.dev
    }

    pub(super) fn magic(&self) -> u64 {
        self.magic
    }

    pub(super) fn root_ino(&self) -> u32 {
        self.root.ino
    }

    pub(super) fn root_dir(&self) -> Arc<OverlayDirectory> {
        Arc::clone(&self.root)
    }

    pub(super) fn unmount(&self) {
        self.mounted.store(false, Ordering::Relaxed);
        let removed = self.entries.exclusive_access().clear();
        for entry in removed {
            detach(Some(entry));
        }
    }

    pub(super) fn get(&self, name: &str) -> Option<OverlayEntry> {
        self.entries.exclusive_access().entries.get(name).cloned()
    }

    pub(super) fn new_file(&self, lower: Option<Ext4File>, linked: bool) -> Result<OverlayData, isize> {
        new_file_with_dev(self.dev, lower, linked)
    }

    pub(super) fn create(&self, name: &str, lower: Option<Ext4File>) -> Result<OverlayData, isize> {
        let mut entries = self.entries.exclusive_access();
        if let Some(OverlayEntry::File(data)) = entries.entries.get(name) {
            return Ok(data.clone());
        }
        entries.check_insert(name)?;
        let data = new_file_with_dev(self.dev, lower, true)?;
        entries.insert(String::from(name), OverlayEntry::File(data.clone()));
        Ok(data)
    }

    pub(super) fn mkdir(&self, name: &str) -> Result<(), isize> {
        let mut entries = self.entries.exclusive_access();
        entries.check_insert(name)?;
        let directory = Arc::new(OverlayDirectory {
            dev: self.dev,
            mount_prefix: self.mount_prefix.clone(),
            mounted: Arc::clone(&self.mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::from(name)) },
            ino: next_inode(),
            _charge: Some(InodeCharge::new()?),
        });
        entries.insert(String::from(name), OverlayEntry::Directory(directory));
        Ok(())
    }

    pub(super) fn symlink(&self, name: &str, target: Vec<u8>) -> Result<(), isize> {
        let mut entries = self.entries.exclusive_access();
        entries.check_insert(name)?;
        let charge = SymlinkCharge::new(target.len())?;
        let link = Arc::new(Symlink::new(
            target,
            Some((self.dev, u64::from(next_inode()))),
            Some(charge),
        ));
        entries.insert(String::from(name), OverlayEntry::Symlink(link));
        Ok(())
    }

    pub(super) fn unlink(&self, name: &str, hide_lower: bool, directory: bool) -> Result<(), isize> {
        let mut entries = self.entries.exclusive_access();
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

    pub(super) fn rename(
        &self,
        old: &str,
        new: &str,
        source: OverlayEntry,
        hide_lower: bool,
    ) -> Result<(), isize> {
        let mut entries = self.entries.exclusive_access();
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
            let prefix_extra = if self.mount_prefix.is_empty() {
                0
            } else {
                self.mount_prefix.len() + 1
            };
            if paths
                .iter()
                .any(|path| prefix_extra + new.len() + path.len() - old.len() >= 4096)
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

    pub(super) fn children(&self, directory: &str) -> BTreeMap<String, OverlayEntry> {
        let mut prefix = String::from(directory);
        if !prefix.is_empty() {
            prefix.push('/');
        }
        self.entries
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
}

lazy_static! {
    static ref OVERLAY: MemFs = MemFs::new_root_overlay();
}

pub(super) fn overlay_get(name: &str) -> Option<OverlayEntry> {
    OVERLAY.get(name)
}

pub(super) fn overlay_create(name: &str, lower: Option<Ext4File>) -> Result<OverlayData, isize> {
    OVERLAY.create(name, lower)
}

pub(super) fn new_file(lower: Option<Ext4File>, linked: bool) -> Result<OverlayData, isize> {
    new_file_with_dev(OVERLAY_DEVICE, lower, linked)
}

fn new_file_with_dev(
    dev: u64,
    lower: Option<Ext4File>,
    linked: bool,
) -> Result<OverlayData, isize> {
    let charge = InodeCharge::new()?;
    let len = lower.as_ref().map_or(0, Ext4File::len);
    Ok(Arc::new(unsafe {
        UPSafeCell::new(OverlayContents {
            dev,
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
    OVERLAY.mkdir(name)
}

pub(super) fn overlay_symlink(name: &str, target: Vec<u8>) -> Result<(), isize> {
    OVERLAY.symlink(name, target)
}

fn detach(entry: Option<OverlayEntry>) {
    match entry {
        Some(OverlayEntry::File(data)) => data.exclusive_access().linked = false,
        Some(OverlayEntry::Directory(dir)) => {
            dir.detached.store(true, Ordering::Relaxed);
            *dir.path.exclusive_access() = String::new();
        }
        Some(OverlayEntry::Symlink(link)) => link.detach(),
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
    OVERLAY.unlink(name, hide_lower, directory)
}

pub(super) fn overlay_rename(
    old: &str,
    new: &str,
    source: OverlayEntry,
    hide_lower: bool,
) -> Result<(), isize> {
    OVERLAY.rename(old, new, source, hide_lower)
}

pub(super) fn overlay_children(directory: &str) -> BTreeMap<String, OverlayEntry> {
    OVERLAY.children(directory)
}

/// The in-memory file owns its pages. MAP_SHARED takes another reference to the same frames.
pub(super) struct OverlayContents {
    dev: u64,
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
        (self.dev, u64::from(self.ino))
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
                unsafe { zero_live_bytes(page.ppn.bytes_ptr().add(tail), end_in_page - tail) };
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
            unsafe { copy_live_bytes(source.ppn.bytes_ptr(), page.ppn.bytes_ptr(), PAGE_SIZE) };
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
        unsafe { self.copy_out(offset, buffer.as_mut_ptr(), buffer.len(), None) }
    }

    // The destination can be another virtual mapping of the file's own frames.
    // Pass a scratch buffer when `dst` may be mapped in userspace, so lower-layer
    // I/O gets kernel-owned bytes even when another hart can write that mapping.
    unsafe fn copy_out(
        &self,
        offset: usize,
        dst: *mut u8,
        capacity: usize,
        mut user_scratch: Option<&mut Vec<u8>>,
    ) -> usize {
        let len = self.len.saturating_sub(offset).min(capacity);
        let mut copied = 0;
        while copied < len {
            let pos = offset + copied;
            let start = pos % PAGE_SIZE;
            let count = (len - copied).min(PAGE_SIZE - start);
            if let Some(page) = self.pages.get(&(pos / PAGE_SIZE)) {
                copy_live_bytes(page.ppn.bytes_ptr().add(start), dst.add(copied), count);
            } else if pos < self.lower_len {
                match user_scratch.as_deref_mut() {
                    #[cfg(feature = "smp")]
                    Some(scratch) => {
                        scratch.resize(count, 0);
                        self.read_lower(pos, scratch);
                        copy_live_bytes(scratch.as_ptr(), dst.add(copied), count);
                    }
                    _ => self.read_lower(
                        pos,
                        core::slice::from_raw_parts_mut(dst.add(copied), count),
                    ),
                }
            } else {
                zero_live_bytes(dst.add(copied), count);
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
            copy_live_bytes(src.add(written), page.ppn.bytes_ptr().add(start), count);
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
        let mut scratch = Vec::new();
        for chunk in buf.buffers {
            let n = unsafe {
                data.contents.copy_out(
                    *offset,
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    Some(&mut scratch),
                )
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
