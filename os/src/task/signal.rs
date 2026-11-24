use bitflags::*;

bitflags! {
    pub struct SignalFlags: u32 {
        const SIGINT    = 1 << 2;
        const SIGBUS    = 1 << 7;
        const SIGKILL   = 1 << 9;
        const SIGUSR1   = 1 << 10;
        const SIGUSR2   = 1 << 12;
        const SIGILL    = 1 << 4;
        const SIGTRAP   = 1 << 5;
        const SIGTERM   = 1 << 15;
        const SIGCHLD   = 1 << 17;
        const SIGABRT   = 1 << 6;
        const SIGFPE    = 1 << 8;
        const SIGSEGV   = 1 << 11;
    }
}

impl SignalFlags {
    pub fn check_error(&self) -> Option<(i32, &'static str)> {
        if self.contains(Self::SIGINT) {
            Some((-2, "Killed, SIGINT=2"))
        } else if self.contains(Self::SIGKILL) {
            Some((-9, "Killed, SIGKILL=9"))
        } else if self.contains(Self::SIGTERM) {
            Some((-15, "Killed, SIGTERM=15"))
        } else if self.contains(Self::SIGILL) {
            Some((-4, "Illegal Instruction, SIGILL=4"))
        } else if self.contains(Self::SIGTRAP) {
            Some((-5, "Trace or Breakpoint Trap, SIGTRAP=5"))
        } else if self.contains(Self::SIGABRT) {
            Some((-6, "Aborted, SIGABRT=6"))
        } else if self.contains(Self::SIGFPE) {
            Some((-8, "Erroneous Arithmetic Operation, SIGFPE=8"))
        } else if self.contains(Self::SIGBUS) {
            Some((-7, "Bus error, SIGBUS=7"))
        } else if self.contains(Self::SIGSEGV) {
            Some((-11, "Segmentation Fault, SIGSEGV=11"))
        } else {
            None
        }
    }
}
