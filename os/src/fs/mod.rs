pub mod ext4;
mod inode;
mod overlay;
pub(crate) mod page_cache;
mod pipe;
mod stdio;

use crate::mm::{FrameTracker, UserBuffer};
use overlay::OverlayFile;

pub trait File: Send + Sync {
    fn readable(&self) -> bool;
    fn writable(&self) -> bool;
    fn read(&self, buf: UserBuffer) -> usize;
    fn write(&self, buf: UserBuffer) -> usize;
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
pub use overlay::{overlay_create, overlay_get, overlay_remove, overlay_rename};
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
    fn write(&self, buf: UserBuffer) -> usize {
        buf.len()
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
    fn write(&self, buf: UserBuffer) -> usize {
        buf.len()
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
    if let Some(d) = overlay_get(name) {
        return Some(d.exclusive_access().read_all());
    }
    if let Some(fs) = EXT4.as_ref() {
        let inode = fs.lookup(name)?;
        Some(fs.read_file(&inode))
    } else {
        open_file(name, OpenFlags::RDONLY).map(|i| i.read_all())
    }
}

/// 打开文件并返回统一的 File 对象（供系统调用使用）
pub fn open_dyn(name: &str, flags: OpenFlags) -> Result<Arc<dyn File + Send + Sync>, isize> {
    // 虚拟设备文件：/dev/null 这类被 lmbench / 测试大量使用，原来 ENOENT
    let dev = name.trim_start_matches("./").trim_start_matches('/');
    match dev {
        "dev/null" => return Ok(Arc::new(NullFile)),
        "dev/zero" => return Ok(Arc::new(ZeroFile)),
        "dev/tty" | "dev/console" => return Ok(Arc::new(Stdout)),
        _ => {}
    }
    if let Some(v) = virtual_file(name) {
        let d = overlay_create(name);
        {
            let mut data = d.exclusive_access();
            if !data.resize(v.len()) {
                return Err(-16);
            }
            if data.write_at(0, &v) != v.len() {
                return Err(-12);
            }
        }
        return Ok(Arc::new(OverlayFile::new(d, flags)));
    }
    // 根目录用一个伪文件代表，供 getdents 使用
    if is_root_path(name) && EXT4.is_some() {
        return Ok(Arc::new(OverlayFile::root(flags)));
    }
    if let Some(d) = overlay_get(name) {
        if flags.contains(OpenFlags::TRUNC) && !d.exclusive_access().resize(0) {
            return Err(-16);
        }
        return Ok(Arc::new(OverlayFile::new(d, flags)));
    }
    if let Some(fs) = EXT4.as_ref() {
        if flags.contains(OpenFlags::CREATE) {
            return Ok(Arc::new(OverlayFile::new(overlay_create(name), flags)));
        }
        if flags.contains(OpenFlags::WRONLY) {
            return Err(-30);
        }
        let inode = fs.lookup(name).ok_or(-2isize)?;
        Ok(Arc::new(ext4::Ext4File::new(Arc::clone(fs), inode)))
    } else {
        open_file(name, flags).map(|i| i as Arc<dyn File + Send + Sync>).ok_or(-2)
    }
}

/// 根目录是否存在某文件
/// 把相对路径解析成 EXT4/easy-fs 里的实际路径（拼上进程 cwd）
pub fn resolve_path(name: &str) -> String {
    let name = name.trim_start_matches("./");
    if name.starts_with('/') {
        return String::from(name.trim_start_matches('/'));
    }
    let cwd = crate::task::current_process().inner_exclusive_access().cwd.clone();
    if cwd.is_empty() || cwd == "/" {
        String::from(name)
    } else {
        let mut s = cwd;
        s.push('/');
        s.push_str(name);
        s
    }
}

pub fn exists(name: &str) -> bool {
    if overlay_get(name).is_some() {
        return true;
    }
    if let Some(fs) = EXT4.as_ref() {
        fs.lookup(name).is_some()
    } else {
        open_file(name, OpenFlags::RDONLY).is_some()
    }
}

/// 根目录下的名字列表
pub fn names() -> Vec<String> {
    if let Some(fs) = EXT4.as_ref() {
        match fs.inode(2) {
            Some(root) => fs.list_dir(&root).into_iter().map(|(n, _, _)| n).collect(),
            None => Vec::new(),
        }
    } else {
        root_names()
    }
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

/// 路径是否是目录
pub fn is_dir_path(name: &str) -> bool {
    let name = name.trim_start_matches("./").trim_start_matches('/');
    if name.is_empty() || name == "." {
        return true;
    }
    if name == "proc" || name == "dev" {
        return true;
    }
    // 性能：overlay 里有的文件直接是普通文件，不要再去 EXT4 目录里查
    // （EXT4 miss 在 QEMU 下要读好几个块，stat 一次能慢到 2ms，直接把 lmbench 拖死）
    if overlay_get(name).is_some() {
        return false;
    }
    if let Some(fs) = EXT4.as_ref() {
        return fs.lookup(name).map(|i| i.is_dir()).unwrap_or(false);
    }
    false
}

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
        let inode = if name.is_empty() || name == "." {
            fs.inode(2)
        } else {
            fs.lookup(name)
        };
        if let Some(ino) = inode {
            return fs.list_dir(&ino);
        }
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
