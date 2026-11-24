//! 极简 EXT4 只读实现（目标：读取官方评测盘根目录里的测试脚本与 ELF）
//!
//! 目前支持：超级块、连续布局的块组描述符、inode、extent 树和目录项遍历。

use super::page_cache::{self, Access, FilePageKey, ReadStream};
use crate::config::PAGE_SIZE;
use crate::drivers::block::BLOCK_DEVICE;
use crate::fs::File;
use crate::mm::{FrameTracker, UserBuffer};
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub const EXT4_MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;
const EXT4_EXTENTS_FL: u32 = 0x0008_0000;
const SECTOR_SIZE: usize = 512;
static NEXT_FS_ID: AtomicUsize = AtomicUsize::new(1);
static SECTORS_READ: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn sectors_read() -> usize {
    SECTORS_READ.load(Ordering::Relaxed)
}

// VirtIO 用一个 DMA 描述符传输整个扇区，对齐后缓冲区始终位于同一页内。
#[repr(align(512))]
struct SectorBuffer([u8; SECTOR_SIZE]);

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// 从磁盘字节偏移读入缓冲区，只访问覆盖请求范围的扇区。
fn read_at(mut offset: u64, mut buf: &mut [u8]) {
    let mut sector = SectorBuffer([0; SECTOR_SIZE]);
    while !buf.is_empty() {
        let start = (offset % SECTOR_SIZE as u64) as usize;
        let len = buf.len().min(SECTOR_SIZE - start);
        BLOCK_DEVICE.read_block((offset / SECTOR_SIZE as u64) as usize, &mut sector.0);
        SECTORS_READ.fetch_add(1, Ordering::Relaxed);
        buf[..len].copy_from_slice(&sector.0[start..start + len]);
        buf = &mut buf[len..];
        offset += len as u64;
    }
}

pub struct Inode {
    pub ino: u32,
    pub mode: u16,
    pub size: u64,
    pub blocks: [u8; 60],
    pub flags: u32,
}

impl Inode {
    pub fn is_dir(&self) -> bool {
        self.mode & 0xF000 == 0x4000
    }
    pub fn is_file(&self) -> bool {
        self.mode & 0xF000 == 0x8000
    }
}

pub struct Ext4Fs {
    id: usize,
    pub block_size: usize,
    pub inode_size: usize,
    inode_count: u32,
    inodes_per_group: u32,
    group_desc_offset: u64,
    group_desc_size: usize,
}

enum BlockMapping {
    Hole,
    Unwritten,
    Physical(u64),
}

impl Ext4Fs {
    pub fn new() -> Option<Self> {
        let mut sb = [0u8; 512];
        read_at(1024, &mut sb);
        if le16(&sb, 56) != EXT4_MAGIC {
            return None;
        }
        let block_size = 1024usize << le32(&sb, 24);
        let inode_size = if le16(&sb, 88) == 0 { 128 } else { le16(&sb, 88) as usize };
        let inode_count = le32(&sb, 0);
        let inodes_per_group = le32(&sb, 40);
        if inodes_per_group == 0 {
            return None;
        }
        let first_data_block = le32(&sb, 20) as u64;
        let incompat = le32(&sb, 96);
        let desc_size = if incompat & 0x80 != 0 {
            let d = le16(&sb, 254);
            if d == 0 { 32 } else { d as usize }
        } else {
            32
        };
        Some(Self {
            id: NEXT_FS_ID.fetch_add(1, Ordering::Relaxed),
            block_size,
            inode_size,
            inode_count,
            inodes_per_group,
            group_desc_offset: (first_data_block + 1) * block_size as u64,
            group_desc_size: desc_size,
        })
    }

    fn read_block(&self, block: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.block_size];
        read_at(block * self.block_size as u64, &mut b);
        b
    }

    pub fn inode(&self, ino: u32) -> Option<Inode> {
        if ino == 0 || ino > self.inode_count {
            return None;
        }
        let group = (ino - 1) / self.inodes_per_group;
        let idx = (ino - 1) % self.inodes_per_group;
        let mut desc = [0; 64];
        let desc_len = self.group_desc_size.min(desc.len());
        read_at(
            self.group_desc_offset + group as u64 * self.group_desc_size as u64,
            &mut desc[..desc_len],
        );
        let inode_table_block = le32(&desc, 8) as u64 | ((le32(&desc, 40) as u64) << 32);
        if inode_table_block == 0 {
            return None;
        }
        let off = inode_table_block * self.block_size as u64
            + idx as u64 * self.inode_size as u64;
        let mut b = vec![0u8; self.inode_size.max(128)];
        read_at(off, &mut b);
        let mut blocks = [0u8; 60];
        blocks.copy_from_slice(&b[40..100]);
        let size = le32(&b, 4) as u64 | ((le32(&b, 108) as u64) << 32);
        Some(Inode {
            ino,
            mode: le16(&b, 0),
            size,
            blocks,
            flags: le32(&b, 32),
        })
    }

    fn map_extent(&self, node: &[u8], logical: u32) -> BlockMapping {
        if node.len() < 12 || le16(node, 0) != EXTENT_MAGIC {
            return BlockMapping::Hole;
        }
        let entries = le16(node, 2) as usize;
        let entries = node[12..].chunks_exact(12).take(entries);
        if le16(node, 6) == 0 {
            for e in entries {
                let ee_block = le32(e, 0);
                let raw_len = le16(e, 4);
                let start = ((le16(e, 6) as u64) << 32) | le32(e, 8) as u64;
                // ee_len == 0x8000 是 32768 个已写入块，只有更大的值表示未写入。
                let unwritten = raw_len > 0x8000;
                let len = if unwritten { raw_len - 0x8000 } else { raw_len } as u32;
                if logical >= ee_block && logical - ee_block < len {
                    return if unwritten {
                        BlockMapping::Unwritten
                    } else {
                        BlockMapping::Physical(start + (logical - ee_block) as u64)
                    };
                }
            }
            BlockMapping::Hole
        } else {
            match entries.take_while(|e| le32(e, 0) <= logical).last() {
                Some(entry) => {
                    let block = ((le16(entry, 8) as u64) << 32) | le32(entry, 4) as u64;
                    self.map_extent(&self.read_block(block), logical)
                }
                None => BlockMapping::Hole,
            }
        }
    }

    fn map_block(&self, inode: &Inode, logical: u64) -> BlockMapping {
        let logical = match u32::try_from(logical) {
            Ok(block) => block,
            Err(_) => return BlockMapping::Hole,
        };
        if inode.flags & EXT4_EXTENTS_FL != 0 {
            self.map_extent(&inode.blocks, logical)
        } else {
            // 直接块指针表（小文件）
            if logical < 12 {
                let block = le32(&inode.blocks, logical as usize * 4);
                if block != 0 {
                    return BlockMapping::Physical(block as u64);
                }
            }
            BlockMapping::Hole
        }
    }

    /// 从文件偏移读取，返回实际写入的字节数。跨过 EOF 时保留输出缓冲区的剩余内容。
    pub fn read_file_at(&self, inode: &Inode, mut offset: u64, buf: &mut [u8]) -> usize {
        let len = inode.size.saturating_sub(offset).min(buf.len() as u64) as usize;
        let block_size = self.block_size as u64;
        let mut remaining = &mut buf[..len];
        while !remaining.is_empty() {
            let in_block = (offset % block_size) as usize;
            let count = remaining.len().min(self.block_size - in_block);
            let (chunk, rest) = remaining.split_at_mut(count);
            match self.map_block(inode, offset / block_size) {
                BlockMapping::Physical(block) => {
                    read_at(block * block_size + in_block as u64, chunk);
                }
                BlockMapping::Hole | BlockMapping::Unwritten => chunk.fill(0),
            }
            offset += count as u64;
            remaining = rest;
        }
        len
    }

    pub fn read_file(&self, inode: &Inode) -> Vec<u8> {
        let mut out = vec![0; inode.size as usize];
        self.read_file_at(inode, 0, &mut out);
        out
    }

    fn file_page(&self, inode: &Inode, index: u64, access: Access) -> Option<Arc<FrameTracker>> {
        let key = FilePageKey {
            fs: self.id,
            inode: inode.ino,
            index,
        };
        page_cache::get_or_load(key, access, |bytes| {
            self.read_file_at(inode, index * PAGE_SIZE as u64, bytes);
        })
    }

    fn read_cached_at(
        &self,
        inode: &Inode,
        mut offset: u64,
        buf: &mut [u8],
        mut stream: Option<&mut ReadStream>,
    ) -> usize {
        if !inode.is_file() {
            return self.read_file_at(inode, offset, buf);
        }
        let len = inode.size.saturating_sub(offset).min(buf.len() as u64) as usize;
        let mut remaining = &mut buf[..len];
        while !remaining.is_empty() {
            let in_page = (offset % PAGE_SIZE as u64) as usize;
            let count = remaining.len().min(PAGE_SIZE - in_page);
            let (chunk, rest) = remaining.split_at_mut(count);
            let access = stream
                .as_deref_mut()
                .map_or_else(Access::read, |stream| stream.access(offset, count));
            if let Some(frame) = self.file_page(inode, offset / PAGE_SIZE as u64, access) {
                let bytes = frame.ppn.get_bytes_array();
                chunk.copy_from_slice(&bytes[in_page..in_page + count]);
            } else {
                self.read_file_at(inode, offset, chunk);
            }
            offset += count as u64;
            remaining = rest;
        }
        len
    }

    pub fn list_dir(&self, inode: &Inode) -> Vec<(String, u32, u8)> {
        let mut out = Vec::new();
        let data = self.read_file(inode);
        let mut off = 0usize;
        while off + 8 <= data.len() {
            let ino = le32(&data, off);
            let rec_len = le16(&data, off + 4) as usize;
            let name_len = data[off + 6] as usize;
            let ftype = data[off + 7];
            if rec_len < 8 || off + rec_len > data.len() {
                break;
            }
            if ino != 0 && name_len > 0 && off + 8 + name_len <= data.len() {
                let name = core::str::from_utf8(&data[off + 8..off + 8 + name_len])
                    .unwrap_or("?")
                    .into();
                out.push((name, ino, ftype));
            }
            off += rec_len;
        }
        out
    }

    /// 按路径查找（仅支持绝对路径，逐级目录）
    pub fn lookup(&self, path: &str) -> Option<Inode> {
        let mut cur = self.inode(2)?;
        for part in path.split('/').filter(|s| !s.is_empty()) {
            if !cur.is_dir() {
                return None;
            }
            let entries = self.list_dir(&cur);
            let (_, ino, _) = entries.iter().find(|(n, _, _)| n == part)?;
            cur = self.inode(*ino)?;
        }
        Some(cur)
    }
}

impl Drop for Ext4Fs {
    fn drop(&mut self) {
        page_cache::discard_fs(self.id);
    }
}

/// 启动时探测并在命中时打印根目录（临时验证用）
pub fn probe() -> bool {
    let fs = match Ext4Fs::new() {
        Some(fs) => fs,
        None => {
            println!("[ext4] no ext4 superblock on this disk");
            return false;
        }
    };
    println!("[ext4] block_size={} inode_size={}", fs.block_size, fs.inode_size);
    if let Some(root) = fs.inode(2) {
        println!("[ext4] root inode: dir={} size={}", root.is_dir(), root.size);
        for (name, ino, ftype) in fs.list_dir(&root) {
            let kind = match ftype { 1 => "file", 2 => "dir", _ => "?" };
            let size = fs.inode(ino).map(|i| i.size).unwrap_or(0);
            println!("[ext4]   {} ino={} type={} size={}", name, ino, kind, size);
        }
    }
    true
}

/// 把 EXT4 的只读 inode 包装成内核的 File
pub struct Ext4File {
    fs: Arc<Ext4Fs>,
    inode: Inode,
    offset: UPSafeCell<usize>,
    stream: UPSafeCell<ReadStream>,
}

impl Ext4File {
    pub fn new(fs: Arc<Ext4Fs>, inode: Inode) -> Self {
        Self {
            fs,
            inode,
            offset: unsafe { UPSafeCell::new(0) },
            stream: unsafe { UPSafeCell::new(ReadStream::default()) },
        }
    }

    pub(super) fn len(&self) -> usize {
        self.inode.size as usize
    }

    pub(super) fn read_at(&self, offset: usize, buffer: &mut [u8]) -> usize {
        self.fs
            .read_cached_at(&self.inode, offset as u64, buffer, None)
    }

    pub(super) fn cached_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        if !self.inode.is_file() || offset >= self.len() || offset % PAGE_SIZE != 0 {
            return None;
        }
        self.fs
            .file_page(&self.inode, (offset / PAGE_SIZE) as u64, Access::Fault)
    }
}

impl File for Ext4File {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, buf: UserBuffer) -> usize {
        let mut off = self.offset.exclusive_access();
        let mut stream = self.stream.exclusive_access();
        let mut total = 0;
        for buffer in buf.buffers {
            let chunk =
                unsafe { core::slice::from_raw_parts_mut(buffer.as_mut_ptr(), buffer.len()) };
            let read = self
                .fs
                .read_cached_at(&self.inode, *off as u64, chunk, Some(&mut stream));
            *off += read;
            total += read;
            if read < chunk.len() {
                break;
            }
        }
        total
    }
    fn size(&self) -> usize {
        self.len()
    }
    fn mapping_size(&self) -> Option<usize> {
        self.inode.is_file().then(|| self.size())
    }
    fn mapping_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        self.cached_page(offset)
    }
    fn seek_to(&self, pos: isize) -> isize {
        if pos < 0 {
            return -1;
        }
        let len = self.inode.size as isize;
        let new = if pos > len { len } else { pos };
        *self.offset.exclusive_access() = new as usize;
        new
    }
    fn seek(&self, offset: isize) -> isize {
        let len = self.inode.size as isize;
        let mut off = self.offset.exclusive_access();
        let new = (*off as isize).saturating_add(offset);
        if new < 0 {
            return -1;
        }
        let new = if new > len { len } else { new };
        *off = new as usize;
        new
    }
    fn write(&self, _buf: UserBuffer) -> isize {
        panic!("EXT4 root is read-only");
    }
}
