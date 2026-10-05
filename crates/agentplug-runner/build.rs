use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn git_head(manifest_dir: &Path) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(manifest_dir).args(["rev-parse", "HEAD"]);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(text)
}

fn release_build_requested() -> bool {
    match std::env::var("AGENTPLUG_RELEASE_BUILD") {
        Ok(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        ),
        Err(_) => false,
    }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let commit = git_head(&manifest_dir).unwrap_or_else(|| "unknown".to_string());
    let build_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let release_build = release_build_requested();
    let generated = format!(
        "pub const COMMIT: &str = {commit:?};\npub const BUILD_TS: u64 = {build_ts};\npub const RELEASE_BUILD: bool = {release_build};\n"
    );

    let out_dir =
        PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set for the build script"));
    let dest = out_dir.join("build_info.rs");
    let unchanged = std::fs::read_to_string(&dest)
        .map(|existing| existing == generated)
        .unwrap_or(false);
    if !unchanged {
        std::fs::write(&dest, generated).expect("write build_info.rs into OUT_DIR");
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AGENTPLUG_RELEASE_BUILD");
    let head = manifest_dir.join(".git").join("HEAD");
    if let Ok(content) = std::fs::read_to_string(&head) {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Some(reference) = content.strip_prefix("ref:") {
            let resolved = manifest_dir.join(".git").join(reference.trim());
            if resolved.exists() {
                println!("cargo:rerun-if-changed={}", resolved.display());
            }
        }
    }
}
