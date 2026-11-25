use super::DirectoryIdentity;
use crate::task::TaskStatus;
use alloc::{format, string::String, sync::Arc, vec::Vec};

const PROCESS_FILES: &[&str] = &["stat", "status", "cmdline", "cwd", "exe"];

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

fn pid_path(path: &str) -> Option<(usize, &str)> {
    let rest = path.strip_prefix("proc/")?;
    let (pid, entry) = rest.split_once('/').unwrap_or((rest, ""));
    if pid.is_empty()
        || !pid.bytes().all(|byte| byte.is_ascii_digit())
        || (pid.len() > 1 && pid.starts_with('0'))
    {
        return None;
    }
    Some((pid.parse().ok()?, entry))
}

pub(super) fn is_directory(path: &str) -> bool {
    directory_identity(path).is_some()
}

pub(super) fn directory_identity(path: &str) -> Option<DirectoryIdentity> {
    if path == "proc" {
        return Some(DirectoryIdentity::Permanent);
    }
    let (pid, entry) = pid_path(path)?;
    if !entry.is_empty() {
        return None;
    }
    Some(DirectoryIdentity::Proc(Arc::downgrade(
        &crate::task::pid2process(pid)?,
    )))
}

pub(super) fn symlink_target(path: &str) -> Option<Vec<u8>> {
    if path == "proc/self" {
        let task = crate::task::current_task()?;
        return Some(format!("{}", task.process.upgrade()?.getpid()).into_bytes());
    }
    let (pid, entry) = pid_path(path)?;
    if !matches!(entry, "cwd" | "exe") {
        return None;
    }
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
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
    pid_path(path).map_or(false, |(pid, entry)| {
        matches!(entry, "stat" | "status" | "cmdline") && crate::task::pid2process(pid).is_some()
    })
}

pub(super) fn snapshot(path: &str) -> Option<Vec<u8>> {
    let (pid, entry) = pid_path(path)?;
    if !matches!(entry, "stat" | "status" | "cmdline") {
        return None;
    }
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    if entry == "cmdline" {
        return Some(inner.cmdline.as_deref().unwrap_or_default().to_vec());
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
    let state = if inner.tasks.iter().flatten().any(|task| {
        let task = task.inner_exclusive_access();
        task.exit_code.is_none()
            && matches!(task.task_status, TaskStatus::Running | TaskStatus::Ready)
    }) {
        'R'
    } else {
        'S'
    };
    let text = if entry == "status" {
        format!(
            "Name:\t{}\nState:\t{}\nTgid:\t{}\nPid:\t{}\nPPid:\t{}\nThreads:\t{}\n",
            name, state, pid, pid, parent, threads
        )
    } else {
        let mut fields = ["0"; 49];
        let parent = format!("{}", parent);
        let threads = format!("{}", threads);
        fields[0] = &parent;
        fields[16] = &threads;
        format!("{} ({}) {} {}\n", pid, name, state, fields.join(" "))
    };
    Some(text.into_bytes())
}

pub(super) fn entries(path: &str) -> Option<Vec<(String, u32, u8)>> {
    if path == "proc" {
        let mut entries: Vec<_> = super::PROC_FILES
            .iter()
            .enumerate()
            .map(|(i, name)| (String::from(*name), 100 + i as u32, 1))
            .collect();
        entries.push((String::from("self"), 120, 7));
        entries.extend(
            crate::task::process_ids()
                .into_iter()
                .map(|pid| (format!("{}", pid), 1000 + pid as u32, 2)),
        );
        return Some(entries);
    }
    let (pid, entry) = pid_path(path)?;
    if !entry.is_empty() {
        return None;
    }
    let process = crate::task::pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    Some(
        PROCESS_FILES
            .iter()
            .enumerate()
            .filter(|(_, name)| **name != "exe" || inner.executable.is_some())
            .map(|(i, name)| {
                (
                    String::from(*name),
                    1200 + i as u32,
                    if i >= 3 { 7 } else { 1 },
                )
            })
            .collect(),
    )
}
