use super::{File, DEV_DEVICE};
use crate::mm::{copy_live_bytes, UserBuffer};
use core::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_u64() -> u64 {
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let time = crate::timer::get_time_ns() as u64;
    let hart = crate::arch::hart_id() as u64;
    let mut z = seq
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(time.rotate_left(17) ^ hart.wrapping_mul(0x517c_c1b7_2722_0a95));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

pub fn fill_random(buf: &UserBuffer) -> usize {
    let mut block = [0u8; 64];
    let mut total = 0;
    for slice in &buf.buffers {
        let len = slice.len();
        let mut offset = 0;
        while offset < len {
            let count = (len - offset).min(block.len());
            for chunk in block[..count].chunks_mut(8) {
                let bytes = next_u64().to_le_bytes();
                chunk.copy_from_slice(&bytes[..chunk.len()]);
            }
            unsafe {
                copy_live_bytes(block.as_ptr(), slice.as_mut_ptr().add(offset), count);
            }
            offset += count;
        }
        total += len;
    }
    total
}

pub struct RandomFile(pub u64);

impl File for RandomFile {
    fn mode(&self) -> u16 {
        0o20666
    }

    fn device_numbers(&self) -> Option<(u32, u32)> {
        Some((1, (self.0 - 298) as u32))
    }

    fn stat_identity(&self) -> Option<(u64, u64)> {
        Some((DEV_DEVICE, self.0))
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read(&self, buf: UserBuffer) -> usize {
        fill_random(&buf)
    }

    fn write(&self, buf: UserBuffer) -> isize {
        let _ = COUNTER.fetch_add((buf.len() as u64) | 1, Ordering::Relaxed);
        buf.len() as isize
    }

    fn seek(&self, _offset: isize) -> isize {
        0
    }

    fn seek_to(&self, _pos: isize) -> isize {
        0
    }
}
