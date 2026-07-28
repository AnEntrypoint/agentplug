use std::sync::atomic::{AtomicU64, Ordering};

/// Private commit charge of the current process, in bytes.
///
/// Deliberately NOT the working set. A forced `EmptyWorkingSet` was measured
/// dropping `WorkingSet64` from 1545MB to 1.0MB while the private bytes held
/// flat: the pages were still committed, merely unmapped from the resident set.
/// A recycle threshold keyed off the working set therefore reads a phantom
/// sawtooth and never fires, which is precisely the failure this module exists
/// to avoid. Wasm linear memory that has been grown stays committed until the
/// `Store` is dropped, so private bytes is the only figure that tracks the
/// retained peak.
///
/// Returns `None` when the platform figure cannot be read, so callers fall back
/// to the dispatch-count trigger rather than recycling on a bogus zero.
pub fn process_private_bytes() -> Option<u64> {
    platform::process_private_bytes()
}

#[cfg(windows)]
mod platform {
    #[repr(C)]
    #[derive(Default)]
    struct ProcessMemoryCountersEx {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut ProcessMemoryCountersEx, cb: u32) -> i32;
    }

    pub fn process_private_bytes() -> Option<u64> {
        let mut counters = ProcessMemoryCountersEx { cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32, ..Default::default() };
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
        if ok == 0 {
            return None;
        }
        Some(counters.private_usage as u64)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    pub fn process_private_bytes() -> Option<u64> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let mut anon_kb = None;
        let mut swap_kb = None;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("RssAnon:") {
                anon_kb = rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok();
            } else if let Some(rest) = line.strip_prefix("VmSwap:") {
                swap_kb = rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok();
            }
        }
        Some((anon_kb? + swap_kb.unwrap_or(0)) * 1024)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    pub fn process_private_bytes() -> Option<u64> {
        None
    }
}

static SHARED_DISPATCHES_SINCE_RELEASE: AtomicU64 = AtomicU64::new(0);

pub fn note_shared_plugin_dispatch() {
    SHARED_DISPATCHES_SINCE_RELEASE.fetch_add(1, Ordering::Relaxed);
}

pub fn shared_dispatches_since_release() -> u64 {
    SHARED_DISPATCHES_SINCE_RELEASE.load(Ordering::Relaxed)
}

pub fn reset_shared_dispatch_count() {
    SHARED_DISPATCHES_SINCE_RELEASE.store(0, Ordering::Relaxed);
}
