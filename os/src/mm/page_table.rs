use super::user_access::{copy_live_bytes, zero_live_bytes, UserValue};
use super::{
    frame_alloc, FrameTracker, MapPermission, PageFault, PhysAddr, PhysPageNum, VirtAddr,
    VirtPageNum, USER_END,
};
use crate::config::PAGE_SIZE;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    pub struct PTEFlags: u8 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
pub struct PageTableEntry {
    pub bits: usize,
}

impl PageTableEntry {
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: crate::arch::pte(ppn.0, flags.bits()),
        }
    }
    fn directory(ppn: PhysPageNum) -> Self {
        Self { bits: crate::arch::directory_entry(ppn.0) }
    }
    fn publish_directory(&mut self, ppn: PhysPageNum) {
        // A hardware walker may follow the pointer as soon as it becomes valid.
        #[cfg(feature = "smp")]
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        *self = Self::directory(ppn);
    }
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    pub fn ppn(&self) -> PhysPageNum {
        crate::arch::pte_ppn(self.bits).into()
    }
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(crate::arch::pte_flags(self.bits)).unwrap()
    }
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

/// Assume that it won't oom when creating/mapping.
impl PageTable {
    pub(super) fn try_new() -> Option<Self> {
        let frame = frame_alloc()?;
        Some(Self {
            root_ppn: frame.ppn,
            frames: vec![frame],
        })
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(crate::arch::token_to_root(satp)),
            frames: Vec::new(),
        }
    }
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let [top, middle, leaf] = vpn.indexes();
        let top_entry = &mut self.root_ppn.get_pte_array()[top];
        if !top_entry.is_valid() {
            // Allocate the full branch before publishing it. A failed allocation
            // can then free its frames without involving remote page-table walkers.
            let middle_frame = frame_alloc()?;
            let leaf_frame = frame_alloc()?;
            middle_frame.ppn.get_pte_array()[middle] = PageTableEntry::directory(leaf_frame.ppn);
            top_entry.publish_directory(middle_frame.ppn);
            self.frames.push(middle_frame);
            self.frames.push(leaf_frame);
        }
        let middle_entry = &mut top_entry.ppn().get_pte_array()[middle];
        if !middle_entry.is_valid() {
            let leaf_frame = frame_alloc()?;
            middle_entry.publish_directory(leaf_frame.ppn);
            self.frames.push(leaf_frame);
        }
        Some(&mut middle_entry.ppn().get_pte_array()[leaf])
    }
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }
    pub fn try_map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) -> bool {
        let pte = match self.find_pte_create(vpn) {
            Some(pte) => pte,
            None => return false,
        };
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
        true
    }
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }
    pub(super) fn remap(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid());
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }
    /// Unlink empty non-root tables. The caller holds their frames until after the TLB flush.
    pub fn unmap_and_reclaim(&mut self, vpn: VirtPageNum, retired: &mut Vec<FrameTracker>) {
        let indexes = vpn.indexes();
        let middle = self.root_ppn.get_pte_array()[indexes[0]].ppn();
        let leaf = middle.get_pte_array()[indexes[1]].ppn();
        self.unmap(vpn);
        for (parent, index, child) in [(middle, indexes[1], leaf), (self.root_ppn, indexes[0], middle)] {
            if child.get_pte_array().iter().any(PageTableEntry::is_valid) {
                break;
            }
            parent.get_pte_array()[index] = PageTableEntry::empty();
            let index = self.frames.iter().position(|frame| frame.ppn == child).unwrap();
            retired.push(self.frames.swap_remove(index));
        }
    }
    /// 改一个已映射页的权限位（mprotect 用，保留原 ppn）
    pub fn set_flags(&mut self, vpn: VirtPageNum, flags: PTEFlags) {
        if let Some(pte) = self.find_pte(vpn) {
            if pte.is_valid() {
                let ppn = pte.ppn();
                *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
            }
        }
    }

    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.find_pte(va.clone().floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    pub fn token(&self) -> usize {
        crate::arch::root_to_token(self.root_ppn.0)
    }
}

/// 旧标量翻译接口的兜底页，使无效地址的访问不触及进程映射或其他内核数据。
#[repr(C, align(4096))]
struct DummyUserPage([u8; PAGE_SIZE]);

static mut DUMMY_USER_PAGE: DummyUserPage = DummyUserPage([0; PAGE_SIZE]);

fn dummy_user_page() -> *mut u8 {
    #[allow(unused_unsafe)]
    unsafe { core::ptr::addr_of_mut!(DUMMY_USER_PAGE) as *mut u8 }
}

/// 物理地址是否像“正常的用户页帧”：落在 [ekernel, ram_end) 内。
/// 之前 getcwd01 的内核 panic 就是 translate 出来 0x80201xxx（内核 .text 区）直接写崩的。
fn phys_addr_sane(pa: usize) -> bool {
    extern "C" {
        fn ekernel();
    }
    let start = PhysAddr::from(ekernel as usize).0;
    let end = crate::mm::ram_end();
    pa >= start && pa < end
}

/// 发现异常页表项时打印一次现场（诊断用），然后调用方退回哑页
fn dummy_warned() -> bool {
    use core::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    WARNED.swap(true, Ordering::Relaxed)
}

fn report_bad_translation(token: usize, va: usize, pa: usize) {
    println!(
        "[bad-pte] pid={} va={:#x} pa={:#x} root_ppn={:#x} token={:#x} last_sys={}",
        crate::task::current_task()
            .map(|t| t.process.upgrade().map(|p| p.getpid()).unwrap_or(0))
            .unwrap_or(0),
        va,
        pa,
        crate::arch::token_to_root(token),
        token,
        crate::syscall::LAST_SYSCALL.load(core::sync::atomic::Ordering::Relaxed)
    );
}

fn checked_phys_addr(token: usize, va: usize, pa: PhysAddr) -> Option<PhysAddr> {
    if phys_addr_sane(pa.0) {
        Some(pa)
    } else {
        if !dummy_warned() {
            report_bad_translation(token, va, pa.0);
        }
        None
    }
}

fn ensure_user_page_mapped(table: &PageTable, va: usize, access: MapPermission) -> bool {
    if va >= USER_END {
        return false;
    }
    let vpn = VirtAddr::from(va).floor();
    if let Some(entry) = table.translate(vpn).filter(PageTableEntry::is_valid) {
        let required = PTEFlags::U | PTEFlags::from_bits(access.bits()).unwrap();
        if entry.flags().contains(required) {
            return true;
        }
        if access != MapPermission::W || !entry.flags().contains(PTEFlags::U) {
            return false;
        }
        // Kernel copyout may need the same COW fault as a user store.
    }
    if let Some(task) = crate::task::current_task() {
        if let Some(process) = task.process.upgrade() {
            if let Some(mut inner) = process.try_inner_exclusive_access() {
                if inner.memory_set.token() == table.token() {
                    return inner.memory_set.handle_page_fault(vpn, access) == PageFault::Resolved;
                }
            }
        }
    }
    false
}

pub fn translated_byte_buffer(
    token: usize,
    ptr: *const u8,
    len: usize,
    access: MapPermission,
) -> Option<Vec<UserSlice>> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start.checked_add(len)?;
    let mut buffers = Vec::new();
    while start < end {
        if !ensure_user_page_mapped(&page_table, start, access) {
            return None;
        }
        let va = VirtAddr::from(start);
        let ppn = page_table.translate(va.floor())?.ppn();
        let pa = checked_phys_addr(token, start, ppn.into())?;
        let first = va.page_offset();
        let count = (end - start).min(PAGE_SIZE - first);
        pin_user_page(token, va.floor());
        buffers.push(UserSlice {
            ptr: (pa.kernel_address() + first) as *mut u8,
            len: count,
        });
        start += count;
    }
    Some(buffers)
}

/// Require a NUL terminator before returning a path to a filesystem operation.
pub fn translated_path(token: usize, ptr: *const u8) -> Result<String, isize> {
    read_user_string(token, ptr, |s: &mut String, byte| s.push(char::from(byte)))
        .map_err(|(_, errno)| errno)
}

pub(crate) fn translated_c_bytes(token: usize, ptr: *const u8) -> Result<Vec<u8>, isize> {
    read_user_string(token, ptr, Vec::push).map_err(|(_, errno)| errno)
}

fn read_user_string<T: Default>(
    token: usize,
    ptr: *const u8,
    mut push: impl FnMut(&mut T, u8),
) -> Result<T, (T, isize)> {
    const PATH_MAX: usize = 4096;
    let page_table = PageTable::from_token(token);
    let mut string = T::default();
    let mut va = ptr as usize;
    let mut page_base = 0;
    for _ in 0..PATH_MAX {
        let offset = va % PAGE_SIZE;
        if page_base == 0 || offset == 0 {
            if !ensure_user_page_mapped(&page_table, va, MapPermission::R) {
                return Err((string, -14)); // EFAULT
            }
            let ppn = page_table.translate(VirtAddr::from(va).floor()).unwrap().ppn();
            page_base = match checked_phys_addr(token, va, ppn.into()) {
                Some(pa) => pa.kernel_address(),
                None => return Err((string, -14)),
            };
        }
        let ch = unsafe { u8::load((page_base + offset) as *const u8) };
        if ch == 0 {
            return Ok(string);
        }
        push(&mut string, ch);
        va += 1;
    }
    Err((string, -36)) // ENAMETOOLONG
}

fn typed_user_addr<T>(token: usize, va: usize, access: MapPermission) -> Option<PhysAddr> {
    if va % core::mem::align_of::<T>() != 0
        || core::mem::size_of::<T>() > PAGE_SIZE - va % PAGE_SIZE
    {
        return None;
    }
    let page_table = PageTable::from_token(token);
    if !ensure_user_page_mapped(&page_table, va, access) {
        return None;
    }
    checked_phys_addr(token, va, page_table.translate_va(VirtAddr::from(va))?)
}

pub(crate) fn read_user<T: UserValue>(token: usize, ptr: *const T) -> T {
    try_read_user(token, ptr).unwrap_or_else(|| unsafe { T::load(dummy_user_page().cast()) })
}

pub(crate) fn try_read_user<T: UserValue>(token: usize, ptr: *const T) -> Option<T> {
    let pa = typed_user_addr::<T>(token, ptr as usize, MapPermission::R)?;
    Some(unsafe { T::load(pa.kernel_address() as *const T) })
}

/// A validated copyout address. Consume it before scheduling or changing the mapping.
pub(crate) struct UserWrite<T>(*mut T);

impl<T: UserValue> UserWrite<T> {
    pub(crate) fn write(self, value: T) {
        unsafe { T::store(self.0, value) };
    }
}

pub(crate) fn prepare_user_write<T: UserValue>(token: usize, ptr: *mut T) -> Option<UserWrite<T>> {
    let pa = typed_user_addr::<T>(token, ptr as usize, MapPermission::W)?;
    Some(UserWrite(pa.kernel_address() as *mut T))
}

pub(crate) fn try_write_user<T: UserValue>(token: usize, ptr: *mut T, value: T) -> Option<()> {
    prepare_user_write(token, ptr)?.write(value);
    Some(())
}

pub(crate) fn write_user<T: UserValue>(token: usize, ptr: *mut T, value: T) {
    prepare_user_write(token, ptr)
        .unwrap_or_else(|| UserWrite(dummy_user_page().cast()))
        .write(value);
}

// User pages can be unmapped while a blocking syscall sleeps. Hold the frame in
// the TCB rather than on the kernel stack, which is not unwound on task exit.
fn pin_user_page(token: usize, vpn: VirtPageNum) {
    if let Some(task) = crate::task::current_task() {
        if let Some(process) = task.process.upgrade() {
            let page = {
                let inner = process.inner_exclusive_access();
                if inner.memory_set.token() == token {
                    inner.memory_set.pin_user_page(vpn)
                } else {
                    None
                }
            };
            if let Some(page) = page {
                task.inner_exclusive_access().user_io_frames.push(page);
            }
        }
    }
}

// Distinct virtual addresses and iovecs may alias the same physical frame.
pub struct UserSlice {
    ptr: *mut u8,
    len: usize,
}

impl UserSlice {
    /// Raw direct-map address for copying to or from another live range.
    pub(crate) fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
    pub(crate) fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.len
    }

    /// Give a kernel consumer a stable slice for this part of a write syscall.
    pub(crate) fn read_with<R>(
        &self,
        _scratch: &mut Vec<u8>,
        consume: impl FnOnce(&[u8]) -> R,
    ) -> R {
        #[cfg(feature = "smp")]
        {
            _scratch.resize(self.len, 0);
            unsafe { copy_live_bytes(self.ptr, _scratch.as_mut_ptr(), self.len) };
            consume(_scratch)
        }
        #[cfg(not(feature = "smp"))]
        consume(unsafe { core::slice::from_raw_parts(self.ptr, self.len) })
    }

    /// Copy the number of bytes the kernel producer actually wrote back to userspace.
    pub(crate) fn write_with(
        &self,
        _scratch: &mut Vec<u8>,
        produce: impl FnOnce(&mut [u8]) -> usize,
    ) -> usize {
        #[cfg(feature = "smp")]
        {
            _scratch.resize(self.len, 0);
            let written = produce(_scratch);
            self.copy_from(&_scratch[..written]);
            written
        }
        #[cfg(not(feature = "smp"))]
        produce(unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) })
    }

    fn copy_from(&self, src: &[u8]) {
        assert!(src.len() <= self.len);
        unsafe { copy_live_bytes(src.as_ptr(), self.ptr, src.len()) };
    }
}

pub struct UserBuffer {
    pub buffers: Vec<UserSlice>,
}

impl UserBuffer {
    pub fn new(buffers: Vec<UserSlice>) -> Self {
        Self { buffers }
    }
    pub fn len(&self) -> usize {
        self.buffers.iter().map(UserSlice::len).sum()
    }

    pub(crate) fn copy_from(&self, src: &[u8]) {
        let mut remaining = src;
        for buffer in &self.buffers {
            if remaining.is_empty() {
                break;
            }
            let (chunk, rest) = remaining.split_at(remaining.len().min(buffer.len()));
            buffer.copy_from(chunk);
            remaining = rest;
        }
    }

    pub(crate) fn fill_zero(&self) -> usize {
        let mut total = 0;
        for buffer in &self.buffers {
            unsafe { zero_live_bytes(buffer.ptr, buffer.len) };
            total += buffer.len;
        }
        total
    }
}

impl IntoIterator for UserBuffer {
    type Item = UserByte;
    type IntoIter = UserBufferIterator;
    fn into_iter(self) -> Self::IntoIter {
        UserBufferIterator {
            buffers: self.buffers,
            current_buffer: 0,
            current_idx: 0,
        }
    }
}

pub struct UserBufferIterator {
    buffers: Vec<UserSlice>,
    current_buffer: usize,
    current_idx: usize,
}

pub struct UserByte(*mut u8);

impl UserByte {
    pub(crate) fn read(self) -> u8 {
        unsafe { u8::load(self.0) }
    }

    pub(crate) fn write(self, byte: u8) {
        unsafe { u8::store(self.0, byte) };
    }
}

impl Iterator for UserBufferIterator {
    type Item = UserByte;
    fn next(&mut self) -> Option<Self::Item> {
        let buffer = self.buffers.get(self.current_buffer)?;
        let ptr = unsafe { buffer.ptr.add(self.current_idx) };
        self.current_idx += 1;
        if self.current_idx == buffer.len {
            self.current_buffer += 1;
            self.current_idx = 0;
        }
        Some(UserByte(ptr))
    }
}
