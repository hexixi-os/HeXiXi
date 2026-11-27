//! System V IPC: Shared Memory (`shm*`), Message Queues (`msg*`), and Semaphores (`sem*`).

use crate::config::PAGE_SIZE;
use crate::fs::{overlay, File};
use crate::mm::{
    translated_byte_buffer, try_read_user, MapPermission, UserBuffer, VirtPageNum, VmaBacking,
    USER_END,
};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_process, current_task, current_user_token, WaitQueue};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;
use lazy_static::lazy_static;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: usize = 0o1000;
const IPC_EXCL: usize = 0o2000;
const IPC_NOWAIT: usize = 0o4000;

const IPC_RMID: i32 = 0;
const IPC_SET: i32 = 1;
const IPC_STAT: i32 = 2;
const IPC_INFO: i32 = 3;

const SHM_LOCK: i32 = 11;
const SHM_UNLOCK: i32 = 12;
const SHM_STAT: i32 = 13;
const SHM_INFO: i32 = 14;
const SHM_RDONLY: usize = 0o10000;
const SHM_RND: usize = 0o20000;
const SHM_REMAP: usize = 0o40000;
const SHM_DEST: u16 = 0o1000;
const SHM_LOCKED: u16 = 0o2000;

const MSG_STAT: i32 = 11;
const MSG_INFO: i32 = 12;
const MSG_STAT_ANY: i32 = 13;
const MSG_NOERROR: usize = 0o10000;
const MSG_EXCEPT: usize = 0o20000;
const MSG_COPY: usize = 0o40000;

const GETPID: i32 = 11;
const GETVAL: i32 = 12;
const GETALL: i32 = 13;
const GETNCNT: i32 = 14;
const GETZCNT: i32 = 15;
const SETVAL: i32 = 16;
const SETALL: i32 = 17;
const SEM_STAT: i32 = 18;
const SEM_INFO: i32 = 19;
const SEM_STAT_ANY: i32 = 20;
const SEM_UNDO: i16 = 0x1000;

const SHMMNI: usize = 256;
const SHMMAX: usize = 64 * 1024 * 1024;
const MSGMNI: usize = 256;
const MSGMAX: usize = 8192;
const MSGMNB: usize = 16384;
const SEMMNI: usize = 256;
const SEMMSL: usize = 250;
const SEMVMX: i32 = 32767;

#[derive(Clone, Copy)]
struct IpcPerm {
    key: i32,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u16,
    seq: u16,
}

impl IpcPerm {
    fn new(key: i32, mode: u16, seq: u16) -> Self {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        Self {
            key,
            uid: inner.euid,
            gid: inner.egid,
            cuid: inner.euid,
            cgid: inner.egid,
            mode: mode & 0o777,
            seq,
        }
    }

    fn encode(&self, out: &mut [u8]) {
        // Linux LP64 struct ipc64_perm (48 bytes)
        out[0..4].copy_from_slice(&self.key.to_ne_bytes());
        out[4..8].copy_from_slice(&self.uid.to_ne_bytes());
        out[8..12].copy_from_slice(&self.gid.to_ne_bytes());
        out[12..16].copy_from_slice(&self.cuid.to_ne_bytes());
        out[16..20].copy_from_slice(&self.cgid.to_ne_bytes());
        out[20..22].copy_from_slice(&self.mode.to_ne_bytes());
        out[24..26].copy_from_slice(&self.seq.to_ne_bytes());
    }

    fn check_perm(&self, want: u16) -> Result<(), isize> {
        if want == 0 {
            return Ok(());
        }
        let p = current_process();
        let inner = p.inner_exclusive_access();
        if inner.euid == 0 {
            return Ok(());
        }
        let bits = if inner.euid == self.uid || inner.euid == self.cuid {
            (self.mode >> 6) & 7
        } else if inner.egid == self.gid || inner.egid == self.cgid || inner.groups.contains(&self.gid) {
            (self.mode >> 3) & 7
        } else {
            self.mode & 7
        };
        if (bits & want) == want {
            Ok(())
        } else {
            Err(-13) // EACCES
        }
    }

    fn can_control(&self) -> bool {
        let p = current_process();
        let inner = p.inner_exclusive_access();
        inner.euid == 0 || inner.euid == self.uid || inner.euid == self.cuid
    }
}

fn now_sec() -> i64 {
    (crate::timer::get_realtime_ns() / 1_000_000_000) as i64
}

// ======================= SHARED MEMORY =======================

struct ShmSegment {
    perm: IpcPerm,
    size: usize,
    atime: i64,
    dtime: i64,
    ctime: i64,
    cpid: i32,
    lpid: i32,
    nattch: usize,
    marked_rmid: bool,
    locked: bool,
    backing: Arc<dyn File + Send + Sync>,
}

#[derive(Clone)]
pub(crate) struct ShmAttachment {
    pub(crate) id: usize,
    base: usize,
    pub(crate) ranges: Vec<Range<VirtPageNum>>,
}

impl ShmAttachment {
    pub(crate) fn unmap(&mut self, removed: &Range<VirtPageNum>) {
        if !self.overlaps(removed) {
            return;
        }
        let mut kept = Vec::new();
        for range in self.ranges.drain(..) {
            if range.end <= removed.start || removed.end <= range.start {
                kept.push(range);
                continue;
            }
            if range.start < removed.start {
                kept.push(range.start..removed.start);
            }
            if removed.end < range.end {
                kept.push(removed.end..range.end);
            }
        }
        self.ranges = kept;
    }

    pub(crate) fn overlaps(&self, range: &Range<VirtPageNum>) -> bool {
        self.ranges.iter().any(|part| part.start < range.end && range.start < part.end)
    }
}

struct ShmTable {
    next_id: usize,
    seq: u16,
    segments: BTreeMap<usize, ShmSegment>,
}

lazy_static! {
    static ref SHM_TABLE: UPSafeCell<ShmTable> = unsafe {
        UPSafeCell::new(ShmTable {
            next_id: 1,
            seq: 1,
            segments: BTreeMap::new(),
        })
    };
}

pub fn sys_shmget(key: i32, size: usize, shmflg: usize) -> isize {
    let mut table = SHM_TABLE.exclusive_access();
    if key != IPC_PRIVATE {
        if let Some((&id, seg)) = table
            .segments
            .iter()
            .find(|(_, s)| !s.marked_rmid && s.perm.key == key)
        {
            if shmflg & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return -17; // EEXIST
            }
            if size > seg.size {
                return -22; // EINVAL
            }
            if let Err(e) = seg.perm.check_perm((shmflg & 0o777) as u16) {
                return e;
            }
            return id as isize;
        }
        if shmflg & IPC_CREAT == 0 {
            return -2; // ENOENT
        }
    }
    if size == 0 || size > SHMMAX {
        return -22; // EINVAL
    }
    if table.segments.len() >= SHMMNI {
        return -28; // ENOSPC
    }
    let aligned = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let backing = match overlay::new_shm_backing(aligned) {
        Ok(b) => b,
        Err(e) => return e,
    };
    let id = table.next_id;
    table.next_id += 1;
    let seq = table.seq;
    table.seq = table.seq.wrapping_add(1);
    let now = now_sec();
    let pid = current_process().getpid() as i32;
    let perm = IpcPerm::new(key, (shmflg & 0o777) as u16, seq);
    table.segments.insert(
        id,
        ShmSegment {
            perm,
            size,
            atime: 0,
            dtime: 0,
            ctime: now,
            cpid: pid,
            lpid: 0,
            nattch: 0,
            marked_rmid: false,
            locked: false,
            backing,
        },
    );
    id as isize
}

pub fn sys_shmat(shmid: i32, shmaddr: usize, shmflg: usize) -> isize {
    if shmid < 0 || (shmaddr == 0 && shmflg & SHM_REMAP != 0) {
        return -22;
    }
    let id = shmid as usize;
    let want_perm = if shmflg & SHM_RDONLY != 0 { 4 } else { 6 };
    let (backing, aligned_size) = {
        let table = SHM_TABLE.exclusive_access();
        let Some(seg) = table.segments.get(&id) else {
            return -22; // EINVAL
        };
        if let Err(e) = seg.perm.check_perm(want_perm) {
            return e;
        }
        (seg.backing.clone(), (seg.size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1))
    };

    let addr = if shmaddr != 0 {
        let a = if shmflg & SHM_RND != 0 {
            shmaddr & !(PAGE_SIZE - 1)
        } else {
            if shmaddr % PAGE_SIZE != 0 {
                return -22;
            }
            shmaddr
        };
        if a < PAGE_SIZE || a.checked_add(aligned_size).map_or(true, |e| e > USER_END) {
            return -22;
        }
        a
    } else {
        0
    };

    let mut perms = MapPermission::U | MapPermission::R;
    if shmflg & SHM_RDONLY == 0 {
        perms |= MapPermission::W;
    }
    if shmflg & 0o100000 != 0 {
        // SHM_EXEC
        perms |= MapPermission::X;
    }
    let pages = aligned_size / PAGE_SIZE;
    let proc = current_process();
    let pid = proc.getpid();
    let mut inner = proc.inner_exclusive_access();
    let range = if addr != 0 {
        let r = VirtPageNum(addr / PAGE_SIZE)..VirtPageNum((addr + aligned_size) / PAGE_SIZE);
        if shmflg & SHM_REMAP != 0 {
            if let Err(e) = inner.memory_set.munmap(r.clone()) {
                return e;
            }
        } else if !inner.memory_set.vacant(&r) {
            return -22; // EINVAL if address in use without SHM_REMAP
        }
        r
    } else {
        const MMAP_BASE: usize = 0x1000_0000;
        let start = VirtPageNum(inner.mmap_top.max(MMAP_BASE) / PAGE_SIZE);
        match inner
            .memory_set
            .find_vacant(start, pages)
            .or_else(|| inner.memory_set.find_vacant(VirtPageNum(MMAP_BASE / PAGE_SIZE), pages))
        {
            Some(r) => {
                inner.mmap_top = r.end.0 * PAGE_SIZE;
                r
            }
            None => return -12,
        }
    };
    let mapped_va = range.start.0 * PAGE_SIZE;
    inner
        .memory_set
        .insert_vma(range.clone(), perms, VmaBacking::file(backing, 0, true));
    {
        let mut table = SHM_TABLE.exclusive_access();
        let seg = table.segments.get_mut(&id).unwrap();
        seg.nattch += 1;
        seg.atime = now_sec();
        seg.lpid = pid as i32;
    }
    if shmflg & SHM_REMAP != 0 {
        // Count the new attachment before detaching any old mapping of the same segment.
        inner.forget_shm_range(&range, pid);
    }
    inner.shm_attachments.push(ShmAttachment {
        id,
        base: mapped_va,
        ranges: vec![range],
    });
    mapped_va as isize
}

pub fn sys_shmdt(shmaddr: usize) -> isize {
    if shmaddr % PAGE_SIZE != 0 {
        return -22;
    }
    let proc = current_process();
    let pid = proc.getpid();
    let found = {
        let mut inner = proc.inner_exclusive_access();
        let Some(pos) = inner.shm_attachments.iter().rposition(|attachment| attachment.base == shmaddr) else {
            return -22;
        };
        let attachment = inner.shm_attachments.remove(pos);
        for range in attachment.ranges {
            if let Err(error) = inner.memory_set.munmap(range) {
                return error;
            }
        }
        attachment.id
    };
    shm_detach_id(found, pid);
    0
}

pub fn sys_shmctl(shmid: i32, cmd: i32, buf: usize) -> isize {
    let token = current_user_token();
    match cmd {
        IPC_INFO => {
            if buf == 0 {
                return -14;
            }
            let mut info = [0u8; 72];
            for (i, val) in [SHMMAX, 1usize, SHMMNI, SHMMNI, SHMMAX / PAGE_SIZE].into_iter().enumerate() {
                info[i * 8..(i + 1) * 8].copy_from_slice(&val.to_ne_bytes());
            }
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 72, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&info);
            let max_idx = SHM_TABLE.exclusive_access().segments.len();
            max_idx as isize
        }
        SHM_INFO => {
            if buf == 0 {
                return -14;
            }
            let table = SHM_TABLE.exclusive_access();
            let used_ids = table.segments.len() as i32;
            let tot_pages: usize = table.segments.values().map(|s| (s.size + PAGE_SIZE - 1) / PAGE_SIZE).sum();
            drop(table);
            let mut info = [0u8; 48];
            info[0..4].copy_from_slice(&used_ids.to_ne_bytes());
            info[8..16].copy_from_slice(&tot_pages.to_ne_bytes());
            info[16..24].copy_from_slice(&tot_pages.to_ne_bytes());
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 48, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&info);
            used_ids as isize
        }
        IPC_STAT | SHM_STAT => {
            if buf == 0 {
                return -14;
            }
            let table = SHM_TABLE.exclusive_access();
            let (real_id, seg) = if cmd == SHM_STAT {
                if shmid < 0 {
                    return -22;
                }
                match table.segments.iter().nth(shmid as usize) {
                    Some((&id, s)) => (id, s),
                    None => return -22,
                }
            } else {
                if shmid < 0 {
                    return -22;
                }
                match table.segments.get(&(shmid as usize)) {
                    Some(s) => (shmid as usize, s),
                    None => return -22,
                }
            };
            if let Err(e) = seg.perm.check_perm(4) {
                return e;
            }
            // Linux LP64 struct shmid64_ds (112 bytes)
            let mut ds = [0u8; 112];
            let mut perm = seg.perm;
            if seg.marked_rmid {
                perm.mode |= SHM_DEST;
            }
            if seg.locked {
                perm.mode |= SHM_LOCKED;
            }
            perm.encode(&mut ds[0..48]);
            ds[48..56].copy_from_slice(&seg.size.to_ne_bytes());
            ds[56..64].copy_from_slice(&seg.atime.to_ne_bytes());
            ds[64..72].copy_from_slice(&seg.dtime.to_ne_bytes());
            ds[72..80].copy_from_slice(&seg.ctime.to_ne_bytes());
            ds[80..84].copy_from_slice(&seg.cpid.to_ne_bytes());
            ds[84..88].copy_from_slice(&seg.lpid.to_ne_bytes());
            ds[88..96].copy_from_slice(&seg.nattch.to_ne_bytes());
            drop(table);
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 112, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&ds);
            if cmd == SHM_STAT { real_id as isize } else { 0 }
        }
        IPC_SET => {
            if shmid < 0 {
                return -22;
            }
            if buf == 0 {
                return -14;
            }
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 48, MapPermission::R) else {
                return -14;
            };
            let mut raw = [0u8; 48];
            for (d, s) in raw.iter_mut().zip(UserBuffer::new(b)) {
                *d = s.read();
            }
            let new_uid = u32::from_ne_bytes(raw[4..8].try_into().unwrap());
            let new_gid = u32::from_ne_bytes(raw[8..12].try_into().unwrap());
            let new_mode = u16::from_ne_bytes([raw[20], raw[21]]) & 0o777;
            let mut table = SHM_TABLE.exclusive_access();
            let Some(seg) = table.segments.get_mut(&(shmid as usize)) else {
                return -22;
            };
            if !seg.perm.can_control() {
                return -1; // EPERM
            }
            seg.perm.uid = new_uid;
            seg.perm.gid = new_gid;
            seg.perm.mode = new_mode;
            seg.ctime = now_sec();
            0
        }
        IPC_RMID => {
            if shmid < 0 {
                return -22;
            }
            let id = shmid as usize;
            let mut table = SHM_TABLE.exclusive_access();
            let Some(seg) = table.segments.get_mut(&id) else {
                return -22;
            };
            if !seg.perm.can_control() {
                return -1;
            }
            seg.marked_rmid = true;
            if seg.nattch == 0 {
                table.segments.remove(&id);
            }
            0
        }
        SHM_LOCK | SHM_UNLOCK => {
            if shmid < 0 {
                return -22;
            }
            let mut table = SHM_TABLE.exclusive_access();
            let Some(seg) = table.segments.get_mut(&(shmid as usize)) else {
                return -22;
            };
            if !seg.perm.can_control() {
                return -1;
            }
            seg.locked = cmd == SHM_LOCK;
            0
        }
        _ => -22,
    }
}

pub fn shm_attach_id(shmid: usize) {
    let mut table = SHM_TABLE.exclusive_access();
    if let Some(seg) = table.segments.get_mut(&shmid) {
        seg.nattch += 1;
    }
}

pub fn shm_detach_id(shmid: usize, pid: usize) {
    let mut table = SHM_TABLE.exclusive_access();
    if let Some(seg) = table.segments.get_mut(&shmid) {
        seg.nattch = seg.nattch.saturating_sub(1);
        seg.dtime = now_sec();
        seg.lpid = pid as i32;
        if seg.nattch == 0 && seg.marked_rmid {
            table.segments.remove(&shmid);
        }
    }
}

// ======================= MESSAGE QUEUES =======================

struct MsgQueue {
    perm: IpcPerm,
    stime: i64,
    rtime: i64,
    ctime: i64,
    cbytes: usize,
    qbytes: usize,
    lspid: i32,
    lrpid: i32,
    messages: VecDeque<(i64, Vec<u8>)>,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
}

struct MsgTable {
    next_id: usize,
    seq: u16,
    queues: BTreeMap<usize, MsgQueue>,
}

lazy_static! {
    static ref MSG_TABLE: UPSafeCell<MsgTable> = unsafe {
        UPSafeCell::new(MsgTable {
            next_id: 1,
            seq: 1,
            queues: BTreeMap::new(),
        })
    };
}

pub fn sys_msgget(key: i32, msgflg: usize) -> isize {
    let mut table = MSG_TABLE.exclusive_access();
    if key != IPC_PRIVATE {
        if let Some((&id, q)) = table.queues.iter().find(|(_, q)| q.perm.key == key) {
            if msgflg & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return -17;
            }
            if let Err(e) = q.perm.check_perm((msgflg & 0o777) as u16) {
                return e;
            }
            return id as isize;
        }
        if msgflg & IPC_CREAT == 0 {
            return -2;
        }
    }
    if table.queues.len() >= MSGMNI {
        return -28;
    }
    let id = table.next_id;
    table.next_id += 1;
    let seq = table.seq;
    table.seq = table.seq.wrapping_add(1);
    let perm = IpcPerm::new(key, (msgflg & 0o777) as u16, seq);
    table.queues.insert(
        id,
        MsgQueue {
            perm,
            stime: 0,
            rtime: 0,
            ctime: now_sec(),
            cbytes: 0,
            qbytes: MSGMNB,
            lspid: 0,
            lrpid: 0,
            messages: VecDeque::new(),
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
        },
    );
    id as isize
}

pub fn sys_msgsnd(msqid: i32, msgp: usize, msgsz: usize, msgflg: usize) -> isize {
    if msqid < 0 || (msgsz as isize) < 0 || msgsz > MSGMAX {
        return -22;
    }
    if msgp == 0 {
        return -14;
    }
    let token = current_user_token();
    let Some(mtype) = try_read_user(token, msgp as *const i64) else {
        return -14;
    };
    if mtype <= 0 {
        return -22;
    }
    let mut data = vec![0u8; msgsz];
    if msgsz > 0 {
        let Some(body_ptr) = msgp.checked_add(8) else {
            return -14;
        };
        let Some(bufs) = translated_byte_buffer(token, body_ptr as *const u8, msgsz, MapPermission::R) else {
            return -14;
        };
        for (d, s) in data.iter_mut().zip(UserBuffer::new(bufs)) {
            *d = s.read();
        }
    }
    let id = msqid as usize;
    let pid = current_process().getpid() as i32;
    let mut first = true;
    loop {
        let mut table = MSG_TABLE.exclusive_access();
        let Some(q) = table.queues.get_mut(&id) else {
            return if first { -22 } else { -43 }; // EINVAL initially, EIDRM if removed while waiting
        };
        first = false;
        if let Err(e) = q.perm.check_perm(2) {
            return e;
        }
        if msgsz > q.qbytes {
            return -22;
        }
        if q.cbytes + msgsz <= q.qbytes && q.messages.len() < 1024 {
            q.cbytes += msgsz;
            q.stime = now_sec();
            q.lspid = pid;
            q.messages.push_back((mtype, data));
            let readers = q.readers.clone();
            drop(table);
            readers.wake_all();
            return 0;
        }
        if msgflg & IPC_NOWAIT != 0 {
            return -11; // EAGAIN
        }
        let writers = q.writers.clone();
        if !writers.register_current_interruptible() {
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

pub fn sys_msgrcv(msqid: i32, msgp: usize, msgsz: usize, msgtyp: isize, msgflg: usize) -> isize {
    if msqid < 0 || (msgsz as isize) < 0 {
        return -22;
    }
    if msgp == 0 {
        return -14;
    }
    let is_copy = msgflg & MSG_COPY != 0;
    if is_copy && (msgflg & IPC_NOWAIT == 0 || msgflg & MSG_EXCEPT != 0 || msgtyp < 0) {
        return -22;
    }
    let id = msqid as usize;
    let pid = current_process().getpid() as i32;
    let token = current_user_token();
    let mut first = true;
    loop {
        let mut table = MSG_TABLE.exclusive_access();
        let Some(q) = table.queues.get_mut(&id) else {
            return if first { -22 } else { -43 };
        };
        first = false;
        if let Err(e) = q.perm.check_perm(4) {
            return e;
        }
        let match_idx = if is_copy {
            let idx = msgtyp as usize;
            (idx < q.messages.len()).then_some(idx)
        } else if msgtyp == 0 {
            (!q.messages.is_empty()).then_some(0)
        } else if msgtyp > 0 {
            let want = msgtyp as i64;
            let except = msgflg & MSG_EXCEPT != 0;
            q.messages
                .iter()
                .position(|(t, _)| if except { *t != want } else { *t == want })
        } else {
            let max_t = msgtyp.saturating_abs() as i64;
            let mut best: Option<(usize, i64)> = None;
            for (i, &(t, _)) in q.messages.iter().enumerate() {
                if t <= max_t && best.map_or(true, |(_, bt)| t < bt) {
                    best = Some((i, t));
                }
            }
            best.map(|(i, _)| i)
        };

        if let Some(idx) = match_idx {
            let msg_len = q.messages[idx].1.len();
            if msg_len > msgsz && (msgflg & MSG_NOERROR == 0) {
                return -7; // E2BIG
            }
            let copy_len = msg_len.min(msgsz);
            // Validate user write buffers BEFORE dequeuing so EFAULT doesn't lose the message!
            let total_user = 8usize.checked_add(copy_len).ok_or(-14isize);
            let Ok(total_user) = total_user else {
                return -14;
            };
            let Some(ubufs) = translated_byte_buffer(token, msgp as *const u8, total_user, MapPermission::W) else {
                return -14;
            };
            let (mtype, payload) = if is_copy {
                q.messages[idx].clone()
            } else {
                let removed = q.messages.remove(idx).unwrap();
                q.cbytes = q.cbytes.saturating_sub(removed.1.len());
                q.rtime = now_sec();
                q.lrpid = pid;
                removed
            };
            let writers = q.writers.clone();
            drop(table);
            let mut out = Vec::with_capacity(8 + copy_len);
            out.extend_from_slice(&mtype.to_ne_bytes());
            out.extend_from_slice(&payload[..copy_len]);
            UserBuffer::new(ubufs).copy_from(&out);
            if !is_copy {
                writers.wake_all();
            }
            return copy_len as isize;
        }

        if msgflg & IPC_NOWAIT != 0 {
            return -42; // ENOMSG
        }
        let readers = q.readers.clone();
        if !readers.register_current_interruptible() {
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

pub fn sys_msgctl(msqid: i32, cmd: i32, buf: usize) -> isize {
    let token = current_user_token();
    match cmd {
        IPC_INFO | MSG_INFO => {
            if buf == 0 {
                return -14;
            }
            let table = MSG_TABLE.exclusive_access();
            let used = table.queues.len() as i32;
            drop(table);
            let mut info = [0u8; 32];
            let fields: [i32; 8] = if cmd == MSG_INFO {
                [used, 0, 0, MSGMNI as i32, MSGMAX as i32, MSGMNB as i32, used, 0]
            } else {
                [32, 1024, MSGMAX as i32, MSGMNB as i32, MSGMNI as i32, 1, 1024, 0xffff]
            };
            for (i, v) in fields.into_iter().enumerate() {
                info[i * 4..(i + 1) * 4].copy_from_slice(&v.to_ne_bytes());
            }
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 32, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&info);
            used as isize
        }
        IPC_STAT | MSG_STAT | MSG_STAT_ANY => {
            if msqid < 0 {
                return -22;
            }
            if buf == 0 {
                return -14;
            }
            let table = MSG_TABLE.exclusive_access();
            let (real_id, q) = if matches!(cmd, MSG_STAT | MSG_STAT_ANY) {
                match table.queues.iter().nth(msqid as usize) {
                    Some((&id, q)) => (id, q),
                    None => return -22,
                }
            } else {
                match table.queues.get(&(msqid as usize)) {
                    Some(q) => (msqid as usize, q),
                    None => return -22,
                }
            };
            if cmd != MSG_STAT_ANY {
                if let Err(e) = q.perm.check_perm(4) {
                    return e;
                }
            }
            // Linux LP64 struct msqid64_ds (120 bytes)
            let mut ds = [0u8; 120];
            q.perm.encode(&mut ds[0..48]);
            ds[48..56].copy_from_slice(&q.stime.to_ne_bytes());
            ds[56..64].copy_from_slice(&q.rtime.to_ne_bytes());
            ds[64..72].copy_from_slice(&q.ctime.to_ne_bytes());
            ds[72..80].copy_from_slice(&(q.cbytes as u64).to_ne_bytes());
            ds[80..88].copy_from_slice(&(q.messages.len() as u64).to_ne_bytes());
            ds[88..96].copy_from_slice(&(q.qbytes as u64).to_ne_bytes());
            ds[96..100].copy_from_slice(&q.lspid.to_ne_bytes());
            ds[100..104].copy_from_slice(&q.lrpid.to_ne_bytes());
            drop(table);
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 120, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&ds);
            if matches!(cmd, MSG_STAT | MSG_STAT_ANY) { real_id as isize } else { 0 }
        }
        IPC_SET => {
            if msqid < 0 {
                return -22;
            }
            if buf == 0 {
                return -14;
            }
            let Some(b) = translated_byte_buffer(token, buf as *const u8, 120, MapPermission::R) else {
                return -14;
            };
            let mut raw = [0u8; 120];
            for (d, s) in raw.iter_mut().zip(UserBuffer::new(b)) {
                *d = s.read();
            }
            let new_uid = u32::from_ne_bytes(raw[4..8].try_into().unwrap());
            let new_gid = u32::from_ne_bytes(raw[8..12].try_into().unwrap());
            let new_mode = u16::from_ne_bytes([raw[20], raw[21]]) & 0o777;
            let new_qbytes = u64::from_ne_bytes(raw[88..96].try_into().unwrap()) as usize;
            if new_qbytes == 0 {
                return -22;
            }
            let euid = current_process().inner_exclusive_access().euid;
            let mut table = MSG_TABLE.exclusive_access();
            let Some(q) = table.queues.get_mut(&(msqid as usize)) else {
                return -22;
            };
            if !q.perm.can_control() || (new_qbytes > q.qbytes && euid != 0) {
                return -1;
            }
            q.perm.uid = new_uid;
            q.perm.gid = new_gid;
            q.perm.mode = new_mode;
            q.qbytes = new_qbytes;
            q.ctime = now_sec();
            0
        }
        IPC_RMID => {
            if msqid < 0 {
                return -22;
            }
            let mut table = MSG_TABLE.exclusive_access();
            let Some(q) = table.queues.get(&(msqid as usize)) else {
                return -22;
            };
            if !q.perm.can_control() {
                return -1;
            }
            let removed = table.queues.remove(&(msqid as usize)).unwrap();
            drop(table);
            removed.readers.wake_all();
            removed.writers.wake_all();
            0
        }
        _ => -22,
    }
}

// ======================= SEMAPHORES =======================

#[derive(Clone, Copy, Default)]
struct SemElem {
    val: i32,
    pid: i32,
    ncnt: u16,
    zcnt: u16,
}

struct SemSet {
    perm: IpcPerm,
    otime: i64,
    ctime: i64,
    sems: Vec<SemElem>,
    undos: BTreeMap<usize, Vec<i32>>, // pid -> per-sem undo delta
    waiters: Arc<WaitQueue>,
}

struct SemTable {
    next_id: usize,
    seq: u16,
    sets: BTreeMap<usize, SemSet>,
}

lazy_static! {
    static ref SEM_TABLE: UPSafeCell<SemTable> = unsafe {
        UPSafeCell::new(SemTable {
            next_id: 1,
            seq: 1,
            sets: BTreeMap::new(),
        })
    };
}

pub fn sys_semget(key: i32, nsems: i32, semflg: usize) -> isize {
    if nsems < 0 || nsems as usize > SEMMSL {
        return -22;
    }
    let mut table = SEM_TABLE.exclusive_access();
    if key != IPC_PRIVATE {
        if let Some((&id, s)) = table.sets.iter().find(|(_, s)| s.perm.key == key) {
            if semflg & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return -17;
            }
            if nsems as usize > s.sems.len() {
                return -22;
            }
            if let Err(e) = s.perm.check_perm((semflg & 0o777) as u16) {
                return e;
            }
            return id as isize;
        }
        if semflg & IPC_CREAT == 0 {
            return -2;
        }
    }
    if nsems == 0 {
        return -22;
    }
    if table.sets.len() >= SEMMNI {
        return -28;
    }
    let id = table.next_id;
    table.next_id += 1;
    let seq = table.seq;
    table.seq = table.seq.wrapping_add(1);
    let perm = IpcPerm::new(key, (semflg & 0o777) as u16, seq);
    table.sets.insert(
        id,
        SemSet {
            perm,
            otime: 0,
            ctime: now_sec(),
            sems: vec![SemElem::default(); nsems as usize],
            undos: BTreeMap::new(),
            waiters: WaitQueue::new(),
        },
    );
    id as isize
}

pub fn sys_semctl(semid: i32, semnum: i32, cmd: i32, arg: usize) -> isize {
    let token = current_user_token();
    let real_cmd = cmd & !0x100; // strip IPC_64 if passed
    match real_cmd {
        IPC_INFO | SEM_INFO => {
            if arg == 0 {
                return -14;
            }
            let used = SEM_TABLE.exclusive_access().sets.len() as i32;
            let mut info = [0u8; 40];
            let fields: [i32; 10] = if real_cmd == SEM_INFO {
                [used, 0, 0, used, SEMMSL as i32, 32000, 500, SEMMNI as i32, SEMVMX, SEMVMX]
            } else {
                [SEMMNI as i32, SEMMNI as i32, 32000, SEMMNI as i32, SEMMSL as i32, 500, 100, 32, SEMVMX, SEMVMX]
            };
            for (i, v) in fields.into_iter().enumerate() {
                info[i * 4..(i + 1) * 4].copy_from_slice(&v.to_ne_bytes());
            }
            let Some(b) = translated_byte_buffer(token, arg as *const u8, 40, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&info);
            used as isize
        }
        IPC_STAT | SEM_STAT | SEM_STAT_ANY => {
            if semid < 0 {
                return -22;
            }
            if arg == 0 {
                return -14;
            }
            let table = SEM_TABLE.exclusive_access();
            let (real_id, s) = if matches!(real_cmd, SEM_STAT | SEM_STAT_ANY) {
                match table.sets.iter().nth(semid as usize) {
                    Some((&id, s)) => (id, s),
                    None => return -22,
                }
            } else {
                match table.sets.get(&(semid as usize)) {
                    Some(s) => (semid as usize, s),
                    None => return -22,
                }
            };
            if real_cmd != SEM_STAT_ANY {
                if let Err(e) = s.perm.check_perm(4) {
                    return e;
                }
            }
            // Linux LP64 struct semid64_ds (88 bytes)
            let mut ds = [0u8; 88];
            s.perm.encode(&mut ds[0..48]);
            ds[48..56].copy_from_slice(&s.otime.to_ne_bytes());
            ds[56..64].copy_from_slice(&s.ctime.to_ne_bytes());
            ds[64..72].copy_from_slice(&(s.sems.len() as u64).to_ne_bytes());
            drop(table);
            let Some(b) = translated_byte_buffer(token, arg as *const u8, 88, MapPermission::W) else {
                return -14;
            };
            UserBuffer::new(b).copy_from(&ds);
            if matches!(real_cmd, SEM_STAT | SEM_STAT_ANY) { real_id as isize } else { 0 }
        }
        IPC_SET => {
            if semid < 0 {
                return -22;
            }
            if arg == 0 {
                return -14;
            }
            let Some(b) = translated_byte_buffer(token, arg as *const u8, 48, MapPermission::R) else {
                return -14;
            };
            let mut raw = [0u8; 48];
            for (d, s_byte) in raw.iter_mut().zip(UserBuffer::new(b)) {
                *d = s_byte.read();
            }
            let new_uid = u32::from_ne_bytes(raw[4..8].try_into().unwrap());
            let new_gid = u32::from_ne_bytes(raw[8..12].try_into().unwrap());
            let new_mode = u16::from_ne_bytes([raw[20], raw[21]]) & 0o777;
            let mut table = SEM_TABLE.exclusive_access();
            let Some(s) = table.sets.get_mut(&(semid as usize)) else {
                return -22;
            };
            if !s.perm.can_control() {
                return -1;
            }
            s.perm.uid = new_uid;
            s.perm.gid = new_gid;
            s.perm.mode = new_mode;
            s.ctime = now_sec();
            0
        }
        IPC_RMID => {
            if semid < 0 {
                return -22;
            }
            let mut table = SEM_TABLE.exclusive_access();
            let Some(s) = table.sets.get(&(semid as usize)) else {
                return -22;
            };
            if !s.perm.can_control() {
                return -1;
            }
            let removed = table.sets.remove(&(semid as usize)).unwrap();
            drop(table);
            removed.waiters.wake_all();
            0
        }
        GETVAL | GETPID | GETNCNT | GETZCNT => {
            if semid < 0 || semnum < 0 {
                return -22;
            }
            let table = SEM_TABLE.exclusive_access();
            let Some(s) = table.sets.get(&(semid as usize)) else {
                return -22;
            };
            if let Err(e) = s.perm.check_perm(4) {
                return e;
            }
            let Some(elem) = s.sems.get(semnum as usize) else {
                return -22;
            };
            match real_cmd {
                GETVAL => elem.val as isize,
                GETPID => elem.pid as isize,
                GETNCNT => elem.ncnt as isize,
                _ => elem.zcnt as isize,
            }
        }
        SETVAL => {
            if semid < 0 || semnum < 0 {
                return -22;
            }
            let val = arg as i32;
            if !(0..=SEMVMX).contains(&val) {
                return -34; // ERANGE
            }
            let mut table = SEM_TABLE.exclusive_access();
            let Some(s) = table.sets.get_mut(&(semid as usize)) else {
                return -22;
            };
            if let Err(e) = s.perm.check_perm(2) {
                return e;
            }
            let idx = semnum as usize;
            if idx >= s.sems.len() {
                return -22;
            }
            s.sems[idx].val = val;
            s.sems[idx].pid = current_process().getpid() as i32;
            for undo in s.undos.values_mut() {
                if let Some(slot) = undo.get_mut(idx) {
                    *slot = 0;
                }
            }
            s.ctime = now_sec();
            let wq = s.waiters.clone();
            drop(table);
            wq.wake_all();
            0
        }
        GETALL => {
            if semid < 0 {
                return -22;
            }
            if arg == 0 {
                return -14;
            }
            let vals: Vec<u16> = {
                let table = SEM_TABLE.exclusive_access();
                let Some(s) = table.sets.get(&(semid as usize)) else {
                    return -22;
                };
                if let Err(e) = s.perm.check_perm(4) {
                    return e;
                }
                s.sems.iter().map(|e| e.val as u16).collect()
            };
            let byte_len = vals.len() * 2;
            let Some(b) = translated_byte_buffer(token, arg as *const u8, byte_len, MapPermission::W) else {
                return -14;
            };
            let raw: Vec<u8> = vals.into_iter().flat_map(u16::to_ne_bytes).collect();
            UserBuffer::new(b).copy_from(&raw);
            0
        }
        SETALL => {
            if semid < 0 {
                return -22;
            }
            if arg == 0 {
                return -14;
            }
            let count = {
                let table = SEM_TABLE.exclusive_access();
                let Some(s) = table.sets.get(&(semid as usize)) else {
                    return -22;
                };
                if let Err(e) = s.perm.check_perm(2) {
                    return e;
                }
                s.sems.len()
            };
            let Some(b) = translated_byte_buffer(token, arg as *const u8, count * 2, MapPermission::R) else {
                return -14;
            };
            let mut raw = vec![0u8; count * 2];
            for (d, s_byte) in raw.iter_mut().zip(UserBuffer::new(b)) {
                *d = s_byte.read();
            }
            let vals: Vec<i32> = raw
                .chunks_exact(2)
                .map(|c| i32::from(u16::from_ne_bytes([c[0], c[1]])))
                .collect();
            if vals.iter().any(|&v| v > SEMVMX) {
                return -34;
            }
            let pid = current_process().getpid() as i32;
            let mut table = SEM_TABLE.exclusive_access();
            let Some(s) = table.sets.get_mut(&(semid as usize)) else {
                return -22;
            };
            for (elem, v) in s.sems.iter_mut().zip(vals) {
                elem.val = v;
                elem.pid = pid;
            }
            s.undos.clear();
            s.ctime = now_sec();
            let wq = s.waiters.clone();
            drop(table);
            wq.wake_all();
            0
        }
        _ => -22,
    }
}

pub fn sys_semtimedop(semid: i32, sops: usize, nsops: usize, timeout: usize) -> isize {
    if semid < 0 || nsops == 0 {
        return -22;
    }
    if nsops > 500 {
        return -7; // E2BIG
    }
    if sops == 0 {
        return -14;
    }
    let token = current_user_token();
    let deadline_ms = if timeout == 0 {
        None
    } else {
        let Some(sec) = try_read_user(token, timeout as *const i64) else {
            return -14;
        };
        let Some(nsec) = timeout
            .checked_add(8)
            .and_then(|p| try_read_user(token, p as *const i64))
        else {
            return -14;
        };
        if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
            return -22;
        }
        let dur_ms = (sec as usize)
            .saturating_mul(1000)
            .saturating_add((nsec as usize + 999_999) / 1_000_000);
        Some(crate::timer::get_time_ms().saturating_add(dur_ms))
    };
    let Some(bufs) = translated_byte_buffer(token, sops as *const u8, nsops * 6, MapPermission::R) else {
        return -14;
    };
    let mut raw = vec![0u8; nsops * 6];
    for (d, s) in raw.iter_mut().zip(UserBuffer::new(bufs)) {
        *d = s.read();
    }
    let mut ops = Vec::with_capacity(nsops);
    let mut want_alter = false;
    for chunk in raw.chunks_exact(6) {
        let num = u16::from_ne_bytes([chunk[0], chunk[1]]) as usize;
        let op = i16::from_ne_bytes([chunk[2], chunk[3]]);
        let flg = i16::from_ne_bytes([chunk[4], chunk[5]]);
        if op != 0 {
            want_alter = true;
        }
        ops.push((num, op, flg));
    }

    let id = semid as usize;
    let pid = current_process().getpid();
    let mut first = true;
    loop {
        let mut table = SEM_TABLE.exclusive_access();
        let Some(s) = table.sets.get_mut(&id) else {
            return if first { -22 } else { -43 }; // EINVAL initially, EIDRM if removed
        };
        first = false;
        if let Err(e) = s.perm.check_perm(if want_alter { 2 } else { 4 }) {
            return e;
        }
        let nsems = s.sems.len();
        if ops.iter().any(|&(num, _, _)| num >= nsems) {
            return -27; // EFBIG
        }
        // Trial apply on a scratch copy so multi-op atomicity holds
        let mut trial: Vec<i32> = s.sems.iter().map(|e| e.val).collect();
        let mut blocked: Option<(usize, i16, i16)> = None;
        for &(num, op, flg) in &ops {
            if op > 0 {
                let next = trial[num] + i32::from(op);
                if next > SEMVMX {
                    return -34; // ERANGE
                }
                trial[num] = next;
            } else if op == 0 {
                if trial[num] != 0 {
                    blocked = Some((num, op, flg));
                    break;
                }
            } else {
                let need = (-i32::from(op)) as i32;
                if trial[num] < need {
                    blocked = Some((num, op, flg));
                    break;
                }
                trial[num] -= need;
            }
        }

        if blocked.is_none() {
            for (idx, &new_val) in trial.iter().enumerate() {
                s.sems[idx].val = new_val;
            }
            for &(num, op, flg) in &ops {
                s.sems[num].pid = pid as i32;
                if flg & SEM_UNDO != 0 && op != 0 {
                    let undo = s.undos.entry(pid).or_insert_with(|| vec![0; nsems]);
                    undo[num] = undo[num].saturating_sub(i32::from(op));
                }
            }
            // Waiters might also become unblocked if values rose or dropped to 0
            s.otime = now_sec();
            let wq = s.waiters.clone();
            drop(table);
            wq.wake_all();
            return 0;
        }

        let (bnum, bop, bflg) = blocked.unwrap();
        if (bflg as usize) & IPC_NOWAIT != 0 {
            return -11; // EAGAIN
        }
        if let Some(dl) = deadline_ms {
            if crate::timer::get_time_ms() >= dl {
                return -11; // EAGAIN
            }
        }
        if bop == 0 {
            s.sems[bnum].zcnt = s.sems[bnum].zcnt.saturating_add(1);
        } else {
            s.sems[bnum].ncnt = s.sems[bnum].ncnt.saturating_add(1);
        }
        let wq = s.waiters.clone();
        if !wq.register_current_interruptible() {
            if bop == 0 {
                s.sems[bnum].zcnt = s.sems[bnum].zcnt.saturating_sub(1);
            } else {
                s.sems[bnum].ncnt = s.sems[bnum].ncnt.saturating_sub(1);
            }
            return -4;
        }
        drop(table);
        if let Some(dl) = deadline_ms {
            crate::timer::add_timer(dl, current_task().unwrap());
        }
        block_current_and_run_next();
        if let Some(t) = current_task() {
            WaitQueue::cancel(&t);
            crate::timer::remove_timer(t);
        }
        {
            let mut table = SEM_TABLE.exclusive_access();
            if let Some(s) = table.sets.get_mut(&id) {
                if bop == 0 {
                    s.sems[bnum].zcnt = s.sems[bnum].zcnt.saturating_sub(1);
                } else {
                    s.sems[bnum].ncnt = s.sems[bnum].ncnt.saturating_sub(1);
                }
            } else {
                return -43; // EIDRM
            }
        }
        crate::task::check_kill_current();
        if super::signal::current_wait_interrupted() {
            return -4;
        }
        if let Some(dl) = deadline_ms {
            if crate::timer::get_time_ms() >= dl {
                return -11;
            }
        }
    }
}

pub(crate) fn on_process_exit(pid: usize, shm: &[ShmAttachment]) {
    for attachment in shm {
        shm_detach_id(attachment.id, pid);
    }
    let mut to_wake = Vec::new();
    {
        let mut table = SEM_TABLE.exclusive_access();
        for s in table.sets.values_mut() {
            if let Some(adj) = s.undos.remove(&pid) {
                let mut changed = false;
                for (elem, delta) in s.sems.iter_mut().zip(adj) {
                    if delta != 0 {
                        elem.val = (elem.val + delta).clamp(0, SEMVMX);
                        elem.pid = pid as i32;
                        changed = true;
                    }
                }
                if changed {
                    to_wake.push(s.waiters.clone());
                }
            }
        }
    }
    for wq in to_wake {
        wq.wake_all();
    }
}

pub fn proc_snapshot(kind: &str) -> Option<Vec<u8>> {
    use core::fmt::Write;
    match kind {
        "shm" => {
            let mut s = alloc::string::String::from(
                "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n",
            );
            let table = SHM_TABLE.exclusive_access();
            for (&id, seg) in &table.segments {
                let mut mode = seg.perm.mode;
                if seg.marked_rmid {
                    mode |= SHM_DEST;
                }
                if seg.locked {
                    mode |= SHM_LOCKED;
                }
                let _ = writeln!(
                    s,
                    "{:10} {:10}  {:4o} {:21} {:5} {:5} {:6} {:5} {:5} {:5} {:5} {:10} {:10} {:10} {:21} {:21}",
                    seg.perm.key, id, mode, seg.size, seg.cpid, seg.lpid, seg.nattch,
                    seg.perm.uid, seg.perm.gid, seg.perm.cuid, seg.perm.cgid,
                    seg.atime, seg.dtime, seg.ctime, seg.size, 0usize
                );
            }
            Some(s.into_bytes())
        }
        "msg" => {
            let mut s = alloc::string::String::from(
                "       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n",
            );
            let table = MSG_TABLE.exclusive_access();
            for (&id, q) in &table.queues {
                let _ = writeln!(
                    s,
                    "{:10} {:10}  {:4o} {:11} {:10} {:5} {:5} {:5} {:5} {:5} {:5} {:10} {:10} {:10}",
                    q.perm.key, id, q.perm.mode, q.cbytes, q.messages.len(),
                    q.lspid, q.lrpid, q.perm.uid, q.perm.gid, q.perm.cuid, q.perm.cgid,
                    q.stime, q.rtime, q.ctime
                );
            }
            Some(s.into_bytes())
        }
        "sem" => {
            let mut s = alloc::string::String::from(
                "       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n",
            );
            let table = SEM_TABLE.exclusive_access();
            for (&id, sem) in &table.sets {
                let _ = writeln!(
                    s,
                    "{} {} {:o} {} {} {} {} {} {} {}",
                    format!("{:10}", sem.perm.key),
                    format!("{:10}", id),
                    sem.perm.mode,
                    sem.sems.len(),
                    sem.perm.uid,
                    sem.perm.gid,
                    sem.perm.cuid,
                    sem.perm.cgid,
                    sem.otime,
                    sem.ctime
                );
            }
            Some(s.into_bytes())
        }
        _ => None,
    }
}
