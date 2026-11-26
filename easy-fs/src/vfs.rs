use super::{
    block_cache_sync_device, DirEntry, DiskInode, DiskInodeType, EasyFileSystem, FsError,
    SpaceUsage, DIRENT_SZ, MAX_FILE_SIZE, NAME_LENGTH_LIMIT,
};
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use spin::Mutex;

pub struct Inode {
    id: u32,
    fs: Arc<Mutex<EasyFileSystem>>,
}

impl Inode {
    pub(crate) fn new(id: u32, fs: Arc<Mutex<EasyFileSystem>>) -> Self {
        Self { id, fs }
    }

    fn handle(&self, id: u32) -> Arc<Self> {
        Arc::new(Self::new(id, Arc::clone(&self.fs)))
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn space_usage(&self) -> SpaceUsage {
        self.fs.lock().space_usage()
    }

    pub fn is_dir(&self) -> bool {
        self.fs.lock().read_disk_inode(self.id).is_dir()
    }

    fn directory_for_update(&self, fs: &EasyFileSystem) -> Result<DiskInode, FsError> {
        if fs.orphaned.contains(&self.id) {
            return Err(FsError::NotFound);
        }
        let inode = fs.read_disk_inode(self.id);
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }
        Ok(inode)
    }

    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        let inode = fs.read_disk_inode(self.id);
        find_entry(&fs, &inode, name).map(|(_, id)| self.handle(id))
    }

    pub fn try_create(&self, name: &str, directory: bool) -> Result<Arc<Inode>, FsError> {
        validate_name(name)?;
        let mut fs = self.fs.lock();
        let mut parent = self.directory_for_update(&fs)?;
        if find_entry(&fs, &parent, name).is_some() {
            return Err(FsError::Exists);
        }

        let inode_id = fs.try_alloc_inode()?;
        let offset = parent.size as usize;
        if let Err(error) = resize_inode(&mut fs, &mut parent, offset + DIRENT_SZ) {
            fs.dealloc_inode(inode_id);
            return Err(error);
        }
        let entry = DirEntry::new(name, inode_id);
        parent.write_at(offset, entry.as_bytes(), &fs.block_device);
        let mut inode = fs.read_disk_inode(inode_id);
        inode.initialize(if directory {
            DiskInodeType::Directory
        } else {
            DiskInodeType::File
        });
        fs.write_disk_inode(inode_id, inode);
        fs.write_disk_inode(self.id, parent);
        Ok(self.handle(inode_id))
    }

    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let inode = self.try_create(name, false).ok()?;
        block_cache_sync_device(&self.fs.lock().block_device);
        Some(inode)
    }

    pub fn ls(&self) -> Vec<String> {
        let fs = self.fs.lock();
        let inode = fs.read_disk_inode(self.id);
        entries(&fs, &inode)
            .filter_map(|(_, entry)| entry.name().map(String::from))
            .collect()
    }

    pub fn size(&self) -> usize {
        self.fs.lock().read_disk_inode(self.id).size as usize
    }

    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let fs = self.fs.lock();
        fs.read_disk_inode(self.id)
            .read_at(offset, buf, &fs.block_device)
    }

    pub fn try_write_at(&self, offset: usize, buf: &[u8]) -> Result<usize, FsError> {
        let mut fs = self.fs.lock();
        let mut inode = fs.read_disk_inode(self.id);
        if !inode.is_file() {
            return Err(FsError::IsDirectory);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let end = offset.checked_add(buf.len()).ok_or(FsError::FileTooLarge)?;
        if end > inode.size as usize {
            resize_inode(&mut fs, &mut inode, end)?;
            fs.write_disk_inode(self.id, inode);
        }
        Ok(inode.write_at(offset, buf, &fs.block_device))
    }

    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        self.try_write_at_sync(offset, buf).unwrap_or(0)
    }

    pub fn try_write_at_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, FsError> {
        let written = self.try_write_at(offset, buf)?;
        block_cache_sync_device(&self.fs.lock().block_device);
        Ok(written)
    }

    pub fn resize(&self, size: usize) -> Result<(), FsError> {
        let mut fs = self.fs.lock();
        let mut inode = fs.read_disk_inode(self.id);
        if !inode.is_file() {
            return Err(FsError::IsDirectory);
        }
        resize_inode(&mut fs, &mut inode, size)?;
        fs.write_disk_inode(self.id, inode);
        Ok(())
    }

    pub fn clear(&self) {
        if self.resize(0).is_ok() {
            block_cache_sync_device(&self.fs.lock().block_device);
        }
    }

    /// Remove a directory entry and keep its inode available to existing handles.
    pub fn unlink(&self, name: &str, is_dir: bool) -> Result<Arc<Inode>, FsError> {
        validate_name(name)?;
        let mut fs = self.fs.lock();
        let mut parent = self.directory_for_update(&fs)?;
        let (index, id) = find_entry(&fs, &parent, name).ok_or(FsError::NotFound)?;
        check_removable(&fs.read_disk_inode(id), is_dir)?;
        remove_entry(&mut fs, &mut parent, index);
        fs.write_disk_inode(self.id, parent);
        fs.orphaned.insert(id);
        Ok(self.handle(id))
    }

    /// Free an unlinked inode after the caller has released its last live handle.
    pub fn reap(&self) {
        let mut fs = self.fs.lock();
        if fs.orphaned.remove(&self.id) {
            let mut inode = fs.read_disk_inode(self.id);
            for block in inode.shrink_size(0, &fs.block_device) {
                fs.dealloc_data(block);
            }
            fs.write_disk_inode(self.id, inode);
            fs.dealloc_inode(self.id);
        }
    }

    /// Move an entry and return any replaced inode for deferred reclamation.
    pub fn rename(
        &self,
        old_name: &str,
        new_parent: &Self,
        new_name: &str,
        no_replace: bool,
    ) -> Result<Option<Arc<Inode>>, FsError> {
        if !Arc::ptr_eq(&self.fs, &new_parent.fs) {
            return Err(FsError::CrossDevice);
        }
        validate_name(old_name)?;
        validate_name(new_name)?;
        let mut fs = self.fs.lock();
        let mut source = self.directory_for_update(&fs)?;
        let (old_index, id) = find_entry(&fs, &source, old_name).ok_or(FsError::NotFound)?;
        let same_parent = self.id == new_parent.id;
        let mut destination = if same_parent {
            source
        } else {
            new_parent.directory_for_update(&fs)?
        };
        let existing = find_entry(&fs, &destination, new_name);
        let is_dir = fs.read_disk_inode(id).is_dir();
        let replaced = if let Some((_, replaced_id)) = existing {
            if no_replace {
                return Err(FsError::Exists);
            }
            if id == replaced_id {
                return Ok(None);
            }
            check_removable(&fs.read_disk_inode(replaced_id), is_dir)?;
            Some(replaced_id)
        } else {
            None
        };
        if !same_parent && is_dir && directory_contains(&fs, id, new_parent.id) {
            return Err(FsError::Invalid);
        }

        let entry = DirEntry::new(new_name, id);
        if same_parent {
            match existing {
                Some((index, _)) => {
                    source.write_at(index * DIRENT_SZ, entry.as_bytes(), &fs.block_device);
                    remove_entry(&mut fs, &mut source, old_index);
                }
                None => {
                    source.write_at(old_index * DIRENT_SZ, entry.as_bytes(), &fs.block_device);
                }
            }
        } else {
            let offset = if let Some((index, _)) = existing {
                index * DIRENT_SZ
            } else {
                let end = destination.size as usize;
                resize_inode(&mut fs, &mut destination, end + DIRENT_SZ)?;
                end
            };
            destination.write_at(offset, entry.as_bytes(), &fs.block_device);
            fs.write_disk_inode(new_parent.id, destination);
            remove_entry(&mut fs, &mut source, old_index);
        }
        fs.write_disk_inode(self.id, source);
        Ok(replaced.map(|id| {
            fs.orphaned.insert(id);
            self.handle(id)
        }))
    }
}

fn validate_name(name: &str) -> Result<(), FsError> {
    if name.len() > NAME_LENGTH_LIMIT {
        return Err(FsError::NameTooLong);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(FsError::Invalid);
    }
    Ok(())
}

fn entries<'a>(
    fs: &'a EasyFileSystem,
    inode: &'a DiskInode,
) -> impl Iterator<Item = (usize, DirEntry)> + 'a {
    let count = if inode.is_dir() {
        inode.size as usize / DIRENT_SZ
    } else {
        0
    };
    (0..count).map(move |index| (index, read_entry(fs, inode, index)))
}

fn read_entry(fs: &EasyFileSystem, inode: &DiskInode, index: usize) -> DirEntry {
    let mut entry = DirEntry::empty();
    inode.read_at(index * DIRENT_SZ, entry.as_bytes_mut(), &fs.block_device);
    entry
}

fn find_entry(fs: &EasyFileSystem, inode: &DiskInode, name: &str) -> Option<(usize, u32)> {
    entries(fs, inode).find_map(|(index, entry)| {
        let id = entry.inode_number();
        if id != 0 && entry.name() == Some(name) && fs.has_inode(id) {
            Some((index, id))
        } else {
            None
        }
    })
}

fn check_removable(inode: &DiskInode, directory: bool) -> Result<(), FsError> {
    if directory {
        if !inode.is_dir() {
            return Err(FsError::NotDirectory);
        }
        if inode.size != 0 {
            return Err(FsError::NotEmpty);
        }
    } else if inode.is_dir() {
        return Err(FsError::IsDirectory);
    }
    Ok(())
}

fn directory_contains(fs: &EasyFileSystem, directory: u32, target: u32) -> bool {
    let mut visited = BTreeSet::new();
    let mut pending = vec![directory];
    while let Some(id) = pending.pop() {
        if id == target {
            return true;
        }
        if !visited.insert(id) {
            continue;
        }
        let inode = fs.read_disk_inode(id);
        pending.extend(
            entries(fs, &inode)
                .map(|(_, entry)| entry.inode_number())
                .filter(|&child| child != 0 && fs.has_inode(child)),
        );
    }
    false
}

fn resize_inode(
    fs: &mut EasyFileSystem,
    inode: &mut DiskInode,
    size: usize,
) -> Result<(), FsError> {
    if size > MAX_FILE_SIZE {
        return Err(FsError::FileTooLarge);
    }
    let size = size as u32;
    if size < inode.size {
        for block in inode.shrink_size(size, &fs.block_device) {
            fs.dealloc_data(block);
        }
    } else if size > inode.size {
        let mut blocks = Vec::new();
        for _ in 0..inode.blocks_num_needed(size) {
            match fs.try_alloc_data() {
                Ok(block) => blocks.push(block),
                Err(error) => {
                    for block in blocks {
                        fs.dealloc_data(block);
                    }
                    return Err(error);
                }
            }
        }
        inode.increase_size(size, blocks, &fs.block_device);
    }
    Ok(())
}

fn remove_entry(fs: &mut EasyFileSystem, directory: &mut DiskInode, index: usize) {
    let last = directory.size as usize / DIRENT_SZ - 1;
    if index != last {
        let entry = read_entry(fs, directory, last);
        directory.write_at(index * DIRENT_SZ, entry.as_bytes(), &fs.block_device);
    }
    for block in directory.shrink_size((last * DIRENT_SZ) as u32, &fs.block_device) {
        fs.dealloc_data(block);
    }
}
