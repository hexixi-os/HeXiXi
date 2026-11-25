use super::{Network, ANY, CLOSE_TIMEOUT_MS, HANDSHAKE_TIMEOUT_MS, NETWORK};
use crate::fs::File;
use crate::mm::UserBuffer;
use crate::sync::UPSafeCell;
use crate::{task, timer};
use alloc::{sync::Arc, vec::Vec};
use smoltcp::{
    iface::SocketHandle,
    socket::{TcpSocket, TcpState},
    wire::IpEndpoint,
    Error,
};

pub(crate) const MSG_PEEK: usize = 0x2;
pub(crate) const MSG_TRUNC: usize = 0x20;
pub(crate) const MSG_DONTWAIT: usize = 0x40;
const EAGAIN: isize = -11;
const MAX_BACKLOG: usize = 8;
const MAX_UDP_PAYLOAD: usize = 1472;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Fresh,
    Listening,
    Connecting(usize),
    Connected,
    Reset,
    Failed(isize),
}

struct State {
    handle: SocketHandle,
    listeners: Vec<SocketHandle>,
    bound: Option<IpEndpoint>,
    local: Option<IpEndpoint>,
    peer: Option<IpEndpoint>,
    phase: Phase,
    pending_error: isize,
    nonblock: bool,
    read_shutdown: bool,
    write_shutdown: bool,
    no_delay: bool,
    reuse_addr: bool,
    keepalive: bool,
    send_limit: usize,
    receive_limit: usize,
    receive_timeout: Option<usize>,
    send_timeout: Option<usize>,
}

pub struct SocketFile {
    kind: Kind,
    state: UPSafeCell<State>,
}

fn copy_in(user: &UserBuffer, target: &mut [u8]) {
    let mut offset = 0;
    let mut scratch = Vec::new();
    for part in &user.buffers {
        if offset == target.len() {
            break;
        }
        part.read_with(&mut scratch, |bytes| {
            let count = bytes.len().min(target.len() - offset);
            target[offset..offset + count].copy_from_slice(&bytes[..count]);
            offset += count;
        });
    }
}

fn endpoint(port: u16) -> IpEndpoint {
    IpEndpoint::new(ANY, port)
}

pub(crate) fn wait_for_io() -> Result<(), isize> {
    let now = timer::get_time_ms();
    let current = task::current_task().unwrap();
    if crate::syscall::signal::interrupts_wait(&current, now) {
        return Err(-4);
    }
    // VirtIO RX is serviced by polling. Sleep until the next poll attempt.
    timer::add_timer(now.saturating_add(1), current);
    task::block_current_and_run_next();
    task::check_kill_current();
    Ok(())
}

impl State {
    fn new(handle: SocketHandle, nonblock: bool) -> Self {
        Self {
            handle,
            nonblock,
            listeners: Vec::new(),
            bound: None,
            local: None,
            peer: None,
            phase: Phase::Fresh,
            pending_error: 0,
            read_shutdown: false,
            write_shutdown: false,
            no_delay: false,
            reuse_addr: false,
            keepalive: false,
            send_limit: super::TCP_BUFFER,
            receive_limit: super::TCP_BUFFER,
            receive_timeout: None,
            send_timeout: None,
        }
    }

    fn refresh_tcp(&mut self, net: &mut Network) {
        let socket = net.iface.get_socket::<TcpSocket>(self.handle);
        if let Phase::Connecting(deadline) = self.phase {
            self.pending_error = match socket.state() {
                TcpState::Established | TcpState::CloseWait => {
                    self.phase = Phase::Connected;
                    net.tcp_receive
                        .insert(self.handle, super::TcpReceive::default());
                    net.preserve_tcp_receive();
                    return;
                }
                TcpState::Closed => -111, // ECONNREFUSED
                _ if timer::get_time_ms() >= deadline => {
                    socket.abort();
                    -110
                }
                _ => return,
            };
            self.phase = Phase::Failed(self.pending_error);
        } else if self.phase == Phase::Connected
            && socket.state() == TcpState::Closed
            && !net.tcp_receive[&self.handle].fin
        {
            self.phase = Phase::Reset;
            self.pending_error = -104; // ECONNRESET
        }
    }

    fn bind(&mut self, net: &mut Network, kind: Kind, local: IpEndpoint) -> Result<(), isize> {
        if self.bound.is_some() || self.phase != Phase::Fresh {
            return Err(-22);
        }
        let local = if kind == Kind::Tcp {
            net.reserve(kind, local)?
        } else {
            net.bind_udp(self.handle, local, self.reuse_addr)?
        };
        if kind == Kind::Udp {
            net.udp_receive_limit(local, self.handle, self.receive_limit);
        }
        self.bound = Some(local);
        Ok(())
    }

    fn ready_listener(&self, net: &mut Network) -> Option<SocketHandle> {
        core::iter::once(self.handle)
            .chain(self.listeners.iter().copied())
            .find(|&handle| {
                matches!(
                    net.iface.get_socket::<TcpSocket>(handle).state(),
                    TcpState::Established | TcpState::CloseWait
                )
            })
    }
}

impl SocketFile {
    pub(crate) fn new(kind: Kind, nonblock: bool) -> Result<Arc<Self>, isize> {
        let mut network = NETWORK.exclusive_access();
        let net = network.as_mut().ok_or(-100isize)?;
        net.poll();
        Ok(Self::from_state(
            kind,
            State::new(
                net.allocate(kind, super::TCP_BUFFER, super::TCP_BUFFER)?,
                nonblock,
            ),
        ))
    }

    fn from_state(kind: Kind, state: State) -> Arc<Self> {
        Arc::new(Self {
            kind,
            state: unsafe { UPSafeCell::new(state) },
        })
    }

    fn with_state<T>(&self, operation: impl FnOnce(&mut Network, &mut State) -> T) -> T {
        let mut network = NETWORK.exclusive_access();
        let net = network.as_mut().unwrap();
        net.poll();
        let mut state = self.state.exclusive_access();
        if self.kind == Kind::Tcp {
            state.refresh_tcp(net);
        }
        operation(net, &mut state)
    }

    fn wait<T>(
        &self,
        flags: usize,
        receiving: bool,
        mut operation: impl FnMut(&mut Network, &mut State) -> Result<T, isize>,
    ) -> Result<T, isize> {
        let deadline = {
            let state = self.state.exclusive_access();
            let timeout = if receiving {
                state.receive_timeout
            } else {
                state.send_timeout
            };
            timeout.map(|duration| timer::get_time_ns().saturating_add(duration))
        };
        loop {
            let (result, nonblock) =
                self.with_state(|net, state| (operation(net, state), state.nonblock));
            if !matches!(result, Err(EAGAIN))
                || nonblock
                || flags & MSG_DONTWAIT != 0
                || deadline.map_or(false, |end| timer::get_time_ns() >= end)
            {
                return result;
            }
            wait_for_io()?;
        }
    }

    pub(crate) fn bind(&self, local: IpEndpoint) -> Result<(), isize> {
        self.with_state(|net, state| state.bind(net, self.kind, local))
    }

    pub(crate) fn listen(&self, backlog: i32) -> Result<(), isize> {
        if self.kind != Kind::Tcp {
            return Err(-95);
        }
        self.with_state(|net, state| {
            if !matches!(state.phase, Phase::Fresh | Phase::Listening) {
                return Err(-22);
            }
            if state.bound.is_none() {
                state.bind(net, Kind::Tcp, endpoint(0))?;
            }
            let local = state.bound.unwrap();
            if state.phase != Phase::Listening {
                net.listen(state.handle, local).map_err(|_| -22isize)?;
                state.phase = Phase::Listening;
            }
            while state.listeners.len() + 1 < backlog.clamp(1, MAX_BACKLOG as i32) as usize {
                let handle = match net.allocate(Kind::Tcp, state.receive_limit, state.send_limit) {
                    Ok(handle) => handle,
                    Err(_) => break,
                };
                net.listen(handle, local).unwrap();
                state.listeners.push(handle);
            }
            Ok(())
        })
    }

    pub(crate) fn accept(
        &self,
        nonblock: bool,
        have_fd: impl Fn() -> bool,
    ) -> Result<(Arc<Self>, IpEndpoint), isize> {
        self.wait(0, true, |net, state| {
            if self.kind != Kind::Tcp {
                return Err(-95);
            }
            if state.phase != Phase::Listening {
                return Err(-22);
            }
            let handle = state.ready_listener(net).ok_or(EAGAIN)?;
            if !have_fd() {
                return Err(-24);
            }
            let replacement = net.allocate(Kind::Tcp, state.receive_limit, state.send_limit)?;
            net.listen(replacement, state.bound.unwrap()).unwrap();
            net.tcp_listeners.remove(&handle);
            if handle == state.handle {
                state.handle = replacement;
            } else {
                *state
                    .listeners
                    .iter_mut()
                    .find(|slot| **slot == handle)
                    .unwrap() = replacement;
            }
            let socket = net.iface.get_socket::<TcpSocket>(handle);
            let keepalive = state
                .keepalive
                .then(|| smoltcp::time::Duration::from_secs(7200));
            socket.set_keep_alive(keepalive);
            socket.set_timeout(None);
            socket.set_nagle_enabled(!state.no_delay);
            let peer = socket.remote_endpoint();
            let mut accepted = State::new(handle, nonblock);
            accepted.phase = Phase::Connected;
            accepted.peer = Some(peer);
            accepted.local = Some(socket.local_endpoint());
            accepted.no_delay = state.no_delay;
            accepted.keepalive = state.keepalive;
            accepted.send_limit = state.send_limit;
            accepted.receive_limit = state.receive_limit;
            accepted.send_timeout = state.send_timeout;
            accepted.receive_timeout = state.receive_timeout;
            net.tcp_receive.insert(handle, super::TcpReceive::default());
            net.preserve_tcp_receive();
            Ok((Self::from_state(Kind::Tcp, accepted), peer))
        })
    }

    pub(crate) fn connect(&self, mut peer: IpEndpoint) -> Result<(), isize> {
        let connect_started = self.with_state(|net, state| {
            if peer.addr.is_unspecified() {
                peer.addr = super::LOOPBACK.into();
            }
            if peer.port == 0 {
                return Err(-22);
            }
            let source = net.source_for(peer.addr)?;
            if self.kind == Kind::Udp {
                if state.bound.is_none() {
                    state.bind(net, Kind::Udp, endpoint(0))?;
                }
                let bound = state.bound.unwrap();
                if bound.addr == super::LOOPBACK.into() && source != bound.addr {
                    return Err(-101);
                }
                state.local = Some(IpEndpoint::new(
                    if bound.addr.is_unspecified() {
                        source
                    } else {
                        bound.addr
                    },
                    bound.port,
                ));
                state.peer = Some(peer);
                net.connect_udp(bound, state.handle, peer);
                return Ok(false);
            }
            match state.phase {
                Phase::Connected | Phase::Reset => return Err(-106),
                Phase::Connecting(_) => return Err(-114),
                Phase::Listening => return Err(-22),
                Phase::Fresh | Phase::Failed(_) => {}
            }
            if state.bound.is_none() {
                state.bind(net, Kind::Tcp, IpEndpoint::new(source, 0))?;
            }
            let mut local = state.bound.unwrap();
            if local.addr.is_unspecified() {
                local.addr = source;
            }
            if local.addr == super::LOOPBACK.into() && local.addr != source {
                return Err(-101);
            }
            let (socket, context) = net.iface.get_socket_and_context::<TcpSocket>(state.handle);
            socket.connect(context, peer, local).map_err(|_| -22isize)?;
            socket.set_keep_alive(
                state
                    .keepalive
                    .then(|| smoltcp::time::Duration::from_secs(7200)),
            );
            socket.set_nagle_enabled(!state.no_delay);
            state.local = Some(local);
            state.peer = Some(peer);
            state.pending_error = 0;
            state.phase =
                Phase::Connecting(timer::get_time_ms().saturating_add(HANDSHAKE_TIMEOUT_MS));
            net.poll();
            if state.nonblock {
                return Err(-115);
            } // EINPROGRESS
            Ok(true)
        })?;
        if connect_started {
            self.wait(0, false, |_net, state| match state.phase {
                Phase::Connected => Ok(()),
                Phase::Failed(error) => Err(error),
                _ => Err(EAGAIN),
            })
        } else {
            Ok(())
        }
    }

    pub(crate) fn local(&self) -> IpEndpoint {
        self.with_state(|_net, state| state.local.or(state.bound).unwrap_or(endpoint(0)))
    }

    pub(crate) fn peer(&self) -> Result<IpEndpoint, isize> {
        self.with_state(|_net, state| {
            if self.kind == Kind::Udp || state.phase == Phase::Connected {
                state.peer.ok_or(-107)
            } else {
                Err(-107)
            }
        })
    }

    pub(crate) fn shutdown(&self, how: usize) -> Result<(), isize> {
        if how > 2 {
            return Err(-22);
        }
        self.with_state(|net, state| {
            if state.peer.is_none() || (self.kind == Kind::Tcp && state.phase != Phase::Connected) {
                return Err(-107);
            }
            if how != 1 {
                state.read_shutdown = true;
            }
            if how != 0 {
                state.write_shutdown = true;
                if self.kind == Kind::Tcp {
                    net.iface.get_socket::<TcpSocket>(state.handle).close();
                }
            }
            net.poll();
            Ok(())
        })
    }

    pub(crate) fn send(
        &self,
        buffer: &UserBuffer,
        flags: usize,
        peer: Option<IpEndpoint>,
    ) -> Result<usize, isize> {
        let size = buffer.len();
        self.wait(flags, false, |net, state| {
            if state.write_shutdown {
                return Err(-32);
            }
            let count = match self.kind {
                Kind::Tcp => {
                    match state.phase {
                        Phase::Connecting(_) => return Err(EAGAIN),
                        Phase::Failed(error) => return Err(error),
                        Phase::Connected => {}
                        _ => return Err(-32),
                    }
                    if peer.is_some() {
                        return Err(-106);
                    }
                    let socket = net.iface.get_socket::<TcpSocket>(state.handle);
                    if !socket.may_send() {
                        return Err(-32);
                    }
                    if size == 0 {
                        return Ok(0);
                    }
                    let free = state.send_limit.saturating_sub(socket.send_queue());
                    if !socket.can_send() || free == 0 {
                        return Err(EAGAIN);
                    }
                    socket
                        .send(|space| {
                            let count = size.min(space.len()).min(free);
                            copy_in(buffer, &mut space[..count]);
                            (count, count)
                        })
                        .map_err(|_| -32isize)?
                }
                Kind::Udp => {
                    let peer = peer.or(state.peer).ok_or(-89isize)?;
                    if peer.port == 0 || peer.addr.is_unspecified() {
                        return Err(-22);
                    }
                    if size > MAX_UDP_PAYLOAD {
                        return Err(-90);
                    }
                    let source = net.source_for(peer.addr)?;
                    if state.bound.is_none() {
                        state.bind(net, Kind::Udp, endpoint(0))?;
                    }
                    let local = state.bound.unwrap();
                    if local.addr == super::LOOPBACK.into() && source != local.addr {
                        return Err(-101);
                    }
                    net.enqueue_datagram(
                        local,
                        state.handle,
                        peer,
                        size,
                        state.send_limit,
                        |data| copy_in(buffer, data),
                    )?;
                    size
                }
            };
            net.poll();
            Ok(count)
        })
    }

    pub(crate) fn receive(
        &self,
        buffer: &UserBuffer,
        flags: usize,
    ) -> Result<(usize, Option<IpEndpoint>), isize> {
        let size = buffer.len();
        self.wait(flags, true, |net, state| {
            if state.read_shutdown {
                return Ok((0, state.peer));
            }
            let result = match self.kind {
                Kind::Tcp => {
                    match state.phase {
                        Phase::Connecting(_) => return Err(EAGAIN),
                        Phase::Failed(error) => return Err(error),
                        Phase::Connected | Phase::Reset => {}
                        _ => return Err(-107),
                    }
                    if size == 0 {
                        return Ok((0, state.peer));
                    }
                    let receive = net.tcp_receive.get_mut(&state.handle).unwrap();
                    let tail = &receive.bytes[receive.offset..];
                    if !tail.is_empty() {
                        let count = size.min(tail.len());
                        buffer.copy_from(&tail[..count]);
                        if flags & MSG_PEEK == 0 {
                            receive.offset += count;
                        }
                        return Ok((count, state.peer));
                    }
                    let socket = net.iface.get_socket::<TcpSocket>(state.handle);
                    if !socket.can_recv() && (receive.fin || state.phase == Phase::Reset) {
                        let error = core::mem::take(&mut state.pending_error);
                        return if error == 0 {
                            Ok((0, state.peer))
                        } else {
                            Err(error)
                        };
                    }
                    let read = if flags & MSG_PEEK != 0 {
                        socket.peek(size).map(|data| {
                            buffer.copy_from(data);
                            data.len()
                        })
                    } else {
                        socket.recv(|data| {
                            let count = data.len().min(size);
                            buffer.copy_from(&data[..count]);
                            (count, count)
                        })
                    };
                    let count = match read {
                        Ok(0) => return Err(EAGAIN),
                        Ok(count) => count,
                        Err(Error::Finished) => 0,
                        Err(_) => return Err(-104),
                    };
                    (count, state.peer)
                }
                Kind::Udp => {
                    let local = state.bound.ok_or(EAGAIN)?;
                    net.receive_datagram(
                        local,
                        state.handle,
                        flags & MSG_PEEK != 0,
                        |data, peer| {
                            let count = data.len().min(size);
                            buffer.copy_from(&data[..count]);
                            (
                                if flags & MSG_TRUNC != 0 {
                                    data.len()
                                } else {
                                    count
                                },
                                Some(peer),
                            )
                        },
                    )
                    .ok_or(EAGAIN)?
                }
            };
            net.poll();
            Ok(result)
        })
    }

    pub(crate) fn set_option(&self, level: usize, option: usize, value: i32) -> Result<(), isize> {
        self.with_state(|net, state| match (level, option) {
            (1, 2) => {
                // SO_REUSEADDR
                state.reuse_addr = value != 0;
                if self.kind == Kind::Udp {
                    if let Some(local) = state.bound {
                        net.reuse_udp(local, state.handle, state.reuse_addr);
                    }
                }
                Ok(())
            }
            (1, 7 | 8) => {
                // SO_SNDBUF, SO_RCVBUF
                let cap = if self.kind == Kind::Tcp {
                    super::TCP_BUFFER
                } else {
                    super::UDP_BUFFER
                };
                let limit = (value.max(0) as usize).saturating_mul(2).clamp(4096, cap);
                match (self.kind, option, state.phase) {
                    (Kind::Tcp, _, Phase::Fresh) => {
                        if option == 7 {
                            state.send_limit = limit;
                        } else {
                            state.receive_limit = limit;
                        }
                        *net.iface.get_socket::<TcpSocket>(state.handle) =
                            super::tcp_socket(state.receive_limit, state.send_limit);
                    }
                    (Kind::Tcp, 7, _) => {
                        let capacity = net
                            .iface
                            .get_socket::<TcpSocket>(state.handle)
                            .send_capacity();
                        state.send_limit = limit.min(capacity);
                    }
                    (Kind::Udp, 8, _) => {
                        state.receive_limit = limit;
                        if let Some(local) = state.bound {
                            net.udp_receive_limit(local, state.handle, limit);
                        }
                    }
                    (Kind::Tcp, 8, _) if limit == state.receive_limit => {}
                    (Kind::Udp, 7, _) => state.send_limit = limit,
                    _ => return Err(-95), // These resident rings have fixed sizes.
                }
                Ok(())
            }
            (1, 9) if self.kind == Kind::Tcp => {
                // SO_KEEPALIVE
                state.keepalive = value != 0;
                let keepalive = state
                    .keepalive
                    .then(|| smoltcp::time::Duration::from_secs(7200));
                net.iface
                    .get_socket::<TcpSocket>(state.handle)
                    .set_keep_alive(keepalive);
                Ok(())
            }
            (6, 1) if self.kind == Kind::Tcp => {
                // TCP_NODELAY
                state.no_delay = value != 0;
                net.iface
                    .get_socket::<TcpSocket>(state.handle)
                    .set_nagle_enabled(!state.no_delay);
                Ok(())
            }
            _ => Err(-92),
        })
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

    pub(crate) fn get_option(&self, level: usize, option: usize) -> Result<i32, isize> {
        self.with_state(|_net, state| match (level, option) {
            (1, 2) => Ok(state.reuse_addr as i32),
            (1, 3) => Ok(if self.kind == Kind::Tcp { 1 } else { 2 }), // SO_TYPE
            (1, 4) => Ok(-core::mem::take(&mut state.pending_error) as i32), // SO_ERROR
            (1, 7) => Ok(state.send_limit as i32),
            (1, 8) => Ok(state.receive_limit as i32),
            (1, 9) if self.kind == Kind::Tcp => Ok(state.keepalive as i32),
            (1, 30) => Ok((state.phase == Phase::Listening) as i32), // SO_ACCEPTCONN
            (1, 38) => Ok(if self.kind == Kind::Tcp { 6 } else { 17 }), // SO_PROTOCOL
            (1, 39) => Ok(2),                                        // SO_DOMAIN
            (6, 1) if self.kind == Kind::Tcp => Ok(state.no_delay as i32),
            (6, 2) if self.kind == Kind::Tcp => Ok(1460), // TCP_MAXSEG for Ethernet IPv4
            _ => Err(-92),
        })
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        let mut network = NETWORK.exclusive_access();
        let net = network.as_mut().unwrap();
        let mut state = self.state.exclusive_access();
        let reservation = state.bound.take();
        if self.kind == Kind::Udp {
            match reservation {
                Some(local) => net.unbind_udp(state.handle, local),
                None => net.remove(state.handle),
            }
            return;
        }
        let graceful = if self.kind == Kind::Tcp && state.phase == Phase::Connected {
            let socket = net.iface.get_socket::<TcpSocket>(state.handle);
            socket.close();
            socket.is_open()
        } else {
            false
        };
        net.tcp_receive.remove(&state.handle);
        if graceful {
            net.closing.push((
                state.handle,
                timer::get_time_ms().saturating_add(CLOSE_TIMEOUT_MS),
                reservation,
            ));
        } else {
            net.remove(state.handle);
            if let Some(local) = reservation {
                net.unreserve(self.kind, local);
            }
        }
        for handle in state.listeners.drain(..) {
            net.remove(handle);
        }
        net.poll();
    }
}

impl File for SocketFile {
    fn readable(&self) -> bool {
        true
    }
    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buffer: UserBuffer) -> usize {
        self.read_result(buffer).max(0) as usize
    }

    fn read_result(&self, buffer: UserBuffer) -> isize {
        self.receive(&buffer, 0)
            .map_or_else(|error| error, |(count, _)| count as isize)
    }

    fn write(&self, buffer: UserBuffer) -> isize {
        self.send(&buffer, 0, None).map_or_else(
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
    fn socket(&self) -> Option<&SocketFile> {
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
        const ERR: u16 = 8;
        const HUP: u16 = 16;
        const RDHUP: u16 = 0x2000;
        self.with_state(|net, state| match self.kind {
            Kind::Tcp => match state.phase {
                Phase::Fresh | Phase::Connecting(_) => 0,
                Phase::Listening => {
                    if state.ready_listener(net).is_some() {
                        IN
                    } else {
                        0
                    }
                }
                Phase::Failed(_) | Phase::Reset => {
                    IN | OUT | HUP | if state.pending_error != 0 { ERR } else { 0 }
                }
                Phase::Connected => {
                    let socket = net.iface.get_socket::<TcpSocket>(state.handle);
                    let rx_closed = state.read_shutdown || net.tcp_receive[&state.handle].fin;
                    (if rx_closed {
                        IN | RDHUP
                    } else if socket.can_recv() {
                        IN
                    } else {
                        0
                    }) | (if (socket.can_send() && socket.send_queue() < state.send_limit)
                        || state.write_shutdown
                    {
                        OUT
                    } else {
                        0
                    }) | (if rx_closed && !socket.may_send() {
                        HUP
                    } else {
                        0
                    })
                }
            },
            Kind::Udp => {
                let ready = state
                    .bound
                    .map_or(false, |local| net.udp_ready(local, state.handle));
                let writable = state.bound.map_or(true, |local| {
                    net.udp_writable(local, state.handle, state.send_limit)
                });
                (if state.read_shutdown || ready { IN } else { 0 })
                    | (if writable { OUT } else { 0 })
            }
        })
    }
}
