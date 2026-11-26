//! 在宿主机上复用内核的 EXT4 读取代码，块设备由普通镜像文件提供。

#![allow(dead_code)]

extern crate alloc;

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::fs::File as KernelFile;

mod drivers {
    pub mod block {
        pub static BLOCK_DEVICE: crate::ImageDevice = crate::ImageDevice;
    }
}

mod config {
    pub const PAGE_SIZE: usize = 4096;
}

mod mm {
    use crate::config::PAGE_SIZE;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LIVE_FRAMES: AtomicUsize = AtomicUsize::new(0);

    pub struct FrameTracker {
        pub ppn: MockPage,
    }

    pub struct MockPage(usize);

    impl MockPage {
        pub fn get_bytes_array(&self) -> &'static mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.bytes_ptr(), PAGE_SIZE) }
        }

        pub(crate) fn bytes_ptr(&self) -> *mut u8 {
            self.0 as *mut u8
        }
    }

    pub fn frame_alloc() -> Option<FrameTracker> {
        let page = Box::into_raw(Box::new([0u8; PAGE_SIZE]));
        LIVE_FRAMES.fetch_add(1, Ordering::Relaxed);
        Some(FrameTracker {
            ppn: MockPage(page as usize),
        })
    }

    pub fn live_frames() -> usize {
        LIVE_FRAMES.load(Ordering::Relaxed)
    }

    impl Drop for FrameTracker {
        fn drop(&mut self) {
            unsafe {
                drop(Box::from_raw(self.ppn.0 as *mut [u8; PAGE_SIZE]));
            }
            LIVE_FRAMES.fetch_sub(1, Ordering::Relaxed);
        }
    }

    pub struct UserSlice {
        ptr: *mut u8,
        len: usize,
    }

    impl UserSlice {
        pub fn len(&self) -> usize {
            self.len
        }

        pub fn write_with(
            &self,
            _scratch: &mut Vec<u8>,
            produce: impl FnOnce(&mut [u8]) -> usize,
        ) -> usize {
            produce(unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) })
        }
    }

    pub struct UserBuffer {
        pub buffers: Vec<UserSlice>,
    }

    impl UserBuffer {
        pub fn from_slices(slices: Vec<&mut [u8]>) -> Self {
            Self {
                buffers: slices
                    .into_iter()
                    .map(|slice| UserSlice {
                        ptr: slice.as_mut_ptr(),
                        len: slice.len(),
                    })
                    .collect(),
            }
        }
    }

    impl IntoIterator for UserBuffer {
        type Item = *mut u8;
        type IntoIter = std::vec::IntoIter<*mut u8>;

        fn into_iter(self) -> Self::IntoIter {
            self.buffers
                .into_iter()
                .flat_map(|slice| (0..slice.len).map(move |i| unsafe { slice.ptr.add(i) }))
                .collect::<Vec<_>>()
                .into_iter()
        }
    }
}

mod fs {
    pub trait File {
        fn mode(&self) -> u16;
        fn owner(&self) -> (u32, u32) {
            (0, 0)
        }
        fn link_count(&self) -> u32 {
            1
        }
        fn stat_identity(&self) -> Option<(u64, u64)> {
            None
        }
        fn readable(&self) -> bool;
        fn writable(&self) -> bool;
        fn read(&self, buf: crate::mm::UserBuffer) -> usize;
        fn write(&self, buf: crate::mm::UserBuffer) -> isize;
        fn supports_positioned_io(&self) -> bool;
        fn read_positioned(&self, offset: usize, buf: crate::mm::UserBuffer) -> isize;
        fn size(&self) -> usize;
        fn mapping_size(&self) -> Option<usize>;
        fn mapping_page(
            &self,
            offset: usize,
            shared: bool,
        ) -> Option<std::sync::Arc<crate::mm::FrameTracker>>;
        fn seek(&self, offset: isize) -> isize;
        fn seek_to(&self, pos: isize) -> isize;
    }
}

#[path = "../../../os/src/fs/ext4.rs"]
mod ext4;
#[path = "../../../os/src/fs/page_cache.rs"]
mod page_cache;
#[path = "../../../os/src/sync/up.rs"]
mod sync;

static DISK: OnceLock<File> = OnceLock::new();
static SECTORS_READ: AtomicUsize = AtomicUsize::new(0);

struct ImageDevice;

impl ImageDevice {
    fn read_block(&self, block: usize, buf: &mut [u8]) {
        SECTORS_READ.fetch_add(1, Ordering::Relaxed);
        DISK.get()
            .unwrap()
            .read_exact_at(buf, block as u64 * 512)
            .unwrap();
    }
}

fn check_sequential_reads(file: &ext4::Ext4File, expected: &[u8]) {
    const REQUEST_SIZE: usize = 4096;
    const REQUESTS: usize = 8;
    let before = SECTORS_READ.load(Ordering::Relaxed);
    for chunk in expected.chunks_exact(REQUEST_SIZE).take(REQUESTS) {
        let mut data = [0; REQUEST_SIZE];
        let buf = mm::UserBuffer::from_slices(vec![&mut data]);
        assert_eq!(file.read(buf), data.len());
        assert_eq!(&data, chunk);
    }
    let sectors = SECTORS_READ.load(Ordering::Relaxed) - before;
    println!(
        "/dense: {} KiB read in {REQUESTS} requests, {sectors} sector reads",
        REQUEST_SIZE * REQUESTS / 1024
    );
    assert_eq!(sectors, REQUEST_SIZE * REQUESTS / 512);

    assert_eq!(file.seek_to(0), 0);
    let before = SECTORS_READ.load(Ordering::Relaxed);
    let mut data = [0; REQUEST_SIZE];
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![&mut data])),
        REQUEST_SIZE
    );
    assert_eq!(&data, &expected[..REQUEST_SIZE]);
    let sectors = SECTORS_READ.load(Ordering::Relaxed) - before;
    println!("/dense: one warm 4 KiB read, {sectors} sector reads");
    assert_eq!(sectors, 0);
}

fn check_read_ranges(file: &ext4::Ext4File, block_size: usize, expected: &[u8]) {
    // 从块末尾读到后一块，目标缓冲区也有不同于磁盘块边界的切分。
    let start = block_size - 13;
    let mut data = [0; 97];
    let (left, right) = data.split_at_mut(7);
    assert_eq!(file.seek_to(start as isize), start as isize);
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![left, right])),
        data.len()
    );
    assert_eq!(&data, &expected[start..start + data.len()]);

    assert!(file.supports_positioned_io());
    assert_eq!(
        file.read_positioned(0, mm::UserBuffer::from_slices(vec![&mut data])),
        data.len() as isize
    );
    assert_eq!(&data, &expected[..data.len()]);

    let mut following = [0; 16];
    let pos = start + data.len();
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![&mut following])),
        following.len()
    );
    assert_eq!(&following, &expected[pos..pos + following.len()]);

    let start = expected.len() - 10;
    let mut tail = [0xa5; 32];
    let (left, right) = tail.split_at_mut(6);
    assert_eq!(file.seek_to(start as isize), start as isize);
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![left, right])),
        10
    );
    assert_eq!(&tail[..10], &expected[start..]);
    assert_eq!(&tail[10..], &[0xa5; 22]);

    let before = SECTORS_READ.load(Ordering::Relaxed);
    assert_eq!(file.read(mm::UserBuffer::from_slices(Vec::new())), 0);
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![&mut tail])),
        0
    );
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed),
        before,
        "EOF or empty read did I/O"
    );
}

fn check_cache_sharing(fs: &Arc<ext4::Ext4Fs>, file: &ext4::Ext4File, expected: &[u8]) {
    const PAGE: usize = config::PAGE_SIZE;
    let other = ext4::Ext4File::new(fs.clone(), fs.lookup("/dense").unwrap());
    let before = SECTORS_READ.load(Ordering::Relaxed);
    let warmed_by_read = file.mapping_page(0, false).unwrap();
    assert!(Arc::ptr_eq(
        &warmed_by_read,
        &other.mapping_page(0, false).unwrap()
    ));
    assert_eq!(&warmed_by_read.ppn.get_bytes_array()[..], &expected[..PAGE]);
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed) - before,
        0,
        "read -> mmap"
    );

    let offset = 8 * PAGE;
    let before = SECTORS_READ.load(Ordering::Relaxed);
    let warmed_by_mmap = other.mapping_page(offset, false).unwrap();
    assert_eq!(SECTORS_READ.load(Ordering::Relaxed) - before, PAGE / 512);
    assert_eq!(file.seek_to(offset as isize), offset as isize);
    let mut data = [0; PAGE];
    let before = SECTORS_READ.load(Ordering::Relaxed);
    assert_eq!(
        file.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    assert_eq!(&data, warmed_by_mmap.ppn.get_bytes_array());
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed) - before,
        0,
        "mmap -> read"
    );

    let other_fs = Arc::new(ext4::Ext4Fs::new().unwrap());
    let same_inode = other_fs.lookup("/dense").unwrap();
    let separate = ext4::Ext4File::new(other_fs, same_inode)
        .mapping_page(0, false)
        .unwrap();
    assert!(!Arc::ptr_eq(&separate, &warmed_by_read));
    println!("/dense: read -> mmap 0 sector reads, mmap -> read 0 sector reads");
}

fn check_cache_bound(file: &ext4::Ext4File) {
    const PAGE: usize = config::PAGE_SIZE;
    let cap = page_cache::CAPACITY;
    for index in 0..cap {
        assert!(file.mapping_page(index * PAGE, false).is_some());
    }
    assert_eq!(mm::live_frames(), cap);
    let before = SECTORS_READ.load(Ordering::Relaxed);
    assert!(file.mapping_page(0, false).is_some());
    assert_eq!(SECTORS_READ.load(Ordering::Relaxed), before);
    assert!(file.mapping_page(cap * PAGE, false).is_some());
    assert!(file.mapping_page(0, false).is_some());
    assert_eq!(SECTORS_READ.load(Ordering::Relaxed) - before, PAGE / 512);
    assert!(file.mapping_page(PAGE, false).is_some());
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed) - before,
        2 * PAGE / 512
    );
    assert!(page_cache::statistics().refaults > 0);

    let pinned: Vec<_> = (0..cap)
        .map(|index| file.mapping_page(index * PAGE, false).unwrap())
        .collect();
    assert_eq!(mm::live_frames(), cap);
    let before_stats = page_cache::statistics();
    assert_eq!(before_stats.resident_pages, cap);
    assert_eq!(before_stats.pinned_pages, cap);
    assert!(!page_cache::reclaim_one());
    let before = SECTORS_READ.load(Ordering::Relaxed);
    for _ in 0..2 {
        let bypassed = file.mapping_page(cap * PAGE, false).unwrap();
        assert_eq!(mm::live_frames(), cap + 1);
        assert!(Arc::ptr_eq(&pinned[0], &file.mapping_page(0, false).unwrap()));
        drop(bypassed);
    }
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed) - before,
        2 * PAGE / 512
    );
    assert_eq!(page_cache::statistics().bypasses - before_stats.bypasses, 2);
    drop(pinned);
    assert_eq!(page_cache::statistics().pinned_pages, 0);
    assert!(page_cache::reclaim_one());
    assert_eq!(mm::live_frames(), cap - 1);
    assert_eq!(page_cache::statistics().resident_pages, cap - 1);
    assert_eq!(
        page_cache::statistics().evictions - before_stats.evictions,
        1
    );
    println!("/dense: {cap} cached frames, LRU / pinning / bypass / reclaim passed");
}

fn check_fragmented_scan(fs: &Arc<ext4::Ext4Fs>, expected: &[u8]) {
    const PAGE: usize = config::PAGE_SIZE;
    const FIRST: usize = 512;
    const COUNT: usize = 16;
    let file = ext4::Ext4File::new(fs.clone(), fs.lookup("/dense").unwrap());
    for index in 0..page_cache::CAPACITY {
        assert!(file.mapping_page(index * PAGE, false).is_some());
    }
    let before = page_cache::statistics();
    file.seek_to((FIRST * PAGE) as isize);
    let mut data = [0; PAGE];
    for index in FIRST..FIRST + COUNT {
        let (first, rest) = data.split_at_mut(512);
        assert_eq!(
            file.read(mm::UserBuffer::from_slices(vec![first, rest])),
            PAGE
        );
        assert_eq!(&data, &expected[index * PAGE..(index + 1) * PAGE]);
    }
    let after = page_cache::statistics();
    assert_eq!(after.bypasses - before.bypasses, 2 * (COUNT - 7));
    assert_eq!(after.resident_pages, page_cache::CAPACITY);

    let revisit = ext4::Ext4File::new(fs.clone(), fs.lookup("/dense").unwrap());
    let offset = (FIRST + COUNT - 1) * PAGE;
    revisit.seek_to(offset as isize);
    assert_eq!(
        revisit.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    let sectors = SECTORS_READ.load(Ordering::Relaxed);
    assert_eq!(
        file.mapping_page(offset, false).unwrap().ppn.get_bytes_array(),
        &data
    );
    assert_eq!(SECTORS_READ.load(Ordering::Relaxed), sectors);
    assert_eq!(page_cache::statistics().refaults, after.refaults);
    println!("/dense: fragmented scan bypasses and a new handle readmits a reused page");

    const SPLIT: usize = 900;
    let a = ext4::Ext4File::new(fs.clone(), fs.lookup("/dense").unwrap());
    let b = ext4::Ext4File::new(fs.clone(), fs.lookup("/dense").unwrap());
    a.seek_to((SPLIT * PAGE) as isize);
    for _ in 0..7 {
        assert_eq!(
            a.read(mm::UserBuffer::from_slices(vec![&mut data])),
            PAGE
        );
    }
    let before = page_cache::statistics();
    b.seek_to(((SPLIT + 7) * PAGE) as isize);
    assert_eq!(
        b.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    assert_eq!(page_cache::statistics().bypasses, before.bypasses);
    assert_eq!(
        a.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    b.seek_to(((SPLIT + 64) * PAGE) as isize);
    assert_eq!(
        b.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    assert_eq!(
        a.read(mm::UserBuffer::from_slices(vec![&mut data])),
        PAGE
    );
    assert_eq!(page_cache::statistics().bypasses - before.bypasses, 1);
    println!("/dense: interleaved file handles keep independent scan positions");
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [image, path, reference] = args.as_slice() else {
        panic!("usage: ext4_probe IMAGE PATH EXPECTED_FILE");
    };
    DISK.set(File::open(image).unwrap()).unwrap();
    let ext4 = Arc::new(ext4::Ext4Fs::new().expect("missing EXT4 superblock"));
    let path = path.to_str().unwrap();
    let inode = ext4.lookup(path).expect("path not found in image");
    let actual = ext4.read_file(&inode);
    let expected = std::fs::read(reference).unwrap();

    assert_eq!(actual.len(), expected.len(), "{path}: length");
    let mismatch = actual.iter().zip(&expected).position(|(a, b)| a != b);
    assert_eq!(mismatch, None, "{path}: first mismatching byte");
    println!("{path}: inode {}, {} bytes match", inode.ino, actual.len());

    let file = ext4::Ext4File::new(Arc::clone(&ext4), inode);
    if path == "/dense" {
        check_sequential_reads(&file, &expected);
        check_cache_sharing(&ext4, &file, &expected);
        check_cache_bound(&file);
        if page_cache::policy_name() == "adaptive" {
            check_fragmented_scan(&ext4, &expected);
        }
    }
    if expected.len() >= ext4.block_size + 100 {
        check_read_ranges(&file, ext4.block_size, &expected);
    }
    drop(file);
    drop(ext4);
    assert_eq!(mm::live_frames(), 0);
}
