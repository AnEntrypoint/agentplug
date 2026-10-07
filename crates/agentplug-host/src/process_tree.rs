use std::collections::HashMap;

pub(crate) fn kill_tree(root: u32) -> usize {
    platform::kill_tree(root)
}

pub(crate) struct ProcessContainment {
    #[cfg(unix)]
    process_group: u32,
    #[cfg(windows)]
    job: platform::Job,
}

pub(crate) fn establish_containment(
    child: &std::process::Child,
) -> Result<ProcessContainment, String> {
    Ok(ProcessContainment {
        #[cfg(unix)]
        process_group: child.id(),
        #[cfg(windows)]
        job: platform::Job::assign(child)?,
    })
}

pub(crate) fn terminate_containment(containment: &ProcessContainment, root: u32) -> usize {
    let mut killed = kill_tree(root);
    #[cfg(unix)]
    {
        killed += (unsafe { libc::kill(-(containment.process_group as i32), libc::SIGKILL) } == 0)
            as usize;
    }
    #[cfg(windows)]
    {
        killed += platform::terminate_job(&containment.job) as usize;
    }
    killed
}

fn descendants_root_first(root: u32, parent_of: &HashMap<u32, u32>) -> Vec<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, parent) in parent_of {
        children.entry(*parent).or_default().push(*pid);
    }
    let mut order = Vec::new();
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::from([root]);
    while let Some(pid) = queue.pop_front() {
        if !visited.insert(pid) {
            continue;
        }
        order.push(pid);
        if let Some(kids) = children.get(&pid) {
            queue.extend(kids.iter().copied());
        }
    }
    order
}

#[cfg(windows)]
mod platform {
    use std::collections::HashMap;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    pub struct Job(usize);

    impl Job {
        pub fn assign(child: &std::process::Child) -> Result<Self, String> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                unsafe { CloseHandle(handle as isize) };
                return Err(format!(
                    "SetInformationJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let assigned =
                unsafe { AssignProcessToJobObject(handle, child.as_raw_handle() as HANDLE) };
            if assigned == 0 {
                unsafe { CloseHandle(handle as isize) };
                return Err(format!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Self(handle as usize))
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0 as isize) };
        }
    }

    pub fn terminate_job(job: &Job) -> bool {
        (unsafe { TerminateJobObject(job.0 as HANDLE, 1) }) != 0
    }

    const TH32CS_SNAPPROCESS: u32 = 0x2;
    const PROCESS_TERMINATE: u32 = 0x1;
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

    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> isize;
        fn Process32FirstW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: isize, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
        fn CloseHandle(handle: isize) -> i32;
        fn TerminateProcess(process: isize, exit_code: u32) -> i32;
    }

    fn parent_map() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot == 0 {
            return parents;
        }
        let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
        entry.size = std::mem::size_of::<ProcessEntry32W>() as u32;
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while more {
            parents.insert(entry.process_id, entry.parent_process_id);
            more = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };
        parents
    }

    fn terminate(pid: u32) -> bool {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle == 0 {
            return false;
        }
        let ok = unsafe { TerminateProcess(handle, 1) };
        unsafe { CloseHandle(handle) };
        ok != 0
    }

    fn taskkill(pid: u32) -> bool {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    pub fn kill_tree(root: u32) -> usize {
        let order = super::descendants_root_first(root, &parent_map());
        order
            .iter()
            .filter(|pid| terminate(**pid) || taskkill(**pid))
            .count()
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use std::collections::HashMap;

    fn parent_map() -> HashMap<u32, u32> {
        let mut parents = HashMap::new();
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return parents;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some(after_comm) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
                continue;
            };
            if let Some(ppid) = after_comm
                .split_whitespace()
                .nth(1)
                .and_then(|p| p.parse::<u32>().ok())
            {
                parents.insert(pid, ppid);
            }
        }
        parents
    }

    pub fn kill_tree(root: u32) -> usize {
        let order = super::descendants_root_first(root, &parent_map());
        order
            .iter()
            .filter(|pid| unsafe { libc::kill(**pid as i32, libc::SIGKILL) } == 0)
            .count()
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use std::collections::HashMap;

    pub fn kill_tree(_root: u32) -> usize {
        0
    }
}
