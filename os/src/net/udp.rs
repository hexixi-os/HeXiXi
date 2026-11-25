use super::{Kind, Network, UDP_BUFFER};
use alloc::{collections::VecDeque, vec, vec::Vec};
use smoltcp::{iface::SocketHandle, socket::UdpSocket, wire::IpEndpoint};

const MAX_DATAGRAMS: usize = 16;

struct Datagram {
    payload: Vec<u8>,
    peer: IpEndpoint,
}

impl Datagram {
    fn charge(&self) -> usize {
        self.payload.len().max(1)
    }
}

pub(super) struct UdpGroup {
    local: IpEndpoint,
    wire: SocketHandle,
    sockets: Vec<UdpMember>,
    sending: Option<(SocketHandle, usize)>,
    next_sender: usize,
}

struct UdpMember {
    handle: SocketHandle,
    reuse_addr: bool,
    peer: Option<IpEndpoint>,
    packets: VecDeque<(Vec<u8>, IpEndpoint)>,
    queued_bytes: usize,
    receive_limit: usize,
    outgoing: VecDeque<Datagram>,
    outgoing_charge: usize,
    in_gen: u64,
    out_gen: u64,
}

impl UdpMember {
    fn new(handle: SocketHandle, reuse_addr: bool) -> Self {
        Self {
            handle,
            reuse_addr,
            peer: None,
            packets: VecDeque::new(),
            queued_bytes: 0,
            receive_limit: UDP_BUFFER,
            outgoing: VecDeque::new(),
            outgoing_charge: 0,
            in_gen: 0,
            out_gen: 0,
        }
    }
}

impl Network {
    pub fn bind_udp(
        &mut self,
        handle: SocketHandle,
        local: IpEndpoint,
        reuse_addr: bool,
    ) -> Result<IpEndpoint, isize> {
        if local.port != 0 {
            if let Some(group) = self
                .udp_groups
                .iter_mut()
                .find(|group| group.local == local)
            {
                if !reuse_addr || group.sockets.iter().any(|member| !member.reuse_addr) {
                    return Err(-98);
                }
                group.sockets.push(UdpMember::new(handle, reuse_addr));
                return Ok(local);
            }
        }
        let local = self.reserve(Kind::Udp, local)?;
        if self
            .iface
            .get_socket::<UdpSocket>(handle)
            .bind(local)
            .is_err()
        {
            self.unreserve(Kind::Udp, local);
            return Err(-22);
        }
        if local.addr.is_unspecified() {
            self.iface.device_mut().track_wildcard_udp(local.port, true);
        }
        self.udp_groups.push(UdpGroup {
            local,
            wire: handle,
            sockets: vec![UdpMember::new(handle, reuse_addr)],
            sending: None,
            next_sender: 0,
        });
        Ok(local)
    }

    pub fn unbind_udp(&mut self, handle: SocketHandle, local: IpEndpoint) {
        let index = self
            .udp_groups
            .iter()
            .position(|group| group.local == local)
            .unwrap();
        let group = &mut self.udp_groups[index];
        group.sockets.retain(|member| member.handle != handle);
        if group.sending.map(|(sender, _)| sender) == Some(handle) {
            group.sending = None;
        }
        let wire = group.wire;
        if group.sockets.is_empty() {
            self.udp_groups.swap_remove(index);
            if local.addr.is_unspecified() {
                self.iface
                    .device_mut()
                    .track_wildcard_udp(local.port, false);
            }
            self.unreserve(Kind::Udp, local);
            self.remove(wire);
        }
        if wire != handle {
            self.remove(handle);
        }
    }

    fn udp_member(&mut self, local: IpEndpoint, handle: SocketHandle) -> &mut UdpMember {
        self.udp_groups
            .iter_mut()
            .find(|group| group.local == local)
            .unwrap()
            .sockets
            .iter_mut()
            .find(|member| member.handle == handle)
            .unwrap()
    }

    pub fn connect_udp(&mut self, local: IpEndpoint, handle: SocketHandle, peer: IpEndpoint) {
        let member = self.udp_member(local, handle);
        member.peer = Some(peer);
        member.packets.retain(|(_, sender)| *sender == peer);
        member.queued_bytes = member.packets.iter().map(|(data, _)| data.len()).sum();
    }

    pub fn reuse_udp(&mut self, local: IpEndpoint, handle: SocketHandle, reuse_addr: bool) {
        self.udp_member(local, handle).reuse_addr = reuse_addr;
    }

    pub fn udp_receive_limit(&mut self, local: IpEndpoint, handle: SocketHandle, limit: usize) {
        self.udp_member(local, handle).receive_limit = limit;
    }

    pub fn udp_ready(&mut self, local: IpEndpoint, handle: SocketHandle) -> bool {
        !self.udp_member(local, handle).packets.is_empty()
    }

    pub fn udp_generations(&mut self, local: IpEndpoint, handle: SocketHandle) -> (u64, u64) {
        let member = self.udp_member(local, handle);
        (member.in_gen, member.out_gen)
    }

    pub fn udp_writable(
        &mut self,
        local: IpEndpoint,
        handle: SocketHandle,
        send_limit: usize,
    ) -> bool {
        let member = self.udp_member(local, handle);
        member.outgoing.len() < MAX_DATAGRAMS && member.outgoing_charge < send_limit
    }

    pub fn enqueue_datagram(
        &mut self,
        local: IpEndpoint,
        handle: SocketHandle,
        peer: IpEndpoint,
        size: usize,
        send_limit: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<(), isize> {
        let member = self.udp_member(local, handle);
        if member.outgoing.len() == MAX_DATAGRAMS
            || member.outgoing_charge + size.max(1) > send_limit
        {
            return Err(-11); // EAGAIN
        }
        let mut datagram = Datagram {
            payload: vec![0; size],
            peer,
        };
        fill(&mut datagram.payload);
        member.outgoing_charge += datagram.charge();
        member.outgoing.push_back(datagram);
        Ok(())
    }

    pub fn receive_datagram<T>(
        &mut self,
        local: IpEndpoint,
        handle: SocketHandle,
        peek: bool,
        consume: impl FnOnce(&[u8], IpEndpoint) -> T,
    ) -> Option<T> {
        let member = self.udp_member(local, handle);
        let (data, sender) = member.packets.front()?;
        let result = consume(data, *sender);
        if !peek {
            member.queued_bytes -= data.len();
            member.packets.pop_front();
        }
        Some(result)
    }

    pub fn transmit_datagrams(&mut self) {
        for group in &mut self.udp_groups {
            let socket = self.iface.get_socket::<UdpSocket>(group.wire);
            if !socket.can_send() {
                continue;
            }
            // The wire socket has one TX slot. Keep charging a datagram to its
            // descriptor until smoltcp releases that slot.
            if let Some((handle, charge)) = group.sending.take() {
                if let Some(member) = group
                    .sockets
                    .iter_mut()
                    .find(|member| member.handle == handle)
                {
                    member.outgoing_charge -= charge;
                    member.out_gen = member.out_gen.wrapping_add(1);
                }
            }
            let count = group.sockets.len();
            for offset in 0..count {
                let index = (group.next_sender + offset) % count;
                let member = &mut group.sockets[index];
                if let Some(datagram) = member.outgoing.front() {
                    if socket.send_slice(&datagram.payload, datagram.peer).is_ok() {
                        group.sending = Some((member.handle, datagram.charge()));
                        member.outgoing.pop_front();
                        group.next_sender = (index + 1) % count;
                    }
                    break;
                }
            }
        }
    }

    pub fn dispatch_datagrams(&mut self) -> bool {
        let mut delivered = false;
        for group in &mut self.udp_groups {
            let socket = self.iface.get_socket::<UdpSocket>(group.wire);
            while let Ok((data, sender)) = socket.recv() {
                let target = group
                    .sockets
                    .iter()
                    .rposition(|member| member.peer == Some(sender))
                    .or_else(|| {
                        group
                            .sockets
                            .iter()
                            .rposition(|member| member.peer.is_none())
                    });
                if let Some(index) = target {
                    let member = &mut group.sockets[index];
                    if member.packets.len() < MAX_DATAGRAMS
                        && member.queued_bytes + data.len() <= member.receive_limit
                    {
                        member.queued_bytes += data.len();
                        member.packets.push_back((data.to_vec(), sender));
                        member.in_gen = member.in_gen.wrapping_add(1);
                        delivered = true;
                    }
                }
            }
        }
        delivered
    }
}
