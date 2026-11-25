//! Linux LP64 rt_sigframe for RV64 and LA64. Only user registers cross this ABI.
use super::{SignalAction, SignalInfo, SA_ONSTACK, SA_SIGINFO, SIGSTUB_VA, UNBLOCKABLE};
use crate::arch::{REG_A0, REG_SP};
use crate::mm::{translated_byte_buffer, MapPermission, UserBuffer, USER_END};
use crate::task::{current_task, current_trap_cx, current_user_token};
use crate::trap::TrapContext;
use alloc::{vec, vec::Vec};

pub(super) const INFO_SIZE: usize = 128;
const UCONTEXT: usize = INFO_SIZE;
const STACK: usize = UCONTEXT + 16;
const MASK: usize = UCONTEXT + 40;
// sigset_t and its reserved space occupy 128 bytes; sigcontext is 16-byte aligned.
const MCONTEXT: usize = UCONTEXT + 176;
#[cfg(target_arch = "riscv64")]
const FRAME_SIZE: usize = MCONTEXT + 32 * 8 + 528;
#[cfg(target_arch = "loongarch64")]
const FP_EXT: usize = MCONTEXT + 272;
#[cfg(target_arch = "loongarch64")]
const FRAME_SIZE: usize = FP_EXT + 288 + 16;
#[cfg(target_arch = "riscv64")]
const MINSIGSTKSZ: usize = 2048;
#[cfg(target_arch = "loongarch64")]
const MINSIGSTKSZ: usize = 4096;

fn copy_in(token: usize, address: usize, len: usize) -> Option<Vec<u8>> {
    let pages = translated_byte_buffer(token, address as *const u8, len, MapPermission::R)?;
    Some(UserBuffer::new(pages).into_iter().map(|byte| byte.read()).collect())
}

pub(super) fn copy_out(token: usize, address: usize, bytes: &[u8]) -> Option<()> {
    let pages = translated_byte_buffer(token, address as *const u8, bytes.len(), MapPermission::W)?;
    UserBuffer::new(pages).copy_from(bytes);
    Some(())
}

fn word(bytes: &[u8], offset: usize) -> usize {
    usize::from_ne_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn put(bytes: &mut [u8], offset: usize, value: usize) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;
const SS_AUTODISARM: u32 = 1 << 31;

#[derive(Clone, Copy, Default)]
pub(crate) struct AltStack {
    base: usize,
    size: usize,
    autodisarm: bool,
}

impl AltStack {
    fn contains(self, sp: usize) -> bool {
        sp > self.base && sp - self.base <= self.size
    }

    fn encode(self, output: &mut [u8], sp: usize) {
        let flags = if self.size == 0 {
            SS_DISABLE
        } else {
            (if self.contains(sp) { SS_ONSTACK } else { 0 })
                | (if self.autodisarm { SS_AUTODISARM } else { 0 })
        };
        put(output, 0, self.base);
        put(output, 8, flags as usize);
        put(output, 16, self.size);
    }

    fn decode(input: &[u8], restoring: bool) -> Result<Self, isize> {
        let mut flags = u32::from_ne_bytes(input[8..12].try_into().unwrap());
        if restoring {
            flags &= !SS_ONSTACK;
        }
        if flags == SS_DISABLE {
            return Ok(Self::default());
        }
        if flags & !SS_AUTODISARM != 0 {
            return Err(-22);
        }
        let base = word(input, 0);
        let size = word(input, 16);
        if size < MINSIGSTKSZ {
            return Err(-12);
        }
        if base.checked_add(size).filter(|&end| end <= USER_END).is_none() {
            return Err(-14);
        }
        Ok(Self { base, size, autodisarm: flags & SS_AUTODISARM != 0 })
    }
}

pub fn sys_sigaltstack(new: usize, old: usize) -> isize {
    let token = current_user_token();
    let task = current_task().unwrap();
    let previous = task.inner_exclusive_access().alt_stack;
    let sp = current_trap_cx().x[REG_SP];
    let next = if new != 0 {
        let Some(input) = copy_in(token, new, 24) else { return -14 };
        if previous.contains(sp) {
            return -1;
        }
        match AltStack::decode(&input, false) {
            Ok(stack) => Some(stack),
            Err(errno) => return errno,
        }
    } else {
        None
    };
    if old != 0 {
        let mut output = [0; 24];
        previous.encode(&mut output, sp);
        if copy_out(token, old, &output).is_none() {
            return -14;
        }
    }
    if let Some(stack) = next {
        task.inner_exclusive_access().alt_stack = stack;
    }
    0
}

pub(super) fn install(action: SignalAction, info: SignalInfo, mask: u64) -> Option<()> {
    let task = current_task().unwrap();
    let stack = task.inner_exclusive_access().alt_stack;
    let cx = current_trap_cx();
    let old_sp = cx.x[REG_SP];
    let use_alt = action.flags & SA_ONSTACK != 0 && stack.size != 0;
    let top = if use_alt && !stack.contains(old_sp) { stack.base + stack.size } else { old_sp };
    let sp = top.checked_sub(FRAME_SIZE)? & !15;
    if (use_alt || stack.contains(old_sp)) && sp < stack.base {
        return None;
    }

    let mut bytes = vec![0; FRAME_SIZE];
    info.encode(&mut bytes[..INFO_SIZE]);
    stack.encode(&mut bytes[STACK..MASK], old_sp);
    put(&mut bytes, MASK, mask as usize);
    put(&mut bytes, MCONTEXT, cx.sepc);
    save_registers(&mut bytes, cx);
    copy_out(current_user_token(), sp, &bytes)?;
    if stack.autodisarm {
        task.inner_exclusive_access().alt_stack = AltStack::default();
    }
    cx.sepc = action.handler;
    cx.x[REG_SP] = sp;
    cx.x[1] = SIGSTUB_VA;
    cx.x[REG_A0] = info.signo;
    if action.flags & SA_SIGINFO != 0 {
        cx.x[REG_A0 + 1] = sp;
        cx.x[REG_A0 + 2] = sp + UCONTEXT;
    }
    Some(())
}

#[cfg(target_arch = "riscv64")]
fn save_registers(bytes: &mut [u8], cx: &TrapContext) {
    for i in 1..32 {
        put(bytes, MCONTEXT + i * 8, cx.x[i]);
    }
}

#[cfg(target_arch = "loongarch64")]
fn save_registers(bytes: &mut [u8], cx: &TrapContext) {
    for i in 1..32 {
        put(bytes, MCONTEXT + (i + 1) * 8, cx.x[i]);
    }
    put(bytes, MCONTEXT + 33 * 8, 1); // SC_USED_FP
    put(bytes, FP_EXT, 0x46505501 | (288 << 32)); // FPU_CTX_MAGIC, extent including header
    for (i, &register) in cx.f.iter().enumerate() {
        put(bytes, FP_EXT + 16 + i * 8, register);
    }
    put(bytes, FP_EXT + 272, cx.fcc);
    put(bytes, FP_EXT + 280, cx.fcsr);
    // The zeroed 16-byte header following this context terminates the extension list.
}

fn restore() -> Option<()> {
    let sp = current_trap_cx().x[REG_SP];
    if sp % 16 != 0 {
        return None;
    }
    let bytes = copy_in(current_user_token(), sp, FRAME_SIZE)?;
    let pc = word(&bytes, MCONTEXT);
    let stack = AltStack::decode(&bytes[STACK..MASK], true).ok()?;
    if pc >= USER_END {
        return None;
    }
    let cx = current_trap_cx();
    #[cfg(target_arch = "loongarch64")]
    {
        if word(&bytes, FP_EXT) != (0x46505501 | (288 << 32)) {
            return None;
        }
        for i in 1..32 {
            cx.x[i] = word(&bytes, MCONTEXT + (i + 1) * 8);
        }
        for i in 0..32 {
            cx.f[i] = word(&bytes, FP_EXT + 16 + i * 8);
        }
        cx.fcc = word(&bytes, FP_EXT + 272) & 0xff;
        cx.fcsr = word(&bytes, FP_EXT + 280) as u32 as usize;
    }
    #[cfg(target_arch = "riscv64")]
    for i in 1..32 {
        cx.x[i] = word(&bytes, MCONTEXT + i * 8);
    }
    cx.sepc = pc;
    let task = current_task().unwrap();
    let mut inner = task.inner_exclusive_access();
    inner.signal_mask = word(&bytes, MASK) as u64 & !UNBLOCKABLE;
    inner.alt_stack = stack;
    Some(())
}

pub fn sys_rt_sigreturn() -> isize {
    if restore().is_none() {
        crate::task::exit_current_by_signal(11);
    }
    0
}
