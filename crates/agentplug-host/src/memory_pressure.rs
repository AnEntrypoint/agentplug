use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct ProcessMemoryBreakdown {
    pub rss_bytes: u64,
    pub anon_bytes: u64,
    pub file_bytes: u64,
    pub shmem_bytes: u64,
    pub swap_bytes: u64,
    pub private_bytes: u64,
}

pub fn process_memory_breakdown() -> Option<ProcessMemoryBreakdown> {
    platform::process_memory_breakdown()
}

pub fn process_private_bytes_tracking_retained_wasm_peak_unlike_working_set() -> Option<u64> {
    platform::process_memory_breakdown().map(|b| b.private_bytes)
}

#[cfg(windows)]
mod platform {
    use super::ProcessMemoryBreakdown;

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

    pub fn process_memory_breakdown() -> Option<ProcessMemoryBreakdown> {
        let mut counters = ProcessMemoryCountersEx { cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32, ..Default::default() };
        let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
        if ok == 0 {
            return None;
        }
        let private = counters.private_usage as u64;
        let working_set = counters.working_set_size as u64;
        Some(ProcessMemoryBreakdown {
            rss_bytes: working_set,
            anon_bytes: private,
            file_bytes: working_set.saturating_sub(private),
            shmem_bytes: 0,
            swap_bytes: 0,
            private_bytes: private,
        })
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ProcessMemoryBreakdown;

    fn kb_field(status: &str, key: &str) -> u64 {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    }

    pub fn process_memory_breakdown() -> Option<ProcessMemoryBreakdown> {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let anon = kb_field(&status, "RssAnon:");
        let shmem = kb_field(&status, "RssShmem:");
        let swap = kb_field(&status, "VmSwap:");
        Some(ProcessMemoryBreakdown {
            rss_bytes: kb_field(&status, "VmRSS:"),
            anon_bytes: anon,
            file_bytes: kb_field(&status, "RssFile:"),
            shmem_bytes: shmem,
            swap_bytes: swap,
            private_bytes: anon + shmem + swap,
        })
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::ProcessMemoryBreakdown;

    pub fn process_memory_breakdown() -> Option<ProcessMemoryBreakdown> {
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
