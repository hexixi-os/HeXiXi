use crate::arch;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct TrapContext {
    pub x: [usize; 32],
    pub status: usize,
    pub sepc: usize,
    pub kernel_satp: usize,
    pub kernel_sp: usize,
    pub trap_handler: usize,
    #[cfg(target_arch = "loongarch64")]
    pub f: [usize; 32],
    #[cfg(target_arch = "loongarch64")]
    pub fcsr: usize,
    #[cfg(target_arch = "loongarch64")]
    pub fcc: usize,
}

// The LoongArch exception entry uses these slot offsets before Rust can run.
#[cfg(target_arch = "loongarch64")]
const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(TrapContext, status) == 32 * 8);
    assert!(offset_of!(TrapContext, sepc) == 33 * 8);
    assert!(offset_of!(TrapContext, kernel_sp) == 35 * 8);
    assert!(offset_of!(TrapContext, trap_handler) == 36 * 8);
    assert!(offset_of!(TrapContext, f) == 37 * 8);
    assert!(offset_of!(TrapContext, fcsr) == 69 * 8);
    assert!(offset_of!(TrapContext, fcc) == 70 * 8);
};

impl TrapContext {
    pub fn set_sp(&mut self, sp: usize) {
        self.x[arch::REG_SP] = sp;
    }
    pub fn app_init_context(
        entry: usize,
        sp: usize,
        kernel_satp: usize,
        kernel_sp: usize,
        trap_handler: usize,
    ) -> Self {
        let mut cx = Self {
            x: [0; 32],
            status: arch::user_status(),
            sepc: entry,
            kernel_satp,
            kernel_sp,
            trap_handler,
            #[cfg(target_arch = "loongarch64")]
            f: [0; 32],
            #[cfg(target_arch = "loongarch64")]
            fcsr: 0,
            #[cfg(target_arch = "loongarch64")]
            fcc: 0,
        };
        cx.set_sp(sp);
        cx
    }
}
