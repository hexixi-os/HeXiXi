#![no_std]

extern crate alloc;

mod bitmap;
mod block_cache;
mod block_dev;
mod efs;
mod layout;
mod vfs;

pub const BLOCK_SZ: usize = 512;
use bitmap::Bitmap;
pub use block_cache::block_cache_sync_device;
use block_cache::get_block_cache;
pub use block_dev::BlockDevice;
pub use efs::{EasyFileSystem, SpaceUsage};
pub use layout::NAME_LENGTH_LIMIT;
use layout::*;
pub use vfs::Inode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    Invalid,
    NoSpace,
    Exists,
    NotFound,
    NotDirectory,
    IsDirectory,
    NotEmpty,
    NameTooLong,
    FileTooLarge,
    CrossDevice,
}
