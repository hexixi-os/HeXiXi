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

mod mm {
    pub struct UserBuffer<'a> {
        pub buffers: Vec<&'a mut [u8]>,
    }

    impl IntoIterator for UserBuffer<'_> {
        type Item = *mut u8;
        type IntoIter = std::vec::IntoIter<*mut u8>;

        fn into_iter(self) -> Self::IntoIter {
            self.buffers
                .into_iter()
                .flatten()
                .map(|byte| byte as *mut u8)
                .collect::<Vec<_>>()
                .into_iter()
        }
    }
}

mod fs {
    pub trait File {
        fn readable(&self) -> bool;
        fn writable(&self) -> bool;
        fn read(&self, buf: crate::mm::UserBuffer) -> usize;
        fn write(&self, buf: crate::mm::UserBuffer) -> usize;
        fn size(&self) -> usize;
        fn seek(&self, offset: isize) -> isize;
        fn seek_to(&self, pos: isize) -> isize;
    }
}

#[path = "../../../os/src/fs/ext4.rs"]
mod ext4;
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
        let buf = mm::UserBuffer {
            buffers: vec![&mut data],
        };
        assert_eq!(file.read(buf), data.len());
        assert_eq!(&data, chunk);
    }
    let sectors = SECTORS_READ.load(Ordering::Relaxed) - before;
    println!(
        "/dense: {} KiB read in {REQUESTS} requests, {sectors} sector reads",
        REQUEST_SIZE * REQUESTS / 1024
    );
    assert_eq!(sectors, REQUEST_SIZE * REQUESTS / 512);
}

fn check_read_ranges(file: &ext4::Ext4File, block_size: usize, expected: &[u8]) {
    // 从块末尾读到后一块，目标缓冲区也有不同于磁盘块边界的切分。
    let start = block_size - 13;
    let mut data = [0; 97];
    let (left, right) = data.split_at_mut(7);
    assert_eq!(file.seek_to(start as isize), start as isize);
    assert_eq!(
        file.read(mm::UserBuffer {
            buffers: vec![left, right]
        }),
        data.len()
    );
    assert_eq!(&data, &expected[start..start + data.len()]);

    let mut following = [0; 16];
    let pos = start + data.len();
    assert_eq!(
        file.read(mm::UserBuffer {
            buffers: vec![&mut following]
        }),
        following.len()
    );
    assert_eq!(&following, &expected[pos..pos + following.len()]);

    let start = expected.len() - 10;
    let mut tail = [0xa5; 32];
    let (left, right) = tail.split_at_mut(6);
    assert_eq!(file.seek_to(start as isize), start as isize);
    assert_eq!(
        file.read(mm::UserBuffer {
            buffers: vec![left, right]
        }),
        10
    );
    assert_eq!(&tail[..10], &expected[start..]);
    assert_eq!(&tail[10..], &[0xa5; 22]);

    let before = SECTORS_READ.load(Ordering::Relaxed);
    assert_eq!(
        file.read(mm::UserBuffer {
            buffers: Vec::new()
        }),
        0
    );
    assert_eq!(
        file.read(mm::UserBuffer {
            buffers: vec![&mut tail]
        }),
        0
    );
    assert_eq!(
        SECTORS_READ.load(Ordering::Relaxed),
        before,
        "EOF or empty read did I/O"
    );
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
    }
    if expected.len() >= ext4.block_size + 100 {
        check_read_ranges(&file, ext4.block_size, &expected);
    }
}
