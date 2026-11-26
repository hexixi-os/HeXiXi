use super::File;
use crate::drivers::BLOCK_DEVICE;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bitflags::*;
use easy_fs::{EasyFileSystem, Inode};
use lazy_static::*;

pub struct OSInode {
    readable: bool,
    writable: bool,
    inner: UPSafeCell<OSInodeInner>,
}

pub struct OSInodeInner {
    offset: usize,
    inode: Arc<Inode>,
}

impl OSInode {
    pub fn new(readable: bool, writable: bool, inode: Arc<Inode>) -> Self {
        Self {
            readable,
            writable,
            inner: unsafe { UPSafeCell::new(OSInodeInner { offset: 0, inode }) },
        }
    }
    pub fn read_all(&self) -> Vec<u8> {
        let mut inner = self.inner.exclusive_access();
        let mut buffer = [0u8; 512];
        let mut v: Vec<u8> = Vec::new();
        loop {
            let len = inner.inode.read_at(inner.offset, &mut buffer);
            if len == 0 {
                break;
            }
            inner.offset += len;
            v.extend_from_slice(&buffer[..len]);
        }
        v
    }
}

lazy_static! {
    pub static ref ROOT_INODE: Arc<Inode> = {
        let efs = EasyFileSystem::open(BLOCK_DEVICE.clone());
        Arc::new(EasyFileSystem::root_inode(&efs))
    };
}

pub fn list_apps() {
    println!("/**** APPS ****");
    for app in ROOT_INODE.ls() {
        println!("{}", app);
    }
    println!("**************/")
}

bitflags! {
    pub struct OpenFlags: u32 {
        const RDONLY = 0;
        const WRONLY = 1 << 0;
        const RDWR = 1 << 1;
        const CREATE = 1 << 9;
        const TRUNC = 1 << 10;
        const EXCL = 1 << 11;
        const DIRECTORY = 1 << 12;
        const APPEND = 1 << 13;
        const TMPFILE = 1 << 14;
    }
}

impl OpenFlags {
    /// Do not check validity for simplicity
    /// Return (readable, writable)
    pub fn read_write(&self) -> (bool, bool) {
        if self.is_empty() {
            (true, false)
        } else if self.contains(Self::WRONLY) {
            (false, true)
        } else {
            (true, true)
        }
    }
}

lazy_static! {
    /// 已 unlink 的文件名（easy-fs 不支持删除目录项，用墓碑模拟）
    static ref UNLINKED: crate::sync::UPSafeCell<alloc::collections::BTreeSet<alloc::string::String>> =
        unsafe { crate::sync::UPSafeCell::new(alloc::collections::BTreeSet::new()) };
}

pub fn mark_unlinked(name: &str) {
    UNLINKED
        .exclusive_access()
        .insert(alloc::string::String::from(name));
}

pub fn is_unlinked(name: &str) -> bool {
    UNLINKED.exclusive_access().contains(name)
}

/// 根目录下的所有文件名（easy-fs 是扁平命名空间）
pub fn root_names() -> alloc::vec::Vec<alloc::string::String> {
    ROOT_INODE.ls()
}

pub fn open_file(name: &str, flags: OpenFlags) -> Option<Arc<OSInode>> {
    if is_unlinked(name) {
        return None;
    }
    let (readable, writable) = flags.read_write();
    if flags.contains(OpenFlags::CREATE) {
        if let Some(inode) = ROOT_INODE.find(name) {
            // clear size
            inode.clear();
            Some(Arc::new(OSInode::new(readable, writable, inode)))
        } else {
            // create file
            ROOT_INODE
                .create(name)
                .map(|inode| Arc::new(OSInode::new(readable, writable, inode)))
        }
    } else {
        ROOT_INODE.find(name).map(|inode| {
            if flags.contains(OpenFlags::TRUNC) {
                inode.clear();
            }
            Arc::new(OSInode::new(readable, writable, inode))
        })
    }
}

impl File for OSInode {
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn size(&self) -> usize {
        self.inner.exclusive_access().inode.size()
    }

    fn supports_positioned_io(&self) -> bool {
        true
    }

    fn read_positioned(&self, offset: usize, buf: UserBuffer) -> isize {
        let inner = self.inner.exclusive_access();
        let mut count = 0;
        let mut scratch = Vec::new();
        for part in buf.buffers {
            let read = part.write_with(&mut scratch, |slice| inner.inode.read_at(offset + count, slice));
            count += read;
            if read < part.len() {
                break;
            }
        }
        count as isize
    }

    fn write_positioned(&self, offset: usize, buf: UserBuffer) -> isize {
        let inner = self.inner.exclusive_access();
        let mut count = 0;
        let mut scratch = Vec::new();
        for part in buf.buffers {
            let written = match part.read_with(&mut scratch, |slice| {
                inner.inode.try_write_at_sync(offset + count, slice)
            }) {
                Ok(written) => written,
                Err(error) if count == 0 => return super::persistent::errno(error),
                Err(_) => break,
            };
            count += written;
            if written < part.len() {
                break;
            }
        }
        count as isize
    }

    fn read(&self, buf: UserBuffer) -> usize {
        let mut inner = self.inner.exclusive_access();
        let mut total_read_size = 0usize;
        let mut scratch = Vec::new();
        for buffer in buf.buffers {
            let read_size = buffer.write_with(&mut scratch, |slice| {
                inner.inode.read_at(inner.offset, slice)
            });
            if read_size == 0 {
                break;
            }
            inner.offset += read_size;
            total_read_size += read_size;
        }
        total_read_size
    }
    fn write(&self, buf: UserBuffer) -> isize {
        let mut inner = self.inner.exclusive_access();
        let mut total_write_size = 0usize;
        let mut scratch = Vec::new();
        for buffer in buf.buffers {
            let write_size = buffer.read_with(&mut scratch, |slice| {
                inner.inode.write_at(inner.offset, slice)
            });
            assert_eq!(write_size, buffer.len());
            inner.offset += write_size;
            total_write_size += write_size;
        }
        total_write_size as isize
    }
    fn seek_to(&self, pos: isize) -> isize {
        if pos < 0 {
            return -22;
        }
        self.inner.exclusive_access().offset = pos as usize;
        pos
    }
    fn seek(&self, delta: isize) -> isize {
        let cur = self.inner.exclusive_access().offset as isize;
        cur.checked_add(delta).map_or(-75, |pos| self.seek_to(pos))
    }
}
