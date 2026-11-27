use super::File;
use crate::mm::{UserBuffer, UserBufferIterator};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, WaitQueue};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU32, Ordering};

const O_NONBLOCK: u32 = 0o4000;
const O_DIRECT: u32 = 0o40000;
const PIPE_BUF: usize = 4096;
const DEFAULT_PIPE_SZ: usize = 16384;
const MAX_PIPE_SZ: usize = 1_048_576;

pub struct Pipe {
    readable: bool,
    writable: bool,
    flags: AtomicU32,
    buffer: Arc<UPSafeCell<PipeRingBuffer>>,
}

impl Pipe {
    pub fn open_fifo(
        buffer: Arc<UPSafeCell<PipeRingBuffer>>,
        readable: bool,
        writable: bool,
        status_flags: u32,
    ) -> Result<Arc<Self>, isize> {
        let nonblock = status_flags & O_NONBLOCK != 0;
        if writable && !readable && nonblock {
            let ring = buffer.exclusive_access();
            if ring.readers_open == 0 {
                return Err(-6); // ENXIO: O_WRONLY | O_NONBLOCK with no reader
            }
        }
        {
            let mut ring = buffer.exclusive_access();
            if readable {
                ring.readers_open += 1;
                ring.write_gen = ring.write_gen.wrapping_add(1);
                ring.writers.wake_all();
            }
            if writable {
                ring.writers_open += 1;
                ring.had_writer = true;
                ring.read_gen = ring.read_gen.wrapping_add(1);
                ring.readers.wake_all();
            }
        }
        crate::task::wake_poll_waiters();
        if !nonblock && !(readable && writable) {
            loop {
                let mut ring = buffer.exclusive_access();
                let peer_ready = if readable {
                    ring.writers_open > 0 || ring.had_writer
                } else {
                    ring.readers_open > 0
                };
                if peer_ready {
                    break;
                }
                let wq = if readable {
                    Arc::clone(&ring.readers)
                } else {
                    Arc::clone(&ring.writers)
                };
                if !wq.register_current_interruptible() {
                    if readable {
                        ring.readers_open -= 1;
                    }
                    if writable {
                        ring.writers_open -= 1;
                    }
                    return Err(-4); // EINTR
                }
                drop(ring);
                block_current_and_run_next();
                if let Some(t) = current_task() {
                    WaitQueue::cancel(&t);
                }
                crate::task::check_kill_current();
                if crate::syscall::signal::current_wait_interrupted() {
                    let mut ring = buffer.exclusive_access();
                    if readable {
                        ring.readers_open -= 1;
                    }
                    if writable {
                        ring.writers_open -= 1;
                    }
                    return Err(-4);
                }
            }
        }
        let mask = if writable { O_NONBLOCK | O_DIRECT } else { O_NONBLOCK };
        Ok(Arc::new(Self {
            readable,
            writable,
            flags: AtomicU32::new(status_flags & mask),
            buffer,
        }))
    }

    pub fn pipe_size(&self) -> usize {
        self.buffer.exclusive_access().arr.len()
    }

    pub fn available_read_bytes(&self) -> usize {
        self.buffer.exclusive_access().available_read()
    }

    pub fn set_pipe_size(&self, requested: usize, privileged: bool) -> isize {
        if requested > i32::MAX as usize {
            return -22;
        }
        let rounded = requested.max(PIPE_BUF).next_power_of_two();
        if !privileged && rounded > MAX_PIPE_SZ {
            return -1; // EPERM
        }
        if rounded > 4 * 1024 * 1024 {
            return -12; // ENOMEM cap to protect kernel heap
        }
        let mut ring = self.buffer.exclusive_access();
        let avail = ring.available_read();
        if rounded < avail {
            return -16; // EBUSY
        }
        if rounded != ring.arr.len() {
            let mut next = alloc::vec![0u8; rounded].into_boxed_slice();
            if avail > 0 {
                let cap = ring.arr.len();
                let first = avail.min(cap - ring.head);
                next[..first].copy_from_slice(&ring.arr[ring.head..ring.head + first]);
                if avail > first {
                    next[first..avail].copy_from_slice(&ring.arr[..avail - first]);
                }
            }
            ring.arr = next;
            ring.head = 0;
            ring.tail = avail % rounded;
            ring.status = if avail == 0 {
                RingBufferStatus::Empty
            } else if avail == rounded {
                RingBufferStatus::Full
            } else {
                RingBufferStatus::Normal
            };
            ring.writers.wake_all();
        }
        rounded as isize
    }

    /// Move bytes under both pipe borrows. Release them before waiting.
    pub fn splice_to(&self, output: &Pipe, scratch: &mut [u8], force_nonblock: bool) -> isize {
        let nonblock = force_nonblock
            || (self.flags.load(Ordering::Relaxed) | output.flags.load(Ordering::Relaxed))
                & O_NONBLOCK != 0;
        loop {
            let mut input_ring = self.buffer.exclusive_access();
            let mut output_ring = output.buffer.exclusive_access();
            let available = input_ring.available_read();
            let waiters = if available == 0 {
                if input_ring.all_write_ends_closed() {
                    return 0;
                }
                &input_ring.readers
            } else if output_ring.all_read_ends_closed() {
                drop(output_ring);
                drop(input_ring);
                crate::syscall::signal::queue_current(13);
                return -32;
            } else {
                let count = available.min(output_ring.available_write()).min(scratch.len());
                if count > 0 {
                    input_ring.copy_head(&mut scratch[..count]);
                    output_ring.append_bytes(&scratch[..count]);
                    input_ring.consume_head(count);
                    drop(output_ring);
                    drop(input_ring);
                    crate::task::wake_poll_waiters();
                    return count as isize;
                }
                &output_ring.writers
            };
            if nonblock {
                return -11;
            }
            if !waiters.register_current_interruptible() {
                return -4;
            }
            drop(output_ring);
            drop(input_ring);
            block_current_and_run_next();
            if let Some(task) = current_task() {
                WaitQueue::cancel(&task);
            }
            crate::task::check_kill_current();
            if crate::syscall::signal::current_wait_interrupted() {
                return -4;
            }
        }
    }

    /// Complete a peek followed by a synchronous write to a regular file.
    pub fn consume_kernel(&self, count: usize) {
        self.buffer.exclusive_access().consume_head(count);
        crate::task::wake_poll_waiters();
    }

    /// Read from a synchronous file only after the pipe can accept the bytes.
    pub fn splice_from_file(
        &self,
        scratch: &mut [u8],
        force_nonblock: bool,
        read: impl FnOnce(&mut [u8]) -> isize,
    ) -> isize {
        let nonblock = force_nonblock || self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0;
        loop {
            let mut ring = self.buffer.exclusive_access();
            if ring.all_read_ends_closed() {
                drop(ring);
                crate::syscall::signal::queue_current(13);
                return -32;
            }
            let count = ring.available_write().min(scratch.len());
            if count > 0 {
                let copied = read(&mut scratch[..count]);
                if copied > 0 {
                    ring.append_bytes(&scratch[..copied as usize]);
                    drop(ring);
                    crate::task::wake_poll_waiters();
                }
                return copied;
            }
            if nonblock {
                return -11;
            }
            if !ring.writers.register_current_interruptible() {
                return -4;
            }
            drop(ring);
            block_current_and_run_next();
            if let Some(task) = current_task() {
                WaitQueue::cancel(&task);
            }
            crate::task::check_kill_current();
            if crate::syscall::signal::current_wait_interrupted() {
                return -4;
            }
        }
    }

    pub fn peek_kernel(&self, dst: &mut [u8], force_nonblock: bool) -> isize {
        if !self.readable {
            return -9;
        }
        if dst.is_empty() {
            return 0;
        }
        let nonblock = force_nonblock || self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0;
        loop {
            let ring = self.buffer.exclusive_access();
            let avail = ring.available_read();
            if avail > 0 {
                let n = avail.min(dst.len());
                ring.copy_head(&mut dst[..n]);
                return n as isize;
            }
            if ring.all_write_ends_closed() {
                return 0;
            }
            if nonblock {
                return -11;
            }
            if !ring.readers.register_current_interruptible() {
                return -4;
            }
            drop(ring);
            block_current_and_run_next();
            if let Some(t) = current_task() {
                WaitQueue::cancel(&t);
            }
            crate::task::check_kill_current();
            if crate::syscall::signal::current_wait_interrupted() {
                return -4;
            }
        }
    }

    pub fn write_kernel(&self, src: &[u8], force_nonblock: bool) -> isize {
        if !self.writable {
            return -9;
        }
        if src.is_empty() {
            let ring = self.buffer.exclusive_access();
            if ring.all_read_ends_closed() {
                drop(ring);
                crate::syscall::signal::queue_current(13);
                return -32;
            }
            return 0;
        }
        let nonblock = force_nonblock || self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0;
        let mut written = 0usize;
        while written < src.len() {
            let mut ring = self.buffer.exclusive_access();
            if ring.all_read_ends_closed() {
                drop(ring);
                crate::syscall::signal::queue_current(13);
                return if written == 0 { -32 } else { written as isize };
            }
            let avail = ring.available_write();
            let rem = src.len() - written;
            if avail == 0 || (nonblock && rem <= PIPE_BUF && avail < rem) {
                if nonblock {
                    return if written == 0 { -11 } else { written as isize };
                }
                if !ring.writers.register_current_interruptible() {
                    return if written == 0 { -4 } else { written as isize };
                }
                drop(ring);
                block_current_and_run_next();
                if let Some(t) = current_task() {
                    WaitQueue::cancel(&t);
                }
                crate::task::check_kill_current();
                if crate::syscall::signal::current_wait_interrupted() {
                    return if written == 0 { -4 } else { written as isize };
                }
                continue;
            }
            let n = avail.min(rem);
            ring.append_bytes(&src[written..written + n]);
            written += n;
            drop(ring);
            crate::task::wake_poll_waiters();
        }
        written as isize
    }

    pub fn same_buffer(&self, other: &Pipe) -> bool {
        Arc::ptr_eq(&self.buffer, &other.buffer)
    }
}

/// 默认环形缓冲大小 16KB（>= PIPE_BUF 4KB，且在 pipe06 打开 512 对管道时仅占 8MB 内核堆）。
const RING_BUFFER_SIZE: usize = DEFAULT_PIPE_SZ;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: alloc::boxed::Box<[u8]>,
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    readers_open: usize,
    writers_open: usize,
    had_writer: bool,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
    read_gen: u64,
    write_gen: u64,
}

impl PipeRingBuffer {
    pub fn new() -> Self {
        Self::with_counts(1, 1, true)
    }

    pub fn new_fifo() -> Self {
        Self::with_counts(0, 0, false)
    }

    fn with_counts(readers_open: usize, writers_open: usize, had_writer: bool) -> Self {
        Self {
            arr: alloc::vec![0u8; RING_BUFFER_SIZE].into_boxed_slice(),
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            readers_open,
            writers_open,
            had_writer,
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            read_gen: 0,
            write_gen: u64::from(readers_open > 0),
        }
    }

    pub fn write_from(&mut self, src: &mut UserBufferIterator, count: usize) -> usize {
        let to_write = count.min(self.available_write());
        if to_write == 0 {
            return 0;
        }
        let cap = self.arr.len();
        let first = to_write.min(cap - self.tail);
        let copied_first = src.copy_to_slice(&mut self.arr[self.tail..self.tail + first]);
        let mut total = copied_first;
        if copied_first == first && to_write > first {
            total += src.copy_to_slice(&mut self.arr[..to_write - first]);
        }
        self.tail = (self.tail + total) % cap;
        if self.tail == self.head {
            self.status = RingBufferStatus::Full;
        } else {
            self.status = RingBufferStatus::Normal;
        }
        total
    }
    pub fn read_into(&mut self, dst: &mut UserBufferIterator, count: usize) -> usize {
        let to_read = count.min(self.available_read());
        if to_read == 0 {
            return 0;
        }
        let cap = self.arr.len();
        let first = to_read.min(cap - self.head);
        let copied_first = dst.copy_from_slice(&self.arr[self.head..self.head + first]);
        let mut total = copied_first;
        if copied_first == first && to_read > first {
            total += dst.copy_from_slice(&self.arr[..to_read - first]);
        }
        self.head = (self.head + total) % cap;
        if self.head == self.tail {
            self.status = RingBufferStatus::Empty;
        } else {
            self.status = RingBufferStatus::Normal;
        }
        total
    }
    fn copy_head(&self, dst: &mut [u8]) {
        let first = dst.len().min(self.arr.len() - self.head);
        let (head, rest) = dst.split_at_mut(first);
        head.copy_from_slice(&self.arr[self.head..self.head + first]);
        rest.copy_from_slice(&self.arr[..rest.len()]);
    }

    fn consume_head(&mut self, count: usize) {
        debug_assert!(count <= self.available_read());
        self.head = (self.head + count) % self.arr.len();
        self.status = if self.head == self.tail {
            RingBufferStatus::Empty
        } else {
            RingBufferStatus::Normal
        };
        self.write_gen = self.write_gen.wrapping_add(1);
        self.writers.wake_all();
    }

    fn append_bytes(&mut self, src: &[u8]) {
        let cap = self.arr.len();
        let first = src.len().min(cap - self.tail);
        self.arr[self.tail..self.tail + first].copy_from_slice(&src[..first]);
        self.arr[..src.len() - first].copy_from_slice(&src[first..]);
        self.tail = (self.tail + src.len()) % cap;
        self.status = if self.tail == self.head {
            RingBufferStatus::Full
        } else {
            RingBufferStatus::Normal
        };
        self.read_gen = self.read_gen.wrapping_add(1);
        self.readers.wake_all();
    }

    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + self.arr.len() - self.head
        }
    }
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            self.arr.len() - self.available_read()
        }
    }
    pub fn all_write_ends_closed(&self) -> bool {
        self.had_writer && self.writers_open == 0
    }
    fn all_read_ends_closed(&self) -> bool {
        self.readers_open == 0
    }
}

/// Return (read_end, write_end)
pub fn make_pipe_with_flags(flags: u32) -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer = Arc::new(unsafe { UPSafeCell::new(PipeRingBuffer::new()) });
    let read_end = Arc::new(Pipe {
        readable: true,
        writable: false,
        flags: AtomicU32::new(flags & O_NONBLOCK),
        buffer: buffer.clone(),
    });
    let write_end = Arc::new(Pipe {
        readable: false,
        writable: true,
        flags: AtomicU32::new(flags & (O_NONBLOCK | O_DIRECT)),
        buffer,
    });
    (read_end, write_end)
}

impl Drop for Pipe {
    fn drop(&mut self) {
        let mut ring = self.buffer.exclusive_access();
        if self.readable {
            ring.readers_open = ring.readers_open.saturating_sub(1);
            ring.write_gen = ring.write_gen.wrapping_add(1);
            ring.writers.wake_all();
        }
        if self.writable {
            ring.writers_open = ring.writers_open.saturating_sub(1);
            ring.read_gen = ring.read_gen.wrapping_add(1);
            ring.readers.wake_all();
        }
        drop(ring);
        crate::task::wake_poll_waiters();
    }
}

pub(crate) struct SuspendedPipeIo(UserBufferIterator);

unsafe impl Send for SuspendedPipeIo {}

fn block_pipe_io(buffer: UserBufferIterator) -> UserBufferIterator {
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .suspended_pipe_io = Some(SuspendedPipeIo(buffer));
    block_current_and_run_next();
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .suspended_pipe_io
        .take()
        .unwrap()
        .0
}

impl File for Pipe {
    fn mode(&self) -> u16 {
        0o010600 // S_IFIFO | 0600
    }
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn pipe(&self) -> Option<&Pipe> {
        Some(self)
    }
    fn status_flags(&self) -> u32 {
        let acc = match (self.readable, self.writable) {
            (true, true) => 2,
            (false, true) => 1,
            _ => 0,
        };
        acc | self.flags.load(Ordering::Relaxed)
    }
    fn set_status_flags(&self, flags: u32) {
        let mask = if self.writable { O_NONBLOCK | O_DIRECT } else { O_NONBLOCK };
        self.flags.store(flags & mask, Ordering::Relaxed);
    }
    fn poll_readable(&self) -> bool {
        let b = self.buffer.exclusive_access();
        b.available_read() > 0 || b.all_write_ends_closed()
    }
    fn poll_writable(&self) -> bool {
        self.buffer.exclusive_access().available_write() > 0
    }
    fn epollable(&self) -> bool {
        true
    }

    fn poll_events(&self) -> u16 {
        let ring = self.buffer.exclusive_access();
        let mut ev = 0u16;
        if self.readable {
            if ring.available_read() != 0 {
                ev |= 1;
            }
            if ring.all_write_ends_closed() {
                ev |= 16;
            }
        }
        if self.writable {
            if ring.available_write() != 0 {
                ev |= 4;
            }
            if ring.all_read_ends_closed() {
                ev |= 8;
            }
        }
        ev
    }

    fn poll_generations(&self) -> (u64, u64) {
        let ring = self.buffer.exclusive_access();
        (ring.read_gen, ring.write_gen)
    }

    fn read(&self, buf: UserBuffer) -> usize {
        self.read_result(buf).max(0) as usize
    }

    fn read_result(&self, buf: UserBuffer) -> isize {
        assert!(self.readable());
        let len = buf.len();
        if len == 0 {
            return 0;
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0;
        let mut buf_iter = buf.into_iter();
        loop {
            let mut ring = self.buffer.exclusive_access();
            let available = ring.available_read();
            let count = available.min(len);
            if count != 0 {
                let read_count = ring.read_into(&mut buf_iter, count);
                ring.write_gen = ring.write_gen.wrapping_add(1);
                ring.writers.wake_all();
                drop(ring);
                crate::task::wake_poll_waiters();
                return read_count as isize;
            }
            if ring.all_write_ends_closed() {
                return 0;
            }
            if nonblock {
                return -11; // EAGAIN
            }
            if !ring.readers.register_current_interruptible() {
                return -4; // EINTR
            }
            drop(ring);
            buf_iter = block_pipe_io(buf_iter);
        }
    }

    fn write(&self, buf: UserBuffer) -> isize {
        assert!(self.writable());
        let len = buf.len();
        if len == 0 {
            let ring = self.buffer.exclusive_access();
            if ring.all_read_ends_closed() {
                drop(ring);
                crate::syscall::signal::queue_current(13);
                return -32;
            }
            return 0;
        }
        let nonblock = self.flags.load(Ordering::Relaxed) & O_NONBLOCK != 0;
        let mut buf_iter = buf.into_iter();
        let mut write_size = 0usize;
        while write_size < len {
            let mut ring = self.buffer.exclusive_access();
            if ring.all_read_ends_closed() {
                crate::syscall::signal::queue_current(13); // SIGPIPE
                return if write_size == 0 {
                    -32
                } else {
                    write_size as isize
                };
            }
            let available = ring.available_write();
            let rem = len - write_size;
            if available == 0 || (nonblock && rem <= PIPE_BUF && available < rem) {
                if nonblock {
                    return if write_size == 0 {
                        -11 // EAGAIN
                    } else {
                        write_size as isize
                    };
                }
                if !ring.writers.register_current_interruptible() {
                    return if write_size == 0 {
                        -4
                    } else {
                        write_size as isize
                    };
                }
                drop(ring);
                buf_iter = block_pipe_io(buf_iter);
                continue;
            }
            let count = available.min(rem);
            let written = ring.write_from(&mut buf_iter, count);
            write_size += written;
            ring.read_gen = ring.read_gen.wrapping_add(1);
            ring.readers.wake_all();
            drop(ring);
            crate::task::wake_poll_waiters();
        }
        write_size as isize
    }
}
