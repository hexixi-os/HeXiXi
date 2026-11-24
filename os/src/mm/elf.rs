use super::{MapPermission, VirtPageNum, USER_END};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::Executable;
use crate::syscall::signal::{SIGCTX_VA, SIGSTUB_VA};
use alloc::{vec, vec::Vec};
use core::ops::Range;

const ENOEXEC: isize = -8;
const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const MAX_PROGRAM_TABLE_SIZE: usize = 64 * 1024;
const PT_LOAD: u32 = 1;

pub struct ElfHeaders {
    pub entry: usize,
    pub stack_base: usize,
    pub programs: Vec<u8>,
}

#[derive(Clone)]
struct LoadSegment {
    start: usize,
    end: usize,
    file_end: usize,
    offset: usize,
    permissions: MapPermission,
}

impl LoadSegment {
    fn pages(&self) -> Range<VirtPageNum> {
        VirtPageNum(self.start / PAGE_SIZE)..VirtPageNum(page_ceil(self.end) / PAGE_SIZE)
    }
}

pub struct ElfImage {
    source: Executable,
    segments: Vec<LoadSegment>,
}

impl ElfImage {
    pub(super) fn parse(source: Executable) -> Result<(Self, ElfHeaders), isize> {
        let mut header = [0; ELF_HEADER_SIZE];
        if source.read_at(0, &mut header) != header.len()
            || header[..7] != *b"\x7fELF\x02\x01\x01"
            || !matches!(u16_at(&header, 16), 2 | 3) // ET_EXEC or ET_DYN
            || u16_at(&header, 18) != 243 // EM_RISCV
            || u32_at(&header, 20) != 1
            || u16_at(&header, 52) as usize != ELF_HEADER_SIZE
            || u16_at(&header, 54) as usize != PROGRAM_HEADER_SIZE
        {
            return Err(ENOEXEC);
        }
        let entry = word_at(&header, 24);
        let phoff = word_at(&header, 32);
        let phlen = u16_at(&header, 56) as usize * PROGRAM_HEADER_SIZE;
        if phoff < ELF_HEADER_SIZE
            || phlen == 0
            || phlen > MAX_PROGRAM_TABLE_SIZE
            || phoff
                .checked_add(phlen)
                .filter(|&end| end <= source.len())
                .is_none()
        {
            return Err(ENOEXEC);
        }
        let mut programs = vec![0; phlen];
        if source.read_at(phoff, &mut programs) != phlen {
            return Err(ENOEXEC);
        }

        let mut segments = Vec::new();
        for program in programs.chunks_exact(PROGRAM_HEADER_SIZE) {
            // Keep direct entry for the official musl binaries, which carry PT_INTERP.
            if u32_at(program, 0) != PT_LOAD {
                continue;
            }
            let offset = word_at(program, 8);
            let start = word_at(program, 16);
            let file_size = word_at(program, 32);
            let mem_size = word_at(program, 40);
            let alignment = word_at(program, 48);
            if file_size > mem_size {
                return Err(ENOEXEC);
            }
            if mem_size == 0 {
                continue;
            }
            let end = start
                .checked_add(mem_size)
                .filter(|&end| end <= USER_END)
                .ok_or(ENOEXEC)?;
            if file_size != 0
                && (start % PAGE_SIZE != offset % PAGE_SIZE
                    || offset
                        .checked_add(file_size)
                        .filter(|&end| end <= source.len())
                        .is_none())
            {
                return Err(ENOEXEC);
            }
            if alignment > 1
                && (!alignment.is_power_of_two() || start % alignment != offset % alignment)
            {
                return Err(ENOEXEC);
            }
            if start < SIGCTX_VA + PAGE_SIZE && end > SIGSTUB_VA {
                return Err(ENOEXEC);
            }

            let flags = u32_at(program, 4);
            let mut permissions = MapPermission::U;
            if flags & 6 != 0 {
                permissions |= MapPermission::R; // Sv39 needs R on writable pages.
            }
            if flags & 2 != 0 {
                permissions |= MapPermission::W;
            }
            if flags & 1 != 0 {
                permissions |= MapPermission::X;
            }
            segments.push(LoadSegment {
                start,
                end,
                file_end: start + file_size,
                offset,
                permissions,
            });
        }
        segments.sort_unstable_by_key(|segment| segment.start);
        if segments.windows(2).any(|pair| pair[0].end > pair[1].start)
            || !segments.iter().any(|segment| {
                segment.start <= entry
                    && entry < segment.end
                    && segment.permissions.contains(MapPermission::X)
            })
        {
            return Err(ENOEXEC);
        }
        // A distant data segment can force the stack past the fixed signal pages.
        let mut stack_base = page_ceil(segments.last().ok_or(ENOEXEC)?.end) + PAGE_SIZE;
        if stack_base < SIGCTX_VA + PAGE_SIZE && stack_base + USER_STACK_SIZE > SIGSTUB_VA {
            stack_base = SIGCTX_VA + 2 * PAGE_SIZE;
        }
        if stack_base + USER_STACK_SIZE + PAGE_SIZE > USER_END {
            return Err(ENOEXEC);
        }

        Ok((
            Self { source, segments },
            ElfHeaders {
                entry,
                stack_base,
                programs,
            },
        ))
    }

    pub(super) fn areas(&self) -> Vec<(Range<VirtPageNum>, MapPermission)> {
        let mut boundaries = Vec::with_capacity(self.segments.len() * 2);
        for segment in &self.segments {
            let pages = segment.pages();
            boundaries.extend([pages.start, pages.end]);
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        boundaries
            .windows(2)
            .filter_map(|pair| {
                let range = pair[0]..pair[1];
                let mut permissions = MapPermission::empty();
                for segment in &self.segments {
                    let pages = segment.pages();
                    if pages.start < range.end && pages.end > range.start {
                        permissions |= segment.permissions;
                    }
                }
                (!permissions.is_empty()).then_some((range, permissions))
            })
            .collect()
    }

    /// Yield initialized bytes for a page. Its other bytes remain zero, including partial BSS.
    pub(super) fn page_parts(
        &self,
        address: usize,
    ) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
        self.segments.iter().filter_map(move |segment| {
            let start = address.max(segment.start);
            let end = (address + PAGE_SIZE).min(segment.file_end);
            (start < end).then(|| {
                (
                    start - address,
                    segment.offset + (start - segment.start),
                    end - start,
                )
            })
        })
    }

    pub(super) fn source(&self) -> &Executable {
        &self.source
    }
}

impl ElfHeaders {
    pub fn program_entry_size(&self) -> usize {
        PROGRAM_HEADER_SIZE
    }

    pub fn program_count(&self) -> usize {
        self.programs.len() / PROGRAM_HEADER_SIZE
    }
}

fn page_ceil(address: usize) -> usize {
    (address + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn word_at(bytes: &[u8], offset: usize) -> usize {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap()) as usize
}
