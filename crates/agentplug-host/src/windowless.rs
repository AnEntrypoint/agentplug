#[cfg(windows)]
extern "system" {
    fn AllocConsole() -> i32;
    fn GetConsoleWindow() -> isize;
    fn GetStdHandle(n_std_handle: u32) -> isize;
    fn SetStdHandle(n_std_handle: u32, handle: isize) -> i32;
    fn ShowWindow(hwnd: isize, n_cmd_show: i32) -> i32;
}

#[cfg(windows)]
const SW_HIDE: i32 = 0;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
const STD_INPUT_HANDLE: u32 = 0xFFFF_FFF6;

#[cfg(windows)]
const STD_OUTPUT_HANDLE: u32 = 0xFFFF_FFF5;

#[cfg(windows)]
const STD_ERROR_HANDLE: u32 = 0xFFFF_FFF4;

pub fn has_console() -> bool {
    #[cfg(windows)]
    {
        unsafe { GetConsoleWindow() != 0 }
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn ensure_hidden_console() {
    #[cfg(windows)]
    {
        unsafe {
            if has_console() {
                return;
            }
            let saved_stdin = GetStdHandle(STD_INPUT_HANDLE);
            let saved_stdout = GetStdHandle(STD_OUTPUT_HANDLE);
            let saved_stderr = GetStdHandle(STD_ERROR_HANDLE);
            if AllocConsole() == 0 {
                return;
            }
            SetStdHandle(STD_INPUT_HANDLE, saved_stdin);
            SetStdHandle(STD_OUTPUT_HANDLE, saved_stdout);
            SetStdHandle(STD_ERROR_HANDLE, saved_stderr);
            let hwnd = GetConsoleWindow();
            if hwnd != 0 {
                ShowWindow(hwnd, SW_HIDE);
            }
        }
    }
}

pub fn apply_windowless(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}
