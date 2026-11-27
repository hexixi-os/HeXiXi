//! Event, timer, signal, and anonymous memory file descriptors:
//! `eventfd2(19)`, `signalfd4(74)`, `timerfd_create(85)`, `timerfd_settime(86)`,
//! `timerfd_gettime(87)`, and `memfd_create(279)`.

use super::net::install;
use crate::fs::{overlay, File};
use crate::mm::{translated_byte_buffer, translated_path, try_read_user, try_write_user, MapPermission, UserBuffer};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_process, current_task, current_user_token, WaitQueue};
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicU32, Ordering};
use lazy_static::lazy_static;

const EFD_SEMAPHORE: u32 = 1;
const EFD_CLOEXEC: u32 = 0o2000000;
const EFD_NONBLOCK: u32 = 0o4000;

struct EventFdInner {
    counter: u64,
    read_gen: u64,
    write_gen: u64,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
}

pub struct EventFdFile {
    semaphore: bool,
    flags: AtomicU32,
    inner: UPSafeCell<EventFdInner>,
}

impl File for EventFdFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn epollable(&self) -> bool {
        true
    }
    fn status_flags(&self) -> u32 {
        2 | self.flags.load(Ordering::Relaxed)
    }
    fn set_status_flags(&self, flags: u32) {
        self.flags.store(flags & EFD_NONBLOCK, Ordering::Relaxed);
    }
    fn poll_readable(&self) -> bool {
        self.inner.exclusive_access().counter > 0
    }
    fn poll_writable(&self) -> bool {
        self.inner.exclusive_access().counter < u64::MAX - 1
    }
    fn poll_events(&self) -> u16 {
        let inner = self.inner.exclusive_access();
        let mut ev = 0u16;
        if inner.counter > 0 {
            ev |= 1; // POLLIN
        }
        if inner.counter < u64::MAX - 1 {
            ev |= 4; // POLLOUT
        }
        if inner.counter == u64::MAX {
            ev |= 8; // POLLERR
        }
        ev
    }
    fn poll_generations(&self) -> (u64, u64) {
        let inner = self.inner.exclusive_access();
        (inner.read_gen, inner.write_gen)
    }
    fn read(&self, buf: UserBuffer) -> usize {
        self.read_result(buf).max(0) as usize
    }
    fn read_result(&self, buf: UserBuffer) -> isize {
        if buf.len() < 8 {
            return -22; // EINVAL
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & EFD_NONBLOCK != 0;
        loop {
            let mut inner = self.inner.exclusive_access();
            if inner.counter > 0 {
                let val = if self.semaphore {
                    inner.counter -= 1;
                    1u64
                } else {
                    let v = inner.counter;
                    inner.counter = 0;
                    v
                };
                inner.write_gen = inner.write_gen.wrapping_add(1);
                let writers = inner.writers.clone();
                drop(inner);
                buf.copy_from(&val.to_ne_bytes());
                writers.wake_all();
                crate::task::wake_poll_waiters();
                return 8;
            }
            if nonblock {
                return -11; // EAGAIN
            }
            if !inner.readers.register_current_interruptible() {
                return -4;
            }
            drop(inner);
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
    fn write(&self, buf: UserBuffer) -> isize {
        if buf.len() < 8 {
            return -22;
        }
        let mut raw = [0u8; 8];
        for (d, s) in raw.iter_mut().zip(buf.into_iter()) {
            *d = s.read();
        }
        let add = u64::from_ne_bytes(raw);
        if add == u64::MAX {
            return -22;
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & EFD_NONBLOCK != 0;
        loop {
            let mut inner = self.inner.exclusive_access();
            if add <= (u64::MAX - 1) - inner.counter {
                inner.counter += add;
                if add > 0 {
                    inner.read_gen = inner.read_gen.wrapping_add(1);
                }
                let readers = inner.readers.clone();
                drop(inner);
                if add > 0 {
                    readers.wake_all();
                    crate::task::wake_poll_waiters();
                }
                return 8;
            }
            if nonblock {
                return -11;
            }
            if !inner.writers.register_current_interruptible() {
                return -4;
            }
            drop(inner);
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
}

pub fn sys_eventfd2(initval: u32, flags: u32) -> isize {
    if flags & !(EFD_SEMAPHORE | EFD_CLOEXEC | EFD_NONBLOCK) != 0 {
        return -22;
    }
    let file = Arc::new(EventFdFile {
        semaphore: flags & EFD_SEMAPHORE != 0,
        flags: AtomicU32::new(flags & EFD_NONBLOCK),
        inner: unsafe {
            UPSafeCell::new(EventFdInner {
                counter: u64::from(initval),
                read_gen: u64::from(initval > 0),
                write_gen: 1,
                readers: WaitQueue::new(),
                writers: WaitQueue::new(),
            })
        },
    });
    install(file, flags & EFD_CLOEXEC != 0).map_or_else(|e| e, |fd| fd as isize)
}

const TFD_CLOEXEC: u32 = 0o2000000;
const TFD_NONBLOCK: u32 = 0o4000;
const TFD_TIMER_ABSTIME: u32 = 1;
const TFD_TIMER_CANCEL_ON_SET: u32 = 2;

struct TimerFdInner {
    deadline_ns: u64, // 0 = disarmed
    interval_ns: u64,
    set_gen: u64,
    expiry_notified: bool,
}

pub struct TimerFdFile {
    clockid: i32,
    flags: AtomicU32,
    inner: UPSafeCell<TimerFdInner>,
}

lazy_static! {
    static ref ARMED_TIMERFDS: UPSafeCell<BTreeMap<usize, Weak<dyn File + Send + Sync>>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

fn timerfd_delay_ms(remaining_ns: u64) -> usize {
    ((remaining_ns / 1_000_000) as usize).max(1)
}

pub(crate) fn next_timerfd_ms() -> Option<usize> {
    let armed = ARMED_TIMERFDS.exclusive_access();
    let remaining = armed.values().filter_map(|weak| {
        let file = weak.upgrade()?;
        let timer = file.timerfd()?;
        let inner = timer.inner.exclusive_access();
        (inner.deadline_ns != 0 && !inner.expiry_notified)
            .then(|| inner.deadline_ns.saturating_sub(timer.now_ns()))
    }).min()?;
    Some(crate::timer::get_time_ms().saturating_add(timerfd_delay_ms(remaining)))
}

pub(crate) fn check_timerfds() {
    let mut ready = false;
    ARMED_TIMERFDS.exclusive_access().retain(|_, weak| {
        let Some(file) = weak.upgrade() else { return false };
        let timer = file.timerfd().unwrap();
        let mut inner = timer.inner.exclusive_access();
        if inner.deadline_ns == 0 {
            return false;
        }
        if !inner.expiry_notified && timer.now_ns() >= inner.deadline_ns {
            inner.expiry_notified = true;
            ready = true;
        }
        true
    });
    if ready {
        crate::task::wake_poll_waiters();
    }
}

impl TimerFdFile {
    fn now_ns(&self) -> u64 {
        if matches!(self.clockid, 0 | 8) {
            crate::timer::get_realtime_ns()
        } else {
            crate::timer::get_time_ns() as u64
        }
    }

    fn expirations_peek(&self) -> u64 {
        let inner = self.inner.exclusive_access();
        if inner.deadline_ns == 0 {
            return 0;
        }
        let now = self.now_ns();
        if now < inner.deadline_ns {
            0
        } else if inner.interval_ns == 0 {
            1
        } else {
            1 + (now - inner.deadline_ns) / inner.interval_ns
        }
    }

    fn consume_expirations(&self) -> (u64, Option<u64>) {
        let mut inner = self.inner.exclusive_access();
        if inner.deadline_ns == 0 {
            return (0, None);
        }
        let now = self.now_ns();
        if now < inner.deadline_ns {
            let wait_mono = if matches!(self.clockid, 0 | 8) {
                let delta = inner.deadline_ns - now;
                (crate::timer::get_time_ns() as u64).saturating_add(delta)
            } else {
                inner.deadline_ns
            };
            return (0, Some(wait_mono));
        }
        inner.set_gen = inner.set_gen.wrapping_add(2);
        inner.expiry_notified = false;
        if inner.interval_ns == 0 {
            inner.deadline_ns = 0;
            (1, None)
        } else {
            let count = 1 + (now - inner.deadline_ns) / inner.interval_ns;
            inner.deadline_ns = inner.deadline_ns.saturating_add(count.saturating_mul(inner.interval_ns));
            let remaining = inner.deadline_ns.saturating_sub(now);
            drop(inner);
            crate::timer::arm_within_ms(timerfd_delay_ms(remaining));
            (count, None)
        }
    }
}

impl File for TimerFdFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn epollable(&self) -> bool {
        true
    }
    fn timerfd(&self) -> Option<&TimerFdFile> {
        Some(self)
    }
    fn status_flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }
    fn set_status_flags(&self, flags: u32) {
        self.flags.store(flags & TFD_NONBLOCK, Ordering::Relaxed);
    }
    fn poll_readable(&self) -> bool {
        self.expirations_peek() > 0
    }
    fn poll_writable(&self) -> bool {
        false
    }
    fn poll_events(&self) -> u16 {
        if self.expirations_peek() > 0 { 1 } else { 0 }
    }
    fn poll_generations(&self) -> (u64, u64) {
        let inner = self.inner.exclusive_access();
        let exp = if inner.deadline_ns != 0 && self.now_ns() >= inner.deadline_ns { 1 } else { 0 };
        (inner.set_gen.wrapping_add(exp), 0)
    }
    fn read(&self, buf: UserBuffer) -> usize {
        self.read_result(buf).max(0) as usize
    }
    fn read_result(&self, buf: UserBuffer) -> isize {
        if buf.len() < 8 {
            return -22;
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & TFD_NONBLOCK != 0;
        loop {
            let (count, next_mono) = self.consume_expirations();
            if count > 0 {
                buf.copy_from(&count.to_ne_bytes());
                return 8;
            }
            if nonblock {
                return -11;
            }
            match next_mono {
                Some(deadline) => {
                    if let Err(e) = crate::net::wait_for_io_until(Some(deadline as usize)) {
                        return e;
                    }
                }
                None => {
                    if let Err(e) = crate::net::wait_for_io_until(None) {
                        return e;
                    }
                }
            }
        }
    }
    fn write(&self, _: UserBuffer) -> isize {
        -22
    }
}

fn downcast_timerfd(file: &Arc<dyn File + Send + Sync>) -> Option<&TimerFdFile> {
    // Identify TimerFdFile via trait helper
    file.timerfd()
}

pub fn sys_timerfd_create(clockid: i32, flags: u32) -> isize {
    // CLOCK_REALTIME(0), CLOCK_MONOTONIC(1), CLOCK_BOOTTIME(7), CLOCK_REALTIME_ALARM(8), CLOCK_BOOTTIME_ALARM(9)
    if !matches!(clockid, 0 | 1 | 7 | 8 | 9) {
        return -22;
    }
    if flags & !(TFD_CLOEXEC | TFD_NONBLOCK) != 0 {
        return -22;
    }
    let file = Arc::new(TimerFdFile {
        clockid,
        flags: AtomicU32::new(flags & TFD_NONBLOCK),
        inner: unsafe {
            UPSafeCell::new(TimerFdInner {
                deadline_ns: 0,
                interval_ns: 0,
                set_gen: 1,
                expiry_notified: false,
            })
        },
    });
    install(file, flags & TFD_CLOEXEC != 0).map_or_else(|e| e, |fd| fd as isize)
}

fn read_itimerspec(ptr: usize) -> Result<(u64, u64), isize> {
    let token = current_user_token();
    let bufs = translated_byte_buffer(token, ptr as *const u8, 32, MapPermission::R).ok_or(-14isize)?;
    let mut raw = [0u8; 32];
    for (d, s) in raw.iter_mut().zip(UserBuffer::new(bufs)) {
        *d = s.read();
    }
    let isec = i64::from_ne_bytes(raw[0..8].try_into().unwrap());
    let insec = i64::from_ne_bytes(raw[8..16].try_into().unwrap());
    let vsec = i64::from_ne_bytes(raw[16..24].try_into().unwrap());
    let vnsec = i64::from_ne_bytes(raw[24..32].try_into().unwrap());
    if isec < 0 || !(0..1_000_000_000).contains(&insec) || vsec < 0 || !(0..1_000_000_000).contains(&vnsec) {
        return Err(-22);
    }
    let interval = (isec as u64).saturating_mul(1_000_000_000).saturating_add(insec as u64);
    let value = (vsec as u64).saturating_mul(1_000_000_000).saturating_add(vnsec as u64);
    Ok((interval, value))
}

fn write_itimerspec(ptr: usize, interval_ns: u64, rem_ns: u64) -> Result<(), isize> {
    let token = current_user_token();
    let bufs = translated_byte_buffer(token, ptr as *const u8, 32, MapPermission::W).ok_or(-14isize)?;
    let mut raw = [0u8; 32];
    raw[0..8].copy_from_slice(&((interval_ns / 1_000_000_000) as i64).to_ne_bytes());
    raw[8..16].copy_from_slice(&((interval_ns % 1_000_000_000) as i64).to_ne_bytes());
    raw[16..24].copy_from_slice(&((rem_ns / 1_000_000_000) as i64).to_ne_bytes());
    raw[24..32].copy_from_slice(&((rem_ns % 1_000_000_000) as i64).to_ne_bytes());
    UserBuffer::new(bufs).copy_from(&raw);
    Ok(())
}

pub fn sys_timerfd_settime(fd: usize, flags: u32, new_val: usize, old_val: usize) -> isize {
    if flags & !(TFD_TIMER_ABSTIME | TFD_TIMER_CANCEL_ON_SET) != 0 {
        return -22;
    }
    if new_val == 0 {
        return -14;
    }
    let file = match super::fs::file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let Some(tfd) = downcast_timerfd(&file) else {
        return -22;
    };
    let (new_interval, new_value) = match read_itimerspec(new_val) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let now = tfd.now_ns();
    if old_val != 0 {
        let inner = tfd.inner.exclusive_access();
        let rem = if inner.deadline_ns == 0 {
            0
        } else {
            inner.deadline_ns.saturating_sub(now).max(1)
        };
        let prev_int = inner.interval_ns;
        drop(inner);
        if let Err(e) = write_itimerspec(old_val, prev_int, rem) {
            return e;
        }
    }
    let mut inner = tfd.inner.exclusive_access();
    inner.set_gen = inner.set_gen.wrapping_add(2);
    inner.expiry_notified = false;
    if new_value == 0 {
        inner.deadline_ns = 0;
        inner.interval_ns = 0;
    } else {
        inner.interval_ns = new_interval;
        inner.deadline_ns = if flags & TFD_TIMER_ABSTIME != 0 {
            new_value.max(1)
        } else {
            now.saturating_add(new_value).max(1)
        };
    }
    let deadline_ns = inner.deadline_ns;
    drop(inner);
    let key = tfd as *const TimerFdFile as usize;
    if deadline_ns == 0 {
        ARMED_TIMERFDS.exclusive_access().remove(&key);
    } else {
        ARMED_TIMERFDS.exclusive_access().insert(key, Arc::downgrade(&file));
        crate::timer::arm_within_ms(timerfd_delay_ms(deadline_ns.saturating_sub(now)));
    }
    crate::task::wake_poll_waiters();
    0
}

pub fn sys_timerfd_gettime(fd: usize, curr_val: usize) -> isize {
    if curr_val == 0 {
        return -14;
    }
    let file = match super::fs::file_by_fd(fd) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let Some(tfd) = downcast_timerfd(&file) else {
        return -22;
    };
    let now = tfd.now_ns();
    let (interval, rem) = {
        let inner = tfd.inner.exclusive_access();
        let r = if inner.deadline_ns == 0 {
            0
        } else if now >= inner.deadline_ns {
            if inner.interval_ns == 0 {
                0
            } else {
                let elapsed = (now - inner.deadline_ns) % inner.interval_ns;
                inner.interval_ns - elapsed
            }
        } else {
            inner.deadline_ns - now
        };
        (inner.interval_ns, r)
    };
    write_itimerspec(curr_val, interval, rem).err().unwrap_or(0)
}

const SFD_CLOEXEC: u32 = 0o2000000;
const SFD_NONBLOCK: u32 = 0o4000;
const UNBLOCKABLE: u64 = (1 << (9 - 1)) | (1 << (19 - 1));

pub struct SignalFdFile {
    mask: UPSafeCell<u64>,
    flags: AtomicU32,
}

impl SignalFdFile {
    pub(crate) fn watches(&self, signal: u64) -> bool {
        *self.mask.exclusive_access() & signal != 0
    }

    fn has_pending(&self) -> bool {
        let mask = *self.mask.exclusive_access();
        let proc = current_process();
        let pinner = proc.inner_exclusive_access();
        let task = current_task().unwrap();
        let tinner = task.inner_exclusive_access();
        ((pinner.signal_pending | tinner.pending_signals) & mask) != 0
    }
}

impl File for SignalFdFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        false
    }
    fn epollable(&self) -> bool {
        true
    }
    fn signalfd(&self) -> Option<&SignalFdFile> {
        Some(self)
    }
    fn status_flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }
    fn set_status_flags(&self, flags: u32) {
        self.flags.store(flags & SFD_NONBLOCK, Ordering::Relaxed);
    }
    fn poll_readable(&self) -> bool {
        self.has_pending()
    }
    fn poll_writable(&self) -> bool {
        false
    }
    fn poll_events(&self) -> u16 {
        if self.has_pending() { 1 } else { 0 }
    }
    fn read(&self, buf: UserBuffer) -> usize {
        self.read_result(buf).max(0) as usize
    }
    fn read_result(&self, buf: UserBuffer) -> isize {
        if buf.len() < 128 {
            return -22;
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & SFD_NONBLOCK != 0;
        let max_items = buf.len() / 128;
        let mut out = alloc::vec::Vec::new();
        loop {
            let mask = *self.mask.exclusive_access();
            while out.len() / 128 < max_items {
                let Some(info) = super::signal::take_pending_info(mask) else {
                    break;
                };
                let mut siginfo = [0u8; 128];
                siginfo[0..4].copy_from_slice(&(info.signo as u32).to_ne_bytes());
                siginfo[8..12].copy_from_slice(&info.code().to_ne_bytes());
                siginfo[12..16].copy_from_slice(&info.sender_pid().to_ne_bytes());
                siginfo[16..20].copy_from_slice(&info.sender_uid().to_ne_bytes());
                siginfo[40..44].copy_from_slice(&info.child_status().to_ne_bytes());
                out.extend_from_slice(&siginfo);
            }
            if !out.is_empty() {
                buf.copy_from(&out);
                return out.len() as isize;
            }
            if nonblock {
                return -11;
            }
            if !crate::task::poll_waiters().register_current_interruptible() {
                return -4;
            }
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
    fn write(&self, _: UserBuffer) -> isize {
        -22
    }
}

pub fn sys_signalfd4(fd: isize, mask_ptr: usize, sizemask: usize, flags: u32) -> isize {
    if sizemask != 8 || flags & !(SFD_CLOEXEC | SFD_NONBLOCK) != 0 {
        return -22;
    }
    let token = current_user_token();
    let Some(mask) = try_read_user(token, mask_ptr as *const u64) else {
        return -14;
    };
    let mask = mask & !UNBLOCKABLE;
    if fd == -1 {
        let file = Arc::new(SignalFdFile {
            mask: unsafe { UPSafeCell::new(mask) },
            flags: AtomicU32::new(flags & SFD_NONBLOCK),
        });
        install(file, flags & SFD_CLOEXEC != 0).map_or_else(|e| e, |new_fd| new_fd as isize)
    } else {
        if fd < 0 {
            return -9;
        }
        let Ok(existing) = super::fs::file_by_fd(fd as usize) else {
            return -9;
        };
        let Some(sfd) = existing.signalfd() else {
            return -22;
        };
        *sfd.mask.exclusive_access() = mask;
        crate::task::wake_poll_waiters();
        fd
    }
}

pub fn sys_memfd_create(uname: *const u8, flags: u32) -> isize {
    const MFD_CLOEXEC: u32 = 0x0001;
    const MFD_ALLOW_SEALING: u32 = 0x0002;
    const MFD_HUGETLB: u32 = 0x0004;
    const MFD_NOEXEC_SEAL: u32 = 0x0008;
    const MFD_EXEC: u32 = 0x0010;
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_HUGETLB | MFD_NOEXEC_SEAL | MFD_EXEC) != 0 {
        return -22;
    }
    if flags & (MFD_NOEXEC_SEAL | MFD_EXEC) == (MFD_NOEXEC_SEAL | MFD_EXEC) {
        return -22;
    }
    if flags & MFD_HUGETLB != 0 {
        return -38; // ENOSYS / no hugetlb
    }
    let token = current_user_token();
    let name = match translated_path(token, uname) {
        Ok(s) => s,
        Err(e) => return e,
    };
    // Linux limits memfd name to 249 bytes (excluding "memfd:" prefix + NUL in 256 bytes)
    if name.len() > 249 {
        return -22;
    }
    let allow_sealing = flags & (MFD_ALLOW_SEALING | MFD_NOEXEC_SEAL) != 0;
    let file = match overlay::new_memfd_file(allow_sealing) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let _ = try_write_user::<u8>;
    install(file, flags & MFD_CLOEXEC != 0).map_or_else(|e| e, |fd| fd as isize)
}
