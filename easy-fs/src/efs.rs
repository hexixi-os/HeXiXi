use super::{
    block_cache_sync_device, get_block_cache, Bitmap, BlockDevice, DiskInode, DiskInodeType,
    FsError, Inode, SuperBlock, DIRENT_SZ, MAX_FILE_SIZE,
};
use crate::BLOCK_SZ;
use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use spin::Mutex;

const BITMAP_BLOCK_BITS: u64 = BLOCK_SZ as u64 * 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpaceUsage {
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u64,
    pub free_inodes: u64,
}

pub struct EasyFileSystem {
    pub block_device: Arc<dyn BlockDevice>,
    pub inode_bitmap: Bitmap,
    pub data_bitmap: Bitmap,
    inode_area_start_block: u32,
    data_area_start_block: u32,
    data_area_blocks: u32,
    pub(crate) orphaned: BTreeSet<u32>,
}

type DataBlock = [u8; BLOCK_SZ];

impl EasyFileSystem {
    pub fn create(
        block_device: Arc<dyn BlockDevice>,
        total_blocks: u32,
        inode_bitmap_blocks: u32,
    ) -> Arc<Mutex<Self>> {
        // calculate block size of areas & create bitmaps
        let inode_bitmap = Bitmap::new(1, inode_bitmap_blocks as usize);
        let inode_num = inode_bitmap.maximum();
        let inode_area_blocks =
            ((inode_num * core::mem::size_of::<DiskInode>() + BLOCK_SZ - 1) / BLOCK_SZ) as u32;
        let inode_total_blocks = inode_bitmap_blocks + inode_area_blocks;
        let data_total_blocks = total_blocks - 1 - inode_total_blocks;
        let data_bitmap_blocks = (data_total_blocks + 4096) / 4097;
        let data_area_blocks = data_total_blocks - data_bitmap_blocks;
        let data_bitmap = Bitmap::new(
            (1 + inode_bitmap_blocks + inode_area_blocks) as usize,
            data_bitmap_blocks as usize,
        );
        let mut efs = Self {
            block_device: Arc::clone(&block_device),
            inode_bitmap,
            data_bitmap,
            inode_area_start_block: 1 + inode_bitmap_blocks,
            data_area_start_block: 1 + inode_total_blocks + data_bitmap_blocks,
            data_area_blocks,
            orphaned: BTreeSet::new(),
        };
        // clear all blocks
        for i in 0..total_blocks {
            get_block_cache(i as usize, Arc::clone(&block_device))
                .lock()
                .modify(0, |data_block: &mut DataBlock| {
                    for byte in data_block.iter_mut() {
                        *byte = 0;
                    }
                });
        }
        // initialize SuperBlock
        get_block_cache(0, Arc::clone(&block_device)).lock().modify(
            0,
            |super_block: &mut SuperBlock| {
                super_block.initialize(
                    total_blocks,
                    inode_bitmap_blocks,
                    inode_area_blocks,
                    data_bitmap_blocks,
                    data_area_blocks,
                );
            },
        );
        // write back immediately
        // create a inode for root node "/"
        assert_eq!(efs.alloc_inode(), 0);
        let (root_inode_block_id, root_inode_offset) = efs.get_disk_inode_pos(0);
        get_block_cache(root_inode_block_id as usize, Arc::clone(&block_device))
            .lock()
            .modify(root_inode_offset, |disk_inode: &mut DiskInode| {
                disk_inode.initialize(DiskInodeType::Directory);
            });
        block_cache_sync_device(&block_device);
        Arc::new(Mutex::new(efs))
    }

    pub fn open(block_device: Arc<dyn BlockDevice>) -> Arc<Mutex<Self>> {
        Self::try_open(block_device, u64::MAX).expect("Error loading EFS!")
    }

    pub fn try_open(
        block_device: Arc<dyn BlockDevice>,
        sectors: u64,
    ) -> Result<Arc<Mutex<Self>>, FsError> {
        if sectors == 0 {
            return Err(FsError::Invalid);
        }
        let header = get_block_cache(0, Arc::clone(&block_device))
            .lock()
            .read(0, |header: &SuperBlock| *header);
        let inode_bitmap_end = 1 + u64::from(header.inode_bitmap_blocks);
        let inode_area_end = inode_bitmap_end + u64::from(header.inode_area_blocks);
        let data_bitmap_end = inode_area_end + u64::from(header.data_bitmap_blocks);
        let data_area_end = data_bitmap_end + u64::from(header.data_area_blocks);
        let inode_count = u64::from(header.inode_bitmap_blocks) * BITMAP_BLOCK_BITS;
        let inodes_per_block = (BLOCK_SZ / core::mem::size_of::<DiskInode>()) as u64;
        if !header.is_valid()
            || inode_count == 0
            || inode_count > u64::from(u32::MAX)
            || u64::from(header.inode_area_blocks) * inodes_per_block != inode_count
            || u64::from(header.data_bitmap_blocks) * BITMAP_BLOCK_BITS
                < u64::from(header.data_area_blocks)
            || data_area_end != u64::from(header.total_blocks)
            || data_area_end > sectors
        {
            return Err(FsError::Invalid);
        }
        let fs = Self {
            block_device,
            inode_bitmap: Bitmap::new(1, header.inode_bitmap_blocks as usize),
            data_bitmap: Bitmap::new(inode_area_end as usize, header.data_bitmap_blocks as usize),
            inode_area_start_block: inode_bitmap_end as u32,
            data_area_start_block: data_bitmap_end as u32,
            data_area_blocks: header.data_area_blocks,
            orphaned: BTreeSet::new(),
        };
        if !fs.inode_bitmap.is_set(&fs.block_device, 0) {
            return Err(FsError::Invalid);
        }
        let root = fs.read_disk_inode(0);
        if !root.is_dir()
            || root.size as usize > MAX_FILE_SIZE
            || root.size as usize % DIRENT_SZ != 0
            || !root.has_valid_blocks(
                fs.data_area_start_block..header.total_blocks,
                &fs.block_device,
            )
        {
            return Err(FsError::Invalid);
        }
        Ok(Arc::new(Mutex::new(fs)))
    }

    pub fn space_usage(&self) -> SpaceUsage {
        let blocks = self.data_area_blocks as usize;
        let inodes = self.inode_bitmap.maximum();
        SpaceUsage {
            blocks: blocks as u64,
            free_blocks: (blocks - self.data_bitmap.used(&self.block_device, blocks)) as u64,
            inodes: inodes as u64,
            free_inodes: (inodes - self.inode_bitmap.used(&self.block_device, inodes)) as u64,
        }
    }

    pub fn root_inode(efs: &Arc<Mutex<Self>>) -> Inode {
        Inode::new(0, Arc::clone(efs))
    }

    pub fn get_disk_inode_pos(&self, inode_id: u32) -> (u32, usize) {
        let inode_size = core::mem::size_of::<DiskInode>();
        let inodes_per_block = (BLOCK_SZ / inode_size) as u32;
        let block_id = self.inode_area_start_block + inode_id / inodes_per_block;
        (
            block_id,
            (inode_id % inodes_per_block) as usize * inode_size,
        )
    }

    pub fn get_data_block_id(&self, data_block_id: u32) -> u32 {
        self.data_area_start_block + data_block_id
    }

    pub fn alloc_inode(&mut self) -> u32 {
        self.try_alloc_inode().unwrap()
    }

    /// Return a block ID not ID in the data area.
    pub fn alloc_data(&mut self) -> u32 {
        self.try_alloc_data().unwrap()
    }

    pub(crate) fn try_alloc_inode(&mut self) -> Result<u32, FsError> {
        self.inode_bitmap
            .alloc(&self.block_device, self.inode_bitmap.maximum())
            .map(|id| id as u32)
            .ok_or(FsError::NoSpace)
    }

    pub(crate) fn try_alloc_data(&mut self) -> Result<u32, FsError> {
        self.data_bitmap
            .alloc(&self.block_device, self.data_area_blocks as usize)
            .map(|id| self.get_data_block_id(id as u32))
            .ok_or(FsError::NoSpace)
    }

    pub(crate) fn dealloc_inode(&mut self, id: u32) {
        self.inode_bitmap.dealloc(&self.block_device, id as usize);
    }

    pub(crate) fn has_inode(&self, id: u32) -> bool {
        self.inode_bitmap.is_set(&self.block_device, id as usize)
    }

    pub(crate) fn read_disk_inode(&self, id: u32) -> DiskInode {
        let (block, offset) = self.get_disk_inode_pos(id);
        get_block_cache(block as usize, Arc::clone(&self.block_device))
            .lock()
            .read(offset, |inode: &DiskInode| *inode)
    }

    pub(crate) fn write_disk_inode(&self, id: u32, inode: DiskInode) {
        let (block, offset) = self.get_disk_inode_pos(id);
        get_block_cache(block as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(offset, |current: &mut DiskInode| *current = inode);
    }

    pub fn dealloc_data(&mut self, block_id: u32) {
        get_block_cache(block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(0, |data_block: &mut DataBlock| {
                data_block.iter_mut().for_each(|p| {
                    *p = 0;
                })
            });
        self.data_bitmap.dealloc(
            &self.block_device,
            (block_id - self.data_area_start_block) as usize,
        )
    }
}
