use bitflags::*;

bitflags! {
    pub struct SignalFlags: u64 {
        const SIGHUP      = 1 << 0;
        const SIGINT      = 1 << 1;
        const SIGQUIT     = 1 << 2;
        const SIGILL      = 1 << 3;
        const SIGTRAP     = 1 << 4;
        const SIGABRT     = 1 << 5;
        const SIGBUS      = 1 << 6;
        const SIGFPE      = 1 << 7;
        const SIGKILL     = 1 << 8;
        const SIGUSR1     = 1 << 9;
        const SIGSEGV     = 1 << 10;
        const SIGUSR2     = 1 << 11;
        const SIGPIPE     = 1 << 12;
        const SIGALRM     = 1 << 13;
        const SIGTERM     = 1 << 14;
        const SIGSTKFLT   = 1 << 15;
        const SIGCHLD     = 1 << 16;
        const SIGCONT     = 1 << 17;
        const SIGSTOP     = 1 << 18;
        const SIGTSTP     = 1 << 19;
        const SIGTTIN     = 1 << 20;
        const SIGTTOU     = 1 << 21;
        const SIGURG      = 1 << 22;
        const SIGXCPU     = 1 << 23;
        const SIGXFSZ     = 1 << 24;
        const SIGVTALRM   = 1 << 25;
        const SIGPROF     = 1 << 26;
        const SIGWINCH    = 1 << 27;
        const SIGIO       = 1 << 28;
        const SIGPWR      = 1 << 29;
        const SIGSYS      = 1 << 30;
        const REALTIME = u64::MAX << 31;
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
