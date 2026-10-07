use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn git_text(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(manifest_dir).args(args);
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
    if text.is_empty() {
        return None;
    }
    Some(text)
}

fn git_head(manifest_dir: &Path) -> Option<String> {
    git_text(manifest_dir, &["rev-parse", "HEAD"])
        .filter(|text| text.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn git_path(manifest_dir: &Path, name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git_text(manifest_dir, &["rev-parse", "--git-path", name])?);
    Some(if path.is_absolute() {
        path
    } else {
        manifest_dir.join(path)
    })
}

fn watch_existing_path(path: &Path) {
    let mut watched = path.to_path_buf();
    while !watched.exists() {
        if !watched.pop() {
            return;
        }
    }
    println!("cargo:rerun-if-changed={}", watched.display());
}

fn watch_git_provenance(manifest_dir: &Path) {
    let mut reference = "HEAD".to_string();
    let mut seen = HashSet::new();
    while seen.insert(reference.clone()) {
        let Some(path) = git_path(manifest_dir, &reference) else {
            break;
        };
        watch_existing_path(&path);
        let Some(next) = git_text(manifest_dir, &["symbolic-ref", "--no-recurse", &reference])
        else {
            break;
        };
        reference = next;
    }
    if let Some(path) = git_path(manifest_dir, "packed-refs").filter(|path| path.exists()) {
        watch_existing_path(&path);
    }
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
    watch_git_provenance(&manifest_dir);
}
