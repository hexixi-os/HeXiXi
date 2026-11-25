use super::File;
use crate::mm::{UserBuffer, UserBufferIterator};
use crate::sync::UPSafeCell;
use crate::task::{block_current_and_run_next, current_task, WaitQueue};
use alloc::sync::{Arc, Weak};

pub struct Pipe {
    readable: bool,
    writable: bool,
    buffer: Arc<UPSafeCell<PipeRingBuffer>>,
}

impl Pipe {
    pub fn read_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: true,
            writable: false,
            buffer,
        }
    }
    pub fn write_end_with_buffer(buffer: Arc<UPSafeCell<PipeRingBuffer>>) -> Self {
        Self {
            readable: false,
            writable: true,
            buffer,
        }
    }
}

/// 环形缓冲大小。原本 64KB（Linux 默认 pipe capacity），但 LTP 的 pipe06 会一路 open
/// 到 EMFILE（1024 个 fd ≈ 512 个 pipe），64KB × 512 = 32MB，再加上别的堆占用
/// 会把 64MB 内核堆打爆。降到 16KB：仍 >= PIPE_BUF(4096)，LTP 的 pipe 用例够用。
const RING_BUFFER_SIZE: usize = 16384;

#[derive(Copy, Clone, PartialEq)]
enum RingBufferStatus {
    Full,
    Empty,
    Normal,
}

pub struct PipeRingBuffer {
    arr: [u8; RING_BUFFER_SIZE],
    head: usize,
    tail: usize,
    status: RingBufferStatus,
    read_end: Option<Weak<Pipe>>,
    write_end: Option<Weak<Pipe>>,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
    read_gen: u64,
    write_gen: u64,
}

impl PipeRingBuffer {
    pub fn new() -> Self {
        Self {
            arr: [0; RING_BUFFER_SIZE],
            head: 0,
            tail: 0,
            status: RingBufferStatus::Empty,
            read_end: None,
            write_end: None,
            readers: WaitQueue::new(),
            writers: WaitQueue::new(),
            read_gen: 0,
            write_gen: 0,
        }
    }
    pub fn set_write_end(&mut self, write_end: &Arc<Pipe>) {
        self.write_end = Some(Arc::downgrade(write_end));
    }
    pub fn write_from(&mut self, src: &mut UserBufferIterator, count: usize) -> usize {
        let to_write = count.min(self.available_write());
        if to_write == 0 {
            return 0;
        }
        let first = to_write.min(RING_BUFFER_SIZE - self.tail);
        let copied_first = src.copy_to_slice(&mut self.arr[self.tail..self.tail + first]);
        let mut total = copied_first;
        if copied_first == first && to_write > first {
            total += src.copy_to_slice(&mut self.arr[..to_write - first]);
        }
        self.tail = (self.tail + total) % RING_BUFFER_SIZE;
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
        let first = to_read.min(RING_BUFFER_SIZE - self.head);
        let copied_first = dst.copy_from_slice(&self.arr[self.head..self.head + first]);
        let mut total = copied_first;
        if copied_first == first && to_read > first {
            total += dst.copy_from_slice(&self.arr[..to_read - first]);
        }
        self.head = (self.head + total) % RING_BUFFER_SIZE;
        if self.head == self.tail {
            self.status = RingBufferStatus::Empty;
        } else {
            self.status = RingBufferStatus::Normal;
        }
        total
    }
    pub fn available_read(&self) -> usize {
        if self.status == RingBufferStatus::Empty {
            0
        } else if self.tail > self.head {
            self.tail - self.head
        } else {
            self.tail + RING_BUFFER_SIZE - self.head
        }
    }
    pub fn available_write(&self) -> usize {
        if self.status == RingBufferStatus::Full {
            0
        } else {
            RING_BUFFER_SIZE - self.available_read()
        }
    }
    pub fn all_write_ends_closed(&self) -> bool {
        self.write_end.as_ref().unwrap().strong_count() == 0
    }
    fn all_read_ends_closed(&self) -> bool {
        self.read_end.as_ref().unwrap().strong_count() == 0
    }
}

/// Return (read_end, write_end)
pub fn make_pipe() -> (Arc<Pipe>, Arc<Pipe>) {
    let buffer = Arc::new(unsafe { UPSafeCell::new(PipeRingBuffer::new()) });
    let read_end = Arc::new(Pipe::read_end_with_buffer(buffer.clone()));
    let write_end = Arc::new(Pipe::write_end_with_buffer(buffer.clone()));
    let mut ring = buffer.exclusive_access();
    ring.read_end = Some(Arc::downgrade(&read_end));
    ring.set_write_end(&write_end);
    drop(ring);
    (read_end, write_end)
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // Each direction has one endpoint shared by dup, fork and in-flight I/O.
        // Its final drop means the peer can observe EOF or a broken pipe.
        let mut ring = self.buffer.exclusive_access();
        if self.readable {
            ring.write_gen = ring.write_gen.wrapping_add(1);
            ring.writers.wake_all();
        } else {
            ring.read_gen = ring.read_gen.wrapping_add(1);
            ring.readers.wake_all();
        }
        drop(ring);
        crate::task::wake_poll_waiters();
    }
}

pub(crate) struct SuspendedPipeIo(UserBufferIterator);

// A suspended iterator is resumed only by its own task, whose TCB pins its frames.
// Another hart may reclaim the iterator on exit, which only frees its descriptors.
unsafe impl Send for SuspendedPipeIo {}

fn block_pipe_io(buffer: UserBufferIterator) -> UserBufferIterator {
    // Group exit can discard the blocked kernel stack without running its destructors.
    current_task()
        .unwrap()
        .inner_exclusive_access()
        .suspended_pipe_io = Some(SuspendedPipeIo(buffer));
    // The kernel lock stays held until the scheduler saves the task context.
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
    fn readable(&self) -> bool {
        self.readable
    }
    fn writable(&self) -> bool {
        self.writable
    }
    /// lmbench 的 benchmp 用 select(0 超时) 轮询管道就绪；必须反映真实状态，
    /// 否则 select 假就绪 → 子进程在 read(尚未写入的 start_signal) 上阻塞 → 父子死锁。
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
        if self.readable {
            (if ring.available_read() != 0 { 1 } else { 0 })
                | (if ring.all_write_ends_closed() { 16 } else { 0 })
        } else {
            (if ring.available_write() != 0 { 4 } else { 0 })
                | (if ring.all_read_ends_closed() { 8 } else { 0 })
        }
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
        let mut buf_iter = buf.into_iter();
        loop {
            let mut ring = self.buffer.exclusive_access();
            let available = ring.available_read();
            let count = available.min(len);
            if count != 0 {
                let read_count = ring.read_into(&mut buf_iter, count);
                ring.write_gen = ring.write_gen.wrapping_add(1);
                if available == RING_BUFFER_SIZE {
                    ring.writers.wake_all();
                }
                drop(ring);
                crate::task::wake_poll_waiters();
                return read_count as isize;
            }
            if ring.all_write_ends_closed() {
                return 0;
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
            let count = available.min(len - write_size);
            if count == 0 {
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
            let written = ring.write_from(&mut buf_iter, count);
            write_size += written;
            ring.read_gen = ring.read_gen.wrapping_add(1);
            if available == RING_BUFFER_SIZE {
                ring.readers.wake_all();
            }
            drop(ring);
            crate::task::wake_poll_waiters();
        }
        write_size as isize
    }
}
