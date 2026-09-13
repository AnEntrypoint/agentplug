use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

use serde_json::Value;

use crate::browser::{cdp_ready_probe, free_port_probe, BrowserRuntimeConfig};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Chrome,
    Lightpanda,
    Steel,
}

pub struct AcquiredEngine {
    pub child: Option<Child>,
    pub port: u16,
    pub owns_process: bool,
}

#[derive(serde::Deserialize, Default)]
struct BrowserEngineFileConfig {
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    lightpanda_path: Option<String>,
    #[serde(default)]
    steel_endpoint: Option<String>,
}

fn load_engine_file_config(cwd: &Path) -> BrowserEngineFileConfig {
    let path = cwd.join(".gm").join("browser-config.json");
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<BrowserEngineFileConfig>(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn steel_endpoint_override(cwd: &Path) -> Option<String> {
    if let Some(v) = std::env::var("GM_STEEL_BROWSER_URL").ok().filter(|s| !s.trim().is_empty()) {
        return Some(v);
    }
    load_engine_file_config(cwd).steel_endpoint.filter(|s| !s.trim().is_empty())
}

fn lightpanda_path_override(cwd: &Path) -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("GM_LIGHTPANDA_PATH") {
        let p = PathBuf::from(v);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(v) = load_engine_file_config(cwd).lightpanda_path {
        let p = PathBuf::from(v);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let names: Vec<String> = if cfg!(windows) {
        vec![format!("{cmd}.exe"), format!("{cmd}.cmd"), cmd.to_string()]
    } else {
        vec![cmd.to_string()]
    };
    std::env::split_paths(&path_var).find_map(|p| {
        for n in &names {
            let cand = p.join(n);
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    })
}

fn find_lightpanda(cwd: &Path) -> Option<PathBuf> {
    if let Some(p) = lightpanda_path_override(cwd) {
        return Some(p);
    }
    which("lightpanda")
}

/// Picks the CDP-capable engine for THIS dispatch, given the caller-supplied
/// hint (rs-plugkit's `browser` verb sends `"engine":"lightpanda"`, its `cdp`
/// verb sends `"engine":"chrome"`, both in the same JSON envelope
/// host_browser_exec already carries -- no wasm ABI signature change, see
/// host_abi.rs's #[link(wasm_import_module="env")] extern block). A field
/// that is missing or unrecognized (an older cached wasm build, or any
/// caller that predates this field) defaults to Chrome -- the pre-existing
/// cdp verb's own behavior -- never to a newly-introduced engine, so cdp's
/// "must work identically to before this change" holds even against a stale
/// caller. A configured steel endpoint always wins regardless of the
/// caller's hint: once an operator opts a project into steel-browser, it
/// becomes the CDP target for every CDP-capable dispatch uniformly
/// (serp/oxibrowser's in-process path is untouched -- it never reaches this
/// function).
pub fn select_engine(cwd: &Path, requested: Option<&str>) -> Engine {
    if steel_endpoint_override(cwd).is_some() {
        return Engine::Steel;
    }
    let effective = requested.map(|s| s.to_string()).or_else(|| load_engine_file_config(cwd).engine);
    match effective.as_deref() {
        Some("lightpanda") if lightpanda_reachable(cwd) => Engine::Lightpanda,
        Some("chrome") | Some("cdp") | None | Some(_) => Engine::Chrome,
    }
}

/// `browser` is lightpanda's real home once a native plugin exists for every
/// platform this runs on; until then it is a transparent alias for `cdp`
/// (real Chrome) wherever lightpanda cannot actually run -- no native
/// Windows binary (confirmed in lightpanda-io/browser's own README) and no
/// `lightpanda_path`/`GM_LIGHTPANDA_PATH` override naming a WSL2/Docker
/// wrapper.
fn lightpanda_reachable(cwd: &Path) -> bool {
    lightpanda_native_binary_available() || lightpanda_path_override(cwd).is_some()
}

fn lightpanda_native_binary_available() -> bool {
    cfg!(target_os = "linux") || cfg!(target_os = "macos")
}

fn spawn_lightpanda_once(binary: &Path, port: u16, profile_dir: &Path) -> Result<Child, String> {
    let mut cmd = Command::new(binary);
    cmd.args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let log_path = profile_dir.join("lightpanda-launch.log");
    let _ = std::fs::create_dir_all(profile_dir);
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("failed to open lightpanda launch log {}: {e}", log_path.display()))?;
    let log_file_err = log_file
        .try_clone()
        .map_err(|e| format!("failed to clone lightpanda launch log handle: {e}"))?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()
        .map_err(|e| format!("lightpanda launch failed: {e}"))
}

fn launch_lightpanda(cwd: &Path, session_id: &str, browser_cfg: &BrowserRuntimeConfig) -> Result<(Child, u16), String> {
    if !lightpanda_native_binary_available() {
        return Err(
            "lightpanda has no native Windows binary (confirmed in lightpanda-io/browser's own README, 'For Windows + WSL2' section) -- \
            run it under WSL2 (`wsl lightpanda serve --host 127.0.0.1 --port <p>`) or Docker \
            (`docker run -d -p 127.0.0.1:9222:9222 lightpanda/browser:nightly`) and point GM_LIGHTPANDA_PATH at a wrapper script, \
            or set .gm/browser-config.json's steel_endpoint to use steel-browser instead, or use the cdp verb for a locally-spawned real Chrome."
                .to_string(),
        );
    }
    let binary = find_lightpanda(cwd).ok_or_else(|| {
        "lightpanda binary not found on PATH -- install lightpanda-io/browser (https://github.com/lightpanda-io/browser releases) \
        or set GM_LIGHTPANDA_PATH / .gm/browser-config.json's lightpanda_path to its binary; \
        alternatively configure steel_endpoint or use the cdp verb (real Chrome)."
            .to_string()
    })?;
    let profile_dir = cwd.join(".gm").join(format!("browser-lightpanda-profile-{}", crate::browser::sanitize_pub(session_id)));
    let _ = std::fs::create_dir_all(&profile_dir);
    let port = free_port_probe();
    let mut child = spawn_lightpanda_once(&binary, port, &profile_dir)?;
    if !cdp_ready_probe(port, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
        let _ = child.kill();
        let _ = child.wait();
        let log_tail = std::fs::read_to_string(profile_dir.join("lightpanda-launch.log"))
            .ok()
            .map(|s| s.lines().rev().take(5).collect::<Vec<_>>().join(" | "))
            .filter(|s| !s.is_empty());
        return Err(match log_tail {
            Some(tail) => format!(
                "lightpanda CDP endpoint did not become ready within {}ms (recent lightpanda output: {tail})",
                browser_cfg.chrome_ready_deadline().as_millis()
            ),
            None => format!(
                "lightpanda CDP endpoint did not become ready within {}ms (lightpanda produced no output at all)",
                browser_cfg.chrome_ready_deadline().as_millis()
            ),
        });
    }
    Ok((child, port))
}

fn parse_steel_port(endpoint: &str) -> Result<u16, String> {
    let trimmed = endpoint.trim();
    let without_scheme = trimmed
        .strip_prefix("http://")
        .or_else(|| trimmed.strip_prefix("ws://"))
        .or_else(|| trimmed.strip_prefix("https://"))
        .or_else(|| trimmed.strip_prefix("wss://"))
        .unwrap_or(trimmed);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    let port_str = host_port.rsplit(':').next().ok_or_else(|| format!("steel_endpoint '{endpoint}' has no port"))?;
    port_str
        .parse::<u16>()
        .map_err(|_| format!("steel_endpoint '{endpoint}' does not end in a valid port number (expected host:port, e.g. 127.0.0.1:9223)"))
}

fn dial_steel(cwd: &Path, browser_cfg: &BrowserRuntimeConfig) -> Result<u16, String> {
    let endpoint = steel_endpoint_override(cwd).ok_or_else(|| "steel-browser not configured".to_string())?;
    let port = parse_steel_port(&endpoint)?;
    if !cdp_ready_probe(port, Instant::now() + browser_cfg.chrome_ready_deadline(), browser_cfg) {
        return Err(format!(
            "configured steel-browser endpoint '{endpoint}' (CDP port {port}) did not respond within {}ms -- \
            confirm the container is running (`docker run -p 3000:3000 -p 9223:9223 ghcr.io/steel-dev/steel-browser`) \
            and that GM_STEEL_BROWSER_URL / .gm/browser-config.json's steel_endpoint points at its real host:port",
            browser_cfg.chrome_ready_deadline().as_millis()
        ));
    }
    Ok(port)
}

/// Acquires a CDP-reachable port for the requested engine, spawning a
/// process only for Chrome/lightpanda -- steel is always dial-only, never
/// spawned, matching BrowserSession's existing `child: Option<Child>`
/// adopted-session shape (used today for OS-orphan adoption; steel reuses
/// the identical "no owned process" case for an always-on external CDP
/// service).
pub fn acquire(engine: Engine, cwd: &Path, session_id: &str, browser_cfg: &BrowserRuntimeConfig) -> Result<AcquiredEngine, String> {
    match engine {
        Engine::Chrome => {
            let (child, port) = crate::browser::launch_chrome_pub(cwd, session_id, browser_cfg)?;
            Ok(AcquiredEngine { child: Some(child), port, owns_process: true })
        }
        Engine::Lightpanda => {
            let (child, port) = launch_lightpanda(cwd, session_id, browser_cfg)?;
            Ok(AcquiredEngine { child: Some(child), port, owns_process: true })
        }
        Engine::Steel => {
            let port = dial_steel(cwd, browser_cfg)?;
            Ok(AcquiredEngine { child: None, port, owns_process: false })
        }
    }
}

pub fn requested_engine_from_envelope(v: &Value) -> Option<String> {
    v.get("engine").and_then(|e| e.as_str()).map(|s| s.to_string())
}
