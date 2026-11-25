use super::{get_block_cache, BlockDevice, BLOCK_SZ};
use alloc::sync::Arc;

type BitmapBlock = [u64; 64];

const BLOCK_BITS: usize = BLOCK_SZ * 8;

pub struct Bitmap {
    start_block_id: usize,
    blocks: usize,
}

/// Return (block_pos, bits64_pos, inner_pos)
fn decomposition(mut bit: usize) -> (usize, usize, usize) {
    let block_pos = bit / BLOCK_BITS;
    bit %= BLOCK_BITS;
    (block_pos, bit / 64, bit % 64)
}

impl Bitmap {
    pub fn new(start_block_id: usize, blocks: usize) -> Self {
        Self {
            start_block_id,
            blocks,
        }
    }

    pub fn alloc(&self, block_device: &Arc<dyn BlockDevice>, limit: usize) -> Option<usize> {
        for block_id in 0..self.blocks {
            let first_bit = block_id * BLOCK_BITS;
            if first_bit >= limit {
                break;
            }
            let cache = get_block_cache(block_id + self.start_block_id, Arc::clone(block_device));
            let mut cache = cache.lock();
            let free_bit = cache.read(0, |bitmap_block: &BitmapBlock| {
                bitmap_block
                    .iter()
                    .enumerate()
                    .find(|(_, word)| **word != u64::MAX)
                    .map(|(word, value)| first_bit + word * 64 + value.trailing_ones() as usize)
            });
            if let Some(bit) = free_bit.filter(|&bit| bit < limit) {
                let (_, word, shift) = decomposition(bit);
                cache.modify(0, |bitmap_block: &mut BitmapBlock| {
                    bitmap_block[word] |= 1u64 << shift;
                });
                return Some(bit);
            }
        }
        None
    }

    pub fn dealloc(&self, block_device: &Arc<dyn BlockDevice>, bit: usize) {
        let (block_pos, bits64_pos, inner_pos) = decomposition(bit);
        get_block_cache(block_pos + self.start_block_id, Arc::clone(block_device))
            .lock()
            .modify(0, |bitmap_block: &mut BitmapBlock| {
                assert!(bitmap_block[bits64_pos] & (1u64 << inner_pos) > 0);
                bitmap_block[bits64_pos] -= 1u64 << inner_pos;
            });
    }

    pub fn used(&self, device: &Arc<dyn BlockDevice>, limit: usize) -> usize {
        let mut used = 0;
        for (index, first_bit) in (0..limit).step_by(BLOCK_BITS).enumerate() {
            let valid = (limit - first_bit).min(BLOCK_BITS);
            let block = get_block_cache(self.start_block_id + index, Arc::clone(device));
            used += block.lock().read(0, |words: &BitmapBlock| {
                words
                    .iter()
                    .take((valid + 63) / 64)
                    .enumerate()
                    .map(|(i, word)| {
                        let bits = (valid - i * 64).min(64);
                        (word & (u64::MAX >> (64 - bits))).count_ones() as usize
                    })
                    .sum::<usize>()
            });
        }
        used
    }

    pub fn maximum(&self) -> usize {
        self.blocks * BLOCK_BITS
    }

    pub fn is_set(&self, block_device: &Arc<dyn BlockDevice>, bit: usize) -> bool {
        let (block, word, shift) = decomposition(bit);
        block < self.blocks
            && get_block_cache(block + self.start_block_id, Arc::clone(block_device))
                .lock()
                .read(0, |bitmap_block: &BitmapBlock| {
                    bitmap_block[word] & (1u64 << shift) != 0
                })
    }
}
