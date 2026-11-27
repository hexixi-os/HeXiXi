//! Advisory file locks (`flock` and `fcntl` POSIX/OFD record locks) and per-description owner/lease state.

use crate::fs::File;
use crate::mm::{translated_byte_buffer, MapPermission, UserBuffer};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_process, current_task, current_user_token, WaitQueue};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

const LOCK_SH: usize = 1;
const LOCK_EX: usize = 2;
const LOCK_NB: usize = 4;
const LOCK_UN: usize = 8;

const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

const F_SETOWN: usize = 8;
const F_GETOWN: usize = 9;
const F_SETSIG: usize = 10;
const F_GETSIG: usize = 11;
const F_SETOWN_EX: usize = 15;
const F_GETOWN_EX: usize = 16;
const F_SETLEASE: usize = 1024;
const F_GETLEASE: usize = 1025;

#[derive(Clone, Copy, PartialEq, Eq)]
enum OwnerKey {
    PosixPid(usize),
    Ofd(usize),
}

#[derive(Clone)]
struct RecordLock {
    owner: OwnerKey,
    pid: i32,
    start: i64,
    end: i64, // inclusive; i64::MAX means to infinity
    kind: i16,
}

#[derive(Default)]
struct InodeLockState {
    records: Vec<RecordLock>,
    waiters: Option<Arc<WaitQueue>>,
}

impl InodeLockState {
    fn wq(&mut self) -> Arc<WaitQueue> {
        self.waiters.get_or_insert_with(WaitQueue::new).clone()
    }
}

struct FlockEntry {
    holders: BTreeMap<usize, bool>, // ofd_key -> exclusive
    waiters: Arc<WaitQueue>,
}

#[derive(Clone, Copy)]
struct FileExtraState {
    owner_type: i32,
    owner_pid: i32,
    sig: i32,
    lease: i16,
}

impl Default for FileExtraState {
    fn default() -> Self {
        Self {
            owner_type: 0,
            owner_pid: 0,
            sig: 0,
            lease: F_UNLCK,
        }
    }
}

lazy_static! {
    static ref RECORD_LOCKS: UPSafeCell<BTreeMap<(u64, u64), InodeLockState>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    static ref FLOCK_TABLE: UPSafeCell<BTreeMap<(u64, u64), FlockEntry>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    static ref FILE_EXTRA: UPSafeCell<BTreeMap<usize, FileExtraState>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
    // Waiting edge: pid -> set of pids blocking it (for EDEADLK detection)
    static ref WAIT_FOR: UPSafeCell<BTreeMap<usize, Vec<usize>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

fn ofd_key(file: &Arc<dyn File + Send + Sync>) -> usize {
    Arc::as_ptr(file) as *const () as usize
}

fn file_identity(file: &Arc<dyn File + Send + Sync>) -> (u64, u64) {
    file.stat_identity().unwrap_or((u64::MAX, ofd_key(file) as u64))
}

pub fn sys_flock(fd: usize, op: usize) -> isize {
    let file = match super::fs::file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if file.path_only() {
        return -9;
    }
    let nonblock = op & LOCK_NB != 0;
    let cmd = op & !LOCK_NB;
    if !matches!(cmd, LOCK_SH | LOCK_EX | LOCK_UN) {
        return -22;
    }
    let key = file_identity(&file);
    let me = ofd_key(&file);
    if cmd == LOCK_UN {
        let mut table = FLOCK_TABLE.exclusive_access();
        if let Some(entry) = table.get_mut(&key) {
            if entry.holders.remove(&me).is_some() {
                let wq = entry.waiters.clone();
                if entry.holders.is_empty() {
                    table.remove(&key);
                }
                drop(table);
                wq.wake_all();
            }
        }
        return 0;
    }
    let want_ex = cmd == LOCK_EX;
    loop {
        let mut table = FLOCK_TABLE.exclusive_access();
        let entry = table.entry(key).or_insert_with(|| FlockEntry {
            holders: BTreeMap::new(),
            waiters: WaitQueue::new(),
        });
        let conflict = entry.holders.iter().any(|(&holder, &is_ex)| {
            holder != me && (want_ex || is_ex)
        });
        if !conflict {
            entry.holders.insert(me, want_ex);
            let wq = entry.waiters.clone();
            drop(table);
            if !want_ex {
                wq.wake_all();
            }
            return 0;
        }
        if nonblock {
            return -11; // EWOULDBLOCK / EAGAIN
        }
        let wq = entry.waiters.clone();
        if !wq.register_current_interruptible() {
            return -4;
        }
        drop(table);
        block_current_and_run_next();
        if let Some(t) = current_task() {
            WaitQueue::cancel(&t);
        }
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4;
        }
    }
}

fn read_flock_struct(arg: usize) -> Result<(i16, i16, i64, i64, i32), isize> {
    let token = current_user_token();
    let bufs = translated_byte_buffer(token, arg as *const u8, 32, MapPermission::R).ok_or(-14isize)?;
    let mut raw = [0u8; 32];
    for (dst, src) in raw.iter_mut().zip(UserBuffer::new(bufs)) {
        *dst = src.read();
    }
    let l_type = i16::from_ne_bytes([raw[0], raw[1]]);
    let l_whence = i16::from_ne_bytes([raw[2], raw[3]]);
    let l_start = i64::from_ne_bytes(raw[8..16].try_into().unwrap());
    let l_len = i64::from_ne_bytes(raw[16..24].try_into().unwrap());
    let l_pid = i32::from_ne_bytes(raw[24..28].try_into().unwrap());
    Ok((l_type, l_whence, l_start, l_len, l_pid))
}

fn write_flock_struct(arg: usize, l_type: i16, l_whence: i16, l_start: i64, l_len: i64, l_pid: i32) -> Result<(), isize> {
    let token = current_user_token();
    let bufs = translated_byte_buffer(token, arg as *const u8, 32, MapPermission::W).ok_or(-14isize)?;
    let mut raw = [0u8; 32];
    raw[0..2].copy_from_slice(&l_type.to_ne_bytes());
    raw[2..4].copy_from_slice(&l_whence.to_ne_bytes());
    raw[8..16].copy_from_slice(&l_start.to_ne_bytes());
    raw[16..24].copy_from_slice(&l_len.to_ne_bytes());
    raw[24..28].copy_from_slice(&l_pid.to_ne_bytes());
    UserBuffer::new(bufs).copy_from(&raw);
    Ok(())
}

fn compute_range(file: &Arc<dyn File + Send + Sync>, whence: i16, start: i64, len: i64) -> Result<(i64, i64), isize> {
    let base: i64 = match whence {
        0 => 0,
        1 => {
            let cur = file.seek(0);
            if cur < 0 { 0 } else { cur as i64 }
        }
        2 => file.size() as i64,
        _ => return Err(-22),
    };
    let s = base.checked_add(start).ok_or(-75isize)?; // EOVERFLOW
    if len == 0 {
        if s < 0 {
            return Err(-22);
        }
        Ok((s, i64::MAX))
    } else if len > 0 {
        if s < 0 {
            return Err(-22);
        }
        let end = s.checked_add(len - 1).ok_or(-75isize)?;
        Ok((s, end))
    } else {
        // Negative l_len locks [s + len, s - 1]
        let real_start = s.checked_add(len).ok_or(-22isize)?;
        if real_start < 0 {
            return Err(-22);
        }
        Ok((real_start, s - 1))
    }
}

fn overlaps(a_start: i64, a_end: i64, b_start: i64, b_end: i64) -> bool {
    a_start <= b_end && b_start <= a_end
}

fn conflicts(existing: &RecordLock, owner: OwnerKey, start: i64, end: i64, kind: i16) -> bool {
    if existing.owner == owner || !overlaps(existing.start, existing.end, start, end) {
        return false;
    }
    existing.kind == F_WRLCK || kind == F_WRLCK
}

pub fn fcntl_getlk(_fd: usize, file: &Arc<dyn File + Send + Sync>, arg: usize, ofd: bool) -> isize {
    if file.path_only() {
        return -9;
    }
    let (l_type, l_whence, l_start, l_len, l_pid) = match read_flock_struct(arg) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !matches!(l_type, F_RDLCK | F_WRLCK) || (ofd && l_pid != 0) {
        return -22;
    }
    let (start, end) = match compute_range(file, l_whence, l_start, l_len) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let pid = current_process().getpid();
    let owner = if ofd { OwnerKey::Ofd(ofd_key(file)) } else { OwnerKey::PosixPid(pid) };
    let key = file_identity(file);
    let found = {
        let table = RECORD_LOCKS.exclusive_access();
        table.get(&key).and_then(|state| {
            state
                .records
                .iter()
                .find(|r| conflicts(r, owner, start, end, l_type))
                .cloned()
        })
    };
    if let Some(r) = found {
        let len = if r.end == i64::MAX { 0 } else { r.end.saturating_sub(r.start).saturating_add(1) };
        let out_pid = match r.owner {
            OwnerKey::PosixPid(_) => r.pid,
            OwnerKey::Ofd(_) => -1,
        };
        write_flock_struct(arg, r.kind, 0, r.start, len, out_pid).err().unwrap_or(0)
    } else {
        write_flock_struct(arg, F_UNLCK, l_whence, l_start, l_len, l_pid).err().unwrap_or(0)
    }
}

fn would_deadlock(me: usize, blockers: &[usize]) -> bool {
    let wf = WAIT_FOR.exclusive_access();
    let mut stack: Vec<usize> = blockers.to_vec();
    let mut visited = Vec::new();
    while let Some(cur) = stack.pop() {
        if cur == me {
            return true;
        }
        if visited.contains(&cur) {
            continue;
        }
        visited.push(cur);
        if let Some(nexts) = wf.get(&cur) {
            stack.extend(nexts.iter().copied());
        }
    }
    false
}

fn apply_record_change(records: &mut Vec<RecordLock>, owner: OwnerKey, pid: i32, start: i64, end: i64, kind: i16) {
    let mut updated = Vec::with_capacity(records.len() + 2);
    for rec in records.drain(..) {
        if rec.owner != owner || !overlaps(rec.start, rec.end, start, end) {
            updated.push(rec);
            continue;
        }
        if rec.start < start {
            updated.push(RecordLock {
                owner: rec.owner,
                pid: rec.pid,
                start: rec.start,
                end: start - 1,
                kind: rec.kind,
            });
        }
        if rec.end > end {
            updated.push(RecordLock {
                owner: rec.owner,
                pid: rec.pid,
                start: end + 1,
                end: rec.end,
                kind: rec.kind,
            });
        }
    }
    if kind != F_UNLCK {
        updated.push(RecordLock {
            owner,
            pid,
            start,
            end,
            kind,
        });
    }
    // Sort and coalesce adjacent locks of the same owner and kind
    updated.sort_by_key(|r| (match r.owner { OwnerKey::PosixPid(p) => (0, p), OwnerKey::Ofd(k) => (1, k) }, r.start));
    let mut merged: Vec<RecordLock> = Vec::with_capacity(updated.len());
    for rec in updated {
        if let Some(last) = merged.last_mut() {
            if last.owner == rec.owner
                && last.kind == rec.kind
                && last.end.saturating_add(1) >= rec.start
            {
                last.end = last.end.max(rec.end);
                continue;
            }
        }
        merged.push(rec);
    }
    *records = merged;
}

pub fn fcntl_setlk(_fd: usize, file: &Arc<dyn File + Send + Sync>, arg: usize, wait: bool, ofd: bool) -> isize {
    if file.path_only() {
        return -9;
    }
    let (l_type, l_whence, l_start, l_len, l_pid) = match read_flock_struct(arg) {
        Ok(v) => v,
        Err(e) => return e,
    };
    if !matches!(l_type, F_RDLCK | F_WRLCK | F_UNLCK) || (ofd && l_pid != 0) {
        return -22;
    }
    if l_type == F_RDLCK && !file.readable() {
        return -9;
    }
    if l_type == F_WRLCK && !file.writable() {
        return -9;
    }
    // POSIX record locks are supported on regular files, devices, pipes, etc., check bad pointer already done
    let (start, end) = match compute_range(file, l_whence, l_start, l_len) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let pid = current_process().getpid();
    let owner = if ofd { OwnerKey::Ofd(ofd_key(file)) } else { OwnerKey::PosixPid(pid) };
    let key = file_identity(file);

    if l_type == F_UNLCK {
        let mut table = RECORD_LOCKS.exclusive_access();
        if let Some(state) = table.get_mut(&key) {
            apply_record_change(&mut state.records, owner, pid as i32, start, end, F_UNLCK);
            let wq = state.waiters.clone();
            if state.records.is_empty() && state.waiters.is_none() {
                table.remove(&key);
            }
            drop(table);
            if let Some(wq) = wq {
                wq.wake_all();
            }
        }
        return 0;
    }

    loop {
        let mut table = RECORD_LOCKS.exclusive_access();
        let state = table.entry(key).or_default();
        let mut blockers = Vec::new();
        for r in &state.records {
            if conflicts(r, owner, start, end, l_type) {
                if let OwnerKey::PosixPid(bpid) = r.owner {
                    if !blockers.contains(&bpid) {
                        blockers.push(bpid);
                    }
                } else {
                    blockers.push(usize::MAX);
                }
            }
        }
        if blockers.is_empty() {
            apply_record_change(&mut state.records, owner, pid as i32, start, end, l_type);
            let wq = state.waiters.clone();
            drop(table);
            WAIT_FOR.exclusive_access().remove(&pid);
            if l_type == F_RDLCK {
                if let Some(wq) = wq {
                    wq.wake_all();
                }
            }
            return 0;
        }
        if !wait {
            return -11; // EAGAIN
        }
        if !ofd {
            let real_blockers: Vec<usize> = blockers.into_iter().filter(|&b| b != usize::MAX).collect();
            if would_deadlock(pid, &real_blockers) {
                return -35; // EDEADLK
            }
            WAIT_FOR.exclusive_access().insert(pid, real_blockers);
        }
        let wq = state.wq();
        if !wq.register_current_interruptible() {
            drop(table);
            WAIT_FOR.exclusive_access().remove(&pid);
            return -4;
        }
        drop(table);
        block_current_and_run_next();
        if let Some(t) = current_task() {
            WaitQueue::cancel(&t);
        }
        WAIT_FOR.exclusive_access().remove(&pid);
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4;
        }
    }
}

pub fn fcntl_owner_sig(file: &Arc<dyn File + Send + Sync>, cmd: usize, arg: usize) -> isize {
    if file.path_only() {
        return -9;
    }
    let k = ofd_key(file);
    let token = current_user_token();
    match cmd {
        F_GETOWN => {
            let ex = FILE_EXTRA.exclusive_access().get(&k).copied().unwrap_or_default();
            if ex.owner_type == 2 { -ex.owner_pid as isize } else { ex.owner_pid as isize }
        }
        F_SETOWN => {
            let who = arg as i32;
            if who == i32::MIN {
                return -22;
            }
            if who > 0 {
                if crate::task::pid2process(who as usize).is_none()
                    && crate::task::tid2task(who as usize).is_none()
                {
                    return -3; // ESRCH
                }
            } else if who < 0 {
                let pgid = (-who) as usize;
                let exists = crate::task::process_ids()
                    .into_iter()
                    .filter_map(crate::task::pid2process)
                    .any(|p| p.inner_exclusive_access().pgid.0 == pgid);
                if !exists {
                    return -3;
                }
            }
            let mut map = FILE_EXTRA.exclusive_access();
            let entry = map.entry(k).or_default();
            if who < 0 {
                entry.owner_type = 2; // F_OWNER_PGRP
                entry.owner_pid = -who;
            } else {
                entry.owner_type = 1; // F_OWNER_PID
                entry.owner_pid = who;
            }
            0
        }
        F_GETOWN_EX => {
            let ex = FILE_EXTRA.exclusive_access().get(&k).copied().unwrap_or_default();
            let mut raw = [0u8; 8];
            raw[0..4].copy_from_slice(&ex.owner_type.to_ne_bytes());
            raw[4..8].copy_from_slice(&ex.owner_pid.to_ne_bytes());
            let Some(bufs) = translated_byte_buffer(token, arg as *const u8, 8, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(bufs).copy_from(&raw);
            0
        }
        F_SETOWN_EX => {
            let Some(bufs) = translated_byte_buffer(token, arg as *const u8, 8, MapPermission::R) else {
                return -14;
            };
            let mut raw = [0u8; 8];
            for (d, s) in raw.iter_mut().zip(UserBuffer::new(bufs)) {
                *d = s.read();
            }
            let otype = i32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let opid = i32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
            if !matches!(otype, 0..=2) || opid < 0 {
                return -22;
            }
            if opid > 0 {
                let found = match otype {
                    0 => crate::task::tid2task(opid as usize).is_some()
                        || crate::task::pid2process(opid as usize).is_some(),
                    1 => crate::task::pid2process(opid as usize).is_some(),
                    2 => crate::task::process_ids()
                        .into_iter()
                        .filter_map(crate::task::pid2process)
                        .any(|p| p.inner_exclusive_access().pgid.0 == opid as usize),
                    _ => false,
                };
                if !found {
                    return -3;
                }
            }
            let mut map = FILE_EXTRA.exclusive_access();
            let entry = map.entry(k).or_default();
            entry.owner_type = if opid == 0 { 0 } else { otype };
            entry.owner_pid = opid;
            0
        }
        F_GETSIG => FILE_EXTRA.exclusive_access().get(&k).map_or(0, |e| e.sig as isize),
        F_SETSIG => {
            if arg > 64 {
                return -22;
            }
            FILE_EXTRA.exclusive_access().entry(k).or_default().sig = arg as i32;
            0
        }
        F_GETLEASE => FILE_EXTRA
            .exclusive_access()
            .get(&k)
            .map_or(F_UNLCK as isize, |e| e.lease as isize),
        F_SETLEASE => {
            let l = arg as i16;
            if !matches!(l, F_RDLCK | F_WRLCK | F_UNLCK) {
                return -22;
            }
            let euid = current_process().inner_exclusive_access().euid;
            let (fuid, _) = file.owner();
            if euid != 0 && euid != fuid {
                return -1; // EPERM
            }
            // Read lease cannot be placed on a file open for writing (`fcntl27`)
            if l == F_RDLCK && file.writable() {
                return -11; // EAGAIN
            }
            FILE_EXTRA.exclusive_access().entry(k).or_default().lease = l;
            0
        }
        _ => -22,
    }
}

pub fn on_file_closed(pid: usize, file: &Arc<dyn File + Send + Sync>) {
    let key = file_identity(file);
    let ofd = ofd_key(file);
    // POSIX rule: closing ANY descriptor for an inode drops all POSIX record locks held by this pid on it.
    {
        let mut table = RECORD_LOCKS.exclusive_access();
        if let Some(state) = table.get_mut(&key) {
            let before = state.records.len();
            state.records.retain(|r| r.owner != OwnerKey::PosixPid(pid));
            if Arc::strong_count(file) <= 1 {
                state.records.retain(|r| r.owner != OwnerKey::Ofd(ofd));
            }
            let changed = state.records.len() != before;
            let wq = state.waiters.clone();
            if state.records.is_empty() && state.waiters.is_none() {
                table.remove(&key);
            }
            drop(table);
            if changed {
                if let Some(wq) = wq {
                    wq.wake_all();
                }
            }
        }
    }
    if Arc::strong_count(file) <= 1 {
        FILE_EXTRA.exclusive_access().remove(&ofd);
        let mut flocks = FLOCK_TABLE.exclusive_access();
        if let Some(entry) = flocks.get_mut(&key) {
            if entry.holders.remove(&ofd).is_some() {
                let wq = entry.waiters.clone();
                if entry.holders.is_empty() {
                    flocks.remove(&key);
                }
                drop(flocks);
                wq.wake_all();
            }
        }
    }
}

pub fn release_process_locks(pid: usize) {
    WAIT_FOR.exclusive_access().remove(&pid);
    let mut to_wake = Vec::new();
    {
        let mut table = RECORD_LOCKS.exclusive_access();
        let mut empty_keys = Vec::new();
        for (&k, state) in table.iter_mut() {
            let before = state.records.len();
            state.records.retain(|r| r.owner != OwnerKey::PosixPid(pid));
            if state.records.len() != before {
                if let Some(wq) = &state.waiters {
                    to_wake.push(wq.clone());
                }
            }
            if state.records.is_empty() && state.waiters.is_none() {
                empty_keys.push(k);
            }
        }
        for k in empty_keys {
            table.remove(&k);
        }
    }
    for wq in to_wake {
        wq.wake_all();
    }
}
