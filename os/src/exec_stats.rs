//! Optional single-hart measurements from exec entry to the first syscall in the new image.
use crate::mm::translated_path;
use crate::task::{current_process, current_user_token};
use crate::timer::get_time_us;
use alloc::string::String;
use core::sync::atomic::{AtomicUsize, Ordering};

static FRAMES: AtomicUsize = AtomicUsize::new(0);
static SECTORS: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn on_frame_alloc() {
    FRAMES.fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn on_ext4_sector_read() {
    SECTORS.fetch_add(1, Ordering::Relaxed);
}

struct Counters {
    frames: usize,
    sectors: usize,
    us: usize,
}

impl Counters {
    fn now() -> Self {
        Self {
            frames: FRAMES.load(Ordering::Relaxed),
            sectors: SECTORS.load(Ordering::Relaxed),
            us: get_time_us(),
        }
    }

    fn since(&self, start: &Self) -> Self {
        Self {
            frames: self.frames.wrapping_sub(start.frames),
            sectors: self.sectors.wrapping_sub(start.sectors),
            us: self.us.wrapping_sub(start.us),
        }
    }
}

pub(crate) struct PendingExec {
    path: String,
    start: Counters,
    prepared: Counters,
}

pub(crate) fn measure(path: *const u8, load: impl FnOnce() -> isize) -> isize {
    let path = translated_path(current_user_token(), path).unwrap_or_default();
    let start = Counters::now();
    let result = load();
    if result >= 0 {
        let prepared = Counters::now().since(&start);
        current_process().inner_exclusive_access().pending_exec = Some(PendingExec {
            path,
            start,
            prepared,
        });
    }
    result
}

pub(crate) fn complete() {
    let process = current_process();
    let sample = process.inner_exclusive_access().pending_exec.take();
    if let Some(sample) = sample {
        let first_call = Counters::now().since(&sample.start);
        crate::fork_stats::complete(process.getpid(), &sample.path);
        println!(
            "[elf-startup] path={} prepare_frames={} prepare_sectors={} prepare_us={} first_call_frames={} first_call_sectors={} first_call_us={}",
            sample.path, sample.prepared.frames, sample.prepared.sectors, sample.prepared.us,
            first_call.frames, first_call.sectors, first_call.us,
        );
    }
}
