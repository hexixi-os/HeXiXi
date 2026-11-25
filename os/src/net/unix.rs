use crate::fs::{File, OpenFlags};
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use crate::task::{current_process, current_task, wake_poll_waiters, WaitQueue};
use alloc::{
    collections::{BTreeMap, VecDeque},
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::lazy_static;

use super::{MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC};

const EAGAIN: isize = -11;
const DEFAULT_BUF_CAP: usize = 64 * 1024;
const MAX_BUF_CAP: usize = 256 * 1024;
const MIN_BUF_CAP: usize = 4 * 1024;
const MAX_DGRAM_PACKETS: usize = 64;
const MAX_DGRAM_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UnixKind {
    Stream,
    Dgram,
    SeqPacket,
}

impl UnixKind {
    pub fn sock_type(self) -> i32 {
        match self {
            Self::Stream => 1,
            Self::Dgram => 2,
            Self::SeqPacket => 5,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum UnixAddress {
    Unnamed,
    Path(Vec<u8>),
    Abstract(Vec<u8>),
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum UnixKey {
    Path(String),
    Abstract(Vec<u8>),
}

enum UnixBinding {
    Bound(Weak<()>),
    Listener(Weak<UPSafeCell<UnixListenerState>>),
    Dgram(Weak<UPSafeCell<DgramQueue>>),
}

lazy_static! {
    static ref UNIX_REGISTRY: UPSafeCell<BTreeMap<UnixKey, UnixBinding>> =
        unsafe { UPSafeCell::new(BTreeMap::new()) };
}

pub(crate) fn unix_unlink(canonical_path: &str) {
    UNIX_REGISTRY
        .exclusive_access()
        .remove(&UnixKey::Path(String::from(canonical_path)));
}

#[derive(Clone, Copy)]
pub struct UnixCred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

fn current_cred() -> UnixCred {
    UnixCred {
        pid: current_process().getpid() as i32,
        uid: 0,
        gid: 0,
    }
}

fn copy_in(buffer: &UserBuffer, dest: &mut [u8]) {
    let mut offset = 0;
    let mut scratch = Vec::new();
    for part in &buffer.buffers {
        if offset == dest.len() {
            break;
        }
        part.read_with(&mut scratch, |bytes| {
            let count = bytes.len().min(dest.len() - offset);
            dest[offset..offset + count].copy_from_slice(&bytes[..count]);
            offset += count;
        });
    }
}

fn copy_in_vec(buffer: &UserBuffer) -> Vec<u8> {
    let mut out = alloc::vec![0u8; buffer.len()];
    copy_in(buffer, &mut out);
    out
}

fn wait_on(queue: &Arc<WaitQueue>, deadline_ns: Option<usize>) -> Result<(), isize> {
    if crate::syscall::signal::current_wait_interrupted() {
        return Err(-4);
    }
    if let Some(end) = deadline_ns {
        if crate::timer::get_time_ns() >= end {
            return Err(EAGAIN);
        }
    }
    let current = current_task().unwrap();
    if !queue.register_current_interruptible() {
        return Err(-4);
    }
    let has_timer = if let Some(end) = deadline_ns {
        let now = crate::timer::get_time_ns();
        let remain_ms = end.saturating_sub(now).saturating_add(999_999) / 1_000_000;
        let expire_ms = crate::timer::get_time_ms().saturating_add(remain_ms.max(1));
        crate::timer::add_timer(expire_ms, Arc::clone(&current));
        true
    } else {
        false
    };
    crate::task::block_current_and_run_next();
    WaitQueue::cancel(&current);
    if has_timer {
        crate::timer::remove_timer(current);
    }
    if crate::syscall::signal::current_wait_interrupted() {
        return Err(-4);
    }
    if let Some(end) = deadline_ns {
        if crate::timer::get_time_ns() >= end {
            return Err(EAGAIN);
        }
    }
    Ok(())
}

struct StreamBuffer {
    buf: VecDeque<u8>,
    cap: usize,
    closed_read: bool,
    closed_write: bool,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
    in_gen: u64,
    out_gen: u64,
}

impl StreamBuffer {
    fn new(cap: usize) -> Arc<UPSafeCell<Self>> {
        Arc::new(unsafe {
            UPSafeCell::new(Self {
                buf: VecDeque::new(),
                cap,
                closed_read: false,
                closed_write: false,
                readers: WaitQueue::new(),
                writers: WaitQueue::new(),
                in_gen: 0,
                out_gen: 0,
            })
        })
    }

    fn close_write(&mut self) {
        if !self.closed_write {
            self.closed_write = true;
            self.in_gen = self.in_gen.wrapping_add(1);
            self.readers.wake_all();
            wake_poll_waiters();
        }
    }

    fn close_read(&mut self) {
        if !self.closed_read {
            self.closed_read = true;
            self.out_gen = self.out_gen.wrapping_add(1);
            self.writers.wake_all();
            wake_poll_waiters();
        }
    }
}

struct DgramQueue {
    packets: VecDeque<(Vec<u8>, UnixAddress)>,
    bytes: usize,
    max_bytes: usize,
    closed_read: bool,
    closed_write: bool,
    readers: Arc<WaitQueue>,
    writers: Arc<WaitQueue>,
    in_gen: u64,
    out_gen: u64,
}

impl DgramQueue {
    fn new(max_bytes: usize) -> Arc<UPSafeCell<Self>> {
        Arc::new(unsafe {
            UPSafeCell::new(Self {
                packets: VecDeque::new(),
                bytes: 0,
                max_bytes: max_bytes.max(MIN_BUF_CAP),
                closed_read: false,
                closed_write: false,
                readers: WaitQueue::new(),
                writers: WaitQueue::new(),
                in_gen: 0,
                out_gen: 0,
            })
        })
    }

    fn is_full(&self, next_len: usize) -> bool {
        self.packets.len() >= MAX_DGRAM_PACKETS
            || (self.bytes > 0 && self.bytes.saturating_add(next_len) > self.max_bytes)
    }

    fn close_write(&mut self) {
        if !self.closed_write {
            self.closed_write = true;
            self.in_gen = self.in_gen.wrapping_add(1);
            self.readers.wake_all();
            wake_poll_waiters();
        }
    }

    fn close_read(&mut self) {
        if !self.closed_read {
            self.closed_read = true;
            self.out_gen = self.out_gen.wrapping_add(1);
            self.writers.wake_all();
            wake_poll_waiters();
        }
    }
}

struct UnixListenerState {
    kind: UnixKind,
    local_addr: UnixAddress,
    cred: UnixCred,
    backlog: usize,
    queue: VecDeque<(Arc<UnixSocket>, UnixAddress)>,
    acceptors: Arc<WaitQueue>,
    connectors: Arc<WaitQueue>,
    closed: bool,
    in_gen: u64,
}

enum UnixPhase {
    Fresh,
    Listening(Arc<UPSafeCell<UnixListenerState>>),
    StreamConnected {
        rx: Arc<UPSafeCell<StreamBuffer>>,
        tx: Arc<UPSafeCell<StreamBuffer>>,
    },
    SeqConnected {
        rx: Arc<UPSafeCell<DgramQueue>>,
        tx: Arc<UPSafeCell<DgramQueue>>,
    },
    Dgram {
        rx: Arc<UPSafeCell<DgramQueue>>,
        tx: Option<Arc<UPSafeCell<DgramQueue>>>,
        paired: bool,
    },
}

struct UnixState {
    phase: UnixPhase,
    local_addr: UnixAddress,
    bound_key: Option<UnixKey>,
    bound_token: Option<Arc<()>>,
    peer_addr: UnixAddress,
    peer_cred: Option<UnixCred>,
    nonblock: bool,
    reuse_addr: bool,
    passcred: bool,
    send_limit: usize,
    receive_limit: usize,
    receive_timeout: Option<usize>,
    send_timeout: Option<usize>,
    read_shutdown: bool,
    write_shutdown: bool,
}

pub struct UnixSocket {
    kind: UnixKind,
    state: UPSafeCell<UnixState>,
}

fn resolve_key(addr: &UnixAddress, follow_final: bool) -> Result<UnixKey, isize> {
    match addr {
        UnixAddress::Unnamed => Err(-22),
        UnixAddress::Abstract(bytes) => Ok(UnixKey::Abstract(bytes.clone())),
        UnixAddress::Path(bytes) => {
            let raw = core::str::from_utf8(bytes).map_err(|_| -22isize)?;
            if raw.is_empty() {
                return Err(-2);
            }
            let canonical = crate::fs::resolve_path_with(raw, follow_final)?;
            Ok(UnixKey::Path(canonical))
        }
    }
}

impl UnixSocket {
    pub fn new(kind: UnixKind, nonblock: bool) -> Arc<Self> {
        let phase = match kind {
            UnixKind::Dgram => UnixPhase::Dgram {
                rx: DgramQueue::new(MAX_DGRAM_BYTES),
                tx: None,
                paired: false,
            },
            UnixKind::Stream | UnixKind::SeqPacket => UnixPhase::Fresh,
        };
        Arc::new(Self {
            kind,
            state: unsafe {
                UPSafeCell::new(UnixState {
                    phase,
                    local_addr: UnixAddress::Unnamed,
                    bound_key: None,
                    bound_token: None,
                    peer_addr: UnixAddress::Unnamed,
                    peer_cred: None,
                    nonblock,
                    reuse_addr: false,
                    passcred: false,
                    send_limit: DEFAULT_BUF_CAP,
                    receive_limit: DEFAULT_BUF_CAP,
                    receive_timeout: None,
                    send_timeout: None,
                    read_shutdown: false,
                    write_shutdown: false,
                })
            },
        })
    }

    pub fn pair(kind: UnixKind, nonblock: bool) -> (Arc<Self>, Arc<Self>) {
        let cred = current_cred();
        let (phase_a, phase_b) = match kind {
            UnixKind::Stream => {
                let a_to_b = StreamBuffer::new(DEFAULT_BUF_CAP);
                let b_to_a = StreamBuffer::new(DEFAULT_BUF_CAP);
                (
                    UnixPhase::StreamConnected {
                        rx: Arc::clone(&b_to_a),
                        tx: Arc::clone(&a_to_b),
                    },
                    UnixPhase::StreamConnected {
                        rx: a_to_b,
                        tx: b_to_a,
                    },
                )
            }
            UnixKind::SeqPacket => {
                let a_to_b = DgramQueue::new(MAX_DGRAM_BYTES);
                let b_to_a = DgramQueue::new(MAX_DGRAM_BYTES);
                (
                    UnixPhase::SeqConnected {
                        rx: Arc::clone(&b_to_a),
                        tx: Arc::clone(&a_to_b),
                    },
                    UnixPhase::SeqConnected {
                        rx: a_to_b,
                        tx: b_to_a,
                    },
                )
            }
            UnixKind::Dgram => {
                let a_to_b = DgramQueue::new(MAX_DGRAM_BYTES);
                let b_to_a = DgramQueue::new(MAX_DGRAM_BYTES);
                (
                    UnixPhase::Dgram {
                        rx: Arc::clone(&b_to_a),
                        tx: Some(Arc::clone(&a_to_b)),
                        paired: true,
                    },
                    UnixPhase::Dgram {
                        rx: a_to_b,
                        tx: Some(b_to_a),
                        paired: true,
                    },
                )
            }
        };
        let make = |phase| {
            Arc::new(Self {
                kind,
                state: unsafe {
                    UPSafeCell::new(UnixState {
                        phase,
                        local_addr: UnixAddress::Unnamed,
                        bound_key: None,
                    bound_token: None,
                        peer_addr: UnixAddress::Unnamed,
                        peer_cred: Some(cred),
                        nonblock,
                        reuse_addr: false,
                        passcred: false,
                        send_limit: DEFAULT_BUF_CAP,
                        receive_limit: DEFAULT_BUF_CAP,
                        receive_timeout: None,
                        send_timeout: None,
                        read_shutdown: false,
                        write_shutdown: false,
                    })
                },
            })
        };
        (make(phase_a), make(phase_b))
    }

    pub(crate) fn bind(&self, addr: UnixAddress) -> Result<(), isize> {
        let mut state = self.state.exclusive_access();
        if state.bound_key.is_some() {
            return Err(-22); // EINVAL: already bound
        }
        if matches!(addr, UnixAddress::Unnamed) {
            // Linux auto-binds an unnamed abstract address; treat as no-op for now.
            return Err(-22);
        }
        let key = resolve_key(&addr, false)?;
        if let UnixKey::Path(ref canonical) = key {
            if canonical.is_empty() {
                return Err(-98);
            }
            if let UnixAddress::Path(ref raw_bytes) = addr {
                if raw_bytes.last() == Some(&b'/') {
                    return Err(if crate::fs::exists(canonical) { -98 } else { -2 });
                }
            }
            if crate::fs::exists(canonical) {
                return Err(-98); // EADDRINUSE
            }
            match crate::fs::open_dyn_with_mode(
                canonical,
                OpenFlags::CREATE | OpenFlags::EXCL | OpenFlags::RDWR,
                0o666,
            ) {
                Ok(_) => {}
                Err(-17) => return Err(-98), // EEXIST -> EADDRINUSE
                Err(e) => return Err(e),
            }
        }
        let mut reg = UNIX_REGISTRY.exclusive_access();
        if let Some(existing) = reg.get(&key) {
            let alive = match existing {
                UnixBinding::Bound(w) => w.strong_count() > 0,
                UnixBinding::Listener(w) => w.strong_count() > 0,
                UnixBinding::Dgram(w) => w.strong_count() > 0,
            };
            if alive {
                return Err(-98); // EADDRINUSE
            }
        }
        if let UnixPhase::Dgram { ref rx, .. } = state.phase {
            reg.insert(key.clone(), UnixBinding::Dgram(Arc::downgrade(rx)));
        } else {
            let token = Arc::new(());
            reg.insert(key.clone(), UnixBinding::Bound(Arc::downgrade(&token)));
            state.bound_token = Some(token);
        }
        state.bound_key = Some(key);
        state.local_addr = addr;
        Ok(())
    }

    pub(crate) fn listen(&self, backlog: i32) -> Result<(), isize> {
        if self.kind == UnixKind::Dgram {
            return Err(-95); // EOPNOTSUPP
        }
        let mut state = self.state.exclusive_access();
        let Some(ref key) = state.bound_key else {
            return Err(-22); // EINVAL: must bind before listen
        };
        let backlog = backlog.clamp(1, 128) as usize;
        match state.phase {
            UnixPhase::Fresh => {
                let listener = Arc::new(unsafe {
                    UPSafeCell::new(UnixListenerState {
                        kind: self.kind,
                        local_addr: state.local_addr.clone(),
                        cred: current_cred(),
                        backlog,
                        queue: VecDeque::new(),
                        acceptors: WaitQueue::new(),
                        connectors: WaitQueue::new(),
                        closed: false,
                        in_gen: 0,
                    })
                });
                UNIX_REGISTRY
                    .exclusive_access()
                    .insert(key.clone(), UnixBinding::Listener(Arc::downgrade(&listener)));
                state.phase = UnixPhase::Listening(listener);
                Ok(())
            }
            UnixPhase::Listening(ref listener) => {
                listener.exclusive_access().backlog = backlog;
                Ok(())
            }
            _ => Err(-22),
        }
    }

    pub(crate) fn accept(
        &self,
        nonblock: bool,
        has_fd_slot: impl Fn() -> bool,
    ) -> Result<(Arc<UnixSocket>, UnixAddress), isize> {
        if self.kind == UnixKind::Dgram {
            return Err(-95);
        }
        loop {
            let (listener, self_nonblock, timeout) = {
                let state = self.state.exclusive_access();
                let UnixPhase::Listening(ref listener) = state.phase else {
                    return Err(-22);
                };
                (Arc::clone(listener), state.nonblock, state.receive_timeout)
            };
            let deadline = timeout.map(|d| crate::timer::get_time_ns().saturating_add(d));
            let acceptors = {
                let mut l = listener.exclusive_access();
                if !l.queue.is_empty() {
                    if !has_fd_slot() {
                        return Err(-24); // EMFILE
                    }
                    let (sock, peer) = l.queue.pop_front().unwrap();
                    sock.state.exclusive_access().nonblock = nonblock;
                    l.connectors.wake_all();
                    return Ok((sock, peer));
                }
                if l.closed {
                    return Err(-22);
                }
                Arc::clone(&l.acceptors)
            };
            if self_nonblock {
                return Err(EAGAIN);
            }
            wait_on(&acceptors, deadline)?;
        }
    }

    pub(crate) fn connect(&self, addr: UnixAddress) -> Result<(), isize> {
        let key = resolve_key(&addr, true)?;
        let missing_err = |key: &UnixKey| -> isize {
            match key {
                UnixKey::Path(p) => {
                    if crate::fs::exists(p) {
                        -111 // ECONNREFUSED
                    } else {
                        -2 // ENOENT
                    }
                }
                UnixKey::Abstract(_) => -111, // ECONNREFUSED
            }
        };

        if self.kind == UnixKind::Dgram {
            let target_rx = {
                let reg = UNIX_REGISTRY.exclusive_access();
                match reg.get(&key) {
                    Some(UnixBinding::Dgram(w)) => w.upgrade().ok_or_else(|| missing_err(&key))?,
                    Some(UnixBinding::Listener(_) | UnixBinding::Bound(_)) => return Err(-91), // EPROTOTYPE
                    None => return Err(missing_err(&key)),
                }
            };
            let mut state = self.state.exclusive_access();
            if let UnixPhase::Dgram { ref mut tx, .. } = state.phase {
                *tx = Some(target_rx);
                state.peer_addr = addr;
                Ok(())
            } else {
                Err(-22)
            }
        } else {
            let (self_nonblock, local_addr, send_cap, recv_cap) = {
                let state = self.state.exclusive_access();
                match state.phase {
                    UnixPhase::Fresh => {}
                    UnixPhase::StreamConnected { .. } | UnixPhase::SeqConnected { .. } => {
                        return Err(-106); // EISCONN
                    }
                    _ => return Err(-22),
                }
                (
                    state.nonblock,
                    state.local_addr.clone(),
                    state.send_limit,
                    state.receive_limit,
                )
            };

            loop {
                let listener = {
                    let reg = UNIX_REGISTRY.exclusive_access();
                    match reg.get(&key) {
                        Some(UnixBinding::Listener(w)) => {
                            w.upgrade().ok_or_else(|| missing_err(&key))?
                        }
                        Some(UnixBinding::Bound(_)) => return Err(-111), // ECONNREFUSED
                        Some(UnixBinding::Dgram(_)) => return Err(-91), // EPROTOTYPE
                        None => return Err(missing_err(&key)),
                    }
                };

                let connectors = {
                    let mut l = listener.exclusive_access();
                    if l.closed {
                        return Err(-111); // ECONNREFUSED
                    }
                    if l.kind != self.kind {
                        return Err(-91); // EPROTOTYPE
                    }
                    if l.queue.len() < l.backlog {
                        let client_cred = current_cred();
                        let server_cred = l.cred;
                        let server_local = l.local_addr.clone();
                        let (client_phase, server_phase) = match self.kind {
                            UnixKind::Stream => {
                                let c_to_s = StreamBuffer::new(send_cap.max(DEFAULT_BUF_CAP));
                                let s_to_c = StreamBuffer::new(recv_cap.max(DEFAULT_BUF_CAP));
                                (
                                    UnixPhase::StreamConnected {
                                        rx: Arc::clone(&s_to_c),
                                        tx: Arc::clone(&c_to_s),
                                    },
                                    UnixPhase::StreamConnected {
                                        rx: c_to_s,
                                        tx: s_to_c,
                                    },
                                )
                            }
                            UnixKind::SeqPacket => {
                                let c_to_s = DgramQueue::new(MAX_DGRAM_BYTES);
                                let s_to_c = DgramQueue::new(MAX_DGRAM_BYTES);
                                (
                                    UnixPhase::SeqConnected {
                                        rx: Arc::clone(&s_to_c),
                                        tx: Arc::clone(&c_to_s),
                                    },
                                    UnixPhase::SeqConnected {
                                        rx: c_to_s,
                                        tx: s_to_c,
                                    },
                                )
                            }
                            UnixKind::Dgram => unreachable!(),
                        };
                        let server_sock = Arc::new(Self {
                            kind: self.kind,
                            state: unsafe {
                                UPSafeCell::new(UnixState {
                                    phase: server_phase,
                                    local_addr: server_local.clone(),
                                    bound_key: None,
                    bound_token: None,
                                    peer_addr: local_addr.clone(),
                                    peer_cred: Some(client_cred),
                                    nonblock: false,
                                    reuse_addr: false,
                                    passcred: false,
                                    send_limit: DEFAULT_BUF_CAP,
                                    receive_limit: DEFAULT_BUF_CAP,
                                    receive_timeout: None,
                                    send_timeout: None,
                                    read_shutdown: false,
                                    write_shutdown: false,
                                })
                            },
                        });
                        l.queue.push_back((server_sock, local_addr));
                        l.in_gen = l.in_gen.wrapping_add(1);
                        l.acceptors.wake_all();
                        wake_poll_waiters();
                        drop(l);

                        let mut state = self.state.exclusive_access();
                        state.phase = client_phase;
                        state.peer_addr = server_local;
                        state.peer_cred = Some(server_cred);
                        return Ok(());
                    }
                    Arc::clone(&l.connectors)
                };

                if self_nonblock {
                    return Err(EAGAIN);
                }
                wait_on(&connectors, None)?;
            }
        }
    }

    pub(crate) fn local_addr(&self) -> UnixAddress {
        self.state.exclusive_access().local_addr.clone()
    }

    pub(crate) fn peer_addr(&self) -> Result<UnixAddress, isize> {
        let state = self.state.exclusive_access();
        match state.phase {
            UnixPhase::StreamConnected { .. } | UnixPhase::SeqConnected { .. } => {
                Ok(state.peer_addr.clone())
            }
            UnixPhase::Dgram { ref tx, .. } if tx.is_some() => Ok(state.peer_addr.clone()),
            _ => Err(-107), // ENOTCONN
        }
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), isize> {
        if how > 2 {
            return Err(-22);
        }
        let mut state = self.state.exclusive_access();
        if matches!(state.phase, UnixPhase::Fresh) {
            return Err(-107); // ENOTCONN
        }
        if how != 1 {
            state.read_shutdown = true;
        }
        if how != 0 {
            state.write_shutdown = true;
        }
        match state.phase {
            UnixPhase::StreamConnected { ref rx, ref tx } => {
                if how != 1 {
                    rx.exclusive_access().close_read();
                }
                if how != 0 {
                    tx.exclusive_access().close_write();
                }
                Ok(())
            }
            UnixPhase::SeqConnected { ref rx, ref tx } => {
                if how != 1 {
                    rx.exclusive_access().close_read();
                }
                if how != 0 {
                    tx.exclusive_access().close_write();
                }
                Ok(())
            }
            UnixPhase::Dgram { ref rx, ref tx, .. } => {
                if how != 1 {
                    rx.exclusive_access().close_read();
                }
                if how != 0 {
                    if let Some(tx) = tx {
                        tx.exclusive_access().close_write();
                    }
                }
                Ok(())
            }
            UnixPhase::Listening(ref listener) => {
                let mut l = listener.exclusive_access();
                l.closed = true;
                l.acceptors.wake_all();
                l.connectors.wake_all();
                wake_poll_waiters();
                Ok(())
            }
            UnixPhase::Fresh => unreachable!(),
        }
    }

    pub(crate) fn send(
        &self,
        buffer: &UserBuffer,
        flags: usize,
        dest: Option<UnixAddress>,
    ) -> Result<usize, isize> {
        let size = buffer.len();
        let (nonblock, timeout, local_addr) = {
            let state = self.state.exclusive_access();
            if state.write_shutdown {
                return Err(-32); // EPIPE
            }
            (
                state.nonblock || flags & MSG_DONTWAIT != 0,
                state.send_timeout,
                state.local_addr.clone(),
            )
        };
        let deadline = timeout.map(|d| crate::timer::get_time_ns().saturating_add(d));

        match self.kind {
            UnixKind::Stream => {
                if dest.is_some() {
                    return Err(-106); // EISCONN / EOPNOTSUPP
                }
                let tx = {
                    let state = self.state.exclusive_access();
                    match state.phase {
                        UnixPhase::StreamConnected { ref tx, .. } => Arc::clone(tx),
                        _ => return Err(-32), // EPIPE / ENOTCONN
                    }
                };
                loop {
                    let writers = {
                        let mut q = tx.exclusive_access();
                        if q.closed_read || q.closed_write {
                            return Err(-32); // EPIPE
                        }
                        if size == 0 {
                            return Ok(0);
                        }
                        let free = q.cap.saturating_sub(q.buf.len());
                        if free > 0 {
                            let count = size.min(free);
                            let mut copied = 0usize;
                            let mut scratch = Vec::new();
                            for part in &buffer.buffers {
                                if copied == count {
                                    break;
                                }
                                part.read_with(&mut scratch, |bytes| {
                                    let take = bytes.len().min(count - copied);
                                    q.buf.extend(&bytes[..take]);
                                    copied += take;
                                });
                            }
                            q.in_gen = q.in_gen.wrapping_add(1);
                            q.readers.wake_all();
                            wake_poll_waiters();
                            return Ok(copied);
                        }
                        Arc::clone(&q.writers)
                    };
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    wait_on(&writers, deadline)?;
                }
            }
            UnixKind::SeqPacket => {
                if dest.is_some() {
                    return Err(-106);
                }
                if size > MAX_DGRAM_BYTES {
                    return Err(-90); // EMSGSIZE
                }
                let tx = {
                    let state = self.state.exclusive_access();
                    match state.phase {
                        UnixPhase::SeqConnected { ref tx, .. } => Arc::clone(tx),
                        _ => return Err(-32),
                    }
                };
                let payload = copy_in_vec(buffer);
                loop {
                    let writers = {
                        let mut q = tx.exclusive_access();
                        if q.closed_read || q.closed_write {
                            return Err(-32);
                        }
                        if !q.is_full(size) {
                            q.bytes += payload.len();
                            q.packets.push_back((payload, local_addr));
                            q.in_gen = q.in_gen.wrapping_add(1);
                            q.readers.wake_all();
                            wake_poll_waiters();
                            return Ok(size);
                        }
                        Arc::clone(&q.writers)
                    };
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    wait_on(&writers, deadline)?;
                }
            }
            UnixKind::Dgram => {
                if size > MAX_DGRAM_BYTES {
                    return Err(-90); // EMSGSIZE
                }
                let target_rx = if let Some(dest) = dest {
                    let key = resolve_key(&dest, true)?;
                    let reg = UNIX_REGISTRY.exclusive_access();
                    match reg.get(&key) {
                        Some(UnixBinding::Dgram(w)) => w.upgrade().ok_or_else(|| match &key {
                            UnixKey::Path(p) if !crate::fs::exists(p) => -2isize,
                            _ => -111isize,
                        })?,
                        Some(UnixBinding::Listener(_) | UnixBinding::Bound(_)) => return Err(-91),
                        None => {
                            return Err(match &key {
                                UnixKey::Path(p) if crate::fs::exists(p) => -111,
                                UnixKey::Path(_) => -2,
                                UnixKey::Abstract(_) => -111,
                            });
                        }
                    }
                } else {
                    let state = self.state.exclusive_access();
                    match state.phase {
                        UnixPhase::Dgram {
                            tx: Some(ref tx), ..
                        } => Arc::clone(tx),
                        _ => return Err(-107), // ENOTCONN
                    }
                };
                let payload = copy_in_vec(buffer);
                loop {
                    let writers = {
                        let mut q = target_rx.exclusive_access();
                        if q.closed_read {
                            return Err(-111); // ECONNREFUSED
                        }
                        if !q.is_full(size) {
                            q.bytes += payload.len();
                            q.packets.push_back((payload, local_addr));
                            q.in_gen = q.in_gen.wrapping_add(1);
                            q.readers.wake_all();
                            wake_poll_waiters();
                            return Ok(size);
                        }
                        Arc::clone(&q.writers)
                    };
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    wait_on(&writers, deadline)?;
                }
            }
        }
    }

    pub(crate) fn receive(
        &self,
        buffer: &UserBuffer,
        flags: usize,
    ) -> Result<(usize, Option<UnixAddress>), isize> {
        let size = buffer.len();
        let (nonblock, timeout) = {
            let state = self.state.exclusive_access();
            if state.read_shutdown {
                return Ok((0, None));
            }
            (
                state.nonblock || flags & MSG_DONTWAIT != 0,
                state.receive_timeout,
            )
        };
        let deadline = timeout.map(|d| crate::timer::get_time_ns().saturating_add(d));

        match self.kind {
            UnixKind::Stream => {
                let rx = {
                    let state = self.state.exclusive_access();
                    match state.phase {
                        UnixPhase::StreamConnected { ref rx, .. } => Arc::clone(rx),
                        _ => return Err(-22), // EINVAL / ENOTCONN
                    }
                };
                if size == 0 {
                    return Ok((0, None));
                }
                loop {
                    let readers = {
                        let mut q = rx.exclusive_access();
                        if !q.buf.is_empty() {
                            let count = size.min(q.buf.len());
                            let (first, second) = q.buf.as_slices();
                            let first_take = count.min(first.len());
                            let second_take = count - first_take;
                            buffer.copy_from_two(&first[..first_take], &second[..second_take]);
                            if flags & MSG_PEEK == 0 {
                                q.buf.drain(..count);
                                q.out_gen = q.out_gen.wrapping_add(1);
                                q.writers.wake_all();
                                wake_poll_waiters();
                            }
                            return Ok((count, None));
                        }
                        if q.closed_write || q.closed_read {
                            return Ok((0, None));
                        }
                        Arc::clone(&q.readers)
                    };
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    wait_on(&readers, deadline)?;
                }
            }
            UnixKind::SeqPacket | UnixKind::Dgram => {
                let (rx, eof_on_peer_close) = {
                    let state = self.state.exclusive_access();
                    match state.phase {
                        UnixPhase::SeqConnected { ref rx, .. } => (Arc::clone(rx), true),
                        UnixPhase::Dgram { ref rx, .. } => (Arc::clone(rx), false),
                        _ => return Err(-22),
                    }
                };
                loop {
                    let readers = {
                        let mut q = rx.exclusive_access();
                        if let Some((data, src)) = if flags & MSG_PEEK != 0 {
                            q.packets.front().cloned()
                        } else {
                            q.packets.pop_front().map(|pkt| {
                                q.bytes = q.bytes.saturating_sub(pkt.0.len());
                                q.out_gen = q.out_gen.wrapping_add(1);
                                q.writers.wake_all();
                                wake_poll_waiters();
                                pkt
                            })
                        } {
                            let count = size.min(data.len());
                            buffer.copy_from(&data[..count]);
                            let reported = if flags & MSG_TRUNC != 0 {
                                data.len()
                            } else {
                                count
                            };
                            let peer = if self.kind == UnixKind::Dgram {
                                Some(src)
                            } else {
                                None
                            };
                            return Ok((reported, peer));
                        }
                        if q.closed_read || (eof_on_peer_close && q.closed_write) {
                            return Ok((0, None));
                        }
                        Arc::clone(&q.readers)
                    };
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    wait_on(&readers, deadline)?;
                }
            }
        }
    }

    pub(crate) fn set_io_timeout(&self, receive: bool, value: Option<usize>) {
        let mut state = self.state.exclusive_access();
        if receive {
            state.receive_timeout = value;
        } else {
            state.send_timeout = value;
        }
    }

    pub(crate) fn io_timeout(&self, receive: bool) -> usize {
        let state = self.state.exclusive_access();
        if receive {
            state.receive_timeout
        } else {
            state.send_timeout
        }
        .unwrap_or(0)
    }

    pub(crate) fn set_option(&self, level: usize, option: usize, value: i32) -> Result<(), isize> {
        let mut state = self.state.exclusive_access();
        match (level, option) {
            (1, 2) => {
                state.reuse_addr = value != 0;
                Ok(())
            }
            (1, 7 | 8) => {
                let limit = (value.max(0) as usize)
                    .saturating_mul(2)
                    .clamp(MIN_BUF_CAP, MAX_BUF_CAP);
                if option == 7 {
                    state.send_limit = limit;
                    if let UnixPhase::StreamConnected { ref tx, .. } = state.phase {
                        tx.exclusive_access().cap = limit;
                    }
                } else {
                    state.receive_limit = limit;
                    if let UnixPhase::StreamConnected { ref rx, .. } = state.phase {
                        rx.exclusive_access().cap = limit;
                    }
                }
                Ok(())
            }
            (1, 16) => {
                // SO_PASSCRED
                state.passcred = value != 0;
                Ok(())
            }
            _ => Err(-92), // ENOPROTOOPT
        }
    }

    pub(crate) fn get_option(&self, level: usize, option: usize) -> Result<i32, isize> {
        let state = self.state.exclusive_access();
        match (level, option) {
            (1, 2) => Ok(state.reuse_addr as i32),
            (1, 3) => Ok(self.kind.sock_type()), // SO_TYPE
            (1, 4) => Ok(0),                     // SO_ERROR
            (1, 7) => Ok(state.send_limit as i32),
            (1, 8) => Ok(state.receive_limit as i32),
            (1, 16) => Ok(state.passcred as i32),
            (1, 30) => Ok(matches!(state.phase, UnixPhase::Listening(_)) as i32), // SO_ACCEPTCONN
            (1, 38) => Ok(0),                                                    // SO_PROTOCOL
            (1, 39) => Ok(1),                                                    // SO_DOMAIN (AF_UNIX)
            _ => Err(-92),
        }
    }

    pub(crate) fn peer_cred(&self) -> UnixCred {
        self.state
            .exclusive_access()
            .peer_cred
            .unwrap_or(UnixCred {
                pid: 0,
                uid: u32::MAX,
                gid: u32::MAX,
            })
    }
}

impl Drop for UnixSocket {
    fn drop(&mut self) {
        let mut state = self.state.exclusive_access();
        if let Some(UnixKey::Abstract(bytes)) = state.bound_key.take() {
            UNIX_REGISTRY
                .exclusive_access()
                .remove(&UnixKey::Abstract(bytes));
        }
        match state.phase {
            UnixPhase::StreamConnected { ref rx, ref tx } => {
                rx.exclusive_access().close_read();
                tx.exclusive_access().close_write();
            }
            UnixPhase::SeqConnected { ref rx, ref tx } => {
                rx.exclusive_access().close_read();
                tx.exclusive_access().close_write();
            }
            UnixPhase::Dgram {
                ref rx,
                ref tx,
                paired,
            } => {
                rx.exclusive_access().close_read();
                if paired {
                    if let Some(tx) = tx {
                        tx.exclusive_access().close_write();
                    }
                }
            }
            UnixPhase::Listening(ref listener) => {
                let mut l = listener.exclusive_access();
                l.closed = true;
                l.acceptors.wake_all();
                l.connectors.wake_all();
                wake_poll_waiters();
            }
            UnixPhase::Fresh => {}
        }
    }
}

impl File for UnixSocket {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }
    fn read(&self, buf: UserBuffer) -> usize {
        self.read_result(buf).max(0) as usize
    }
    fn read_result(&self, buf: UserBuffer) -> isize {
        self.receive(&buf, 0)
            .map_or_else(|error| error, |(count, _)| count as isize)
    }
    fn write(&self, buf: UserBuffer) -> isize {
        self.send(&buf, 0, None).map_or_else(
            |error| {
                if error == -32 {
                    crate::syscall::signal::queue_current(13);
                }
                error
            },
            |count| count as isize,
        )
    }
    fn mode(&self) -> u16 {
        0o140777
    }
    fn unix_socket(&self) -> Option<&UnixSocket> {
        Some(self)
    }
    fn status_flags(&self) -> u32 {
        2 | if self.state.exclusive_access().nonblock {
            0x800
        } else {
            0
        }
    }
    fn set_status_flags(&self, flags: u32) {
        self.state.exclusive_access().nonblock = flags & 0x800 != 0;
    }
    fn poll_readable(&self) -> bool {
        self.poll_events() & 1 != 0
    }
    fn poll_writable(&self) -> bool {
        self.poll_events() & 4 != 0
    }
    fn poll_events(&self) -> u16 {
        const IN: u16 = 1;
        const OUT: u16 = 4;
        const HUP: u16 = 16;
        const RDHUP: u16 = 0x2000;
        let state = self.state.exclusive_access();
        match state.phase {
            UnixPhase::Fresh => 0,
            UnixPhase::Listening(ref listener) => {
                if !listener.exclusive_access().queue.is_empty() {
                    IN
                } else {
                    0
                }
            }
            UnixPhase::StreamConnected { ref rx, ref tx } => {
                let r = rx.exclusive_access();
                let t = tx.exclusive_access();
                let rx_closed = state.read_shutdown || r.closed_write;
                let tx_closed = state.write_shutdown || t.closed_read;
                let mut mask = 0u16;
                if !r.buf.is_empty() || rx_closed {
                    mask |= IN;
                }
                if rx_closed {
                    mask |= RDHUP;
                }
                if (!tx_closed && t.buf.len() < t.cap) || tx_closed {
                    mask |= OUT;
                }
                if rx_closed && tx_closed {
                    mask |= HUP;
                }
                mask
            }
            UnixPhase::SeqConnected { ref rx, ref tx } => {
                let r = rx.exclusive_access();
                let t = tx.exclusive_access();
                let rx_closed = state.read_shutdown || r.closed_write;
                let tx_closed = state.write_shutdown || t.closed_read;
                let mut mask = 0u16;
                if !r.packets.is_empty() || rx_closed {
                    mask |= IN;
                }
                if rx_closed {
                    mask |= RDHUP;
                }
                if (!tx_closed && !t.is_full(0)) || tx_closed {
                    mask |= OUT;
                }
                if rx_closed && tx_closed {
                    mask |= HUP;
                }
                mask
            }
            UnixPhase::Dgram { ref rx, ref tx, .. } => {
                let r = rx.exclusive_access();
                let mut mask = 0u16;
                if !r.packets.is_empty() || state.read_shutdown {
                    mask |= IN;
                }
                let writable = match tx {
                    Some(tx) => {
                        let t = tx.exclusive_access();
                        !t.is_full(0) || t.closed_read
                    }
                    None => true,
                };
                if writable {
                    mask |= OUT;
                }
                mask
            }
        }
    }
    fn poll_generations(&self) -> (u64, u64) {
        let state = self.state.exclusive_access();
        match state.phase {
            UnixPhase::Fresh => (0, 0),
            UnixPhase::Listening(ref listener) => (listener.exclusive_access().in_gen, 0),
            UnixPhase::StreamConnected { ref rx, ref tx } => (
                rx.exclusive_access().in_gen,
                tx.exclusive_access().out_gen,
            ),
            UnixPhase::SeqConnected { ref rx, ref tx } => (
                rx.exclusive_access().in_gen,
                tx.exclusive_access().out_gen,
            ),
            UnixPhase::Dgram { ref rx, ref tx, .. } => (
                rx.exclusive_access().in_gen,
                tx.as_ref().map_or(0, |q| q.exclusive_access().out_gen),
            ),
        }
    }
}
