//! Opt-in measurements for a single fork→exec workload on one hart.
use crate::{sync::UPSafeCell, timer::get_time_us};
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

static LIVE_FRAMES: AtomicUsize = AtomicUsize::new(0);
static PEAK_FRAMES: AtomicUsize = AtomicUsize::new(0);
static COPIES: AtomicUsize = AtomicUsize::new(0);
static PENDING: UPSafeCell<Option<Trial>> = unsafe { UPSafeCell::new(None) };

struct Trial {
    child: Option<usize>,
    frames: usize,
    copies: usize,
    start: usize,
    fork_copies: usize,
    fork_us: usize,
}

pub(crate) fn on_frame_alloc() {
    let live = LIVE_FRAMES.fetch_add(1, Relaxed) + 1;
    PEAK_FRAMES.fetch_max(live, Relaxed);
}

pub(crate) fn on_frame_free() {
    LIVE_FRAMES.fetch_sub(1, Relaxed);
}

pub(crate) fn on_frame_copy() {
    COPIES.fetch_add(1, Relaxed);
}

pub(crate) fn begin() {
    let frames = LIVE_FRAMES.load(Relaxed);
    PEAK_FRAMES.store(frames, Relaxed);
    *PENDING.exclusive_access() = Some(Trial {
        child: None,
        frames,
        copies: COPIES.load(Relaxed),
        start: get_time_us(),
        fork_copies: 0,
        fork_us: 0,
    });
}

pub(crate) fn prepared(pid: usize) {
    if let Some(trial) = PENDING.exclusive_access().as_mut() {
        trial.child = Some(pid);
        trial.fork_copies = COPIES.load(Relaxed).wrapping_sub(trial.copies);
        trial.fork_us = get_time_us().wrapping_sub(trial.start);
    }
}

pub(crate) fn complete(pid: usize, path: &str) {
    let mut pending = PENDING.exclusive_access();
    if pending.as_ref().and_then(|trial| trial.child) != Some(pid) {
        return;
    }
    let trial = pending.take().unwrap();
    drop(pending);
    let elapsed = get_time_us().wrapping_sub(trial.start);
    let copies = COPIES.load(Relaxed).wrapping_sub(trial.copies);
    let peak = PEAK_FRAMES.load(Relaxed).saturating_sub(trial.frames);
    println!(
        "[fork-cow] path={} fork_copies={} total_copies={} extra_peak_frames={} fork_us={} fork_exec_us={}",
        path, trial.fork_copies, copies, peak, trial.fork_us, elapsed,
    );
}
