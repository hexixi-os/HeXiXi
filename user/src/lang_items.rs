use super::{getpid, kill, SignalFlags};

#[panic_handler]
fn panic_handler(panic_info: &core::panic::PanicInfo) -> ! {
    #[cfg(target_arch = "riscv64")]
    let err = panic_info.message().unwrap();
    #[cfg(target_arch = "loongarch64")]
    let err = panic_info.message();
    if let Some(location) = panic_info.location() {
        println!(
            "Panicked at {}:{}, {}",
            location.file(),
            location.line(),
            err
        );
    } else {
        println!("Panicked: {}", err);
    }
    kill(getpid() as usize, SignalFlags::SIGABRT.bits());
    unreachable!()
}
