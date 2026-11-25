use super::virtio_hal::VirtioHal;
use crate::arch::mmio_address;
use crate::board::{PCI_ECAM, PCI_MMIO_END, PCI_MMIO_START};
use crate::sync::UPSafeCell;
use virtio_pci::transport::{
    pci::{
        bus::{Cam, Command, PciRoot},
        virtio_device_type, PciTransport,
    },
    DeviceType,
};

// The single-hart LoongArch kernel probes devices with interrupts serialized.
static PCI_MMIO_NEXT: UPSafeCell<u64> = unsafe { UPSafeCell::new(PCI_MMIO_START) };

pub(super) fn probe_virtio(device_type: DeviceType) -> Option<PciTransport> {
    probe_virtio_nth(device_type, 0)
}

pub(super) fn probe_virtio_nth(device_type: DeviceType, index: usize) -> Option<PciTransport> {
    let mut next = PCI_MMIO_NEXT.exclusive_access();
    let mut root = unsafe { PciRoot::new(mmio_address(PCI_ECAM) as *mut u8, Cam::Ecam) };
    let (function, _) = root
        .enumerate_bus(0)
        .filter(|(_, info)| virtio_device_type(info) == Some(device_type))
        .nth(index)?;

    // Direct boot has no PCI firmware. Assign memory BARs from the shared
    // MMIO window with decoding disabled and leave legacy I/O decoding off.
    root.set_command(function, Command::empty());
    let mut address = *next;
    let mut index = 0;
    while index < 6 {
        let bar = root.bar_info(function, index).ok()?;
        if let Some((_, size)) = bar.memory_address_size().filter(|&(_, size)| size != 0) {
            if !size.is_power_of_two() {
                return None;
            }
            let size = u64::from(size);
            address = (address + size - 1) & !(size - 1);
            if address + size > PCI_MMIO_END {
                return None;
            }
            if bar.takes_two_entries() {
                root.set_bar_64(function, index, address);
            } else {
                root.set_bar_32(function, index, address as u32);
            }
            address += size;
        }
        index += if bar.takes_two_entries() { 2 } else { 1 };
    }
    root.set_command(
        function,
        Command::MEMORY_SPACE | Command::BUS_MASTER | Command::INTERRUPT_DISABLE,
    );
    match PciTransport::new::<VirtioHal>(&mut root, function) {
        Ok(transport) => {
            *next = address;
            Some(transport)
        }
        Err(error) => {
            root.set_command(function, Command::empty());
            log::warn!("VirtIO PCI {:?} transport: {:?}", device_type, error);
            None
        }
    }
}
