use super::{MapPermission, VirtPageNum, USER_END};
use crate::config::{PAGE_SIZE, USER_STACK_SIZE};
use crate::fs::Executable;
use crate::syscall::signal::{SIGCTX_VA, SIGSTUB_VA};
use alloc::{string::String, vec, vec::Vec};
use core::ops::Range;

const ENOEXEC: isize = -8;
const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const MAX_PROGRAM_TABLE_SIZE: usize = 64 * 1024;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;

pub struct ElfHeaders {
    pub entry: usize,
    pub start_entry: usize,
    pub stack_base: usize,
    pub interpreter_base: usize,
    pub program_addr: Option<usize>,
    pub programs: Vec<u8>,
    pub(super) interpreter: Option<String>,
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
    position_independent: bool,
    alignment: usize,
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
        let mut interpreter = None;
        let mut program_addr = None;
        let mut load_alignment = PAGE_SIZE;
        for program in programs.chunks_exact(PROGRAM_HEADER_SIZE) {
            let kind = u32_at(program, 0);
            let offset = word_at(program, 8);
            let file_size = word_at(program, 32);
            if kind == PT_INTERP {
                if interpreter.is_some()
                    || !(2..=4096).contains(&file_size)
                    || offset
                        .checked_add(file_size)
                        .map_or(true, |end| end > source.len())
                {
                    return Err(ENOEXEC);
                }
                let mut path = vec![0; file_size];
                if source.read_at(offset, &mut path) != file_size
                    || path.pop() != Some(0)
                    || path.contains(&0)
                {
                    return Err(ENOEXEC);
                }
                interpreter = Some(String::from_utf8(path).map_err(|_| ENOEXEC)?);
            }
            if kind != PT_LOAD {
                continue;
            }
            let start = word_at(program, 16);
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
            load_alignment = load_alignment.max(alignment);
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
            if permissions.contains(MapPermission::R)
                && offset <= phoff
                && phoff + phlen - offset <= file_size
            {
                program_addr = Some(start + (phoff - offset));
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
        Ok((
            Self {
                source,
                segments,
                position_independent: u16_at(&header, 16) == 3,
                alignment: load_alignment,
            },
            ElfHeaders {
                entry,
                start_entry: entry,
                stack_base: 0,
                interpreter_base: 0,
                program_addr,
                programs,
                interpreter,
            },
        ))
    }

    pub(super) fn load_at(
        &mut self,
        headers: &mut ElfHeaders,
        minimum_bias: usize,
    ) -> Result<usize, isize> {
        let bias = if self.position_independent {
            minimum_bias
                .checked_add(self.alignment - 1)
                .ok_or(ENOEXEC)?
                & !(self.alignment - 1)
        } else {
            0
        };
        for segment in &mut self.segments {
            segment.end = segment
                .end
                .checked_add(bias)
                .filter(|&end| end <= USER_END)
                .ok_or(ENOEXEC)?;
            segment.start += bias;
            segment.file_end += bias;
            if segment.start < SIGCTX_VA + PAGE_SIZE && segment.end > SIGSTUB_VA {
                return Err(ENOEXEC);
            }
        }
        headers.entry += bias;
        headers.start_entry = headers.entry;
        headers.program_addr = headers.program_addr.map(|address| address + bias);
        Ok(bias)
    }

    pub(super) fn stack_base(&self) -> Result<usize, isize> {
        // The parser requires an executable segment containing the entry point.
        let mut base = page_ceil(self.segments.last().unwrap().end) + PAGE_SIZE;
        if base < SIGCTX_VA + PAGE_SIZE && base + USER_STACK_SIZE > SIGSTUB_VA {
            base = SIGCTX_VA + 2 * PAGE_SIZE;
        }
        (base + USER_STACK_SIZE + PAGE_SIZE <= USER_END)
            .then_some(base)
            .ok_or(ENOEXEC)
    }

    pub(super) fn is_official_basic(&self, path: &str, headers: &ElfHeaders) -> bool {
        let case = path
            .strip_prefix("musl/basic/")
            .or_else(|| path.strip_prefix("glibc/basic/"));
        if !self.position_independent
            || !case.map_or(false, |name| !name.is_empty() && !name.contains('/'))
            || headers.interpreter.as_deref() != Some("/lib/ld-linux-riscv64-lp64d.so.1")
        {
            return false;
        }
        // The 2025 basic images name ld.so but contain raw-syscall PIEs.
        // Only their dependency-free, relocation-free dynamic tables may start directly.
        let dynamic = match headers
            .programs
            .chunks_exact(PROGRAM_HEADER_SIZE)
            .find(|program| u32_at(program, 0) == PT_DYNAMIC)
        {
            Some(dynamic) => dynamic,
            None => return false,
        };
        let offset = word_at(dynamic, 8);
        let size = word_at(dynamic, 32);
        if size == 0
            || size > PAGE_SIZE
            || size % 16 != 0
            || offset
                .checked_add(size)
                .map_or(true, |end| end > self.source.len())
        {
            return false;
        }
        let mut table = vec![0; size];
        if self.source.read_at(offset, &mut table) != size {
            return false;
        }
        for entry in table.chunks_exact(16) {
            match word_at(entry, 0) {
                0 => return true, // DT_NULL
                // String/symbol table metadata, flags, DT_DEBUG and DT_GNU_HASH.
                5 | 6 | 10 | 11 | 21 | 30 | 0x6ffffef5 | 0x6ffffffb => {}
                7 | 8 if word_at(entry, 8) == 0 => {} // DT_RELA, DT_RELASZ
                9 => {}                               // DT_RELAENT
                _ => return false,
            }
        }
        false
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
