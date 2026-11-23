//! 极简 EXT4 只读实现（目标：读取官方评测盘根目录里的测试脚本与 ELF）
//!
//! 目前支持：超级块、连续布局的块组描述符、inode、extent 树和目录项遍历。

use crate::drivers::block::BLOCK_DEVICE;
use crate::fs::File;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use alloc::sync::Arc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const EXT4_MAGIC: u16 = 0xEF53;
const EXTENT_MAGIC: u16 = 0xF30A;
const EXT4_EXTENTS_FL: u32 = 0x0008_0000;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// 按 512 字节扇区读入（起始必须是 512 的倍数）
fn read_sectors(start_sector: u64, buf: &mut [u8]) {
    let n = buf.len() / 512;
    for i in 0..n {
        let mut tmp = [0u8; 512];
        BLOCK_DEVICE.read_block(start_sector as usize + i, &mut tmp);
        buf[i * 512..(i + 1) * 512].copy_from_slice(&tmp);
    }
}

/// 任意字节偏移读取
fn read_at(byte_off: u64, buf: &mut [u8]) {
    let start = byte_off / 512 * 512;
    let end = (byte_off + buf.len() as u64 + 511) / 512 * 512;
    let mut tmp = vec![0u8; (end - start) as usize];
    read_sectors(start / 512, &mut tmp);
    let s = (byte_off - start) as usize;
    buf.copy_from_slice(&tmp[s..s + buf.len()]);
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

    fn map_block(&self, inode: &Inode, logical: u32) -> BlockMapping {
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

    pub fn read_file(&self, inode: &Inode) -> Vec<u8> {
        let mut out = vec![0; inode.size as usize];
        for (logical, bytes) in out.chunks_mut(self.block_size).enumerate() {
            if let BlockMapping::Physical(block) = self.map_block(inode, logical as u32) {
                bytes.copy_from_slice(&self.read_block(block)[..bytes.len()]);
            }
        }
        out
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
}

impl Ext4File {
    pub fn new(fs: Arc<Ext4Fs>, inode: Inode) -> Self {
        Self {
            fs,
            inode,
            offset: unsafe { UPSafeCell::new(0) },
        }
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
        let data = self.fs.read_file(&self.inode);
        let mut off = self.offset.exclusive_access();
        let mut n = 0usize;
        for byte_ptr in buf {
            if *off >= data.len() {
                break;
            }
            unsafe {
                byte_ptr.write_volatile(data[*off]);
            }
            *off += 1;
            n += 1;
        }
        n
    }
    fn size(&self) -> usize {
        self.inode.size as usize
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
    fn write(&self, _buf: UserBuffer) -> usize {
        panic!("EXT4 root is read-only");
    }
}
