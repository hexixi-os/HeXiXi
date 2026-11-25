use super::{get_block_cache, BlockDevice, BLOCK_SZ};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{Debug, Formatter, Result};
use core::ops::Range;

const EFS_MAGIC: u32 = 0x3b800001;
const INODE_DIRECT_COUNT: usize = 28;
pub const NAME_LENGTH_LIMIT: usize = 27;
const INODE_INDIRECT1_COUNT: usize = BLOCK_SZ / 4;
const INODE_INDIRECT2_COUNT: usize = INODE_INDIRECT1_COUNT * INODE_INDIRECT1_COUNT;
const DIRECT_BOUND: usize = INODE_DIRECT_COUNT;
const INDIRECT1_BOUND: usize = DIRECT_BOUND + INODE_INDIRECT1_COUNT;
const INDIRECT2_BOUND: usize = INDIRECT1_BOUND + INODE_INDIRECT2_COUNT;
pub const MAX_FILE_SIZE: usize = INDIRECT2_BOUND * BLOCK_SZ;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SuperBlock {
    magic: u32,
    pub total_blocks: u32,
    pub inode_bitmap_blocks: u32,
    pub inode_area_blocks: u32,
    pub data_bitmap_blocks: u32,
    pub data_area_blocks: u32,
}

impl Debug for SuperBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result {
        f.debug_struct("SuperBlock")
            .field("total_blocks", &self.total_blocks)
            .field("inode_bitmap_blocks", &self.inode_bitmap_blocks)
            .field("inode_area_blocks", &self.inode_area_blocks)
            .field("data_bitmap_blocks", &self.data_bitmap_blocks)
            .field("data_area_blocks", &self.data_area_blocks)
            .finish()
    }
}

impl SuperBlock {
    pub fn initialize(
        &mut self,
        total_blocks: u32,
        inode_bitmap_blocks: u32,
        inode_area_blocks: u32,
        data_bitmap_blocks: u32,
        data_area_blocks: u32,
    ) {
        *self = Self {
            magic: EFS_MAGIC,
            total_blocks,
            inode_bitmap_blocks,
            inode_area_blocks,
            data_bitmap_blocks,
            data_area_blocks,
        }
    }
    pub fn is_valid(&self) -> bool {
        self.magic == EFS_MAGIC
    }
}

#[repr(u8)]
pub enum DiskInodeType {
    File,
    Directory,
}

type IndirectBlock = [u32; BLOCK_SZ / 4];
type DataBlock = [u8; BLOCK_SZ];

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DiskInode {
    pub size: u32,
    pub direct: [u32; INODE_DIRECT_COUNT],
    pub indirect1: u32,
    pub indirect2: u32,
    type_: u8,
}

impl DiskInode {
    /// indirect1 and indirect2 block are allocated only when they are needed.
    pub fn initialize(&mut self, type_: DiskInodeType) {
        self.size = 0;
        self.direct.iter_mut().for_each(|v| *v = 0);
        self.indirect1 = 0;
        self.indirect2 = 0;
        self.type_ = type_ as u8;
    }
    pub fn is_dir(&self) -> bool {
        self.type_ == DiskInodeType::Directory as u8
    }
    pub fn is_file(&self) -> bool {
        self.type_ == DiskInodeType::File as u8
    }

    pub fn has_valid_blocks(
        &self,
        data_area: Range<u32>,
        block_device: &Arc<dyn BlockDevice>,
    ) -> bool {
        let blocks = self.data_blocks() as usize;
        let valid_entries = |entries: &[u32]| entries.iter().all(|id| data_area.contains(id));
        if !valid_entries(&self.direct[..blocks.min(INODE_DIRECT_COUNT)]) {
            return false;
        }
        if blocks > INODE_DIRECT_COUNT {
            if !data_area.contains(&self.indirect1) {
                return false;
            }
            let entries = (blocks - INODE_DIRECT_COUNT).min(INODE_INDIRECT1_COUNT);
            let valid = get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |block: &IndirectBlock| valid_entries(&block[..entries]));
            if !valid {
                return false;
            }
        }
        if blocks > INDIRECT1_BOUND {
            if !data_area.contains(&self.indirect2) {
                return false;
            }
            let indirect2: IndirectBlock =
                get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
                    .lock()
                    .read(0, |block: &IndirectBlock| *block);
            let mut remaining = blocks - INDIRECT1_BOUND;
            for block_id in indirect2
                .iter()
                .take((remaining + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT)
            {
                if !data_area.contains(block_id) {
                    return false;
                }
                let entries = remaining.min(INODE_INDIRECT1_COUNT);
                let valid = get_block_cache(*block_id as usize, Arc::clone(block_device))
                    .lock()
                    .read(0, |block: &IndirectBlock| valid_entries(&block[..entries]));
                if !valid {
                    return false;
                }
                remaining -= entries;
            }
        }
        true
    }
    /// Return block number correspond to size.
    pub fn data_blocks(&self) -> u32 {
        Self::_data_blocks(self.size)
    }
    fn _data_blocks(size: u32) -> u32 {
        size / BLOCK_SZ as u32 + u32::from(size % BLOCK_SZ as u32 != 0)
    }
    /// Return number of blocks needed include indirect1/2.
    pub fn total_blocks(size: u32) -> u32 {
        let data_blocks = Self::_data_blocks(size) as usize;
        let mut total = data_blocks as usize;
        // indirect1
        if data_blocks > INODE_DIRECT_COUNT {
            total += 1;
        }
        // indirect2
        if data_blocks > INDIRECT1_BOUND {
            total += 1;
            // sub indirect1
            total +=
                (data_blocks - INDIRECT1_BOUND + INODE_INDIRECT1_COUNT - 1) / INODE_INDIRECT1_COUNT;
        }
        total as u32
    }
    pub fn blocks_num_needed(&self, new_size: u32) -> u32 {
        assert!(new_size >= self.size);
        Self::total_blocks(new_size) - Self::total_blocks(self.size)
    }
    pub fn get_block_id(&self, inner_id: u32, block_device: &Arc<dyn BlockDevice>) -> u32 {
        let inner_id = inner_id as usize;
        if inner_id < INODE_DIRECT_COUNT {
            self.direct[inner_id]
        } else if inner_id < INDIRECT1_BOUND {
            get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect_block: &IndirectBlock| {
                    indirect_block[inner_id - INODE_DIRECT_COUNT]
                })
        } else {
            let last = inner_id - INDIRECT1_BOUND;
            let indirect1 = get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect2: &IndirectBlock| {
                    indirect2[last / INODE_INDIRECT1_COUNT]
                });
            get_block_cache(indirect1 as usize, Arc::clone(block_device))
                .lock()
                .read(0, |indirect1: &IndirectBlock| {
                    indirect1[last % INODE_INDIRECT1_COUNT]
                })
        }
    }
    pub fn increase_size(
        &mut self,
        new_size: u32,
        new_blocks: Vec<u32>,
        block_device: &Arc<dyn BlockDevice>,
    ) {
        let mut current_blocks = self.data_blocks();
        self.size = new_size;
        let mut total_blocks = self.data_blocks();
        let mut new_blocks = new_blocks.into_iter();
        // fill direct
        while current_blocks < total_blocks.min(INODE_DIRECT_COUNT as u32) {
            self.direct[current_blocks as usize] = new_blocks.next().unwrap();
            current_blocks += 1;
        }
        // alloc indirect1
        if total_blocks > INODE_DIRECT_COUNT as u32 {
            if current_blocks == INODE_DIRECT_COUNT as u32 {
                self.indirect1 = new_blocks.next().unwrap();
            }
            current_blocks -= INODE_DIRECT_COUNT as u32;
            total_blocks -= INODE_DIRECT_COUNT as u32;
        } else {
            return;
        }
        // fill indirect1
        get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect1: &mut IndirectBlock| {
                while current_blocks < total_blocks.min(INODE_INDIRECT1_COUNT as u32) {
                    indirect1[current_blocks as usize] = new_blocks.next().unwrap();
                    current_blocks += 1;
                }
            });
        // alloc indirect2
        if total_blocks > INODE_INDIRECT1_COUNT as u32 {
            if current_blocks == INODE_INDIRECT1_COUNT as u32 {
                self.indirect2 = new_blocks.next().unwrap();
            }
            current_blocks -= INODE_INDIRECT1_COUNT as u32;
            total_blocks -= INODE_INDIRECT1_COUNT as u32;
        } else {
            return;
        }
        // fill indirect2 from (a0, b0) -> (a1, b1)
        let mut a0 = current_blocks as usize / INODE_INDIRECT1_COUNT;
        let mut b0 = current_blocks as usize % INODE_INDIRECT1_COUNT;
        let a1 = total_blocks as usize / INODE_INDIRECT1_COUNT;
        let b1 = total_blocks as usize % INODE_INDIRECT1_COUNT;
        // alloc low-level indirect1
        get_block_cache(self.indirect2 as usize, Arc::clone(block_device))
            .lock()
            .modify(0, |indirect2: &mut IndirectBlock| {
                while (a0 < a1) || (a0 == a1 && b0 < b1) {
                    if b0 == 0 {
                        indirect2[a0] = new_blocks.next().unwrap();
                    }
                    // fill current
                    get_block_cache(indirect2[a0] as usize, Arc::clone(block_device))
                        .lock()
                        .modify(0, |indirect1: &mut IndirectBlock| {
                            indirect1[b0] = new_blocks.next().unwrap();
                        });
                    // move to next
                    b0 += 1;
                    if b0 == INODE_INDIRECT1_COUNT {
                        b0 = 0;
                        a0 += 1;
                    }
                }
            });
    }

    /// Shrink the inode and return data and index blocks that are no longer referenced.
    pub fn shrink_size(&mut self, size: u32, block_device: &Arc<dyn BlockDevice>) -> Vec<u32> {
        use core::mem::replace;

        let old_blocks = self.data_blocks() as usize;
        let new_blocks = Self::_data_blocks(size) as usize;
        let mut freed = Vec::new();
        for index in (new_blocks..old_blocks).rev() {
            if index < INODE_DIRECT_COUNT {
                freed.push(replace(&mut self.direct[index], 0));
            } else if index < INDIRECT1_BOUND {
                let slot = index - INODE_DIRECT_COUNT;
                let data = get_block_cache(self.indirect1 as usize, Arc::clone(block_device))
                    .lock()
                    .modify(0, |block: &mut IndirectBlock| replace(&mut block[slot], 0));
                freed.push(data);
                if slot == 0 {
                    freed.push(replace(&mut self.indirect1, 0));
                }
            } else {
                let relative = index - INDIRECT1_BOUND;
                let outer = relative / INODE_INDIRECT1_COUNT;
                let inner = relative % INODE_INDIRECT1_COUNT;
                let table = get_block_cache(self.indirect2 as usize, Arc::clone(block_device));
                let leaf = if inner == 0 {
                    table
                        .lock()
                        .modify(0, |block: &mut IndirectBlock| replace(&mut block[outer], 0))
                } else {
                    table.lock().read(0, |block: &IndirectBlock| block[outer])
                };
                let data = get_block_cache(leaf as usize, Arc::clone(block_device))
                    .lock()
                    .modify(0, |block: &mut IndirectBlock| replace(&mut block[inner], 0));
                freed.push(data);
                if inner == 0 {
                    freed.push(leaf);
                    if outer == 0 {
                        freed.push(replace(&mut self.indirect2, 0));
                    }
                }
            }
        }

        let tail = size as usize % BLOCK_SZ;
        if tail != 0 {
            let block = self.get_block_id((new_blocks - 1) as u32, block_device);
            get_block_cache(block as usize, Arc::clone(block_device))
                .lock()
                .modify(0, |block: &mut DataBlock| block[tail..].fill(0));
        }
        self.size = size;
        freed
    }

    pub fn read_at(
        &self,
        offset: usize,
        buf: &mut [u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {
        let mut start = offset;
        let end = offset.saturating_add(buf.len()).min(self.size as usize);
        if start >= end {
            return 0;
        }
        let mut start_block = start / BLOCK_SZ;
        let mut read_size = 0usize;
        loop {
            // calculate end of current block
            let mut end_current_block = (start / BLOCK_SZ + 1) * BLOCK_SZ;
            end_current_block = end_current_block.min(end);
            // read and update read size
            let block_read_size = end_current_block - start;
            let dst = &mut buf[read_size..read_size + block_read_size];
            get_block_cache(
                self.get_block_id(start_block as u32, block_device) as usize,
                Arc::clone(block_device),
            )
            .lock()
            .read(0, |data_block: &DataBlock| {
                let src = &data_block[start % BLOCK_SZ..start % BLOCK_SZ + block_read_size];
                dst.copy_from_slice(src);
            });
            read_size += block_read_size;
            // move to next block
            if end_current_block == end {
                break;
            }
            start_block += 1;
            start = end_current_block;
        }
        read_size
    }
    /// File size must be adjusted before.
    pub fn write_at(
        &mut self,
        offset: usize,
        buf: &[u8],
        block_device: &Arc<dyn BlockDevice>,
    ) -> usize {
        let mut start = offset;
        let end = offset.saturating_add(buf.len()).min(self.size as usize);
        if start >= end {
            return 0;
        }
        let mut start_block = start / BLOCK_SZ;
        let mut write_size = 0usize;
        loop {
            // calculate end of current block
            let mut end_current_block = (start / BLOCK_SZ + 1) * BLOCK_SZ;
            end_current_block = end_current_block.min(end);
            // write and update write size
            let block_write_size = end_current_block - start;
            get_block_cache(
                self.get_block_id(start_block as u32, block_device) as usize,
                Arc::clone(block_device),
            )
            .lock()
            .modify(0, |data_block: &mut DataBlock| {
                let src = &buf[write_size..write_size + block_write_size];
                let dst = &mut data_block[start % BLOCK_SZ..start % BLOCK_SZ + block_write_size];
                dst.copy_from_slice(src);
            });
            write_size += block_write_size;
            // move to next block
            if end_current_block == end {
                break;
            }
            start_block += 1;
            start = end_current_block;
        }
        write_size
    }
}

#[repr(C)]
pub struct DirEntry {
    name: [u8; NAME_LENGTH_LIMIT + 1],
    inode_number: u32,
}

pub const DIRENT_SZ: usize = 32;

impl DirEntry {
    pub fn empty() -> Self {
        Self {
            name: [0u8; NAME_LENGTH_LIMIT + 1],
            inode_number: 0,
        }
    }
    pub fn new(name: &str, inode_number: u32) -> Self {
        let mut bytes = [0u8; NAME_LENGTH_LIMIT + 1];
        bytes[..name.len()].copy_from_slice(name.as_bytes());
        Self {
            name: bytes,
            inode_number,
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), DIRENT_SZ) }
    }
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut((self as *mut Self).cast::<u8>(), DIRENT_SZ) }
    }
    pub fn name(&self) -> Option<&str> {
        let len = self.name.iter().position(|&byte| byte == 0)?;
        core::str::from_utf8(&self.name[..len]).ok()
    }
    pub fn inode_number(&self) -> u32 {
        self.inode_number
    }
}
