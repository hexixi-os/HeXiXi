use super::{ext4::Ext4File, pipe::PipeRingBuffer, symlink::Symlink, File, OpenFlags};
use crate::config::PAGE_SIZE;
use crate::mm::{
    copy_live_bytes, frame_alloc_charged, ram_end, zero_live_bytes, FrameTracker, UserBuffer,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub(super) type OverlayData = Arc<UPSafeCell<OverlayContents>>;
const MAX_DATA_PAGES: usize = 128 * 1024 * 1024 / PAGE_SIZE;
const MAX_ENTRIES: usize = 16_384;
const MAX_PATH_BYTES: usize = 2 * 1024 * 1024;
const MUTABLE_STATUS_FLAGS: u32 = 0o2000 | 0o4000 | 0o20000 | 0o40000 | 0o1000000;

const F_SEAL_SEAL: u32 = 0x0001;
const F_SEAL_SHRINK: u32 = 0x0002;
const F_SEAL_GROW: u32 = 0x0004;
const F_SEAL_WRITE: u32 = 0x0008;
const F_SEAL_FUTURE_WRITE: u32 = 0x0010;
const F_SEAL_ALL: u32 = F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE;

static OVERLAY_INODES: AtomicUsize = AtomicUsize::new(0);
static SYMLINK_BYTES: AtomicUsize = AtomicUsize::new(0);
static TOTAL_PATH_BYTES: AtomicUsize = AtomicUsize::new(0);
pub(super) const OVERLAY_DEVICE: u64 = 1 << 32;
static NEXT_DEVICE: AtomicU64 = AtomicU64::new(2);
static OVERLAY_PAGES: AtomicUsize = AtomicUsize::new(0);
static NEXT_INODE: AtomicUsize = AtomicUsize::new(2);

pub(super) fn now_ts() -> (i64, i64) {
    let ns = crate::timer::get_realtime_ns();
    ((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as i64)
}

pub(super) fn apply_xattr_map(
    map: &mut BTreeMap<String, Vec<u8>>,
    name: &str,
    set_val: Option<(&[u8], usize)>,
    remove: bool,
) -> Result<Vec<u8>, isize> {
    if remove {
        return map.remove(name).map(|_| Vec::new()).ok_or(-61); // ENODATA
    }
    if let Some((val, flags)) = set_val {
        if flags & !3 != 0 {
            return Err(-22);
        }
        let exists = map.contains_key(name);
        if flags == 1 && exists {
            return Err(-17); // XATTR_CREATE -> EEXIST
        }
        if flags == 2 && !exists {
            return Err(-61); // XATTR_REPLACE -> ENODATA
        }
        if map.len() >= 64 && !exists {
            return Err(-28);
        }
        map.insert(String::from(name), Vec::from(val));
        return Ok(Vec::new());
    }
    if name.is_empty() {
        let mut list = Vec::new();
        for k in map.keys() {
            list.extend_from_slice(k.as_bytes());
            list.push(0);
        }
        return Ok(list);
    }
    map.get(name).cloned().ok_or(-61) // ENODATA
}

fn data_page_limit() -> usize {
    let ram_pages = ram_end().saturating_sub(crate::arch::RAM_BASE) / PAGE_SIZE;
    MAX_DATA_PAGES.min(ram_pages / 4)
}

fn next_device() -> u64 {
    NEXT_DEVICE.fetch_add(1, Ordering::Relaxed) << 32
}

fn current_fsuid_fsgid() -> (u32, u32) {
    crate::task::current_task()
        .and_then(|t| t.process.upgrade())
        .map_or((0, 0), |p| {
            let inner = p.inner_exclusive_access();
            (inner.fsuid, inner.fsgid)
        })
}

pub(super) struct OverlayDirectory {
    mode: AtomicU16,
    uid: AtomicU32,
    gid: AtomicU32,
    dev: u64,
    mount_prefix: String,
    mounted: Arc<AtomicBool>,
    detached: AtomicBool,
    pub(super) path: UPSafeCell<String>,
    pub(super) ino: u32,
    times: UPSafeCell<((i64, i64), (i64, i64), (i64, i64))>,
    pub(super) xattrs: UPSafeCell<BTreeMap<String, Vec<u8>>>,
    _charge: Option<InodeCharge>,
}

impl OverlayDirectory {
    pub(super) fn mode(&self) -> u16 {
        self.mode.load(Ordering::Relaxed)
    }

    pub(super) fn owner(&self) -> (u32, u32) {
        (self.uid.load(Ordering::Relaxed), self.gid.load(Ordering::Relaxed))
    }

    pub(super) fn set_mode(&self, perm: u16) {
        self.mode.store(0o40000 | (perm & 0o7777), Ordering::Relaxed);
        self.times.exclusive_access().2 = now_ts();
    }

    pub(super) fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) {
        if let Some(u) = uid {
            self.uid.store(u, Ordering::Relaxed);
        }
        if let Some(g) = gid {
            self.gid.store(g, Ordering::Relaxed);
        }
        self.times.exclusive_access().2 = now_ts();
    }

    pub(super) fn timestamps(&self) -> ((i64, i64), (i64, i64), (i64, i64)) {
        *self.times.exclusive_access()
    }

    pub(super) fn set_timestamps(&self, atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) {
        let mut t = self.times.exclusive_access();
        if let Some(a) = atime {
            t.0 = a;
        }
        if let Some(m) = mtime {
            t.1 = m;
        }
        t.2 = now_ts();
    }

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

pub(super) struct FifoNode {
    pub(super) mode: AtomicU16,
    pub(super) uid: AtomicU32,
    pub(super) gid: AtomicU32,
    pub(super) dev: u64,
    pub(super) ino: u32,
    pub(super) nlink: AtomicU32,
    pub(super) times: UPSafeCell<((i64, i64), (i64, i64), (i64, i64))>,
    pub(super) xattrs: UPSafeCell<BTreeMap<String, Vec<u8>>>,
    pub(super) buffer: Arc<UPSafeCell<PipeRingBuffer>>,
    _charge: InodeCharge,
}

pub(super) struct SpecialNode {
    pub(super) mode: AtomicU16,
    pub(super) uid: AtomicU32,
    pub(super) gid: AtomicU32,
    pub(super) dev: u64,
    pub(super) ino: u32,
    pub(super) rdev: (u32, u32),
    pub(super) nlink: AtomicU32,
    pub(super) times: UPSafeCell<((i64, i64), (i64, i64), (i64, i64))>,
    pub(super) xattrs: UPSafeCell<BTreeMap<String, Vec<u8>>>,
    _charge: InodeCharge,
}

pub(super) struct SpecialFile {
    node: Arc<SpecialNode>,
    readable: bool,
    writable: bool,
    extra_flags: AtomicU32,
}

impl SpecialFile {
    pub(super) fn new(node: Arc<SpecialNode>, readable: bool, writable: bool, flags: u32) -> Self {
        Self {
            node,
            readable,
            writable,
            extra_flags: AtomicU32::new(flags & MUTABLE_STATUS_FLAGS),
        }
    }
}

impl File for SpecialFile {
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn status_flags(&self) -> u32 {
        let acc = match (self.readable, self.writable) {
            (true, true) => 2,
            (false, true) => 1,
            _ => 0,
        };
        acc | self.extra_flags.load(Ordering::Relaxed)
    }
    fn set_status_flags(&self, flags: u32) {
        self.extra_flags
            .store(flags & MUTABLE_STATUS_FLAGS, Ordering::Relaxed);
    }
    fn read(&self, _: UserBuffer) -> usize {
        0
    }
    fn write(&self, buf: UserBuffer) -> isize {
        buf.len() as isize
    }
    fn mode(&self) -> u16 {
        self.node.mode.load(Ordering::Relaxed)
    }
    fn owner(&self) -> (u32, u32) {
        (
            self.node.uid.load(Ordering::Relaxed),
            self.node.gid.load(Ordering::Relaxed),
        )
    }
    fn set_mode(&self, perm: u16) -> isize {
        let cur = self.node.mode.load(Ordering::Relaxed);
        self.node
            .mode
            .store((cur & !0o7777) | (perm & 0o7777), Ordering::Relaxed);
        self.node.times.exclusive_access().2 = now_ts();
        0
    }
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> isize {
        if let Some(u) = uid {
            self.node.uid.store(u, Ordering::Relaxed);
        }
        if let Some(g) = gid {
            self.node.gid.store(g, Ordering::Relaxed);
        }
        self.node.times.exclusive_access().2 = now_ts();
        0
    }
    fn stat_identity(&self) -> Option<(u64, u64)> {
        Some((self.node.dev, u64::from(self.node.ino)))
    }
    fn device_numbers(&self) -> Option<(u32, u32)> {
        Some(self.node.rdev)
    }
    fn link_count(&self) -> u32 {
        self.node.nlink.load(Ordering::Relaxed)
    }
    fn timestamps(&self) -> ((i64, i64), (i64, i64), (i64, i64)) {
        *self.node.times.exclusive_access()
    }
    fn set_timestamps(&self, atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> isize {
        let mut t = self.node.times.exclusive_access();
        if let Some(a) = atime {
            t.0 = a;
        }
        if let Some(m) = mtime {
            t.1 = m;
        }
        t.2 = now_ts();
        0
    }
    fn xattr_op(
        &self,
        name: &str,
        set_val: Option<(&[u8], usize)>,
        remove: bool,
    ) -> Result<Vec<u8>, isize> {
        apply_xattr_map(&mut self.node.xattrs.exclusive_access(), name, set_val, remove)
    }
}

pub(super) struct FifoStatFile(pub(super) Arc<FifoNode>);

impl File for FifoStatFile {
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
    fn mode(&self) -> u16 {
        self.0.mode.load(Ordering::Relaxed)
    }
    fn owner(&self) -> (u32, u32) {
        (
            self.0.uid.load(Ordering::Relaxed),
            self.0.gid.load(Ordering::Relaxed),
        )
    }
    fn stat_identity(&self) -> Option<(u64, u64)> {
        Some((self.0.dev, u64::from(self.0.ino)))
    }
    fn link_count(&self) -> u32 {
        self.0.nlink.load(Ordering::Relaxed)
    }
    fn timestamps(&self) -> ((i64, i64), (i64, i64), (i64, i64)) {
        *self.0.times.exclusive_access()
    }
    fn set_timestamps(&self, atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> isize {
        let mut t = self.0.times.exclusive_access();
        if let Some(a) = atime {
            t.0 = a;
        }
        if let Some(m) = mtime {
            t.1 = m;
        }
        t.2 = now_ts();
        0
    }
    fn xattr_op(
        &self,
        name: &str,
        set_val: Option<(&[u8], usize)>,
        remove: bool,
    ) -> Result<Vec<u8>, isize> {
        apply_xattr_map(&mut self.0.xattrs.exclusive_access(), name, set_val, remove)
    }
}

#[derive(Clone)]
pub(super) enum OverlayEntry {
    File(OverlayData),
    Directory(Arc<OverlayDirectory>),
    Symlink(Arc<Symlink>),
    Fifo(Arc<FifoNode>),
    Special(Arc<SpecialNode>),
    Whiteout,
}

impl OverlayEntry {
    pub(super) fn dirent(&self) -> Option<(u32, u8)> {
        match self {
            Self::File(data) => Some((data.exclusive_access().ino, 1)),
            Self::Directory(dir) => Some((dir.ino, 2)),
            Self::Symlink(link) => link.stat_identity().map(|(_, ino)| (ino as u32, 7)),
            Self::Fifo(fifo) => Some((fifo.ino, 5)),
            Self::Special(spec) => {
                let ftype = match spec.mode.load(Ordering::Relaxed) & 0o170000 {
                    0o020000 => 3, // CHR
                    0o060000 => 4, // BLK
                    0o140000 => 6, // SOCK
                    _ => 0,
                };
                Some((spec.ino, ftype))
            }
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

pub(crate) struct MemFs {
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
        let ts = now_ts();
        let root = Arc::new(OverlayDirectory {
            mode: AtomicU16::new(0o40755),
            uid: AtomicU32::new(0),
            gid: AtomicU32::new(0),
            dev: OVERLAY_DEVICE,
            mount_prefix: String::new(),
            mounted: Arc::clone(&mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::new()) },
            ino: 1,
            times: unsafe { UPSafeCell::new((ts, ts, ts)) },
            xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
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
        let root_mode = if magic == 0x0102_1994 { 0o41777 } else { 0o40755 };
        let (fsuid, fsgid) = current_fsuid_fsgid();
        let ts = now_ts();
        let root = Arc::new(OverlayDirectory {
            mode: AtomicU16::new(root_mode),
            uid: AtomicU32::new(fsuid),
            gid: AtomicU32::new(fsgid),
            dev,
            mount_prefix: String::from(mount_prefix),
            mounted: Arc::clone(&mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::new()) },
            ino: next_inode(),
            times: unsafe { UPSafeCell::new((ts, ts, ts)) },
            xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
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

    pub(super) fn new_file(&self, lower: Option<Ext4File>, linked: bool, permissions: u16) -> Result<OverlayData, isize> {
        new_file_with_dev(self.dev, lower, linked, permissions, None, None)
    }

    pub(super) fn create_with_gid(
        &self,
        name: &str,
        lower: Option<Ext4File>,
        permissions: u16,
        gid_override: Option<u32>,
    ) -> Result<OverlayData, isize> {
        let mut entries = self.entries.exclusive_access();
        if let Some(OverlayEntry::File(data)) = entries.entries.get(name) {
            return Ok(data.clone());
        }
        entries.check_insert(name)?;
        let data = new_file_with_dev(self.dev, lower, true, permissions, gid_override, None)?;
        entries.insert(String::from(name), OverlayEntry::File(data.clone()));
        Ok(data)
    }

    pub(super) fn mkdir_with_gid(
        &self,
        name: &str,
        permissions: u16,
        gid_override: Option<u32>,
    ) -> Result<(), isize> {
        let (fsuid, fsgid) = current_fsuid_fsgid();
        let gid = gid_override.unwrap_or(fsgid);
        let ts = now_ts();
        let mut entries = self.entries.exclusive_access();
        entries.check_insert(name)?;
        let directory = Arc::new(OverlayDirectory {
            mode: AtomicU16::new(0o40000 | permissions),
            uid: AtomicU32::new(fsuid),
            gid: AtomicU32::new(gid),
            dev: self.dev,
            mount_prefix: self.mount_prefix.clone(),
            mounted: Arc::clone(&self.mounted),
            detached: AtomicBool::new(false),
            path: unsafe { UPSafeCell::new(String::from(name)) },
            ino: next_inode(),
            times: unsafe { UPSafeCell::new((ts, ts, ts)) },
            xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
            _charge: Some(InodeCharge::new()?),
        });
        entries.insert(String::from(name), OverlayEntry::Directory(directory));
        Ok(())
    }

    pub(super) fn mknod(
        &self,
        name: &str,
        mode: u16,
        rdev: (u32, u32),
        gid_override: Option<u32>,
    ) -> Result<(), isize> {
        let ftype = mode & 0o170000;
        let perms = mode & 0o7777;
        if ftype == 0 || ftype == 0o100000 {
            let _ = self.create_with_gid(name, None, perms, gid_override)?;
            return Ok(());
        }
        let (fsuid, fsgid) = current_fsuid_fsgid();
        let gid = gid_override.unwrap_or(fsgid);
        let ts = now_ts();
        let charge = InodeCharge::new()?;
        let mut entries = self.entries.exclusive_access();
        entries.check_insert(name)?;
        let entry = if ftype == 0o010000 {
            OverlayEntry::Fifo(Arc::new(FifoNode {
                mode: AtomicU16::new(0o010000 | perms),
                uid: AtomicU32::new(fsuid),
                gid: AtomicU32::new(gid),
                dev: self.dev,
                ino: next_inode(),
                nlink: AtomicU32::new(1),
                times: unsafe { UPSafeCell::new((ts, ts, ts)) },
                xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
                buffer: Arc::new(unsafe { UPSafeCell::new(PipeRingBuffer::new_fifo()) }),
                _charge: charge,
            }))
        } else {
            OverlayEntry::Special(Arc::new(SpecialNode {
                mode: AtomicU16::new(ftype | perms),
                uid: AtomicU32::new(fsuid),
                gid: AtomicU32::new(gid),
                dev: self.dev,
                ino: next_inode(),
                rdev,
                nlink: AtomicU32::new(1),
                times: unsafe { UPSafeCell::new((ts, ts, ts)) },
                xattrs: unsafe { UPSafeCell::new(BTreeMap::new()) },
                _charge: charge,
            }))
        };
        entries.insert(String::from(name), entry);
        Ok(())
    }

    pub(super) fn link_entry(&self, new_name: &str, entry: OverlayEntry) -> Result<(), isize> {
        let mut entries = self.entries.exclusive_access();
        entries.check_insert(new_name)?;
        match &entry {
            OverlayEntry::File(data) => {
                let mut d = data.exclusive_access();
                d.nlink = d.nlink.saturating_add(1);
                d.ctime = now_ts();
            }
            OverlayEntry::Fifo(fifo) => {
                fifo.nlink.fetch_add(1, Ordering::Relaxed);
                fifo.times.exclusive_access().2 = now_ts();
            }
            OverlayEntry::Special(spec) => {
                spec.nlink.fetch_add(1, Ordering::Relaxed);
                spec.times.exclusive_access().2 = now_ts();
            }
            OverlayEntry::Symlink(link) => {
                link.add_link();
            }
            OverlayEntry::Directory(_) => return Err(-1), // EPERM
            OverlayEntry::Whiteout => return Err(-2),
        }
        entries.insert(String::from(new_name), entry);
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

pub(super) fn overlay_create(name: &str, lower: Option<Ext4File>, permissions: u16) -> Result<OverlayData, isize> {
    OVERLAY.create_with_gid(name, lower, permissions, None)
}

pub(super) fn overlay_create_with_gid(
    name: &str,
    lower: Option<Ext4File>,
    permissions: u16,
    gid_override: Option<u32>,
) -> Result<OverlayData, isize> {
    OVERLAY.create_with_gid(name, lower, permissions, gid_override)
}

pub(super) fn new_file(lower: Option<Ext4File>, linked: bool, permissions: u16) -> Result<OverlayData, isize> {
    new_file_with_dev(OVERLAY_DEVICE, lower, linked, permissions, None, None)
}

pub(crate) fn new_memfd_file(allow_sealing: bool) -> Result<Arc<dyn File + Send + Sync>, isize> {
    let initial_seals = Some(if allow_sealing { 0 } else { F_SEAL_SEAL });
    let data = new_file_with_dev(OVERLAY_DEVICE, None, false, 0o777, None, initial_seals)?;
    Ok(Arc::new(OverlayFile::new(data, OpenFlags::RDWR)))
}

pub(crate) fn new_shm_backing(size: usize) -> Result<Arc<dyn File + Send + Sync>, isize> {
    let data = new_file_with_dev(OVERLAY_DEVICE, None, false, 0o666, None, None)?;
    resize(&data, size);
    Ok(Arc::new(OverlayFile::new(data, OpenFlags::RDWR)))
}

fn new_file_with_dev(
    dev: u64,
    lower: Option<Ext4File>,
    linked: bool,
    permissions: u16,
    gid_override: Option<u32>,
    seals: Option<u32>,
) -> Result<OverlayData, isize> {
    let charge = InodeCharge::new()?;
    let len = lower.as_ref().map_or(0, Ext4File::len);
    let (uid, mut gid) = match lower.as_ref() {
        Some(f) => f.owner(),
        None => current_fsuid_fsgid(),
    };
    if lower.is_none() {
        if let Some(g) = gid_override {
            gid = g;
        }
    }
    let ts = now_ts();
    Ok(Arc::new(unsafe {
        UPSafeCell::new(OverlayContents {
            mode: 0o100000 | permissions,
            uid,
            gid,
            dev,
            ino: next_inode(),
            nlink: u32::from(linked),
            atime: ts,
            mtime: ts,
            ctime: ts,
            xattrs: BTreeMap::new(),
            seals,
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

pub(super) fn overlay_mkdir_with_gid(name: &str, permissions: u16, gid_override: Option<u32>) -> Result<(), isize> {
    OVERLAY.mkdir_with_gid(name, permissions, gid_override)
}

pub(super) fn overlay_mknod(name: &str, mode: u16, rdev: (u32, u32), gid_override: Option<u32>) -> Result<(), isize> {
    OVERLAY.mknod(name, mode, rdev, gid_override)
}

pub(super) fn overlay_link(new_name: &str, entry: OverlayEntry) -> Result<(), isize> {
    OVERLAY.link_entry(new_name, entry)
}

pub(super) fn overlay_symlink(name: &str, target: Vec<u8>) -> Result<(), isize> {
    OVERLAY.symlink(name, target)
}

fn detach(entry: Option<OverlayEntry>) {
    match entry {
        Some(OverlayEntry::File(data)) => {
            let mut d = data.exclusive_access();
            d.nlink = d.nlink.saturating_sub(1);
            d.ctime = now_ts();
        }
        Some(OverlayEntry::Directory(dir)) => {
            dir.detached.store(true, Ordering::Relaxed);
            *dir.path.exclusive_access() = String::new();
        }
        Some(OverlayEntry::Symlink(link)) => link.detach(),
        Some(OverlayEntry::Fifo(fifo)) => {
            let _ = fifo
                .nlink
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| c.checked_sub(1));
        }
        Some(OverlayEntry::Special(spec)) => {
            let _ = spec
                .nlink
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |c| c.checked_sub(1));
        }
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
    pub(super) mode: u16,
    pub(super) uid: u32,
    pub(super) gid: u32,
    dev: u64,
    pub(super) ino: u32,
    pub(super) nlink: u32,
    pub(super) atime: (i64, i64),
    pub(super) mtime: (i64, i64),
    pub(super) ctime: (i64, i64),
    pub(super) xattrs: BTreeMap<String, Vec<u8>>,
    pub(super) seals: Option<u32>,
    _charge: InodeCharge,
    contents: FileData,
}

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
        let ts = now_ts();
        data.mtime = ts;
        data.ctime = ts;
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

    pub(super) fn len(&self) -> usize {
        self.contents.len
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
    status: AtomicU32,
}

impl OverlayFile {
    pub(super) fn new(data: OverlayData, flags: OpenFlags) -> Self {
        Self::new_with_status(data, flags, 0)
    }

    pub(super) fn new_with_status(data: OverlayData, flags: OpenFlags, extra: u32) -> Self {
        let mut st = extra & MUTABLE_STATUS_FLAGS;
        if flags.contains(OpenFlags::APPEND) {
            st |= 0o2000;
        }
        Self {
            data,
            offset: unsafe { UPSafeCell::new(0) },
            flags,
            status: AtomicU32::new(st),
        }
    }

    fn read_buffer(&self, start: usize, buf: UserBuffer) -> usize {
        let mut data = self.data.exclusive_access();
        if self.status.load(Ordering::Relaxed) & 0o1000000 == 0 {
            data.atime = now_ts();
        }
        let mut offset = start;
        let mut scratch = Vec::new();
        for chunk in buf.buffers {
            let n = unsafe {
                data.contents.copy_out(
                    offset,
                    chunk.as_mut_ptr(),
                    chunk.len(),
                    Some(&mut scratch),
                )
            };
            offset += n;
            if n < chunk.len() {
                break;
            }
        }
        offset - start
    }

    fn write_buffer(&self, start: usize, buf: UserBuffer, positioned: bool) -> Result<(usize, usize), isize> {
        const EPERM: isize = -1;
        const EFBIG: isize = -27;
        const ENOSPC: isize = -28;

        let mut d = self.data.exclusive_access();
        if let Some(seals) = d.seals {
            if seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
                return Err(EPERM);
            }
        }
        let append = !positioned && (self.status.load(Ordering::Relaxed) & 0o2000 != 0);
        let mut position = if append { d.contents.len } else { start };
        let total_req: usize = buf.buffers.iter().map(|c| c.len()).sum();
        if let Some(seals) = d.seals {
            if seals & F_SEAL_GROW != 0 && position.saturating_add(total_req) > d.contents.len {
                return Err(EPERM);
            }
        }
        let first = position;
        for chunk in buf.buffers {
            let len = chunk.len();
            if len == 0 {
                continue;
            }
            let count = len.min((isize::MAX as usize).saturating_sub(position));
            if count == 0 {
                if position == first {
                    return Err(EFBIG);
                }
                break;
            }
            let n = unsafe { d.contents.copy_in(position, chunk.as_ptr(), count) };
            if n == 0 && position == first {
                return Err(ENOSPC);
            }
            position += n;
            if n < len {
                break;
            }
        }
        if position > first {
            let ts = now_ts();
            d.mtime = ts;
            d.ctime = ts;
            // Clear SUID/SGID on write by non-root (or POSIX write semantics)
            if d.mode & 0o6000 != 0 {
                let (euid, _) = current_fsuid_fsgid();
                if euid != 0 {
                    d.mode &= !0o6000;
                }
            }
        }
        Ok((position, position - first))
    }
}

impl File for OverlayFile {
    fn mode(&self) -> u16 {
        self.data.exclusive_access().mode
    }

    fn owner(&self) -> (u32, u32) {
        let d = self.data.exclusive_access();
        (d.uid, d.gid)
    }

    fn status_flags(&self) -> u32 {
        let acc = match (self.readable(), self.writable()) {
            (true, true) => 2,
            (false, true) => 1,
            _ => 0,
        };
        acc | self.status.load(Ordering::Relaxed)
    }

    fn set_status_flags(&self, flags: u32) {
        self.status
            .store(flags & MUTABLE_STATUS_FLAGS, Ordering::Relaxed);
    }

    fn timestamps(&self) -> ((i64, i64), (i64, i64), (i64, i64)) {
        let d = self.data.exclusive_access();
        (d.atime, d.mtime, d.ctime)
    }

    fn set_timestamps(&self, atime: Option<(i64, i64)>, mtime: Option<(i64, i64)>) -> isize {
        let mut d = self.data.exclusive_access();
        if let Some(a) = atime {
            d.atime = a;
        }
        if let Some(m) = mtime {
            d.mtime = m;
        }
        d.ctime = now_ts();
        0
    }

    fn seals(&self) -> Option<u32> {
        self.data.exclusive_access().seals
    }

    fn add_seals(&self, add: u32) -> isize {
        if add & !F_SEAL_ALL != 0 {
            return -22;
        }
        if !self.writable() {
            return -1; // EPERM
        }
        let (cur, id) = {
            let d = self.data.exclusive_access();
            let Some(cur) = d.seals else {
                return -22;
            };
            if cur & F_SEAL_SEAL != 0 {
                return -1; // EPERM
            }
            (cur, d.identity())
        };
        // Drop data borrow before scanning VMAs (which calls stat_identity on mapped files).
        if add & F_SEAL_WRITE != 0 && cur & F_SEAL_WRITE == 0 {
            if crate::task::has_shared_writable_mapping(id) {
                return -16; // EBUSY
            }
        }
        let mut d = self.data.exclusive_access();
        let Some(cur) = d.seals else {
            return -22;
        };
        if cur & F_SEAL_SEAL != 0 {
            return -1;
        }
        d.seals = Some(cur | add);
        0
    }

    fn xattr_op(
        &self,
        name: &str,
        set_val: Option<(&[u8], usize)>,
        remove: bool,
    ) -> Result<Vec<u8>, isize> {
        let mut d = self.data.exclusive_access();
        let res = apply_xattr_map(&mut d.xattrs, name, set_val, remove);
        if (set_val.is_some() || remove) && res.is_ok() {
            d.ctime = now_ts();
        }
        res
    }

    fn set_mode(&self, perm: u16) -> isize {
        let mut d = self.data.exclusive_access();
        d.mode = (d.mode & !0o7777) | (perm & 0o7777);
        d.ctime = now_ts();
        0
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> isize {
        let mut d = self.data.exclusive_access();
        if let Some(u) = uid {
            d.uid = u;
        }
        if let Some(g) = gid {
            d.gid = g;
        }
        if uid.is_some() || gid.is_some() {
            d.mode &= !0o6000;
        }
        d.ctime = now_ts();
        0
    }

    fn punch_hole(&self, offset: usize, len: usize) -> isize {
        if !self.writable() {
            return -9;
        }
        let mut d = self.data.exclusive_access();
        if let Some(seals) = d.seals {
            if seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
                return -1;
            }
        }
        let file_len = d.contents.len;
        if offset >= file_len || len == 0 {
            return 0;
        }
        let end = offset.saturating_add(len).min(file_len);
        let zeros = [0u8; 256];
        let mut pos = offset;
        while pos < end {
            let chunk = (end - pos).min(zeros.len());
            let n = unsafe { d.contents.copy_in(pos, zeros.as_ptr(), chunk) };
            if n == 0 {
                return -28;
            }
            pos += n;
        }
        let ts = now_ts();
        d.mtime = ts;
        d.ctime = ts;
        0
    }

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
        self.data.exclusive_access().nlink
    }

    fn read(&self, buf: UserBuffer) -> usize {
        let mut offset = self.offset.exclusive_access();
        let count = self.read_buffer(*offset, buf);
        *offset += count;
        count
    }

    fn write(&self, buf: UserBuffer) -> isize {
        let mut offset = self.offset.exclusive_access();
        match self.write_buffer(*offset, buf, false) {
            Ok((end, count)) => {
                if count != 0 {
                    *offset = end;
                }
                count as isize
            }
            Err(error) => error,
        }
    }

    fn supports_positioned_io(&self) -> bool {
        true
    }

    fn read_positioned(&self, offset: usize, buf: UserBuffer) -> isize {
        self.read_buffer(offset, buf) as isize
    }

    fn write_positioned(&self, offset: usize, buf: UserBuffer) -> isize {
        self.write_buffer(offset, buf, true)
            .map_or_else(|error| error, |(_, count)| count as isize)
    }

    fn size(&self) -> usize {
        self.data.exclusive_access().contents.len
    }

    fn set_len(&self, len: usize) -> isize {
        if !self.writable() {
            return -22;
        }
        {
            let d = self.data.exclusive_access();
            if let Some(seals) = d.seals {
                if seals & F_SEAL_WRITE != 0 {
                    return -1;
                }
                if seals & F_SEAL_SHRINK != 0 && len < d.contents.len {
                    return -1;
                }
                if seals & F_SEAL_GROW != 0 && len > d.contents.len {
                    return -1;
                }
            }
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
        self.data
            .exclusive_access()
            .seals
            .map_or(true, |s| s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) == 0)
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
