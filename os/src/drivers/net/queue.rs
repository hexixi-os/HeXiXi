use super::VirtioHal;
use crate::config::PAGE_SIZE;
use core::mem::{align_of, size_of};
use core::ptr::{addr_of, addr_of_mut, NonNull};
use core::sync::atomic::{fence, Ordering};
use virtio_pci::{transport::Transport, BufferDirection, Hal};

pub(super) const QUEUE_SIZE: usize = 16;
pub(super) const BUFFER_SIZE: usize = 2048;
const DESCRIPTOR_WRITE: u16 = 2;
const NO_INTERRUPT: u16 = 1;

#[repr(C)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct AvailableRing {
    flags: u16,
    index: u16,
    ring: [u16; QUEUE_SIZE],
    used_event: u16,
}

#[repr(C)]
struct UsedEntry {
    id: u32,
    length: u32,
}

// Putting the used ring on a separate page satisfies the legacy MMIO layout.
#[repr(C, align(4096))]
struct UsedRing {
    flags: u16,
    index: u16,
    ring: [UsedEntry; QUEUE_SIZE],
    available_event: u16,
}

#[repr(C)]
struct QueueMemory {
    descriptors: [Descriptor; QUEUE_SIZE],
    available: AvailableRing,
    used: UsedRing,
    buffers: [[u8; BUFFER_SIZE]; QUEUE_SIZE],
}

const _: [(); PAGE_SIZE] = [(); align_of::<UsedRing>()];
const AVAILABLE_OFFSET: usize = size_of::<[Descriptor; QUEUE_SIZE]>();
const AVAILABLE_END: usize = AVAILABLE_OFFSET + size_of::<AvailableRing>();
const USED_OFFSET: usize = (AVAILABLE_END + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
const BUFFER_OFFSET: usize = USED_OFFSET + size_of::<UsedRing>();
const QUEUE_PAGES: usize = size_of::<QueueMemory>() / PAGE_SIZE;

pub(super) struct NetQueue {
    physical: usize,
    memory: NonNull<QueueMemory>,
    available_index: u16,
    used_index: u16,
    in_flight: [bool; QUEUE_SIZE],
    failed: bool,
}

impl NetQueue {
    pub(super) fn new() -> Self {
        let (physical, memory) = VirtioHal::dma_alloc(QUEUE_PAGES, BufferDirection::Both);
        Self {
            physical,
            memory: memory.cast(),
            available_index: 0,
            used_index: 0,
            in_flight: [false; QUEUE_SIZE],
            failed: false,
        }
    }

    pub(super) fn configure(&self, transport: &mut impl Transport, index: u16) {
        // The driver polls both queues and doesn't negotiate event-index notifications.
        unsafe {
            addr_of_mut!((*self.memory.as_ptr()).available.flags)
                .write_volatile(NO_INTERRUPT.to_le());
        }
        fence(Ordering::SeqCst);
        transport.queue_set(
            index,
            QUEUE_SIZE as u32,
            self.physical,
            self.physical + AVAILABLE_OFFSET,
            self.physical + USED_OFFSET,
        );
    }

    pub(super) fn buffer(&mut self, index: usize) -> &mut [u8; BUFFER_SIZE] {
        assert!(!self.in_flight[index]);
        // The device owns only submitted buffers. The mutable borrow is limited to this queue.
        unsafe { &mut *addr_of_mut!((*self.memory.as_ptr()).buffers[index]) }
    }

    pub(super) fn free_slot(&self) -> Option<usize> {
        if self.failed {
            return None;
        }
        self.in_flight.iter().position(|&in_flight| !in_flight)
    }

    pub(super) fn submit(&mut self, index: usize, length: usize, device_writes: bool) {
        debug_assert!(!self.in_flight[index] && length <= BUFFER_SIZE);
        self.in_flight[index] = true;
        let memory = self.memory.as_ptr();
        let slot = self.available_index as usize % QUEUE_SIZE;
        let address = self.physical + BUFFER_OFFSET + index * BUFFER_SIZE;
        let flags = if device_writes { DESCRIPTOR_WRITE } else { 0 };
        unsafe {
            addr_of_mut!((*memory).descriptors[index]).write_volatile(Descriptor {
                address: (address as u64).to_le(),
                length: (length as u32).to_le(),
                flags: flags.to_le(),
                next: 0,
            });
            addr_of_mut!((*memory).available.ring[slot]).write_volatile((index as u16).to_le());
            fence(Ordering::SeqCst);
            self.available_index = self.available_index.wrapping_add(1);
            addr_of_mut!((*memory).available.index).write_volatile(self.available_index.to_le());
        }
        fence(Ordering::SeqCst);
    }

    pub(super) fn complete(&mut self) -> Option<(usize, usize)> {
        if self.failed {
            return None;
        }
        let memory = self.memory.as_ptr();
        let device_index = unsafe { addr_of!((*memory).used.index).read_volatile() };
        let pending = u16::from_le(device_index).wrapping_sub(self.used_index) as usize;
        if pending == 0 {
            return None;
        }
        if pending > QUEUE_SIZE {
            self.failed = true;
            log::warn!("VirtIO network device overran the used ring");
            return None;
        }

        fence(Ordering::SeqCst);
        let slot = self.used_index as usize % QUEUE_SIZE;
        let used = unsafe { addr_of!((*memory).used.ring[slot]).read_volatile() };
        let index = u32::from_le(used.id) as usize;
        if !self.in_flight.get(index).copied().unwrap_or(false) {
            self.failed = true;
            log::warn!("VirtIO network device completed an inactive descriptor");
            return None;
        }
        self.used_index = self.used_index.wrapping_add(1);
        self.in_flight[index] = false;
        Some((index, u32::from_le(used.length) as usize))
    }
}

impl Drop for NetQueue {
    fn drop(&mut self) {
        // VirtioNet only drops a configured queue after the device acknowledges reset.
        unsafe { VirtioHal::dma_dealloc(self.physical, self.memory.cast(), QUEUE_PAGES) };
    }
}
