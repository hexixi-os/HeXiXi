use super::fs::{file_by_fd, poll_deadline, poll_file, poll_wait, with_io_file};
use super::net::install;
use crate::fs::File;
use crate::mm::{translated_byte_buffer, MapPermission, UserBuffer};
use crate::sync::UPSafeCell;
use crate::task::current_user_token;
use alloc::{
    collections::VecDeque,
    sync::{Arc, Weak},
};

const EPOLL_CLOEXEC: usize = 0x80000;
const EPOLL_CTL_ADD: usize = 1;
const EPOLL_CTL_DEL: usize = 2;
const EPOLL_CTL_MOD: usize = 3;
const EPOLLERR_HUP: u32 = 8 | 16;
const EPOLLONESHOT: u32 = 1 << 30;
const SUPPORTED_EVENTS: u32 = 0x2000 | 0x1ff | EPOLLONESHOT;
const EVENT_SIZE: usize = 16;
const MAX_WATCHES: usize = 1024;

struct Watch {
    fd: usize,
    file: Weak<dyn File + Send + Sync>,
    events: u32,
    data: u64,
    enabled: bool,
}

pub struct Epoll {
    watches: UPSafeCell<VecDeque<Watch>>,
}

impl Watch {
    fn ready(&self) -> u32 {
        if !self.enabled {
            return 0;
        }
        self.file.upgrade().map_or(0, |file| {
            u32::from(poll_file(file.as_ref())) & (self.events | EPOLLERR_HUP)
        })
    }
}

impl Epoll {
    fn ctl(
        &self,
        op: usize,
        fd: usize,
        target: Arc<dyn File + Send + Sync>,
        pointer: usize,
    ) -> Result<(), isize> {
        if !matches!(op, EPOLL_CTL_ADD | EPOLL_CTL_DEL | EPOLL_CTL_MOD) || target.epoll().is_some()
        {
            return Err(-22);
        }
        if !target.epollable() {
            return Err(-1);
        }
        let target = Arc::downgrade(&target);
        let mut watches = self.watches.exclusive_access();
        watches.retain(|watch| watch.file.strong_count() != 0);
        let existing = watches
            .iter()
            .position(|watch| watch.fd == fd && watch.file.ptr_eq(&target));
        if op == EPOLL_CTL_DEL {
            watches.remove(existing.ok_or(-2isize)?);
            return Ok(());
        }
        let token = current_user_token();
        let user =
            translated_byte_buffer(token, pointer as *const u8, EVENT_SIZE, MapPermission::R)
                .ok_or(-14isize)?;
        // RV64 and LA64 both place the 64-bit data field at offset eight.
        let mut bytes = UserBuffer::new(user).into_iter().map(|byte| byte.read());
        let events = u32::from_ne_bytes(core::array::from_fn(|_| bytes.next().unwrap()));
        let mut data_bytes = bytes.skip(4);
        let data = u64::from_ne_bytes(core::array::from_fn(|_| data_bytes.next().unwrap()));
        if events & !SUPPORTED_EVENTS != 0 {
            return Err(-22);
        }
        let watch = Watch {
            fd,
            file: target,
            events,
            data,
            enabled: true,
        };
        match op {
            EPOLL_CTL_ADD if existing.is_some() => Err(-17),
            EPOLL_CTL_ADD if watches.len() >= MAX_WATCHES => Err(-28),
            EPOLL_CTL_ADD => {
                watches.push_back(watch);
                Ok(())
            }
            EPOLL_CTL_MOD => {
                watches[existing.ok_or(-2isize)?] = watch;
                Ok(())
            }
            _ => Err(-22),
        }
    }

    fn collect(&self, pointer: usize, max_events: usize) -> Result<usize, isize> {
        let token = current_user_token();
        let mut watches = self.watches.exclusive_access();
        watches.retain(|watch| watch.file.strong_count() != 0);
        let mut written = 0;
        for _ in 0..watches.len() {
            if written == max_events {
                break;
            }
            let watch = watches.front_mut().unwrap();
            let events = watch.ready();
            if events != 0 {
                let address = pointer + written * EVENT_SIZE;
                let buffer = match translated_byte_buffer(
                    token,
                    address as *const u8,
                    EVENT_SIZE,
                    MapPermission::W,
                ) {
                    Some(buffer) => UserBuffer::new(buffer),
                    None if written != 0 => break,
                    None => return Err(-14),
                };
                let mut event = [0; EVENT_SIZE];
                event[..4].copy_from_slice(&events.to_ne_bytes());
                event[8..].copy_from_slice(&watch.data.to_ne_bytes());
                buffer.copy_from(&event);
                written += 1;
                if watch.events & EPOLLONESHOT != 0 {
                    watch.enabled = false;
                }
            }
            watches.rotate_left(1);
        }
        Ok(written)
    }
}

impl File for Epoll {
    fn readable(&self) -> bool {
        false
    }
    fn writable(&self) -> bool {
        false
    }
    fn read(&self, _: UserBuffer) -> usize {
        0
    }
    fn write(&self, _: UserBuffer) -> isize {
        -22
    }
    fn poll_readable(&self) -> bool {
        self.watches
            .exclusive_access()
            .iter()
            .any(|watch| watch.ready() != 0)
    }
    fn poll_writable(&self) -> bool {
        false
    }
    fn poll_events(&self) -> u16 {
        self.poll_readable() as u16
    }
    fn epoll(&self) -> Option<&Epoll> {
        Some(self)
    }
}

pub(super) fn create([flags, ..]: [usize; 6]) -> isize {
    if flags & !EPOLL_CLOEXEC != 0 {
        return -22;
    }
    let epoll = Arc::new(Epoll {
        watches: unsafe { UPSafeCell::new(VecDeque::new()) },
    });
    install(epoll, flags != 0).map_or_else(|error| error, |fd| fd as isize)
}

pub(super) fn ctl([epfd, op, fd, pointer, ..]: [usize; 6]) -> isize {
    (|| {
        let epoll = file_by_fd(epfd)?;
        let target = file_by_fd(fd)?;
        epoll.epoll().ok_or(-22isize)?.ctl(op, fd, target, pointer)
    })()
    .err()
    .unwrap_or(0)
}

pub(super) fn wait(
    [fd, pointer, max, timeout, mask, _mask_size]: [usize; 6],
    timespec: bool,
) -> isize {
    let result = (|| {
        if max == 0 || max > i32::MAX as usize / EVENT_SIZE {
            return Err(-22);
        }
        if mask != 0 {
            return Err(-38);
        }
        if pointer == 0 || pointer.checked_add(max * EVENT_SIZE).is_none() {
            return Err(-14);
        }
        let deadline = if timespec {
            poll_deadline(timeout)?
        } else {
            let millis = timeout as i32;
            (millis >= 0).then(|| {
                crate::timer::get_time_ns()
                    .saturating_add((millis as usize).saturating_mul(1_000_000))
            })
        };
        with_io_file(file_by_fd(fd)?, |file| {
            let epoll = file.epoll().ok_or(-22isize)?;
            poll_wait(deadline, || epoll.collect(pointer, max))
        })
    })();
    result.map_or_else(|error| error, |count| count as isize)
}
