use super::{BlockDevice, BLOCK_SZ};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;
use spin::Mutex;

#[repr(align(8))]
struct AlignedBlock([u8; BLOCK_SZ]);

pub struct BlockCache {
    cache: Box<AlignedBlock>,
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
    modified: bool,
}

impl BlockCache {
    /// Load a new BlockCache from disk.
    pub fn new(block_id: usize, block_device: Arc<dyn BlockDevice>) -> Self {
        let mut cache = Box::new(AlignedBlock([0; BLOCK_SZ]));
        block_device.read_block(block_id, &mut cache.0);
        Self {
            cache,
            block_id,
            block_device,
            modified: false,
        }
    }

    pub fn get_ref<T>(&self, offset: usize) -> &T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        assert_eq!(offset % core::mem::align_of::<T>(), 0);
        unsafe { &*self.cache.0.as_ptr().add(offset).cast::<T>() }
    }

    pub fn get_mut<T>(&mut self, offset: usize) -> &mut T
    where
        T: Sized,
    {
        let type_size = core::mem::size_of::<T>();
        assert!(offset + type_size <= BLOCK_SZ);
        assert_eq!(offset % core::mem::align_of::<T>(), 0);
        self.modified = true;
        unsafe { &mut *self.cache.0.as_mut_ptr().add(offset).cast::<T>() }
    }

    pub fn read<T, V>(&self, offset: usize, f: impl FnOnce(&T) -> V) -> V {
        f(self.get_ref(offset))
    }

    pub fn modify<T, V>(&mut self, offset: usize, f: impl FnOnce(&mut T) -> V) -> V {
        f(self.get_mut(offset))
    }

    pub fn sync(&mut self) {
        if self.modified {
            self.modified = false;
            self.block_device.write_block(self.block_id, &self.cache.0);
        }
    }
}

impl Drop for BlockCache {
    fn drop(&mut self) {
        self.sync()
    }
}

const BLOCK_CACHE_SIZE: usize = 16;

#[derive(PartialEq, Eq)]
struct BlockKey {
    device: usize,
    block: usize,
}

fn device_key(device: &Arc<dyn BlockDevice>) -> usize {
    Arc::as_ptr(device) as *const () as usize
}

pub struct BlockCacheManager {
    queue: VecDeque<(BlockKey, Arc<Mutex<BlockCache>>)>,
}

impl BlockCacheManager {
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    pub fn get_block_cache(
        &mut self,
        block_id: usize,
        block_device: Arc<dyn BlockDevice>,
    ) -> Arc<Mutex<BlockCache>> {
        let key = BlockKey {
            device: device_key(&block_device),
            block: block_id,
        };
        if let Some((_, cache)) = self.queue.iter().find(|(current, _)| *current == key) {
            return Arc::clone(cache);
        }
        while self.queue.len() >= BLOCK_CACHE_SIZE {
            if let Some(index) = self
                .queue
                .iter()
                .position(|(_, cache)| Arc::strong_count(cache) == 1)
            {
                self.queue.remove(index);
            } else {
                // A flush can temporarily hold references to every cached block.
                break;
            }
        }
        let cache = Arc::new(Mutex::new(BlockCache::new(block_id, block_device)));
        self.queue.push_back((key, Arc::clone(&cache)));
        cache
    }
}

lazy_static! {
    pub static ref BLOCK_CACHE_MANAGER: Mutex<BlockCacheManager> =
        Mutex::new(BlockCacheManager::new());
}

pub fn get_block_cache(
    block_id: usize,
    block_device: Arc<dyn BlockDevice>,
) -> Arc<Mutex<BlockCache>> {
    BLOCK_CACHE_MANAGER
        .lock()
        .get_block_cache(block_id, block_device)
}

pub fn block_cache_sync_device(device: &Arc<dyn BlockDevice>) {
    let device = device_key(device);
    // Release the manager lock before waiting for any block lock.
    let caches: Vec<_> = BLOCK_CACHE_MANAGER
        .lock()
        .queue
        .iter()
        .filter(|(key, _)| key.device == device)
        .map(|(_, cache)| Arc::clone(cache))
        .collect();
    for cache in caches {
        cache.lock().sync();
    }
}
