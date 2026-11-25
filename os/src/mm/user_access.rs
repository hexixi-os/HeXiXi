//! Access to physical memory that another hart may be modifying in userspace.

/// A plain syscall ABI value that can be read from and written to user memory.
///
/// Implementations must accept all bit patterns and avoid padding. Under SMP,
/// they must use architecture loads and stores for every field.
pub(crate) unsafe trait UserValue: Copy {
    /// `src` must point to a readable, aligned user value in the kernel direct map.
    unsafe fn load(src: *const Self) -> Self;
    /// `dst` must point to a writable, aligned user value in the kernel direct map.
    unsafe fn store(dst: *mut Self, value: Self);
}

macro_rules! user_integers {
    ($($ty:ty => $load:literal, $store:literal);+ $(;)?) => {$(
        unsafe impl UserValue for $ty {
            #[inline]
            unsafe fn load(src: *const Self) -> Self {
                #[cfg(all(feature = "smp", target_arch = "riscv64"))]
                {
                    let value: usize;
                    core::arch::asm!(
                        concat!($load, " {value}, 0({src})"),
                        value = lateout(reg) value,
                        src = in(reg) src,
                        options(nostack),
                    );
                    value as Self
                }
                #[cfg(not(all(feature = "smp", target_arch = "riscv64")))]
                {
                    src.read()
                }
            }

            #[inline]
            unsafe fn store(dst: *mut Self, value: Self) {
                #[cfg(all(feature = "smp", target_arch = "riscv64"))]
                core::arch::asm!(
                    concat!($store, " {value}, 0({dst})"),
                    value = in(reg) value as usize,
                    dst = in(reg) dst,
                    options(nostack),
                );
                #[cfg(not(all(feature = "smp", target_arch = "riscv64")))]
                dst.write(value);
            }
        }
    )+};
}

// Naturally aligned scalar accesses, including a futex word, use one instruction.
// The assembly carries memory effects so Rust never accesses the live bytes itself.
user_integers! {
    u8 => "lbu", "sb";
    u16 => "lhu", "sh";
    u32 => "lwu", "sw";
    i32 => "lw", "sw";
    u64 => "ld", "sd";
    i64 => "ld", "sd";
    usize => "ld", "sd";
}

unsafe impl UserValue for [usize; 2] {
    #[inline]
    unsafe fn load(src: *const Self) -> Self {
        let p = src as *const usize;
        [usize::load(p), usize::load(p.add(1))]
    }

    #[inline]
    unsafe fn store(dst: *mut Self, value: Self) {
        let p = dst as *mut usize;
        usize::store(p, value[0]);
        usize::store(p.add(1), value[1]);
    }
}

/// Copy between physical byte ranges, either of which may be exposed to userspace.
/// Both ranges must be valid for `len` bytes and may overlap.
pub(crate) unsafe fn copy_live_bytes(src: *const u8, dst: *mut u8, len: usize) {
    #[cfg(all(feature = "smp", target_arch = "riscv64"))]
    {
        if src == dst as *const u8 {
            return;
        }
        if (dst as usize).wrapping_sub(src as usize) < len {
            for i in (0..len).rev() {
                u8::store(dst.add(i), u8::load(src.add(i)));
            }
        } else {
            for i in 0..len {
                u8::store(dst.add(i), u8::load(src.add(i)));
            }
        }
    }
    #[cfg(not(all(feature = "smp", target_arch = "riscv64")))]
    core::ptr::copy(src, dst, len);
}

/// Zero a writable `len`-byte physical range that may be exposed to userspace.
pub(crate) unsafe fn zero_live_bytes(dst: *mut u8, len: usize) {
    #[cfg(all(feature = "smp", target_arch = "riscv64"))]
    for i in 0..len {
        u8::store(dst.add(i), 0);
    }
    #[cfg(not(all(feature = "smp", target_arch = "riscv64")))]
    dst.write_bytes(0, len);
}
