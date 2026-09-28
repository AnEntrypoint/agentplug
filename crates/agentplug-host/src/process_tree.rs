use std::collections::HashMap;

pub(crate) fn tree_working_set_bytes(roots: &[u32]) -> HashMap<u32, u64> {
    platform::tree_working_set_bytes(roots)
}

fn sum_over_descendants(roots: &[u32], parent_of: &HashMap<u32, u32>, bytes_of: impl Fn(u32) -> Option<u64>) -> HashMap<u32, u64> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent) in parent_of {
        children.entry(*parent).or_default().push(*pid);
    }
    roots
        .iter()
        .filter_map(|root| {
            let mut total = 0u64;
            let mut seen_any = false;
            let mut stack = vec![*root];
            let mut visited = std::collections::HashSet::new();
            while let Some(pid) = stack.pop() {
                if !visited.insert(pid) {
                    continue;
                }
                if let Some(b) = bytes_of(pid) {
                    total += b;
                    seen_any = true;
                }
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids.iter().copied());
                }
            }
            seen_any.then_some((*root, total))
        })
        .collect()
}

#[cfg(windows)]
mod platform {
    use super::sum_over_descendants;
    use std::collections::HashMap;

    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const PROCESS_VM_READ: u32 = 0x10;
    const INVALID_HANDLE_VALUE: isize = -1;

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        pri_class_base: i32,
        flags: u32,
        exe_file: [u16; 260],
    }

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
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut ProcessMemoryCountersEx, cb: u32) -> i32;
    }

    fn parent_map() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        // SAFETY: plain Win32 snapshot call with constant arguments; the returned handle is checked and closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot == 0 {
            return parents;
        }
        // SAFETY: ProcessEntry32W is a plain-old-data C struct for which all-zero bytes are a valid value.
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        // SAFETY: snapshot is a live handle and entry is a properly sized, writable ProcessEntry32W.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            parents.insert(entry.process_id, entry.parent_process_id);
            // SAFETY: same handle and entry as the First call above.
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: snapshot is the live handle created above and is closed exactly once.
        unsafe { CloseHandle(snapshot) };
        parents
    }

    fn working_set_of(pid: u32) -> Option<u64> {
        // SAFETY: OpenProcess only reads its by-value arguments; a zero handle is handled below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, 0, pid) };
        if handle == 0 {
            return None;
        }
        let mut counters = ProcessMemoryCountersEx { cb: std::mem::size_of::<ProcessMemoryCountersEx>() as u32, ..Default::default() };
        // SAFETY: handle is a live process handle and counters is a writable struct whose cb matches its size.
        let ok = unsafe { K32GetProcessMemoryInfo(handle, &mut counters, counters.cb) };
        // SAFETY: handle was opened above and is closed exactly once.
        unsafe { CloseHandle(handle) };
        (ok != 0).then_some(counters.working_set_size as u64)
    }

    pub fn tree_working_set_bytes(roots: &[u32]) -> HashMap<u32, u64> {
        sum_over_descendants(roots, &parent_map(), working_set_of)
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::sum_over_descendants;
    use std::collections::HashMap;

    fn parent_map() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else { return parents };
        for entry in entries.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else { continue };
            let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else { continue };
            if let Some(ppid) = after_comm.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) {
                parents.insert(pid, ppid);
            }
        }
        parents
    }

    fn resident_bytes_of(pid: u32) -> Option<u64> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: sysconf takes a constant name and has no memory-safety preconditions.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        (page_size > 0).then(|| resident_pages * page_size as u64)
    }

    pub fn tree_working_set_bytes(roots: &[u32]) -> HashMap<u32, u64> {
        sum_over_descendants(roots, &parent_map(), resident_bytes_of)
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::collections::HashMap;

    pub fn tree_working_set_bytes(_roots: &[u32]) -> HashMap<u32, u64> {
        HashMap::new()
    }
}
