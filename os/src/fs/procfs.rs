use super::DirectoryIdentity;
use crate::task::TaskStatus;
use alloc::{format, string::String, sync::Arc, vec::Vec};

const PROCESS_FILES: &[&str] = &["stat", "status", "cmdline", "mounts", "maps", "cwd", "exe"];
const PROCESS_DIRS: &[&str] = &["fd", "task"];

pub(super) fn memory_info() -> Vec<u8> {
    let total = crate::mm::ram_end().saturating_sub(crate::arch::RAM_BASE) / 1024;
    let free = crate::mm::free_frame_count() * (crate::config::PAGE_SIZE / 1024);
    format!(
        "MemTotal:       {:8} kB\nMemFree:        {:8} kB\nMemAvailable:   {:8} kB\n",
        total, free, free
    )
    .into_bytes()
}

pub(super) fn cpu_info() -> Vec<u8> {
    #[cfg(feature = "smp")]
    let mask = crate::arch::online_hart_mask();
    #[cfg(not(feature = "smp"))]
    let mask = 1usize << crate::arch::hart_id();
    let mut info = String::new();
    for hart in (0..usize::BITS).filter(|&hart| mask & (1usize << hart) != 0) {
        use core::fmt::Write;
        let _ = writeln!(
            info,
            "processor\t: {}\nmodel name\t: {}\n",
            hart,
            crate::arch::NAME
        );
    }
    info.into_bytes()
}

fn parse_decimal(s: &str) -> Option<usize> {
    if s.is_empty()
        || !s.bytes().all(|byte| byte.is_ascii_digit())
        || (s.len() > 1 && s.starts_with('0'))
    {
        return None;
    }
    s.parse().ok()
}

fn pid_path(path: &str) -> Option<(usize, &str)> {
    let rest = path.strip_prefix("proc/")?;
    let (pid, entry) = rest.split_once('/').unwrap_or((rest, ""));
    Some((parse_decimal(pid)?, entry))
}

fn resolve_task_entry(pid: usize, entry: &str) -> Option<(usize, &str)> {
    if let Some(rest) = entry.strip_prefix("task/") {
        let (tid_str, sub) = rest.split_once('/').unwrap_or((rest, ""));
        let tid = parse_decimal(tid_str)?;
        let process = crate::task::pid2process(pid)?;
        let inner = process.inner_exclusive_access();
        let has_tid = inner.tasks.iter().flatten().any(|t| {
            t.linux_tid() == tid && t.inner_exclusive_access().exit_code.is_none()
        });
        has_tid.then_some((tid, sub))
    } else {
        Some((pid, entry))
    }
}

fn sysctl_value(path: &str) -> Option<&'static str> {
    match path {
        "proc/sys/kernel/pid_max" => Some("32768\n"),
        "proc/sys/kernel/threads-max" => Some("4096\n"),
        "proc/sys/kernel/tainted" => Some("0\n"),
        "proc/sys/kernel/osrelease" => Some("6.0.0-rcore\n"),
        "proc/sys/kernel/ostype" => Some("Linux\n"),
        "proc/sys/kernel/ngroups_max" => Some("65536\n"),
        "proc/sys/kernel/overflowuid" => Some("65534\n"),
        "proc/sys/kernel/overflowgid" => Some("65534\n"),
        "proc/sys/fs/file-max" => Some("65536\n"),
        "proc/sys/fs/pipe-max-size" => Some("1048576\n"),
        "proc/sys/vm/overcommit_memory" => Some("0\n"),
        "proc/sys/vm/max_map_count" => Some("65530\n"),
        _ => None,
    }
}

pub(super) fn is_directory(path: &str) -> bool {
    directory_identity(path).is_some()
}

pub(super) fn directory_identity(path: &str) -> Option<DirectoryIdentity> {
    if matches!(
        path,
        "proc" | "proc/sys" | "proc/sys/kernel" | "proc/sys/fs" | "proc/sys/vm" | "proc/sysvipc"
    ) {
        return Some(DirectoryIdentity::Permanent);
    }
    let (pid, entry) = pid_path(path)?;
    let process = crate::task::pid2process(pid)?;
    if entry.is_empty() || entry == "fd" || entry == "task" {
        return Some(DirectoryIdentity::Proc(Arc::downgrade(&process)));
    }
    let (_, sub) = resolve_task_entry(pid, entry)?;
    if sub.is_empty() || sub == "fd" {
        return Some(DirectoryIdentity::Proc(Arc::downgrade(&process)));
    }
    None
}

pub(super) fn symlink_target(path: &str) -> Option<Vec<u8>> {
    if path == "proc/self" {
        let task = crate::task::current_task()?;
        return Some(format!("{}", task.process.upgrade()?.getpid()).into_bytes());
    }
    if path == "proc/thread-self" {
        let task = crate::task::current_task()?;
        let pid = task.process.upgrade()?.getpid();
        return Some(format!("{}/task/{}", pid, task.linux_tid()).into_bytes());
    }
    let (pid, raw_entry) = pid_path(path)?;
    let (_, entry) = resolve_task_entry(pid, raw_entry)?;
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    if let Some(fd_str) = entry.strip_prefix("fd/") {
        let fd = parse_decimal(fd_str)?;
        let file = inner.fd_table.get(fd)?.as_ref()?.clone();
        drop(inner);
        if let Some(dir) = file.directory() {
            let p = dir.location().path();
            return Some(format!("/{}", p.trim_start_matches('/')).into_bytes());
        }
        if file.is_tty() {
            return Some(Vec::from(&b"/dev/console"[..]));
        }
        if let Some((dev, ino)) = file.stat_identity() {
            if dev == super::DEV_DEVICE {
                let dev_name = match ino {
                    301 => Some("/dev/null"),
                    302 => Some("/dev/zero"),
                    303 => Some("/dev/tty"),
                    304 => Some("/dev/console"),
                    306 => Some("/dev/random"),
                    307 => Some("/dev/urandom"),
                    _ => None,
                };
                if let Some(name) = dev_name {
                    return Some(Vec::from(name.as_bytes()));
                }
            }
        }
        if file.socket().is_some() || file.unix_socket().is_some() {
            return Some(format!("socket:[{}]", fd).into_bytes());
        }
        return Some(format!("anon_inode:[fd-{}]", fd).into_bytes());
    }
    match entry {
        "cwd" => {
            let cwd = inner.cwd.clone();
            drop(inner);
            super::directory_is_live(&cwd).then(|| format!("/{}", cwd.path()).into_bytes())
        }
        "exe" => Some(format!("/{}", inner.executable.as_deref()?).into_bytes()),
        _ => None,
    }
}

pub(super) fn process_file_exists(path: &str) -> bool {
    if sysctl_value(path).is_some()
        || matches!(path, "proc/sysvipc/shm" | "proc/sysvipc/sem" | "proc/sysvipc/msg")
    {
        return true;
    }
    pid_path(path).map_or(false, |(pid, raw_entry)| {
        resolve_task_entry(pid, raw_entry).map_or(false, |(_, entry)| {
            matches!(entry, "stat" | "status" | "cmdline" | "mounts" | "maps")
                && crate::task::pid2process(pid).is_some()
        })
    })
}

pub(super) fn snapshot(path: &str) -> Option<Vec<u8>> {
    if let Some(val) = sysctl_value(path) {
        return Some(Vec::from(val.as_bytes()));
    }
    if let Some(ipc_kind) = path.strip_prefix("proc/sysvipc/") {
        return crate::syscall::sysvipc::proc_snapshot(ipc_kind);
    }
    let (pid, raw_entry) = pid_path(path)?;
    let (tid, entry) = resolve_task_entry(pid, raw_entry)?;
    if !matches!(entry, "stat" | "status" | "cmdline" | "mounts" | "maps") {
        return None;
    }
    if entry == "mounts" {
        return Some(super::mount::mounts_snapshot());
    }
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    if entry == "cmdline" {
        return Some(inner.cmdline.as_deref().unwrap_or_default().to_vec());
    }
    if entry == "maps" {
        return Some(inner.memory_set.maps_snapshot().into_bytes());
    }
    let name = inner
        .executable
        .as_deref()
        .and_then(|path| path.rsplit('/').next())
        .unwrap_or("init");
    let parent = inner
        .parent
        .as_ref()
        .and_then(|parent| parent.upgrade())
        .map_or(0, |parent| parent.getpid());
    let threads = inner.thread_count();
    let state = if inner.is_zombie {
        'Z'
    } else if inner.job_stopped {
        'T'
    } else if inner.tasks.iter().flatten().any(|task| {
        let task = task.inner_exclusive_access();
        task.exit_code.is_none()
            && matches!(task.task_status, TaskStatus::Running | TaskStatus::Ready)
    }) {
        'R'
    } else {
        'S'
    };
    let text = if entry == "status" {
        let groups = inner
            .groups
            .iter()
            .map(|g| format!("{}", g))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nUid:\t{}\t{}\t{}\t{}\nGid:\t{}\t{}\t{}\t{}\nGroups:\t{}\nThreads:\t{}\n",
            name,
            state,
            pid,
            tid,
            parent,
            inner.uid,
            inner.euid,
            inner.suid,
            inner.fsuid,
            inner.gid,
            inner.egid,
            inner.sgid,
            inner.fsgid,
            groups,
            threads
        )
    } else {
        let mut fields = ["0"; 49];
        let parent = format!("{}", parent);
        let pgrp = format!("{}", inner.pgid.0);
        let session = format!("{}", inner.sid.0);
        let threads = format!("{}", threads);
        fields[0] = &parent;
        fields[1] = &pgrp;
        fields[2] = &session;
        fields[16] = &threads;
        format!("{} ({}) {} {}\n", tid, name, state, fields.join(" "))
    };
    Some(text.into_bytes())
}

pub(super) fn entries(path: &str) -> Option<Vec<(String, u32, u8)>> {
    match path {
        "proc" => {
            let mut entries: Vec<_> = super::PROC_FILES
                .iter()
                .enumerate()
                .map(|(i, name)| (String::from(*name), 100 + i as u32, 1))
                .collect();
            entries.push((String::from("sys"), 118, 2));
            entries.push((String::from("sysvipc"), 119, 2));
            entries.push((String::from("self"), 120, 7));
            entries.push((String::from("thread-self"), 121, 7));
            entries.extend(
                crate::task::process_ids()
                    .into_iter()
                    .map(|pid| (format!("{}", pid), 1000 + pid as u32, 2)),
            );
            return Some(entries);
        }
        "proc/sys" => {
            return Some(alloc::vec![
                (String::from("."), 200, 2),
                (String::from(".."), 1, 2),
                (String::from("kernel"), 201, 2),
                (String::from("fs"), 202, 2),
                (String::from("vm"), 203, 2),
            ]);
        }
        "proc/sys/kernel" => {
            return Some(alloc::vec![
                (String::from("."), 201, 2),
                (String::from(".."), 200, 2),
                (String::from("pid_max"), 210, 1),
                (String::from("threads-max"), 211, 1),
                (String::from("tainted"), 212, 1),
                (String::from("osrelease"), 213, 1),
                (String::from("ostype"), 214, 1),
                (String::from("ngroups_max"), 215, 1),
                (String::from("overflowuid"), 216, 1),
                (String::from("overflowgid"), 217, 1),
            ]);
        }
        "proc/sys/fs" => {
            return Some(alloc::vec![
                (String::from("."), 202, 2),
                (String::from(".."), 200, 2),
                (String::from("file-max"), 220, 1),
                (String::from("pipe-max-size"), 221, 1),
            ]);
        }
        "proc/sys/vm" => {
            return Some(alloc::vec![
                (String::from("."), 203, 2),
                (String::from(".."), 200, 2),
                (String::from("overcommit_memory"), 230, 1),
                (String::from("max_map_count"), 231, 1),
            ]);
        }
        "proc/sysvipc" => {
            return Some(alloc::vec![
                (String::from("."), 119, 2),
                (String::from(".."), 1, 2),
                (String::from("shm"), 240, 1),
                (String::from("sem"), 241, 1),
                (String::from("msg"), 242, 1),
            ]);
        }
        _ => {}
    }
    let (pid, raw_entry) = pid_path(path)?;
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    if raw_entry == "task" {
        let mut list = alloc::vec![
            (String::from("."), 1300, 2),
            (String::from(".."), 1000 + pid as u32, 2),
        ];
        for task in inner.tasks.iter().flatten() {
            if task.inner_exclusive_access().exit_code.is_none() {
                let tid = task.linux_tid();
                list.push((format!("{}", tid), 3000 + tid as u32, 2));
            }
        }
        return Some(list);
    }
    let (_, entry) = resolve_task_entry(pid, raw_entry)?;
    if entry == "fd" {
        let mut list = alloc::vec![
            (String::from("."), 1400, 2),
            (String::from(".."), 1000 + pid as u32, 2),
        ];
        for (fd, slot) in inner.fd_table.iter().enumerate() {
            if slot.is_some() {
                list.push((format!("{}", fd), 2000 + fd as u32, 7));
            }
        }
        return Some(list);
    }
    if !entry.is_empty() {
        return None;
    }
    let mut out: Vec<(String, u32, u8)> = PROCESS_DIRS
        .iter()
        .enumerate()
        .map(|(i, name)| (String::from(*name), 1190 + i as u32, 2))
        .collect();
    out.extend(
        PROCESS_FILES
            .iter()
            .enumerate()
            .filter(|(_, name)| **name != "exe" || inner.executable.is_some())
            .map(|(i, name)| {
                (
                    String::from(*name),
                    1200 + i as u32,
                    if matches!(*name, "cwd" | "exe") { 7 } else { 1 },
                )
            }),
    );
    Some(out)
}
