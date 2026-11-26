use super::{errno, page_cache, File, OpenFlags, PersistentFs, PersistentNode};
use crate::config::PAGE_SIZE;
use crate::mm::{copy_live_bytes, zero_live_bytes, FrameTracker, UserBuffer};
use crate::sync::UPSafeCell;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::ops::Range;
use page_cache::{Access, FilePageKey};

pub(super) struct TrackedPage {
    frame: Weak<FrameTracker>,
    writeable_mapping: bool,
    unsynced: Option<Arc<FrameTracker>>,
}

fn page_indices(offset: usize, length: usize) -> Range<usize> {
    let end = offset.saturating_add(length);
    offset / PAGE_SIZE..end / PAGE_SIZE + usize::from(end % PAGE_SIZE != 0)
}

impl PersistentNode {
    fn page_key(&self, index: usize) -> FilePageKey {
        FilePageKey {
            fs: self.fs_id,
            inode: self.inode.id(),
            index: index as u64,
        }
    }

    fn mapped_frame(&self, index: usize) -> Option<Arc<FrameTracker>> {
        self.mappings
            .exclusive_access()
            .get(&index)
            .and_then(|entry| entry.frame.upgrade())
    }

    fn resident_frame(&self, index: usize) -> Option<Arc<FrameTracker>> {
        self.mapped_frame(index)
            .or_else(|| page_cache::existing(self.page_key(index)))
    }

    fn load(&self, index: usize, access: Access) -> Option<Arc<FrameTracker>> {
        self.mapped_frame(index).or_else(|| {
            page_cache::get_or_load(self.page_key(index), access, |bytes| {
                self.inode.read_at(index * PAGE_SIZE, bytes);
            })
        })
    }

    fn map(&self, offset: usize, writable: bool) -> Option<Arc<FrameTracker>> {
        let index = offset / PAGE_SIZE;
        let frame = self.load(index, Access::Fault)?;
        let mut mappings = self.mappings.exclusive_access();
        let entry = mappings.entry(index).or_insert_with(|| TrackedPage {
            frame: Arc::downgrade(&frame),
            writeable_mapping: false,
            unsynced: None,
        });
        if entry.frame.strong_count() == 0 {
            entry.frame = Arc::downgrade(&frame);
            entry.writeable_mapping = false;
        }
        // A shared writable mapping may store without another kernel trap.
        if writable {
            entry.writeable_mapping = true;
            entry.unsynced = Some(frame.clone());
        }
        Some(frame)
    }

    fn read_at(&self, mut offset: usize, mut out: &mut [u8]) -> usize {
        let size = self.inode.size();
        let count = size.saturating_sub(offset).min(out.len());
        out = &mut out[..count];
        while !out.is_empty() {
            let page_offset = offset % PAGE_SIZE;
            let n = out.len().min(PAGE_SIZE - page_offset);
            if let Some(page) = self.load(offset / PAGE_SIZE, Access::read()) {
                unsafe {
                    copy_live_bytes(page.ppn.bytes_ptr().add(page_offset), out.as_mut_ptr(), n)
                };
            } else {
                self.inode.read_at(offset, &mut out[..n]);
            }
            out = &mut out[n..];
            offset += n;
        }
        count
    }

    fn zero_tail(&self, offset: usize) {
        let page_offset = offset % PAGE_SIZE;
        if page_offset != 0 {
            if let Some(page) = self.resident_frame(offset / PAGE_SIZE) {
                unsafe {
                    zero_live_bytes(
                        page.ppn.bytes_ptr().add(page_offset),
                        PAGE_SIZE - page_offset,
                    )
                };
            }
        }
    }

    fn write_at(&self, offset: usize, input: &[u8]) -> Result<usize, isize> {
        let old_size = self.inode.size();
        let written = self.inode.try_write_at(offset, input).map_err(errno)?;
        if written == 0 {
            return Ok(0);
        }
        if offset > old_size {
            self.zero_tail(old_size);
        }
        let mut remaining = &input[..written];
        let mut offset = offset;
        while !remaining.is_empty() {
            let page_offset = offset % PAGE_SIZE;
            let count = remaining.len().min(PAGE_SIZE - page_offset);
            if let Some(page) = self.resident_frame(offset / PAGE_SIZE) {
                unsafe {
                    copy_live_bytes(
                        remaining.as_ptr(),
                        page.ppn.bytes_ptr().add(page_offset),
                        count,
                    )
                };
            }
            offset += count;
            remaining = &remaining[count..];
        }
        Ok(written)
    }

    fn resize(&self, size: usize) -> Result<(), isize> {
        let old_size = self.inode.size();
        if size == old_size {
            return Ok(());
        }
        self.inode.resize(size).map_err(errno)?;
        self.zero_tail(size.min(old_size));
        if size < old_size {
            let first = size / PAGE_SIZE + usize::from(size % PAGE_SIZE != 0);
            let retired = self.mappings.exclusive_access().split_off(&first);
            page_cache::discard_inode_pages(self.fs_id, self.inode.id(), first as u64);
            crate::task::invalidate_file_pages(self.identity(), first * PAGE_SIZE);
            drop(retired);
        }
        Ok(())
    }

    pub(super) fn writeback(&self, offset: usize, length: usize) -> Result<(), isize> {
        let len = self.inode.size();
        let mut scratch = Vec::new();
        for (&index, entry) in self
            .mappings
            .exclusive_access()
            .range(page_indices(offset, length))
        {
            if !entry.writeable_mapping {
                continue;
            }
            let frame = match entry.frame.upgrade() {
                Some(frame) => frame,
                None => continue,
            };
            let start = index * PAGE_SIZE;
            let count = len.saturating_sub(start).min(PAGE_SIZE);
            if count != 0 {
                scratch.resize(count, 0);
                unsafe { copy_live_bytes(frame.ppn.bytes_ptr(), scratch.as_mut_ptr(), count) };
                self.inode.try_write_at(start, &scratch).map_err(errno)?;
            }
        }
        Ok(())
    }

    pub(super) fn finish_writeback(&self, offset: usize, length: usize) {
        let mut mappings = self.mappings.exclusive_access();
        for (&index, entry) in mappings.range_mut(page_indices(offset, length)) {
            let cached = usize::from(page_cache::contains(self.page_key(index)));
            if entry.frame.strong_count() <= cached + usize::from(entry.unsynced.is_some()) {
                entry.unsynced = None;
                entry.writeable_mapping = false;
            }
        }
        mappings.retain(|_, entry| entry.frame.strong_count() != 0);
    }
}

pub(super) struct PersistentFile {
    fs: Arc<PersistentFs>,
    node: Arc<PersistentNode>,
    offset: UPSafeCell<usize>,
    flags: OpenFlags,
}

impl PersistentFile {
    pub(super) fn open(
        fs: Arc<PersistentFs>,
        node: Arc<PersistentNode>,
        flags: OpenFlags,
    ) -> Result<Self, isize> {
        let writer = flags.read_write().1;
        if writer || flags.contains(OpenFlags::TRUNC) {
            fs.check_writable()?;
            if node.life.exclusive_access().executables != 0 {
                return Err(-26);
            }
            if flags.contains(OpenFlags::TRUNC) {
                node.resize(0)?;
            }
        }
        if writer {
            node.life.exclusive_access().writers += 1;
        }
        Ok(Self {
            fs,
            node,
            offset: unsafe { UPSafeCell::new(0) },
            flags,
        })
    }

    fn read_buffer(&self, start: usize, buffer: UserBuffer) -> usize {
        let mut offset = start;
        let mut scratch = Vec::new();
        for part in &buffer.buffers {
            let count = part.write_with(&mut scratch, |bytes| self.node.read_at(offset, bytes));
            offset += count;
            if count != part.len() {
                break;
            }
        }
        offset - start
    }

    fn write_buffer(&self, offset: usize, buffer: UserBuffer) -> Result<(usize, usize), isize> {
        let start = if self.flags.contains(OpenFlags::APPEND) {
            self.size()
        } else {
            offset
        };
        let mut cursor = start;
        let mut scratch = Vec::new();
        for part in &buffer.buffers {
            match part.read_with(&mut scratch, |bytes| self.node.write_at(cursor, bytes)) {
                Ok(count) => {
                    cursor += count;
                    if count < part.len() {
                        break;
                    }
                }
                Err(error) if cursor == start => return Err(error),
                Err(_) => break,
            }
        }
        Ok((cursor, cursor - start))
    }
}

impl Drop for PersistentFile {
    fn drop(&mut self) {
        if self.writable() {
            self.node.life.exclusive_access().writers -= 1;
        }
    }
}

impl File for PersistentFile {
    fn readable(&self) -> bool {
        self.flags.read_write().0
    }
    fn writable(&self) -> bool {
        self.flags.read_write().1
    }
    fn stat_identity(&self) -> Option<(u64, u64)> {
        Some(self.node.identity())
    }
    fn link_count(&self) -> u32 {
        u32::from(self.node.linked())
    }
    fn size(&self) -> usize {
        self.node.inode.size()
    }
    fn mapping_size(&self) -> Option<usize> {
        Some(self.size())
    }
    fn supports_mapped_writes(&self) -> bool {
        !self.fs.readonly
    }
    fn mapping_page(&self, offset: usize, shared: bool) -> Option<Arc<FrameTracker>> {
        self.node.map(offset, shared && self.writable())
    }

    fn read(&self, buffer: UserBuffer) -> usize {
        let mut offset = self.offset.exclusive_access();
        let count = self.read_buffer(*offset, buffer);
        *offset += count;
        count
    }

    fn write(&self, buffer: UserBuffer) -> isize {
        let mut offset = self.offset.exclusive_access();
        match self.write_buffer(*offset, buffer) {
            Ok((end, count)) => {
                if count != 0 {
                    *offset = end;
                }
                count as isize
            }
            Err(error) => error,
        }
    }

    fn supports_positioned_io(&self) -> bool {
        true
    }

    fn read_positioned(&self, offset: usize, buffer: UserBuffer) -> isize {
        self.read_buffer(offset, buffer) as isize
    }

    fn write_positioned(&self, offset: usize, buffer: UserBuffer) -> isize {
        self.write_buffer(offset, buffer).map_or_else(|error| error, |(_, count)| count as isize)
    }

    fn set_len(&self, len: usize) -> isize {
        if self.fs.readonly {
            return -30;
        }
        if !self.writable() {
            return -22;
        }
        self.node.resize(len).err().unwrap_or(0)
    }

    fn seek(&self, offset: isize) -> isize {
        let start = *self.offset.exclusive_access() as isize;
        start
            .checked_add(offset)
            .map_or(-75, |position| self.seek_to(position))
    }

    fn seek_to(&self, position: isize) -> isize {
        if position < 0 {
            return -22;
        }
        *self.offset.exclusive_access() = position as usize;
        position
    }

    fn space_usage(&self) -> Option<(easy_fs::SpaceUsage, bool)> {
        Some((self.fs.space_usage(), self.fs.readonly()))
    }

    fn sync_range(&self, offset: usize, length: usize) -> Result<(), isize> {
        if self.fs.readonly {
            return Ok(());
        }
        if length != 0 {
            self.node.writeback(offset, length)?;
        }
        self.fs.barrier()?;
        if length != 0 {
            self.node.finish_writeback(offset, length);
        }
        Ok(())
    }

    fn sync_fs(&self) -> Result<(), isize> {
        self.fs.sync()
    }
}

pub struct PersistentExecutable {
    fs: Arc<PersistentFs>,
    node: Arc<PersistentNode>,
}

impl PersistentExecutable {
    pub(super) fn open(fs: Arc<PersistentFs>, node: Arc<PersistentNode>) -> Result<Self, isize> {
        let mut life = node.life.exclusive_access();
        if life.writers != 0 {
            return Err(-26);
        } // ETXTBSY
        life.executables += 1;
        drop(life);
        Ok(Self { fs, node })
    }

    pub(crate) fn len(&self) -> usize {
        self.node.inode.size()
    }
    pub(crate) fn dev(&self) -> u64 {
        self.fs.dev()
    }
    pub(crate) fn read_at(&self, offset: usize, bytes: &mut [u8]) -> usize {
        self.node.read_at(offset, bytes)
    }
    pub(crate) fn cached_page(&self, offset: usize) -> Option<Arc<FrameTracker>> {
        self.node.load(offset / PAGE_SIZE, Access::Fault)
    }
}

impl Drop for PersistentExecutable {
    fn drop(&mut self) {
        self.node.life.exclusive_access().executables -= 1;
    }
}
