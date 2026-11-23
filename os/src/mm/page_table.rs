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
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
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
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
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
                let frame = frame_alloc()?;
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
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
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        assert!(self.try_map(vpn, ppn, flags), "out of page-table frames");
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
        8usize << 60 | self.root_ppn.0
    }
}

/// 旧标量翻译接口的兜底页，使无效地址的访问不触及进程映射或其他内核数据。
#[repr(C, align(4096))]
struct DummyUserPage([u8; PAGE_SIZE]);

static mut DUMMY_USER_PAGE: DummyUserPage = DummyUserPage([0; PAGE_SIZE]);

fn dummy_user_page() -> *mut u8 {
    unsafe { core::ptr::addr_of_mut!(DUMMY_USER_PAGE) as *mut u8 }
}

/// 物理地址是否像“正常的用户页帧”：落在 [ekernel, ram_end) 内。
/// 之前 getcwd01 的内核 panic 就是 translate 出来 0x80201xxx（内核 .text 区）直接写崩的。
fn phys_addr_sane(pa: usize) -> bool {
    extern "C" {
        fn ekernel();
    }
    let start = ekernel as usize;
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
        token & ((1usize << 44) - 1),
        token,
        crate::syscall::LAST_SYSCALL.load(core::sync::atomic::Ordering::Relaxed)
    );
}

fn ensure_user_page_mapped(table: &PageTable, va: usize, access: MapPermission) -> bool {
    if va >= USER_END {
        return false;
    }
    let vpn = VirtAddr::from(va).floor();
    if let Some(entry) = table.translate(vpn).filter(PageTableEntry::is_valid) {
        let required = PTEFlags::U | PTEFlags::from_bits(access.bits()).unwrap();
        return entry.flags().contains(required);
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
        let pa = usize::from(PhysAddr::from(ppn));
        if !phys_addr_sane(pa) {
            if !dummy_warned() {
                report_bad_translation(token, start, pa);
            }
            return None;
        }
        let first = va.page_offset();
        let count = (end - start).min(PAGE_SIZE - first);
        pin_vma_page(token, va.floor());
        buffers.push(UserSlice {
            ptr: (pa + first) as *mut u8,
            len: count,
        });
        start += count;
    }
    Some(buffers)
}

/// Load a string from other address spaces into kernel space without an end `\0`.
pub fn translated_str(token: usize, ptr: *const u8) -> String {
    let page_table = PageTable::from_token(token);
    let mut string = String::new();
    let mut va = ptr as usize;
    let mut checked_vpn = usize::MAX;
    loop {
        let vpn = VirtAddr::from(va).floor().0;
        if vpn != checked_vpn {
            if !ensure_user_page_mapped(&page_table, va, MapPermission::R) {
                // 非法指针：按空串处理，避免内核 panic
                return string;
            }
            checked_vpn = vpn;
        }
        let ch: u8 = *(page_table
            .translate_va(VirtAddr::from(va))
            .unwrap()
            .get_ref());
        if ch == 0 {
            break;
        }
        string.push(ch as char);
        va += 1;
        // 防御：用户给的“字符串”可能没有结尾 0
        if string.len() > 4095 {
            break;
        }
    }
    string
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
    let pa = page_table.translate_va(VirtAddr::from(va))?;
    if !phys_addr_sane(usize::from(pa)) {
        if !dummy_warned() {
            report_bad_translation(token, va, usize::from(pa));
        }
        return None;
    }
    Some(pa)
}

pub fn translated_ref<T>(token: usize, ptr: *const T) -> &'static T {
    match typed_user_addr::<T>(token, ptr as usize, MapPermission::R) {
        Some(pa) => pa.get_ref(),
        None => unsafe { &*(dummy_user_page() as *const T) },
    }
}

pub fn translated_refmut<T>(token: usize, ptr: *mut T) -> &'static mut T {
    match typed_user_addr::<T>(token, ptr as usize, MapPermission::W) {
        Some(pa) => pa.get_mut(),
        None => unsafe { &mut *(dummy_user_page() as *mut T) },
    }
}

// VMA pages can be unmapped while a blocking syscall sleeps. Hold the frame in
// the TCB rather than on the kernel stack, which is not unwound on task exit.
fn pin_vma_page(token: usize, vpn: VirtPageNum) {
    if let Some(task) = crate::task::current_task() {
        if let Some(process) = task.process.upgrade() {
            let page = {
                let inner = process.inner_exclusive_access();
                if inner.memory_set.token() == token {
                    inner.memory_set.pin_vma_page(vpn)
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
    pub fn as_ptr(&self) -> *const u8 {
        self.ptr
    }
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
    pub fn len(&self) -> usize {
        self.len
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
}

impl IntoIterator for UserBuffer {
    type Item = *mut u8;
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

impl Iterator for UserBufferIterator {
    type Item = *mut u8;
    fn next(&mut self) -> Option<Self::Item> {
        let buffer = self.buffers.get(self.current_buffer)?;
        let ptr = unsafe { buffer.ptr.add(self.current_idx) };
        self.current_idx += 1;
        if self.current_idx == buffer.len {
            self.current_buffer += 1;
            self.current_idx = 0;
        }
        Some(ptr)
    }
}
