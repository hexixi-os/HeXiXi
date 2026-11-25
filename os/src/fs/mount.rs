use super::{overlay::MemFs, persistent::{self, PersistentFs}};
use crate::sync::UPSafeCell;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub const EXT4_MAGIC: u64 = 0xef53;
pub const TMPFS_MAGIC: u64 = 0x0102_1994;
pub const PROC_MAGIC: u64 = 0x9fa0;
pub const MSDOS_MAGIC: u64 = 0x4d44;

static NEXT_MOUNT_ID: AtomicUsize = AtomicUsize::new(1);

fn next_mount_id() -> usize {
    NEXT_MOUNT_ID.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
pub(super) enum MountBackend {
    RootOverlay,
    Procfs,
    Devfs,
    Memfs(Arc<MemFs>),
    Persistent(Arc<PersistentFs>),
}

#[derive(Clone)]
pub(super) struct MountEntry {
    pub id: usize,
    pub source: String,
    pub target: String,
    pub fstype: String,
    pub backend: MountBackend,
    pub system: bool,
}

impl MountEntry {
    pub fn magic(&self) -> u64 {
        match &self.backend {
            MountBackend::RootOverlay => EXT4_MAGIC,
            MountBackend::Procfs => PROC_MAGIC,
            MountBackend::Devfs => TMPFS_MAGIC,
            MountBackend::Memfs(fs) => fs.magic(),
            MountBackend::Persistent(_) => persistent::MAGIC,
        }
    }
}

fn default_mounts() -> Vec<MountEntry> {
    let mut mounts = Vec::new();
    if super::has_ext4() {
        mounts.push(MountEntry {
            id: next_mount_id(),
            source: String::from("/dev/root"),
            target: String::new(),
            fstype: String::from("ext4"),
            backend: MountBackend::RootOverlay,
            system: true,
        });
    }
    mounts.push(MountEntry {
        id: next_mount_id(),
        source: String::from("proc"),
        target: String::from("proc"),
        fstype: String::from("proc"),
        backend: MountBackend::Procfs,
        system: true,
    });
    mounts.push(MountEntry {
        id: next_mount_id(),
        source: String::from("devtmpfs"),
        target: String::from("dev"),
        fstype: String::from("devtmpfs"),
        backend: MountBackend::Devfs,
        system: true,
    });
    mounts
}

lazy_static! {
    static ref MOUNTS: UPSafeCell<Vec<MountEntry>> = unsafe { UPSafeCell::new(default_mounts()) };
}

fn split_mount_target<'a>(path: &'a str, target: &str) -> Option<&'a str> {
    if target.is_empty() {
        Some(path)
    } else if path == target {
        Some("")
    } else {
        path.strip_prefix(target)
            .and_then(|rest| rest.strip_prefix('/'))
    }
}

pub(super) fn resolve_mount(path: &str) -> Option<(MountEntry, &str)> {
    let mounts = MOUNTS.exclusive_access();
    let mut best: Option<(usize, &MountEntry, &str)> = None;
    for (index, entry) in mounts.iter().enumerate() {
        if let Some(rel) = split_mount_target(path, &entry.target) {
            let replace = match best {
                None => true,
                Some((best_idx, best_entry, _)) => {
                    entry.target.len() > best_entry.target.len()
                        || (entry.target.len() == best_entry.target.len() && index > best_idx)
                }
            };
            if replace {
                best = Some((index, entry, rel));
            }
        }
    }
    best.map(|(_, entry, rel)| (entry.clone(), rel))
}

pub(super) fn is_mount_point(path: &str) -> bool {
    MOUNTS
        .exclusive_access()
        .iter()
        .any(|entry| entry.target == path)
}

pub(super) fn has_descendant_mount(path: &str) -> bool {
    MOUNTS.exclusive_access().iter().any(|entry| {
        entry.target.strip_prefix(path).map_or(false, |remaining| remaining.starts_with('/'))
    })
}

pub(super) fn mount_tmpfs_at_boot(target: &str) {
    if is_mount_point(target) {
        return;
    }
    if let Ok(fs) = MemFs::new_mountable(target, TMPFS_MAGIC) {
        MOUNTS.exclusive_access().push(MountEntry {
            id: next_mount_id(),
            source: String::from("tmpfs"),
            target: String::from(target),
            fstype: String::from("tmpfs"),
            backend: MountBackend::Memfs(fs),
            system: false,
        });
    }
}

pub(super) fn child_mounts(dir: &str) -> Vec<(String, u32)> {
    let mounts = MOUNTS.exclusive_access();
    let mut out: Vec<(String, u32)> = Vec::new();
    for entry in mounts.iter() {
        let child = if dir.is_empty() {
            (!entry.target.is_empty() && !entry.target.contains('/')).then_some(entry.target.as_str())
        } else {
            entry
                .target
                .strip_prefix(dir)
                .and_then(|rest| rest.strip_prefix('/'))
                .filter(|rest| !rest.is_empty() && !rest.contains('/'))
        };
        if let Some(name) = child {
            let ino = match &entry.backend {
                MountBackend::RootOverlay => 2,
                MountBackend::Procfs => 99,
                MountBackend::Devfs => 300,
                MountBackend::Memfs(fs) => fs.root_ino(),
                MountBackend::Persistent(fs) => fs.root_ino(),
            };
            if let Some(existing) = out.iter_mut().find(|(existing, _)| existing == name) {
                existing.1 = ino;
            } else {
                out.push((String::from(name), ino));
            }
        }
    }
    out
}

pub(super) fn mounts_snapshot() -> Vec<u8> {
    let mounts = MOUNTS.exclusive_access();
    let mut text = String::new();
    for entry in mounts.iter() {
        let target = if entry.target.is_empty() {
            String::from("/")
        } else {
            alloc::format!("/{}", entry.target)
        };
        let access = if matches!(&entry.backend, MountBackend::Persistent(fs) if fs.readonly()) { "ro" } else { "rw" };
        let _ = writeln!(text, "{} {} {} {},relatime 0 0", entry.source, target, entry.fstype, access);
    }
    text.into_bytes()
}

pub(super) fn magic_for_dev(dev: u64) -> Option<u64> {
    MOUNTS.exclusive_access().iter().find_map(|entry| match &entry.backend {
        MountBackend::Memfs(fs) if fs.dev() == dev => Some(fs.magic()),
        MountBackend::Persistent(fs) if fs.dev() == dev => Some(persistent::MAGIC),
        _ => None,
    })
}

pub fn mount_fs(source: &str, target: &str, fstype: &str, flags: usize) -> Result<(), isize> {
    if !super::has_ext4() {
        return if fstype == "easyfs" { Err(-19) } else { Ok(()) };
    }
    if fstype == "easyfs" && (target.is_empty() || is_mount_point(target)) { return Err(-16); }
    if fstype == "easyfs"
        || !(crate::config::legacy_oscomp_image() && super::is_legacy_basic_mnt(target))
    {
        super::require_directory(target)?;
    }
    let source = if source.is_empty() { "none" } else { source };
    let backend = match fstype {
        "easyfs" => MountBackend::Persistent(PersistentFs::mount(source, flags)?),
        "tmpfs" | "ramfs" => MountBackend::Memfs(MemFs::new_mountable(target, TMPFS_MAGIC)?),
        "vfat" | "msdos" => MountBackend::Memfs(MemFs::new_mountable(target, MSDOS_MAGIC)?),
        "ext4" | "ext2" => MountBackend::Memfs(MemFs::new_mountable(target, EXT4_MAGIC)?),
        "proc" => MountBackend::Procfs,
        "devtmpfs" | "devfs" => MountBackend::Devfs,
        _ => return Err(-19), // ENODEV
    };
    MOUNTS.exclusive_access().push(MountEntry {
        id: next_mount_id(),
        source: String::from(source),
        target: String::from(target),
        fstype: String::from(fstype),
        backend,
        system: false,
    });
    Ok(())
}

pub fn umount_fs(target: &str, flags: usize) -> Result<(), isize> {
    const MNT_FORCE: usize = 1;
    const MNT_DETACH: usize = 2;
    const MNT_EXPIRE: usize = 4;
    const UMOUNT_NOFOLLOW: usize = 8;
    if flags & !(MNT_FORCE | MNT_DETACH | MNT_EXPIRE | UMOUNT_NOFOLLOW) != 0 {
        return Err(-22);
    }
    if !super::has_ext4() {
        return Ok(());
    }
    let candidate = {
        let mounts = MOUNTS.exclusive_access();
        mounts
            .iter()
            .enumerate()
            .rev()
            .find(|(_, entry)| entry.target == target)
            .map(|(index, entry)| (index, entry.clone()))
    };
    let (index, entry) = match candidate {
        Some(found) => found,
        None => {
            if !(crate::config::legacy_oscomp_image() && super::is_legacy_basic_mnt(target)) {
                super::require_directory(target)?;
            }
            return Err(-22); // EINVAL: not mounted
        }
    };
    if entry.system {
        return Err(-16); // EBUSY
    }
    let dev = match &entry.backend {
        MountBackend::Memfs(fs) => Some(fs.dev()),
        MountBackend::Persistent(fs) => Some(fs.dev()),
        _ => None,
    };
    if flags & MNT_DETACH == 0 && crate::task::mount_is_busy(target, dev) {
        return Err(-16); // EBUSY
    }
    if let MountBackend::Persistent(fs) = &entry.backend {
        if flags & (MNT_FORCE | MNT_DETACH | MNT_EXPIRE) != 0 { return Err(-22); }
        if has_descendant_mount(target) { return Err(-16); }
        fs.sync()?;
    }
    let removed = {
        let mut mounts = MOUNTS.exclusive_access();
        if index < mounts.len() && mounts[index].id == entry.id {
            Some(mounts.remove(index))
        } else {
            None
        }
    };
    if let Some(MountEntry {
        backend: MountBackend::Memfs(fs),
        ..
    }) = removed
    {
        fs.unmount();
    }
    Ok(())
}
