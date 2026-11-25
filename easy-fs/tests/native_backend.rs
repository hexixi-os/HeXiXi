use easy_fs::{block_cache_sync_device, BlockDevice, EasyFileSystem, FsError, BLOCK_SZ};
use std::sync::{Arc, Mutex};

static TEST_DISK: Mutex<()> = Mutex::new(());

#[derive(Clone)]
struct MemoryDisk(Arc<Mutex<Vec<[u8; BLOCK_SZ]>>>);

impl MemoryDisk {
    fn new(sectors: usize) -> Self {
        Self(Arc::new(Mutex::new(vec![[0; BLOCK_SZ]; sectors])))
    }

    fn device(&self) -> Arc<dyn BlockDevice> {
        Arc::new(self.clone())
    }

    fn contains(&self, value: &[u8]) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|block| block.windows(value.len()).any(|window| window == value))
    }
}

impl BlockDevice for MemoryDisk {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) {
        buf.copy_from_slice(&self.0.lock().unwrap()[block_id]);
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) {
        self.0.lock().unwrap()[block_id].copy_from_slice(buf);
    }
}

#[test]
fn mount_checks_geometry_before_using_image() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let image = MemoryDisk::new(1_040);
    assert_eq!(
        EasyFileSystem::try_open(MemoryDisk::new(0).device(), 0).err(),
        Some(FsError::Invalid)
    );
    assert_eq!(
        EasyFileSystem::try_open(image.device(), 1_040).err(),
        Some(FsError::Invalid)
    );

    EasyFileSystem::create(image.device(), 1_040, 1);
    assert_eq!(
        EasyFileSystem::try_open(image.device(), 1_039).err(),
        Some(FsError::Invalid)
    );
    let mounted = EasyFileSystem::try_open(image.device(), 1_040).unwrap();
    assert!(EasyFileSystem::root_inode(&mounted).is_dir());

    let mut sectors = image.0.lock().unwrap();
    sectors[0][12..16].copy_from_slice(&u32::MAX.to_le_bytes());
    drop(sectors);
    assert_eq!(
        EasyFileSystem::try_open(image.device(), 1_040).err(),
        Some(FsError::Invalid)
    );
}

#[test]
fn cache_keeps_devices_separate_and_flushes_only_the_requested_disk() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let first = MemoryDisk::new(1_040);
    let second = MemoryDisk::new(1_040);
    let first_device = first.device();
    let second_device = second.device();
    let first_fs = EasyFileSystem::create(Arc::clone(&first_device), 1_040, 1);
    let second_fs = EasyFileSystem::create(Arc::clone(&second_device), 1_040, 1);

    let first_file = EasyFileSystem::root_inode(&first_fs)
        .try_create("file", false)
        .unwrap();
    let second_file = EasyFileSystem::root_inode(&second_fs)
        .try_create("file", false)
        .unwrap();
    assert_eq!(first_file.id(), second_file.id());
    first_file.try_write_at(0, b"first disk").unwrap();
    second_file.try_write_at(0, b"second disk").unwrap();
    let mut read = [0; 16];
    assert_eq!(first_file.read_at(0, &mut read), 10);
    assert_eq!(&read[..10], b"first disk");

    block_cache_sync_device(&first_device);
    assert!(first.contains(b"first disk"));
    assert!(!second.contains(b"second disk"));
    block_cache_sync_device(&second_device);
    assert!(second.contains(b"second disk"));

    let reopened = EasyFileSystem::try_open(second.device(), 1_040).unwrap();
    let file = EasyFileSystem::root_inode(&reopened).find("file").unwrap();
    assert_eq!(file.read_at(0, &mut read), 11);
    assert_eq!(&read[..11], b"second disk");
}

#[test]
fn full_disk_leaves_file_and_directory_unchanged() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_032).device(), 1_032, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let file = root.try_create("file", false).unwrap();
    file.try_write_at(0, &[8; BLOCK_SZ]).unwrap();
    assert_eq!(
        file.try_write_at(0, &[3; BLOCK_SZ * 5]),
        Err(FsError::NoSpace)
    );
    assert_eq!(file.size(), BLOCK_SZ);
    let mut content = [0; BLOCK_SZ];
    assert_eq!(file.read_at(0, &mut content), BLOCK_SZ);
    assert_eq!(content, [8; BLOCK_SZ]);
    assert_eq!(file.resize(BLOCK_SZ * 4), Ok(()));

    for index in 0..15 {
        root.try_create(&format!("item{index}"), false).unwrap();
    }
    assert_eq!(
        root.try_create("no-room", false).err(),
        Some(FsError::NoSpace)
    );
    assert_eq!(root.ls().len(), 16);
    assert!(root.find("no-room").is_none());
    assert_eq!(file.resize(BLOCK_SZ * 3), Ok(()));
    assert_eq!(root.try_create("fits-now", false).unwrap().id(), 17);
}

#[test]
fn truncate_releases_indirect_blocks_and_zeros_the_retained_tail() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_200).device(), 1_200, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let file = root.try_create("file", false).unwrap();
    let size = 158 * BLOCK_SZ + 12;
    let payload = vec![5; size];
    file.try_write_at(0, &payload).unwrap();

    let truncated_size = 28 * BLOCK_SZ - 7;
    file.resize(truncated_size).unwrap();
    let reuse = root.try_create("reuse", false).unwrap();
    reuse.resize(139 * BLOCK_SZ).unwrap();
    reuse.resize(0).unwrap();
    file.resize(size).unwrap();

    let mut bytes = vec![1; size];
    assert_eq!(file.read_at(0, &mut bytes), size);
    assert!(bytes[..truncated_size].iter().all(|&byte| byte == 5));
    assert!(bytes[truncated_size..].iter().all(|&byte| byte == 0));
    assert_eq!(
        file.try_write_at(usize::MAX, b"!"),
        Err(FsError::FileTooLarge)
    );
    assert_eq!(file.try_write_at(usize::MAX, b""), Ok(0));
    assert_eq!(file.size(), size);
}

#[test]
fn replaced_and_unlinked_inodes_stay_alive_until_reaped() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_060).device(), 1_060, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let left = root.try_create("left", true).unwrap();
    let right = root.try_create("right", true).unwrap();
    let source = left.try_create("foo", false).unwrap();
    let former = right.try_create("foo", false).unwrap();
    former.try_write_at(0, b"old").unwrap();
    assert_eq!(
        left.rename("foo", &right, "foo", true).err(),
        Some(FsError::Exists)
    );
    let orphan = left.rename("foo", &right, "foo", false).unwrap().unwrap();
    assert!(left.find("foo").is_none());
    assert_eq!(right.find("foo").unwrap().id(), source.id());
    assert_eq!(orphan.id(), former.id());
    let mut payload = [0; 3];
    assert_eq!(former.read_at(0, &mut payload), 3);
    assert_eq!(&payload, b"old");
    assert_ne!(root.try_create("first", false).unwrap().id(), former.id());
    orphan.reap();
    assert_eq!(root.try_create("second", false).unwrap().id(), former.id());

    let child = right.unlink("foo", false).unwrap();
    assert_eq!(child.id(), source.id());
    child.try_write_at(0, b"new").unwrap();
    assert_eq!(source.read_at(0, &mut payload), 3);
    assert_eq!(&payload, b"new");
    child.reap();
    assert!(right.find("foo").is_none());
}

#[test]
fn directory_mutations_reject_invalid_names_and_cycles() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_060).device(), 1_060, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let parent = root.try_create("parent", true).unwrap();
    let child = parent.try_create("child", true).unwrap();
    assert_eq!(
        root.rename("parent", &child, "cycle", false).err(),
        Some(FsError::Invalid)
    );
    assert_eq!(root.unlink("parent", true).err(), Some(FsError::NotEmpty));
    assert_eq!(
        root.unlink("parent", false).err(),
        Some(FsError::IsDirectory)
    );
    assert_eq!(
        parent.try_create("a/b", false).err(),
        Some(FsError::Invalid)
    );
    assert_eq!(
        parent
            .try_create("1234567890123456789012345678", false)
            .err(),
        Some(FsError::NameTooLong)
    );
    assert_eq!(child.try_write_at(0, b"x"), Err(FsError::IsDirectory));
}

#[test]
fn rename_preserves_source_when_destination_directory_cannot_grow() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_030).device(), 1_030, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let source = root.try_create("source", true).unwrap();
    let dest = root.try_create("dest", true).unwrap();
    let file = source.try_create("file", false).unwrap();
    file.resize(BLOCK_SZ).unwrap();
    assert_eq!(
        source.rename("file", &dest, "moved", false).err(),
        Some(FsError::NoSpace)
    );
    assert_eq!(source.find("file").unwrap().id(), file.id());
    assert!(dest.find("moved").is_none());
    source.rename("file", &source, "renamed", false).unwrap();
    assert_eq!(source.find("renamed").unwrap().id(), file.id());
}

#[test]
fn same_directory_replacement_preserves_inode_when_target_is_last_entry() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_040).device(), 1_040, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let source = root.try_create("source", false).unwrap();
    let target = root.try_create("target", false).unwrap();
    let orphan = root
        .rename("source", &root, "target", false)
        .unwrap()
        .unwrap();
    assert_eq!(orphan.id(), target.id());
    assert_eq!(root.ls(), ["target"]);
    assert_eq!(root.find("target").unwrap().id(), source.id());
    orphan.reap();
}

#[test]
fn free_space_tracks_file_growth_and_reaping() {
    let _guard = TEST_DISK.lock().unwrap_or_else(|error| error.into_inner());
    let fs = EasyFileSystem::create(MemoryDisk::new(1_040).device(), 1_040, 1);
    let root = EasyFileSystem::root_inode(&fs);
    let empty = root.space_usage();
    assert_eq!(empty.free_blocks, empty.blocks);
    assert_eq!(empty.free_inodes, empty.inodes - 1);
    let file = root.try_create("space", false).unwrap();
    file.try_write_at(0, &[1; 2 * BLOCK_SZ]).unwrap();
    let used = root.space_usage();
    assert_eq!(used.free_blocks, empty.free_blocks - 3);
    assert_eq!(used.free_inodes, empty.free_inodes - 1);
    let removed = root.unlink("space", false).unwrap();
    assert_eq!(root.space_usage().free_inodes, used.free_inodes);
    removed.reap();
    assert_eq!(root.space_usage(), empty);
}
