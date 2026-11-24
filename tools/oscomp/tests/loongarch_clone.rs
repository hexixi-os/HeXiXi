//! Standalone ELF exercising the Linux LoongArch64 clone register order.
#![no_std]
#![no_main]

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicI32, Ordering};

#[repr(align(16))]
struct Stack([u8; 4096]);

static mut STACK: Stack = Stack([0; 4096]);
static TLS: u64 = 0x1234;
static CHILD_TID: AtomicI32 = AtomicI32::new(0);
static RESULT: AtomicI32 = AtomicI32::new(0);

extern "C" fn child(tls: usize) -> ! {
    let tid = CHILD_TID.load(Ordering::Relaxed);
    RESULT.store(
        if tls == &TLS as *const u64 as usize && tid > 0 {
            tid
        } else {
            -1
        },
        Ordering::Release,
    );
    exit(0)
}

#[no_mangle]
extern "C" fn _start() -> ! {
    const SHARE_PROCESS: usize = 0xf00; // VM, FS, FILES, SIGHAND
    const CLONE_THREAD: usize = 0x1_0000;
    const CLONE_SETTLS: usize = 0x8_0000;
    const CLONE_CHILD_SETTID: usize = 0x100_0000;
    const FLAGS: usize = SHARE_PROCESS | CLONE_THREAD | CLONE_SETTLS | CLONE_CHILD_SETTID;
    let tid: isize;
    unsafe {
        let top = core::ptr::addr_of_mut!(STACK.0).cast::<u8>().add(4096);
        asm!(
            "syscall 0",
            "bnez $a0, 2f",
            "move $a0, $tp",
            "jirl $ra, $a5, 0",
            "2:",
            inlateout("$a0") FLAGS => tid,
            in("$a1") top,
            in("$a2") 0usize,
            in("$a3") &CHILD_TID,
            in("$a4") &TLS,
            in("$a5") child,
            in("$a7") 220usize,
        );
    }
    for _ in 0..64 {
        let observed = RESULT.load(Ordering::Acquire) as isize;
        if tid <= 0 || observed < 0 {
            exit(1);
        }
        if observed == tid {
            let marker = b"loongarch-clone-ok\n";
            unsafe {
                asm!(
                    "syscall 0",
                    inlateout("$a0") 1usize => _,
                    in("$a1") marker.as_ptr(),
                    in("$a2") marker.len(),
                    in("$a7") 64usize,
                );
            }
            exit(0);
        }
        unsafe {
            asm!("syscall 0", in("$a7") 124usize, lateout("$a0") _);
        }
    }
    exit(2)
}

fn exit(code: usize) -> ! {
    unsafe {
        asm!("syscall 0", in("$a0") code, in("$a7") 93usize, options(noreturn));
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    exit(3)
}
