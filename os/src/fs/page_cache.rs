use crate::mm::{frame_alloc, FrameTracker};
use crate::sync::UPSafeCell;
use alloc::{collections::BTreeMap, sync::Arc};

pub(super) const CAPACITY: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FilePageKey {
    pub fs: usize,
    pub inode: u32,
    pub index: u64,
}

struct Entry {
    frame: Arc<FrameTracker>,
    used_at: u64,
}

#[derive(Default)]
struct PageCache {
    entries: BTreeMap<FilePageKey, Entry>,
    clock: u64,
}

static CACHE: UPSafeCell<Option<PageCache>> = unsafe { UPSafeCell::new(None) };

impl PageCache {
    fn get(&mut self, key: FilePageKey) -> Option<Arc<FrameTracker>> {
        let entry = self.entries.get_mut(&key)?;
        self.clock = self.clock.saturating_add(1);
        entry.used_at = self.clock;
        Some(entry.frame.clone())
    }

    fn evict_one(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.frame) == 1)
            .min_by_key(|(_, entry)| entry.used_at)
            .map(|(key, _)| *key);
        victim.and_then(|key| self.entries.remove(&key)).is_some()
    }

    fn admit(&mut self, key: FilePageKey, frame: Arc<FrameTracker>) -> Arc<FrameTracker> {
        if let Some(existing) = self.get(key) {
            return existing;
        }
        if self.entries.len() < CAPACITY || self.evict_one() {
            self.clock = self.clock.saturating_add(1);
            self.entries.insert(
                key,
                Entry {
                    frame: frame.clone(),
                    used_at: self.clock,
                },
            );
        }
        // All entries can be pinned by active mappings. Serve the request without growing the cache.
        frame
    }
}

pub(super) fn get_or_load(
    key: FilePageKey,
    load: impl FnOnce(&mut [u8]),
) -> Option<Arc<FrameTracker>> {
    let hit = CACHE
        .exclusive_access()
        .as_mut()
        .and_then(|cache| cache.get(key));
    if hit.is_some() {
        return hit;
    }

    // The allocator may reclaim a cached frame. Release the cache borrow before allocating or doing I/O.
    let frame = Arc::new(frame_alloc()?);
    load(frame.ppn.get_bytes_array());
    Some(
        CACHE
            .exclusive_access()
            .get_or_insert_with(PageCache::default)
            .admit(key, frame),
    )
}

pub(crate) fn reclaim_one() -> bool {
    CACHE.try_exclusive_access().map_or(false, |mut cache| {
        cache.as_mut().map_or(false, PageCache::evict_one)
    })
}

pub(super) fn discard_fs(fs: usize) {
    if let Some(cache) = CACHE.exclusive_access().as_mut() {
        cache.entries.retain(|key, _| key.fs != fs);
    }
}
