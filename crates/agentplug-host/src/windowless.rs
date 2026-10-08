#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// Allocates nothing. Windows Terminal hosts every console on this machine, and
// SW_HIDE does not hide a Terminal tab, so allocating a console for the runner
// itself opened a visible tab on every start. Children carry CREATE_NO_WINDOW
// from apply_windowless, which gives each one a hidden console that its own
// children inherit.
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
