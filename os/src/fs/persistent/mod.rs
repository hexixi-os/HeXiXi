mod file;

use super::{open_directory, page_cache, DirectoryIdentity, File, OpenFlags};
use crate::drivers::block::{secondary_disk, SecondaryDisk};
use crate::sync::UPSafeCell;
use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use easy_fs::{block_cache_sync_device, BlockDevice, EasyFileSystem, FsError, Inode};
pub use file::PersistentExecutable;
use file::PersistentFile;

pub(super) const MAGIC: u64 = 0x3b80_0001;
const MS_RDONLY: usize = 1;
const MS_SILENT: usize = 1 << 15;
const DEVICE_BASE: u64 = 1 << 25;
static ACTIVE: UPSafeCell<Option<Weak<PersistentFs>>> = unsafe { UPSafeCell::new(None) };

pub(super) fn errno(error: FsError) -> isize {
    match error {
        FsError::Invalid => -22,
        FsError::NoSpace => -28,
        FsError::Exists => -17,
        FsError::NotFound => -2,
        FsError::NotDirectory => -20,
        FsError::IsDirectory => -21,
        FsError::NotEmpty => -39,
        FsError::NameTooLong => -36,
        FsError::FileTooLarge => -27,
        FsError::CrossDevice => -18,
    }
}

pub(super) struct PersistentFs {
    id: usize,
    disk: Arc<SecondaryDisk>,
    device: Arc<dyn BlockDevice>,
    root: Arc<Inode>,
    readonly: bool,
    nodes: UPSafeCell<BTreeMap<u32, Arc<PersistentNode>>>,
}

pub(super) struct PersistentNode {
    inode: Arc<Inode>,
    fs_id: usize,
    path: UPSafeCell<String>,
    life: UPSafeCell<Life>,
    mappings: UPSafeCell<BTreeMap<usize, file::TrackedPage>>,
}

struct Life {
    linked: bool,
    writers: usize,
    executables: usize,
}

impl PersistentNode {
    pub(super) fn path(&self) -> String {
        self.path.exclusive_access().clone()
    }
    pub(super) fn linked(&self) -> bool {
        self.life.exclusive_access().linked
    }
    pub(super) fn identity(&self) -> (u64, u64) {
        (
            DEVICE_BASE + self.fs_id as u64,
            u64::from(self.inode.id()) + 1,
        )
    }
}

impl Drop for PersistentNode {
    fn drop(&mut self) {
        if !self.linked() {
            page_cache::discard_inode_pages(self.fs_id, self.inode.id(), 0);
            self.inode.reap();
        }
    }
}

impl PersistentFs {
    pub(super) fn mount(source: &str, flags: usize) -> Result<Arc<Self>, isize> {
        if source != "/dev/vdb" {
            return Err(-19);
        }
        if flags & !(MS_RDONLY | MS_SILENT) != 0 {
            return Err(-22);
        }
        if ACTIVE
            .exclusive_access()
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some()
        {
            return Err(-16);
        }
        let disk = secondary_disk().ok_or(-19isize)?;
        let readonly = flags & MS_RDONLY != 0;
        if !readonly && disk.readonly() {
            return Err(-30);
        }
        let mut header = [0; easy_fs::BLOCK_SZ];
        disk.read_sector(0, &mut header)?;
        if u32::from_le_bytes(header[..4].try_into().unwrap()) != MAGIC as u32 {
            return Err(-22);
        }
        let device: Arc<dyn BlockDevice> = disk.clone();
        let filesystem =
            EasyFileSystem::try_open(device.clone(), disk.capacity()).map_err(errno)?;
        let fs = Arc::new(Self {
            id: page_cache::new_fs_id(),
            disk,
            device,
            readonly,
            root: Arc::new(EasyFileSystem::root_inode(&filesystem)),
            nodes: unsafe { UPSafeCell::new(BTreeMap::new()) },
        });
        *ACTIVE.exclusive_access() = Some(Arc::downgrade(&fs));
        Ok(fs)
    }

    pub(super) fn sync_active() -> Result<(), isize> {
        let active = ACTIVE.exclusive_access().as_ref().and_then(Weak::upgrade);
        active.map_or(Ok(()), |fs| fs.sync())
    }

    fn check_writable(&self) -> Result<(), isize> {
        if self.readonly {
            Err(-30)
        } else {
            Ok(())
        }
    }

    pub(super) fn space_usage(&self) -> easy_fs::SpaceUsage {
        self.root.space_usage()
    }

    pub(super) fn dev(&self) -> u64 {
        DEVICE_BASE + self.id as u64
    }
    pub(super) fn readonly(&self) -> bool {
        self.readonly
    }
    pub(super) fn root_ino(&self) -> u32 {
        self.root.id() + 1
    }

    fn lookup(&self, relative: &str) -> Result<Arc<Inode>, isize> {
        let mut node = self.root.clone();
        for part in relative.split('/').filter(|part| !part.is_empty()) {
            if !node.is_dir() {
                return Err(-20);
            }
            node = node.find(part).ok_or(-2isize)?;
        }
        Ok(node)
    }

    fn parent<'a>(&self, path: &'a str) -> Result<(Arc<Inode>, &'a str), isize> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if name.is_empty() {
            return Err(-16);
        }
        let node = self.lookup(parent)?;
        if !node.is_dir() {
            return Err(-20);
        }
        Ok((node, name))
    }

    fn node(&self, inode: Arc<Inode>, full: &str) -> Arc<PersistentNode> {
        let mut entries = self.nodes.exclusive_access();
        entries
            .entry(inode.id())
            .or_insert_with(|| {
                Arc::new(PersistentNode {
                    inode,
                    fs_id: self.id,
                    path: unsafe { UPSafeCell::new(String::from(full)) },
                    life: unsafe {
                        UPSafeCell::new(Life {
                            linked: true,
                            writers: 0,
                            executables: 0,
                        })
                    },
                    mappings: unsafe { UPSafeCell::new(BTreeMap::new()) },
                })
            })
            .clone()
    }

    pub(super) fn is_directory(&self, relative: &str) -> Result<bool, isize> {
        self.lookup(relative).map(|node| node.is_dir())
    }

    pub(super) fn directory(
        self: &Arc<Self>,
        relative: &str,
        full: &str,
    ) -> Result<DirectoryIdentity, isize> {
        let inode = self.lookup(relative)?;
        if !inode.is_dir() {
            return Err(-20);
        }
        Ok(DirectoryIdentity::Persistent(
            self.clone(),
            self.node(inode, full),
        ))
    }

    pub(super) fn open(
        self: &Arc<Self>,
        relative: &str,
        full: &str,
        flags: OpenFlags,
    ) -> Result<Arc<dyn File + Send + Sync>, isize> {
        let exclusive = flags.contains(OpenFlags::CREATE | OpenFlags::EXCL);
        let inode = match self.lookup(relative) {
            Ok(_) if exclusive => return Err(-17),
            Ok(inode) => inode,
            Err(-2)
                if flags.contains(OpenFlags::CREATE) && !flags.contains(OpenFlags::DIRECTORY) =>
            {
                self.check_writable()?;
                let (parent, name) = self.parent(relative)?;
                parent.try_create(name, false).map_err(errno)?
            }
            Err(error) => return Err(error),
        };
        if inode.is_dir() {
            return open_directory(
                full,
                flags,
                DirectoryIdentity::Persistent(self.clone(), self.node(inode, full)),
            );
        }
        if flags.contains(OpenFlags::DIRECTORY) {
            return Err(-20);
        }
        Ok(Arc::new(PersistentFile::open(
            self.clone(),
            self.node(inode, full),
            flags,
        )?))
    }

    pub(super) fn executable(
        self: &Arc<Self>,
        relative: &str,
        full: &str,
    ) -> Result<PersistentExecutable, isize> {
        let inode = self.lookup(relative)?;
        if inode.is_dir() {
            return Err(-13);
        }
        PersistentExecutable::open(self.clone(), self.node(inode, full))
    }

    pub(super) fn mkdir(&self, relative: &str) -> Result<(), isize> {
        self.check_writable()?;
        if relative.is_empty() {
            return Err(-17);
        }
        let (parent, name) = self.parent(relative)?;
        parent.try_create(name, true).map(|_| ()).map_err(errno)
    }

    fn detach(&self, inode: Arc<Inode>) {
        let node = self.nodes.exclusive_access().remove(&inode.id());
        if let Some(node) = node {
            node.life.exclusive_access().linked = false;
        } else {
            inode.reap();
        }
    }

    pub(super) fn unlink(&self, relative: &str, directory: bool) -> Result<(), isize> {
        self.check_writable()?;
        let (parent, name) = self.parent(relative)?;
        self.detach(parent.unlink(name, directory).map_err(errno)?);
        Ok(())
    }

    pub(super) fn rename(
        &self,
        old: &str,
        new: &str,
        old_full: &str,
        new_full: &str,
        no_replace: bool,
    ) -> Result<(), isize> {
        self.check_writable()?;
        let (old_parent, old_name) = self.parent(old)?;
        let (new_parent, new_name) = self.parent(new)?;
        if let Some(replaced) = old_parent
            .rename(old_name, &new_parent, new_name, no_replace)
            .map_err(errno)?
        {
            self.detach(replaced);
        }
        for node in self
            .nodes
            .exclusive_access()
            .values()
            .filter(|node| node.inode.is_dir())
        {
            let mut path = node.path.exclusive_access();
            if let Some(rest) = path
                .strip_prefix(old_full)
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
            {
                *path = alloc::format!("{}{}", new_full, rest);
            }
        }
        Ok(())
    }

    pub(super) fn entries(&self, relative: &str) -> Vec<(String, u32, u8)> {
        let dir = match self.lookup(relative) {
            Ok(dir) if dir.is_dir() => dir,
            _ => return Vec::new(),
        };
        let parent = self
            .parent(relative)
            .map_or(self.root_ino(), |(parent, _)| parent.id() + 1);
        let mut entries = vec![
            (String::from("."), dir.id() + 1, 2),
            (String::from(".."), parent, 2),
        ];
        entries.extend(dir.ls().into_iter().filter_map(|name| {
            let inode = dir.find(&name)?;
            Some((name, inode.id() + 1, if inode.is_dir() { 2 } else { 1 }))
        }));
        entries
    }

    fn barrier(&self) -> Result<(), isize> {
        if self.readonly {
            return Ok(());
        }
        block_cache_sync_device(&self.device);
        self.disk.flush()
    }

    pub(super) fn sync(&self) -> Result<(), isize> {
        if self.readonly {
            return Ok(());
        }
        let nodes = self.nodes.exclusive_access();
        for node in nodes.values() {
            node.writeback(0, usize::MAX)?;
        }
        self.barrier()?;
        for node in nodes.values() {
            node.finish_writeback(0, usize::MAX);
        }
        Ok(())
    }
}

impl Drop for PersistentFs {
    fn drop(&mut self) {
        page_cache::discard_fs(self.id);
    }
}
