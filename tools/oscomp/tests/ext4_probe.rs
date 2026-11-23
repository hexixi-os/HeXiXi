//! 在宿主机上复用内核的 EXT4 读取代码，块设备由普通镜像文件提供。

#![allow(dead_code)]

extern crate alloc;

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::OnceLock;

mod drivers {
    pub mod block {
        pub static BLOCK_DEVICE: crate::ImageDevice = crate::ImageDevice;
    }
}

mod mm {
    pub type UserBuffer = Vec<*mut u8>;
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

struct ImageDevice;

impl ImageDevice {
    fn read_block(&self, block: usize, buf: &mut [u8]) {
        DISK.get()
            .unwrap()
            .read_exact_at(buf, block as u64 * 512)
            .unwrap();
    }
}

fn main() {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [image, path, reference] = args.as_slice() else {
        panic!("usage: ext4_probe IMAGE PATH EXPECTED_FILE");
    };
    DISK.set(File::open(image).unwrap()).unwrap();
    let ext4 = ext4::Ext4Fs::new().expect("missing EXT4 superblock");
    let path = path.to_str().unwrap();
    let inode = ext4.lookup(path).expect("path not found in image");
    let actual = ext4.read_file(&inode);
    let expected = std::fs::read(reference).unwrap();

    assert_eq!(actual.len(), expected.len(), "{path}: length");
    let mismatch = actual.iter().zip(&expected).position(|(a, b)| a != b);
    assert_eq!(mismatch, None, "{path}: first mismatching byte");
    println!("{path}: inode {}, {} bytes match", inode.ino, actual.len());
}
