mod queue;

use super::virtio_hal::VirtioHal;
use crate::config::PAGE_SIZE;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::mem::ManuallyDrop;
use queue::{NetQueue, BUFFER_SIZE, QUEUE_SIZE};
use virtio_pci::{
    transport::{DeviceStatus, DeviceType, Transport},
    Error,
};

#[cfg(target_arch = "riscv64")]
use virtio_pci::transport::mmio::MmioTransport as NetTransport;
#[cfg(target_arch = "loongarch64")]
use virtio_pci::transport::pci::PciTransport as NetTransport;

const RX_QUEUE: u16 = 0;
const TX_QUEUE: u16 = 1;
const FEATURE_MAC: u64 = 1 << 5;
const FEATURE_VERSION_1: u64 = 1 << 32;
const LEGACY_HEADER_SIZE: usize = 10;
const MODERN_HEADER_SIZE: usize = 12;

pub struct VirtioNet {
    // The library's PCI destructor waits indefinitely for reset. Use our own bounded reset.
    transport: ManuallyDrop<NetTransport>,
    mac: [u8; 6],
    header_size: usize,
    rx: ManuallyDrop<NetQueue>,
    tx: ManuallyDrop<NetQueue>,
}

impl VirtioNet {
    pub fn probe() -> Option<Self> {
        let transport = probe_transport()?;
        match Self::new(transport) {
            Ok(driver) => Some(driver),
            Err(error) => {
                log::warn!("VirtIO network initialization failed: {:?}", error);
                None
            }
        }
    }

    fn new(transport: NetTransport) -> Result<Self, Error> {
        let mut transport = ManuallyDrop::new(transport);
        if !reset(&mut transport) {
            return Err(Error::NotReady);
        }
        transport.set_status(DeviceStatus::ACKNOWLEDGE);
        let mut status = DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER;
        transport.set_status(status);

        let legacy = transport.requires_legacy_layout();
        let features = FEATURE_MAC | if legacy { 0 } else { FEATURE_VERSION_1 };
        if transport.read_device_features() & features != features {
            transport.set_status(status | DeviceStatus::FAILED);
            return Err(Error::Unsupported);
        }
        transport.write_driver_features(features);
        if !legacy {
            status |= DeviceStatus::FEATURES_OK;
            transport.set_status(status);
            if !transport.get_status().contains(DeviceStatus::FEATURES_OK) {
                transport.set_status(status | DeviceStatus::FAILED);
                return Err(Error::Unsupported);
            }
        }

        let mac = transport.config_space::<[u8; 6]>()?;
        let mac = unsafe { mac.as_ptr().read_volatile() };
        transport.set_guest_page_size(PAGE_SIZE as u32);
        for queue in [RX_QUEUE, TX_QUEUE] {
            if transport.queue_used(queue) || transport.max_queue_size(queue) < QUEUE_SIZE as u32 {
                transport.set_status(status | DeviceStatus::FAILED);
                return Err(Error::InvalidParam);
            }
        }

        let mut rx = NetQueue::new();
        let tx = NetQueue::new();
        rx.configure(&mut *transport, RX_QUEUE);
        tx.configure(&mut *transport, TX_QUEUE);
        for index in 0..QUEUE_SIZE {
            rx.submit(index, BUFFER_SIZE, true);
        }
        transport.set_status(status | DeviceStatus::DRIVER_OK);
        transport.notify(RX_QUEUE);

        Ok(Self {
            transport,
            mac,
            header_size: if legacy {
                LEGACY_HEADER_SIZE
            } else {
                MODERN_HEADER_SIZE
            },
            rx: ManuallyDrop::new(rx),
            tx: ManuallyDrop::new(tx),
        })
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    pub fn receive(&mut self) -> Option<Vec<u8>> {
        self.transport.ack_interrupt();
        let (index, length) = self.rx.complete()?;
        let frame = (self.header_size..=BUFFER_SIZE)
            .contains(&length)
            .then(|| self.rx.buffer(index)[self.header_size..length].to_vec());
        self.rx.submit(index, BUFFER_SIZE, true);
        self.transport.notify(RX_QUEUE);
        frame
    }

    pub fn can_send(&mut self) -> bool {
        self.reap_transmit();
        self.tx.free_slot().is_some()
    }

    /// Queues an Ethernet frame and returns false if no transmit buffer is free.
    pub fn send(&mut self, bytes: &[u8]) -> bool {
        if bytes.len() > BUFFER_SIZE - self.header_size {
            return false;
        }
        self.reap_transmit();
        let index = match self.tx.free_slot() {
            Some(index) => index,
            None => return false,
        };
        let length = self.header_size + bytes.len();
        let buffer = self.tx.buffer(index);
        buffer[..self.header_size].fill(0);
        buffer[self.header_size..length].copy_from_slice(bytes);
        self.tx.submit(index, length, false);
        self.transport.notify(TX_QUEUE);
        true
    }

    fn reap_transmit(&mut self) {
        for _ in 0..QUEUE_SIZE {
            if self.tx.complete().is_none() {
                break;
            }
        }
    }
}

impl Drop for VirtioNet {
    fn drop(&mut self) {
        if reset(&mut self.transport) {
            // Reset releases device ownership of every posted RX and in-flight TX buffer.
            unsafe {
                ManuallyDrop::drop(&mut self.rx);
                ManuallyDrop::drop(&mut self.tx);
            }
        } else {
            log::warn!("VirtIO network reset timed out; retaining DMA memory");
        }
    }
}

fn reset(transport: &mut NetTransport) -> bool {
    transport.set_status(DeviceStatus::empty());
    for _ in 0..1024 {
        if transport.get_status().is_empty() {
            return true;
        }
        spin_loop();
    }
    false
}

#[cfg(target_arch = "riscv64")]
fn probe_transport() -> Option<NetTransport> {
    use crate::board::{VIRTIO_MMIO_BASE, VIRTIO_MMIO_SLOTS, VIRTIO_MMIO_SLOT_SIZE};
    use core::ptr::NonNull;
    use virtio_pci::transport::mmio::VirtIOHeader;

    const DEVICE_ID_OFFSET: usize = 0x008;

    (0..VIRTIO_MMIO_SLOTS).find_map(|slot| {
        let address = VIRTIO_MMIO_BASE + slot * VIRTIO_MMIO_SLOT_SIZE;
        // Dropping a transport resets its device. Only construct one for a NIC.
        let device_id = unsafe { ((address + DEVICE_ID_OFFSET) as *const u32).read_volatile() };
        if device_id != DeviceType::Network as u32 {
            return None;
        }
        let header = NonNull::new(address as *mut VirtIOHeader)?;
        unsafe { NetTransport::new(header) }.ok()
    })
}

#[cfg(target_arch = "loongarch64")]
fn probe_transport() -> Option<NetTransport> {
    super::pci::probe_virtio(DeviceType::Network)
}
