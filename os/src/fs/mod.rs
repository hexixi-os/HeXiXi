mod directory;
mod executable;
pub mod ext4;
mod inode;
mod overlay;
pub(crate) mod page_cache;
mod pipe;
mod stdio;

use crate::mm::{FrameTracker, UserBuffer};
use directory::DirectoryIdentity;
pub use directory::{DirectoryFile, DirectoryLocation};
pub use executable::{open_executable, Executable};
use overlay::{
    overlay_children, overlay_create, overlay_get, overlay_mkdir, overlay_rename, overlay_unlink,
    OverlayData, OverlayEntry, OverlayFile,
};

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> usize;
    fn write(&self, buf: UserBuffer) -> isize;
    fn directory(&self) -> Option<&DirectoryFile> {
        None
    }
    fn mapping_size(&self) -> Option<usize> {
        None
    }
    fn mapping_lease(&self) -> Option<Arc<()>> {
        None
    }
    fn supports_mapped_writes(&self) -> bool {
        false
    }
    fn mapping_page(&self, _offset: usize) -> Option<Arc<FrameTracker>> {
        None
    }
    /// 文件大小（fstat/mmap 需要）
    fn size(&self) -> usize {
        0
    }
    /// 调整读写偏移（相对当前），返回新偏移
    fn seek(&self, _offset: isize) -> isize {
        -1
    }
    /// 绝对定位（SEEK_SET / SEEK_END），返回新偏移
    fn seek_to(&self, _pos: isize) -> isize {
        -1
    }
    /// 修改文件长度（truncate/ftruncate），返回 0 成功
    fn set_len(&self, _len: usize) -> isize {
        0
    }
    /// 非阻塞探测：是否有数据可读（select/pselect 用；普通文件恒为 true）
    fn poll_readable(&self) -> bool {
        true
    }
    /// 非阻塞探测：是否可写
    fn poll_writable(&self) -> bool {
        true
    }
}

pub use inode::{is_unlinked, list_apps, mark_unlinked, open_file, root_names, OSInode, OpenFlags};
pub use pipe::{make_pipe, Pipe};

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::*;

lazy_static! {
    /// 启动探测到的 EXT4 根文件系统（只读）
    pub static ref EXT4: Option<Arc<ext4::Ext4Fs>> = ext4::Ext4Fs::new().map(Arc::new);
}

pub fn has_ext4() -> bool {
    EXT4.is_some()
}

/// /dev/null：读永远 EOF，写直接丢弃
pub struct NullFile;

impl File for NullFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, _buf: UserBuffer) -> usize {
        0
    }
    fn write(&self, buf: UserBuffer) -> isize {
        buf.len() as isize
    }
    fn poll_readable(&self) -> bool {
        true
    }
    fn poll_writable(&self) -> bool {
        true
    }
}

/// /dev/zero：读返回全 0，写丢弃
pub struct ZeroFile;

impl File for ZeroFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, buf: UserBuffer) -> usize {
        let mut n = 0usize;
        for ptr in buf {
            unsafe {
                *ptr = 0;
            }
            n += 1;
        }
        n
    }
    fn write(&self, buf: UserBuffer) -> isize {
        buf.len() as isize
    }
}

/// 读出整个文件（内核内部用，例如 exec 加载 ELF）
pub fn read_file_all(name: &str) -> Option<Vec<u8>> {
    read_file_all_inner(name)
}

fn read_file_all_inner(name: &str) -> Option<Vec<u8>> {
    if let Some(v) = virtual_file(name) {
        return Some(v);
    }
    match overlay_get(name) {
        Some(OverlayEntry::File(data)) => return Some(data.exclusive_access().read_all()),
        Some(_) => return None,
        None => {}
    }
    if let Some(fs) = EXT4.as_ref() {
        let inode = lower_inode(name)?;
        (!inode.is_dir()).then(|| fs.read_file(&inode))
    } else {
        open_file(name, OpenFlags::RDONLY).map(|i| i.read_all())
    }
}

/// An upper-layer directory has its own children. A whiteout or regular file
/// at an ancestor likewise makes the corresponding lower subtree unreachable.
fn lower_inode(name: &str) -> Option<ext4::Inode> {
    let fs = EXT4.as_ref()?;
    if lower_ancestor_is_hidden(name) {
        return None;
    }
    fs.lookup(name)
}

fn lower_ancestor_is_hidden(name: &str) -> bool {
    let mut path = name;
    while let Some((parent, _)) = path.rsplit_once('/') {
        if overlay_get(parent).is_some() {
            return true;
        }
        path = parent;
    }
    false
}

pub fn path_is_directory(name: &str) -> Result<bool, isize> {
    if is_root_path(name) || matches!(name, "proc" | "dev") {
        return Ok(true);
    }
    match overlay_get(name) {
        Some(OverlayEntry::File(_)) => return Ok(false),
        Some(OverlayEntry::Directory(_)) => return Ok(true),
        Some(OverlayEntry::Whiteout) => return Err(-2),
        None => {}
    }
    if EXT4.is_some() {
        lower_inode(name).map(|inode| inode.is_dir()).ok_or(-2)
    } else {
        open_file(name, OpenFlags::RDONLY).map(|_| false).ok_or(-2)
    }
}

fn require_parent_directories(name: &str) -> Result<(), isize> {
    for (separator, _) in name.match_indices('/') {
        if !path_is_directory(&name[..separator])? {
            return Err(-20); // ENOTDIR
        }
    }
    Ok(())
}

pub fn require_directory(name: &str) -> Result<(), isize> {
    require_parent_directories(name)?;
    if path_is_directory(name)? {
        Ok(())
    } else {
        Err(-20) // ENOTDIR
    }
}

pub fn directory_is_live(directory: &DirectoryLocation) -> bool {
    let path = directory.path();
    match &directory.identity {
        DirectoryIdentity::Permanent => true,
        DirectoryIdentity::Lower => overlay_get(path).is_none() && !lower_ancestor_is_hidden(path),
        DirectoryIdentity::Upper(opened) => match overlay_get(path) {
            Some(OverlayEntry::Directory(current)) => Arc::ptr_eq(opened, &current),
            _ => false,
        },
    }
}

pub fn working_directory(name: &str) -> Result<DirectoryLocation, isize> {
    let identity = if !has_ext4() || is_root_path(name) || matches!(name, "proc" | "dev") {
        // easy-fs only remembers a cwd string in its flat namespace.
        DirectoryIdentity::Permanent
    } else {
        require_directory(name)?;
        match overlay_get(name) {
            Some(OverlayEntry::Directory(identity)) => DirectoryIdentity::Upper(identity),
            _ => DirectoryIdentity::Lower,
        }
    };
    Ok(DirectoryLocation::new(name, identity))
}

fn open_overlay(data: OverlayData, flags: OpenFlags) -> Result<Arc<dyn File + Send + Sync>, isize> {
    if flags.contains(OpenFlags::TRUNC) && !data.exclusive_access().resize(0) {
        return Err(-16);
    }
    Ok(Arc::new(OverlayFile::new(data, flags)))
}

fn open_directory(
    name: &str,
    flags: OpenFlags,
    identity: DirectoryIdentity,
) -> Result<Arc<dyn File + Send + Sync>, isize> {
    if flags.contains(OpenFlags::CREATE | OpenFlags::EXCL) {
        return Err(-17);
    }
    if flags.intersects(OpenFlags::WRONLY | OpenFlags::RDWR | OpenFlags::TRUNC) {
        return Err(-21); // EISDIR
    }
    Ok(Arc::new(DirectoryFile::new(name, identity)))
}

/// 打开文件并返回统一的 File 对象（供系统调用使用）
pub fn open_dyn(name: &str, flags: OpenFlags) -> Result<Arc<dyn File + Send + Sync>, isize> {
    let create = flags.contains(OpenFlags::CREATE);
    let exclusive = flags.contains(OpenFlags::CREATE | OpenFlags::EXCL);
    let directory = flags.contains(OpenFlags::DIRECTORY);

    if is_root_path(name) || matches!(name, "proc" | "dev") {
        return open_directory(name, flags, DirectoryIdentity::Permanent);
    }
    if matches!(name, "dev/null" | "dev/zero" | "dev/tty" | "dev/console") {
        if exclusive || directory {
            return Err(if exclusive { -17 } else { -20 });
        }
        return Ok(match name {
            "dev/null" => Arc::new(NullFile),
            "dev/zero" => Arc::new(ZeroFile),
            _ => Arc::new(Stdout),
        });
    }
    if let Some(v) = virtual_file(name) {
        if exclusive || directory {
            return Err(if exclusive { -17 } else { -20 });
        }
        let data = overlay_create(name);
        {
            let mut data = data.exclusive_access();
            if !data.resize(v.len()) {
                return Err(-16);
            }
            if data.write_at(0, &v) != v.len() {
                return Err(-12);
            }
        }
        return Ok(Arc::new(OverlayFile::new(data, flags)));
    }

    let upper = overlay_get(name);
    match upper {
        Some(OverlayEntry::Directory(identity)) => {
            return open_directory(name, flags, DirectoryIdentity::Upper(identity));
        }
        Some(OverlayEntry::File(data)) => {
            if exclusive || directory {
                return Err(if exclusive { -17 } else { -20 });
            }
            return open_overlay(data, flags);
        }
        _ => {}
    }

    if let Some(fs) = EXT4.as_ref() {
        let inode = if upper.is_none() { lower_inode(name) } else { None };
        if let Some(inode) = inode {
            if exclusive {
                return Err(-17);
            }
            if inode.is_dir() {
                return open_directory(name, flags, DirectoryIdentity::Lower);
            }
            if directory {
                return Err(-20);
            }
            if !create {
                if flags.intersects(OpenFlags::WRONLY | OpenFlags::RDWR | OpenFlags::TRUNC) {
                    return Err(-30);
                }
                return Ok(Arc::new(ext4::Ext4File::new(Arc::clone(fs), inode)));
            }
        }
        require_parent_directories(name)?;
        if !create || directory {
            return Err(-2);
        }
        open_overlay(overlay_create(name), flags)
    } else {
        let exists = || open_file(name, OpenFlags::RDONLY).is_some();
        if directory {
            return Err(if exists() { -20 } else { -2 });
        }
        if exclusive && exists() {
            return Err(-17);
        }
        open_file(name, flags).map(|i| i as Arc<dyn File + Send + Sync>).ok_or(-2)
    }
}

/// Resolve against a canonical base. Check any prefix that a dot component would hide.
pub fn resolve_from(name: &str, base: &str) -> Result<String, isize> {
    let mut parts = Vec::new();
    if !name.starts_with('/') {
        parts.extend(base.split('/').filter(|part| !part.is_empty()));
    }
    let mut unchecked = false;
    for part in name.split('/') {
        match part {
            "" => {}
            "." | ".." => {
                if unchecked && has_ext4() {
                    require_directory(&parts.join("/"))?;
                }
                if part == ".." {
                    parts.pop();
                }
                unchecked = false;
            }
            _ => {
                parts.push(part);
                unchecked = true;
            }
        }
    }
    Ok(parts.join("/"))
}

pub fn resolve_path(name: &str) -> Result<String, isize> {
    if name.starts_with('/') {
        return resolve_from(name, "");
    }
    let cwd = crate::task::current_process().inner_exclusive_access().cwd.clone();
    if !directory_is_live(&cwd) {
        return Err(-2);
    }
    resolve_from(name, cwd.path())
}

pub fn exists(name: &str) -> bool {
    path_is_directory(name).is_ok() || virtual_file(name).is_some()
}

pub fn mkdir(name: &str) -> Result<(), isize> {
    if EXT4.is_none() {
        // easy-fs 本身只有扁平目录，保留模拟实现。
        return open_file(name, OpenFlags::CREATE | OpenFlags::RDONLY).map(|_| ()).ok_or(-1);
    }
    require_parent_directories(name)?;
    if exists(name) {
        return Err(-17); // EEXIST
    }
    overlay_mkdir(name);
    Ok(())
}

pub fn unlink(name: &str, remove_directory: bool) -> Result<(), isize> {
    if EXT4.is_none() {
        if remove_directory {
            return Err(-20);
        }
        if !exists(name) {
            return Err(-2);
        }
        mark_unlinked(name);
        return Ok(());
    }

    require_parent_directories(name)?;
    let is_directory = path_is_directory(name)?;
    match (is_directory, remove_directory) {
        (true, false) => return Err(-21), // EISDIR
        (false, true) => return Err(-20), // ENOTDIR
        (true, true) => {
            if is_root_path(name) || matches!(name, "proc" | "dev") {
                return Err(-16); // EBUSY
            }
            if list_dir_path(name).iter().any(|(child, _, _)| child != "." && child != "..") {
                return Err(-39); // ENOTEMPTY
            }
        }
        (false, false) => {}
    }
    overlay_unlink(name, lower_inode(name).is_some());
    Ok(())
}

pub fn rename_overlay_file(old: &str, new: &str) -> Result<(), isize> {
    if EXT4.is_some() {
        require_parent_directories(old)?;
        require_parent_directories(new)?;
    }
    if path_is_directory(new) == Ok(true) {
        return Err(-21);
    }
    if overlay_rename(old, new, lower_inode(old).is_some()) {
        Ok(())
    } else {
        Err(-2)
    }
}

/// 根目录下的名字列表
pub fn names() -> Vec<String> {
    list_dir_path("").into_iter().map(|(name, _, _)| name).collect()
}
pub use stdio::{Stdin, Stdout};

/// 最小 /proc 虚拟文件（供 busybox 的 df/free/uptime 等使用）
pub fn virtual_file(name: &str) -> Option<Vec<u8>> {
    let n = name.trim_start_matches("./").trim_start_matches('/');
    let text: &str = match n {
        "proc/meminfo" => {
            "MemTotal:         262144 kB\nMemFree:          200000 kB\nMemAvailable:     200000 kB\nBuffers:               0 kB\nCached:                0 kB\n"
        }
        "proc/loadavg" => "0.00 0.00 0.00 1/1 1\n",
        "proc/uptime" => "100.00 100.00\n",
        "proc/version" => "Linux version 6.0.0-rcore (rcore@local) #1\n",
        "proc/stat" => "cpu  0 0 0 0 0 0 0 0 0 0\n",
        "proc/cpuinfo" => "processor\t: 0\nisa\t\t: rv64imafdc\n",
        "proc/filesystems" => "nodev\tproc\nnodev\tsysfs\n\text4\n",
        "proc/mounts" => "/dev/root / ext4 rw,relatime 0 0\nproc /proc proc rw 0 0\n",
        _ => {
            // /proc/<pid>/{stat,cmdline,status}
            if let Some(rest) = n.strip_prefix("proc/") {
                let mut it = rest.splitn(2, '/');
                let pid = it.next().unwrap_or("");
                let file = it.next().unwrap_or("");
                if pid.chars().all(|c| c.is_ascii_digit()) && !pid.is_empty() {
                    let p: u32 = pid.parse().unwrap_or(1);
                    return Some(match file {
                        "stat" => Vec::from(
                            alloc::format!(
                                "{} (init) S 1 1 1 0 -1 0 0 0 0 0 10 10 0 0 20 0 1 0 1 0 100 0 0\n",
                                p
                            )
                            .as_bytes(),
                        ),
                        "cmdline" => Vec::from(alloc::format!("init\0").as_bytes()),
                        "status" => Vec::from(
                            alloc::format!("Name:\tinit\nPid:\t{}\nState:\tS\n", p).as_bytes(),
                        ),
                        _ => return None,
                    });
                }
            }
            return None;
        }
    };
    Some(Vec::from(text.as_bytes()))
}

/// /proc 下有哪些文件
pub const PROC_FILES: &[&str] = &[
    "meminfo", "loadavg", "uptime", "version", "stat", "cpuinfo", "filesystems", "mounts",
];

/// 列出某个目录下的条目（名字, inode, 类型）
pub fn list_dir_path(name: &str) -> Vec<(String, u32, u8)> {
    let name = name.trim_start_matches("./").trim_start_matches('/');
    if name == "dev" {
        return alloc::vec![
            (String::from("null"), 301u32, 1u8),
            (String::from("zero"), 302u32, 1u8),
            (String::from("tty"), 303u32, 1u8),
        ];
    }
    if name == "proc" {
        let mut v: Vec<(String, u32, u8)> = PROC_FILES
            .iter()
            .enumerate()
            .map(|(i, n)| (String::from(*n), (100 + i) as u32, 1u8))
            .collect();
        // 伪进程目录，供 ps 使用
        v.push((String::from("1"), 201, 2));
        v.push((String::from("2"), 202, 2));
        return v;
    }
    if let Some(fs) = EXT4.as_ref() {
        let mut entries = match overlay_get(name) {
            Some(OverlayEntry::Directory(_)) => {
                alloc::vec![(String::from("."), 1, 2), (String::from(".."), 1, 2)]
            }
            Some(_) => return Vec::new(),
            None => match lower_inode(name).filter(ext4::Inode::is_dir) {
                Some(inode) => fs.list_dir(&inode),
                None => return Vec::new(),
            },
        };
        let mut children = overlay_children(name);
        entries.retain_mut(|(child, _, file_type)| match children.remove(child) {
            Some(entry) => match entry.dirent_type() {
                Some(kind) => {
                    *file_type = kind;
                    true
                }
                None => false,
            },
            None => true,
        });
        entries.extend(children.into_iter().filter_map(|(child, entry)| {
            entry.dirent_type().map(|file_type| (child, 1, file_type))
        }));
        return entries;
    }
    // easy-fs：只有扁平根目录
    root_names()
        .into_iter()
        .enumerate()
        .map(|(i, n)| (n, (i + 1) as u32, 1u8))
        .collect()
}

/// 在 open_dyn 里处理根目录等特殊路径
pub fn is_root_path(name: &str) -> bool {
    let n = name.trim_start_matches("./").trim_start_matches('/');
    n.is_empty() || n == "."
}
