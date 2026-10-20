#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn ensure_hidden_console() {}

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
