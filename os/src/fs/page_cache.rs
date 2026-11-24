use crate::config::PAGE_SIZE;
use crate::mm::{frame_alloc, FrameTracker};
use crate::sync::UPSafeCell;
use alloc::{
    collections::{BTreeMap, VecDeque},
    sync::Arc,
};

pub(super) const CAPACITY: usize = 256;
const HISTORY_LIMIT: usize = CAPACITY * 2;
const SCAN_PAGES: usize = 8;

#[cfg(all(feature = "page_cache_none", feature = "page_cache_adaptive"))]
compile_error!("select at most one optional page cache policy");

#[derive(PartialEq, Eq)]
enum Policy {
    None,
    Lru,
    Adaptive,
}

const POLICY: Policy = if cfg!(feature = "page_cache_none") {
    Policy::None
} else if cfg!(feature = "page_cache_adaptive") {
    Policy::Adaptive
} else {
    Policy::Lru
};

pub(super) fn policy_name() -> &'static str {
    match POLICY {
        Policy::None => "none",
        Policy::Lru => "lru",
        Policy::Adaptive => "adaptive",
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct FilePageKey {
    pub fs: usize,
    pub inode: u32,
    pub index: u64,
}

#[derive(Clone, Copy)]
pub(super) enum Access {
    Fault,
    Read {
        sequential: bool,
        continuing_page: bool,
    },
}

impl Access {
    pub(super) fn read() -> Self {
        Self::Read {
            sequential: false,
            continuing_page: false,
        }
    }

    fn is_sequential(self) -> bool {
        matches!(
            self,
            Self::Read {
                sequential: true,
                ..
            }
        )
    }

    fn continues_page(self) -> bool {
        matches!(
            self,
            Self::Read {
                continuing_page: true,
                ..
            }
        )
    }
}

#[derive(Default)]
pub(super) struct ReadStream {
    last: Option<(u64, u64)>, // page index and exclusive ending byte offset
    pages: usize,
}

impl ReadStream {
    pub(super) fn access(&mut self, offset: u64, length: usize) -> Access {
        if POLICY != Policy::Adaptive {
            return Access::read();
        }
        let page = offset / PAGE_SIZE as u64;
        let continuing_page = match self.last {
            Some((prior, end)) if end == offset => {
                if page != prior {
                    self.pages = self.pages.saturating_add(1);
                }
                page == prior
            }
            _ => {
                self.pages = 1;
                false
            }
        };
        self.last = Some((page, offset + length as u64));
        Access::Read {
            sequential: self.pages >= SCAN_PAGES,
            continuing_page,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct CacheStats {
    pub hits: usize,
    pub misses: usize,
    pub evictions: usize,
    pub bypasses: usize,
    pub refaults: usize,
    pub resident_pages: usize,
    pub pinned_pages: usize,
    pub history_entries: usize,
}

struct Entry {
    frame: Arc<FrameTracker>,
    used_at: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Departure {
    Evicted,
    Bypassed,
}

/// Nonresident keys only. The queue is bounded by events, including consumed
/// entries, so a scan or repeated insertion cannot grow either collection.
#[derive(Default)]
struct History {
    entries: BTreeMap<FilePageKey, (u64, Departure)>,
    order: VecDeque<(FilePageKey, u64)>,
}

impl History {
    fn remember(&mut self, key: FilePageKey, stamp: u64, reason: Departure) {
        if self.entries.contains_key(&key) {
            return;
        }
        if self.order.len() == HISTORY_LIMIT {
            let (old, stamp) = self.order.pop_front().unwrap();
            if self.entries.get(&old).map(|entry| entry.0) == Some(stamp) {
                self.entries.remove(&old);
            }
        }
        self.entries.insert(key, (stamp, reason));
        self.order.push_back((key, stamp));
    }

    fn observe(&mut self, key: FilePageKey, access: Access) -> Option<Departure> {
        let &(_, reason) = self.entries.get(&key)?;
        if reason == Departure::Bypassed && access.continues_page() {
            // A second buffer slice from the same streamed page is not reuse.
            return None;
        }
        self.entries.remove(&key);
        Some(reason)
    }

    fn discard_fs(&mut self, fs: usize) {
        self.entries.retain(|key, _| key.fs != fs);
        self.order.retain(|(key, _)| key.fs != fs);
    }
}

#[derive(Default)]
struct PageCache {
    entries: BTreeMap<FilePageKey, Entry>,
    history: History,
    stats: CacheStats,
    clock: u64,
}

static CACHE: UPSafeCell<Option<PageCache>> = unsafe { UPSafeCell::new(None) };

impl PageCache {
    fn touch(&mut self, key: FilePageKey) -> Option<Arc<FrameTracker>> {
        let entry = self.entries.get_mut(&key)?;
        self.clock = self.clock.saturating_add(1);
        entry.used_at = self.clock;
        Some(entry.frame.clone())
    }

    fn remember(&mut self, key: FilePageKey, reason: Departure) {
        self.clock = self.clock.saturating_add(1);
        self.history.remember(key, self.clock, reason);
    }

    fn bypass(&mut self, key: FilePageKey) {
        self.stats.bypasses = self.stats.bypasses.saturating_add(1);
        if POLICY == Policy::Adaptive {
            self.remember(key, Departure::Bypassed);
        }
    }

    fn evict_one(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .filter(|(_, entry)| Arc::strong_count(&entry.frame) == 1)
            .min_by_key(|(_, entry)| entry.used_at)
            .map(|(key, _)| *key);
        match victim {
            Some(key) => {
                self.entries.remove(&key);
                self.stats.evictions = self.stats.evictions.saturating_add(1);
                self.remember(key, Departure::Evicted);
                true
            }
            None => false,
        }
    }

    fn admit(&mut self, key: FilePageKey, frame: Arc<FrameTracker>) -> Arc<FrameTracker> {
        if let Some(existing) = self.touch(key) {
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
        } else {
            // All resident frames can be held by active mappings.
            self.bypass(key);
        }
        frame
    }

    fn statistics(&self) -> CacheStats {
        CacheStats {
            resident_pages: self.entries.len(),
            pinned_pages: self
                .entries
                .values()
                .filter(|entry| Arc::strong_count(&entry.frame) > 1)
                .count(),
            history_entries: self.history.entries.len(),
            ..self.stats
        }
    }
}

pub(super) fn statistics() -> CacheStats {
    CACHE
        .exclusive_access()
        .as_ref()
        .map_or_else(CacheStats::default, PageCache::statistics)
}

pub(super) fn get_or_load(
    key: FilePageKey,
    access: Access,
    load: impl FnOnce(&mut [u8]),
) -> Option<Arc<FrameTracker>> {
    let retain = {
        let mut state = CACHE.exclusive_access();
        let cache = state.get_or_insert_with(PageCache::default);
        if let Some(hit) = cache.touch(key) {
            cache.stats.hits = cache.stats.hits.saturating_add(1);
            return Some(hit);
        }
        cache.stats.misses = cache.stats.misses.saturating_add(1);
        let reused = cache.history.observe(key, access);
        if reused == Some(Departure::Evicted) {
            cache.stats.refaults = cache.stats.refaults.saturating_add(1);
        }
        let bypass_scan = POLICY == Policy::Adaptive
            && access.is_sequential()
            && cache.entries.len() >= CAPACITY
            && reused.is_none();
        if POLICY == Policy::None || bypass_scan {
            cache.bypass(key);
            if matches!(access, Access::Read { .. }) {
                return None; // The caller reads its byte range directly.
            }
            false
        } else {
            true
        }
    };

    // Frame allocation may reclaim a cache entry. Keep its borrow outside allocation and I/O.
    let frame = match frame_alloc() {
        Some(frame) => Arc::new(frame),
        None => {
            if retain {
                CACHE.exclusive_access().as_mut().unwrap().bypass(key);
            }
            return None;
        }
    };
    load(frame.ppn.get_bytes_array());
    if retain {
        Some(CACHE.exclusive_access().as_mut().unwrap().admit(key, frame))
    } else {
        Some(frame)
    }
}

pub(crate) fn reclaim_one() -> bool {
    CACHE.try_exclusive_access().map_or(false, |mut cache| {
        cache.as_mut().map_or(false, PageCache::evict_one)
    })
}

pub(super) fn discard_fs(fs: usize) {
    if let Some(cache) = CACHE.exclusive_access().as_mut() {
        cache.entries.retain(|key, _| key.fs != fs);
        cache.history.discard_fs(fs);
    }
}
