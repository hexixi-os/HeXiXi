mod device;
mod socket;
mod udp;

pub use socket::SocketFile;
pub(crate) use socket::{wait_for_io, Kind, MSG_DONTWAIT, MSG_PEEK, MSG_TRUNC};

use crate::sync::UPSafeCell;
use crate::timer;
use alloc::{collections::BTreeMap, vec, vec::Vec};
use device::{EthernetDevice, GATEWAY, GUEST, LOOPBACK};
use smoltcp::iface::{Interface, InterfaceBuilder, NeighborCache, Routes, SocketHandle};
use smoltcp::socket::{
    TcpSocket, TcpSocketBuffer, TcpState, UdpPacketMetadata, UdpSocket, UdpSocketBuffer,
};
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, Ipv4Address};

pub(crate) const ANY: IpAddress = IpAddress::Ipv4(Ipv4Address([0; 4]));
const MAX_STACK_SOCKETS: usize = 128;
const TCP_BUFFER: usize = 32 * 1024;
const UDP_BUFFER: usize = 32 * 1024;
const FIRST_EPHEMERAL: u16 = 49152;
const CLOSE_TIMEOUT_MS: usize = 65_000;
const HANDSHAKE_TIMEOUT_MS: usize = 30_000;

static NETWORK: UPSafeCell<Option<Network>> = unsafe { UPSafeCell::new(None) };

pub(crate) fn init() {
    let device = EthernetDevice::new();
    let mut addresses = vec![IpCidr::new(LOOPBACK.into(), 8)];
    let mut routes = Routes::new(BTreeMap::new());
    if device.has_nic() {
        addresses.insert(0, IpCidr::new(GUEST.into(), 24));
        routes.add_default_ipv4_route(GATEWAY).unwrap();
        println!("[net] VirtIO IPv4 {}", GUEST);
    }
    let mac = device.mac;
    let iface = InterfaceBuilder::new(device, Vec::new())
        .hardware_addr(mac.into())
        .neighbor_cache(NeighborCache::new(BTreeMap::new()))
        .ip_addrs(addresses)
        .routes(routes)
        .random_seed(timer::get_time() as u64)
        .finalize();
    *NETWORK.exclusive_access() = Some(Network {
        iface,
        reservations: Vec::new(),
        closing: Vec::new(),
        tcp_receive: BTreeMap::new(),
        tcp_listeners: BTreeMap::new(),
        udp_groups: Vec::new(),
        last_timer_poll: None,
        next_port: FIRST_EPHEMERAL,
        socket_count: 0,
    });
}

pub(crate) fn poll() {
    if let Some(network) = NETWORK.exclusive_access().as_mut() {
        let now = timer::get_time_ms();
        if network.last_timer_poll != Some(now) {
            network.last_timer_poll = Some(now);
            network.poll();
        }
    }
}

#[derive(Default)]
struct TcpReceive {
    fin: bool,
    bytes: Vec<u8>,
    offset: usize,
}

struct Network {
    iface: Interface<'static, EthernetDevice>,
    reservations: Vec<(Kind, IpEndpoint)>,
    closing: Vec<(SocketHandle, usize, Option<IpEndpoint>)>,
    tcp_receive: BTreeMap<SocketHandle, TcpReceive>,
    tcp_listeners: BTreeMap<SocketHandle, IpEndpoint>,
    udp_groups: Vec<udp::UdpGroup>,
    last_timer_poll: Option<usize>,
    next_port: u16,
    socket_count: usize,
}

fn tcp_socket(receive: usize, send: usize) -> TcpSocket<'static> {
    TcpSocket::new(
        TcpSocketBuffer::new(vec![0; receive]),
        TcpSocketBuffer::new(vec![0; send]),
    )
}

fn arm_listener(socket: &mut TcpSocket, local: IpEndpoint) -> smoltcp::Result<()> {
    socket.listen(local)?;
    socket.set_timeout(Some(Duration::from_millis(HANDSHAKE_TIMEOUT_MS as u64)));
    Ok(())
}

impl Network {
    fn poll(&mut self) {
        let now = timer::get_time_ms();
        self.refresh_listeners();
        self.preserve_tcp_receive();
        self.transmit_datagrams();
        self.iface.device_mut().begin_poll();
        let _ = self.iface.poll(Instant::from_millis(now as i64));
        self.dispatch_datagrams();
        self.refresh_listeners();
        self.preserve_tcp_receive();
        let mut index = 0;
        while index < self.closing.len() {
            let (handle, deadline, reservation) = self.closing[index];
            if now >= deadline || !self.iface.get_socket::<TcpSocket>(handle).is_open() {
                self.remove(handle);
                if let Some(endpoint) = reservation {
                    self.unreserve(Kind::Tcp, endpoint);
                }
                self.closing.swap_remove(index);
            } else {
                index += 1;
            }
        }
    }

    fn listen(&mut self, handle: SocketHandle, local: IpEndpoint) -> smoltcp::Result<()> {
        arm_listener(self.iface.get_socket::<TcpSocket>(handle), local)?;
        self.tcp_listeners.insert(handle, local);
        Ok(())
    }

    fn refresh_listeners(&mut self) {
        for (&handle, &local) in &self.tcp_listeners {
            let socket = self.iface.get_socket::<TcpSocket>(handle);
            match socket.state() {
                TcpState::Closed => {
                    let _ = arm_listener(socket, local);
                }
                TcpState::Established | TcpState::CloseWait => socket.set_timeout(None),
                _ => {}
            }
        }
    }

    fn preserve_tcp_receive(&mut self) {
        for (&handle, receive) in &mut self.tcp_receive {
            let socket = self.iface.get_socket::<TcpSocket>(handle);
            match socket.state() {
                TcpState::CloseWait => receive.fin = true,
                TcpState::Closing | TcpState::LastAck | TcpState::TimeWait => {
                    receive.fin = true;
                    // The wire socket can discard its ring buffer after TIME-WAIT.
                    while socket.can_recv() {
                        let count = socket
                            .recv(|bytes| {
                                receive.bytes.extend_from_slice(bytes);
                                (bytes.len(), bytes.len())
                            })
                            .unwrap_or(0);
                        if count == 0 {
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn allocate(&mut self, kind: Kind, receive: usize, send: usize) -> Result<SocketHandle, isize> {
        if self.socket_count >= MAX_STACK_SOCKETS {
            return Err(-105); // ENOBUFS
        }
        let handle = match kind {
            Kind::Tcp => self.iface.add_socket(tcp_socket(receive, send)),
            Kind::Udp => self.iface.add_socket(UdpSocket::new(
                UdpSocketBuffer::new(vec![UdpPacketMetadata::EMPTY; 16], vec![0; UDP_BUFFER]),
                UdpSocketBuffer::new(vec![UdpPacketMetadata::EMPTY], vec![0; UDP_BUFFER]),
            )),
        };
        self.socket_count += 1;
        Ok(handle)
    }

    fn remove(&mut self, handle: SocketHandle) {
        self.tcp_receive.remove(&handle);
        self.tcp_listeners.remove(&handle);
        self.iface.remove_socket(handle);
        self.socket_count -= 1;
    }

    fn local_address(&self, address: IpAddress) -> bool {
        address == IpAddress::Ipv4(LOOPBACK)
            || (address == IpAddress::Ipv4(GUEST) && self.iface.device().has_nic())
    }

    fn source_for(&self, address: IpAddress) -> Result<IpAddress, isize> {
        match address {
            IpAddress::Ipv4(address) if address == LOOPBACK => Ok(address.into()),
            IpAddress::Ipv4(address) if address.0[0] != 127 && self.iface.device().has_nic() => {
                Ok(GUEST.into())
            }
            _ => Err(-101), // ENETUNREACH
        }
    }

    fn reserve(&mut self, kind: Kind, mut local: IpEndpoint) -> Result<IpEndpoint, isize> {
        if !local.addr.is_unspecified() && !self.local_address(local.addr) {
            return Err(-99); // EADDRNOTAVAIL
        }
        let in_use = |local: IpEndpoint| {
            self.reservations.iter().any(|&(protocol, other)| {
                kind == protocol
                    && other.port == local.port
                    && (local.addr == other.addr
                        || local.addr.is_unspecified()
                        || other.addr.is_unspecified())
            })
        };
        if local.port == 0 {
            for _ in FIRST_EPHEMERAL..=u16::MAX {
                local.port = self.next_port;
                self.next_port = self.next_port.checked_add(1).unwrap_or(FIRST_EPHEMERAL);
                if !in_use(local) {
                    self.reservations.push((kind, local));
                    return Ok(local);
                }
            }
            return Err(-98);
        }
        if in_use(local) {
            return Err(-98); // EADDRINUSE
        }
        self.reservations.push((kind, local));
        Ok(local)
    }

    fn unreserve(&mut self, kind: Kind, endpoint: IpEndpoint) {
        self.reservations.retain(|&entry| entry != (kind, endpoint));
    }
}
