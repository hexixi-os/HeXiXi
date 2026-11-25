use crate::drivers::net::VirtioNet;
use alloc::{
    collections::{BTreeSet, VecDeque},
    vec,
    vec::Vec,
};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::time::Instant;
use smoltcp::wire::{
    ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Address,
    Ipv4Packet, UdpPacket,
};

pub(super) const LOOPBACK: Ipv4Address = Ipv4Address([127, 0, 0, 1]);
pub(super) const GUEST: Ipv4Address = Ipv4Address([10, 0, 2, 15]);
pub(super) const GATEWAY: Ipv4Address = Ipv4Address([10, 0, 2, 2]);
const POLL_BUDGET: usize = 32;
const LOCAL_QUEUE_LIMIT: usize = 64;

pub(super) struct EthernetDevice {
    nic: Option<VirtioNet>,
    pub mac: EthernetAddress,
    local: VecDeque<Vec<u8>>,
    wildcard_udp: BTreeSet<u16>,
    rx_budget: usize,
}

impl EthernetDevice {
    pub fn new() -> Self {
        let nic = VirtioNet::probe();
        let mac = EthernetAddress(
            nic.as_ref()
                .map_or([2, 0, 0, 0, 0, 1], VirtioNet::mac_address),
        );
        Self {
            nic,
            mac,
            local: VecDeque::new(),
            wildcard_udp: BTreeSet::new(),
            rx_budget: 0,
        }
    }

    pub fn has_nic(&self) -> bool {
        self.nic.is_some()
    }

    pub fn begin_poll(&mut self) {
        self.rx_budget = POLL_BUDGET;
    }

    pub fn track_wildcard_udp(&mut self, port: u16, bound: bool) {
        if bound {
            self.wildcard_udp.insert(port);
        } else {
            self.wildcard_udp.remove(&port);
        }
    }

    fn select_udp_loopback_source(&self, bytes: &mut [u8]) {
        let mut frame = match EthernetFrame::new_checked(bytes) {
            Ok(frame) => frame,
            Err(_) => return,
        };
        if frame.ethertype() != EthernetProtocol::Ipv4 {
            return;
        }
        let mut ip = match Ipv4Packet::new_checked(frame.payload_mut()) {
            Ok(ip) => ip,
            Err(_) => return,
        };
        if ip.protocol() != IpProtocol::Udp || ip.dst_addr() != LOOPBACK || ip.src_addr() != GUEST {
            return;
        }
        let mut udp = match UdpPacket::new_checked(ip.payload_mut()) {
            Ok(udp) => udp,
            Err(_) => return,
        };
        if self.wildcard_udp.contains(&udp.src_port()) {
            // smoltcp 0.8 uses the first assigned IPv4 address for wildcard UDP.
            udp.fill_checksum(&LOOPBACK.into(), &LOOPBACK.into());
            ip.set_src_addr(LOOPBACK);
            ip.fill_checksum();
        }
    }

    fn from_wire(bytes: &[u8]) -> bool {
        let frame = match EthernetFrame::new_checked(bytes) {
            Ok(frame) => frame,
            Err(_) => return false,
        };
        match frame.ethertype() {
            EthernetProtocol::Ipv4 => Ipv4Packet::new_checked(frame.payload())
                .map_or(false, |ip| {
                    ip.src_addr().0[0] != 127 && ip.dst_addr().0[0] != 127
                }),
            EthernetProtocol::Arp => {
                match ArpPacket::new_checked(frame.payload())
                    .and_then(|packet| ArpRepr::parse(&packet))
                {
                    Ok(ArpRepr::EthernetIpv4 {
                        source_protocol_addr,
                        target_protocol_addr,
                        ..
                    }) => source_protocol_addr.0[0] != 127 && target_protocol_addr.0[0] != 127,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    fn is_local(&self, bytes: &[u8]) -> bool {
        let frame = match EthernetFrame::new_checked(bytes) {
            Ok(frame) => frame,
            Err(_) => return false,
        };
        if frame.dst_addr() == self.mac {
            return true;
        }
        if frame.ethertype() != EthernetProtocol::Arp {
            return false;
        }
        // Feed requests for our own IPs back through smoltcp's ARP responder.
        match ArpPacket::new_checked(frame.payload()).and_then(|packet| ArpRepr::parse(&packet)) {
            Ok(ArpRepr::EthernetIpv4 {
                target_protocol_addr,
                ..
            }) => {
                target_protocol_addr == LOOPBACK
                    || (target_protocol_addr == GUEST && self.has_nic())
            }
            _ => false,
        }
    }
}

impl<'a> Device<'a> for EthernetDevice {
    type RxToken = Receive;
    type TxToken = Transmit<'a>;

    fn receive(&'a mut self) -> Option<(Receive, Transmit<'a>)> {
        if let Some(bytes) = self.local.pop_front() {
            return Some((Receive(bytes), Transmit(self)));
        }
        while self.rx_budget != 0 {
            let nic = self.nic.as_mut()?;
            // smoltcp can send an immediate reply through the token for this frame.
            if !nic.can_send() {
                return None;
            }
            let bytes = nic.receive()?;
            self.rx_budget -= 1;
            if Self::from_wire(&bytes) {
                return Some((Receive(bytes), Transmit(self)));
            }
        }
        None
    }

    fn transmit(&'a mut self) -> Option<Transmit<'a>> {
        Some(Transmit(self))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1514;
        caps.max_burst_size = Some(POLL_BUDGET);
        caps
    }
}

pub(super) struct Receive(Vec<u8>);

impl RxToken for Receive {
    fn consume<R, F>(mut self, _now: Instant, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        f(&mut self.0)
    }
}

pub(super) struct Transmit<'a>(&'a mut EthernetDevice);

impl TxToken for Transmit<'_> {
    fn consume<R, F>(self, _now: Instant, len: usize, f: F) -> smoltcp::Result<R>
    where
        F: FnOnce(&mut [u8]) -> smoltcp::Result<R>,
    {
        let mut bytes = vec![0; len];
        let result = f(&mut bytes)?;
        let device = self.0;
        if device.is_local(&bytes) {
            device.select_udp_loopback_source(&mut bytes);
            if device.local.len() == LOCAL_QUEUE_LIMIT {
                return Err(smoltcp::Error::Exhausted);
            }
            device.local.push_back(bytes);
        } else if !device.nic.as_mut().map_or(false, |nic| nic.send(&bytes)) {
            return Err(smoltcp::Error::Exhausted);
        }
        Ok(result)
    }
}
