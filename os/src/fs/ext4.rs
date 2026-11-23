//! 极简 EXT4 只读实现（目标：读取官方评测盘根目录里的测试脚本与 ELF）
//!
//! 目前支持：超级块、块组描述符、inode 读取、extent 树（深度 0/1）、目录项遍历。

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
    pub inodes_per_group: u32,
    inode_table_block: u64,
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
        let inodes_per_group = le32(&sb, 40);
        let first_data_block = le32(&sb, 20) as u64;
        let incompat = le32(&sb, 96);
        let desc_size = if incompat & 0x80 != 0 {
            let d = le16(&sb, 254);
            if d == 0 { 32 } else { d as usize }
        } else {
            32
        };
        // 块组 0 的描述符在 (first_data_block + 1) 号块的开头
        let gdt_off = (first_data_block + 1) * block_size as u64;
        let mut d = vec![0u8; desc_size];
        read_at(gdt_off, &mut d);
        let inode_table_block = le32(&d, 8) as u64;
        Some(Self {
            block_size,
            inode_size,
            inodes_per_group,
            inode_table_block,
        })
    }

    fn read_block(&self, block: u64) -> Vec<u8> {
        let mut b = vec![0u8; self.block_size];
        read_at(block * self.block_size as u64, &mut b);
        b
    }

    pub fn inode(&self, ino: u32) -> Option<Inode> {
        if ino == 0 {
            return None;
        }
        let idx = (ino - 1) % self.inodes_per_group;
        let off = self.inode_table_block * self.block_size as u64
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

    fn map_extent(&self, node: &[u8], logical: u32) -> Option<u64> {
        if node.len() < 12 || le16(node, 0) != EXTENT_MAGIC {
            return None;
        }
        let entries = le16(node, 4) as usize;
        let depth = le16(node, 6) as usize;
        if depth == 0 {
            for i in 0..entries {
                let e = &node[12 + i * 12..12 + i * 12 + 12];
                let ee_block = le32(e, 0);
                let raw_len = le16(e, 4);
                let start = ((le16(e, 6) as u64) << 32) | le32(e, 8) as u64;
                let len = if raw_len > 0x8000 { (raw_len - 0x8000) } else { raw_len } as u32;
                if logical >= ee_block && logical < ee_block + len.max(1) {
                    return Some(start + (logical - ee_block) as u64);
                }
            }
            None
        } else {
            let mut chosen: Option<u64> = None;
            for i in 0..entries {
                let e = &node[12 + i * 12..12 + i * 12 + 12];
                let ei_block = le32(e, 0);
                let leaf = ((le16(e, 8) as u64) << 32) | le32(e, 4) as u64;
                if logical >= ei_block {
                    chosen = Some(leaf);
                }
            }
            let leaf = chosen?;
            let blk = self.read_block(leaf);
            self.map_extent(&blk, logical)
        }
    }

    pub fn map_block(&self, inode: &Inode, logical: u32) -> Option<u64> {
        if inode.flags & EXT4_EXTENTS_FL != 0 {
            self.map_extent(&inode.blocks, logical)
        } else {
            // 直接块指针表（小文件）
            let idx = logical as usize;
            if idx < 12 {
                let b = le32(&inode.blocks, idx * 4);
                if b == 0 { None } else { Some(b as u64) }
            } else {
                None
            }
        }
    }

    pub fn read_file(&self, inode: &Inode) -> Vec<u8> {
        let mut out = Vec::new();
        let nblocks = (inode.size as usize + self.block_size - 1) / self.block_size;
        for lb in 0..nblocks {
            match self.map_block(inode, lb as u32) {
                Some(pb) => {
                    let blk = self.read_block(pb);
                    out.extend_from_slice(&blk);
                }
                // 稀疏文件的“洞”（extent 没覆盖的逻辑块）按 POSIX 语义读作全零。
                // 以前这里 break，会把洞当成文件结尾，导致 ELF 被截断、exec 时 panic
                // （mkfs.ext4 -d / cp 默认都会把全零块做成洞）。
                None => out.resize(out.len() + self.block_size, 0),
            }
        }
        out.truncate(inode.size as usize);
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
        if let Some(f) = fs.lookup("/big.bin") {
            let data = fs.read_file(&f);
            let sum: u32 = data.iter().fold(0u32, |a, b| a.wrapping_add(*b as u32));
            println!("[ext4] /big.bin len={} sum={}", data.len(), sum);
        }
        if let Some(d) = fs.lookup("/testdir") {
            for (name, ino, ftype) in fs.list_dir(&d) {
                println!("[ext4]   testdir/{} ino={} type={}", name, ino, ftype);
            }
        }
        if let Some(f) = fs.lookup("/hello.txt") {
            let data = fs.read_file(&f);
            let s = core::str::from_utf8(&data).unwrap_or("<binary>");
            println!("[ext4] /hello.txt = {:?}", s);
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
