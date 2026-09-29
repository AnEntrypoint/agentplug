use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use wait_timeout::ChildExt;

use crate::browser::CDP_EVAL_JS;

pub(crate) const GPU_PROBE_JS: &str = include_str!("gpu_probe.js");

const ANTI_THROTTLE_ARGS: [&str; 5] = [
    "--disable-background-timer-throttling",
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    "--disable-features=CalculateNativeWinOcclusion",
    "--disable-ipc-flooding-protection",
];

const UNCAPPED_ARGS: [&str; 4] = [
    "--ignore-gpu-blocklist",
    "--enable-unsafe-webgpu",
    "--disable-frame-rate-limit",
    "--disable-gpu-vsync",
];

const LUID_RESOLVER_POWERSHELL: &str = r#"
$live = @{}
(Get-Counter '\GPU Adapter Memory(*)\Dedicated Usage').CounterSamples | ForEach-Object {
  if ($_.InstanceName -match 'luid_0x([0-9a-f]+)_0x([0-9a-f]+)_') {
    $high = [Convert]::ToInt64($matches[1], 16)
    $low = [Convert]::ToInt64($matches[2], 16)
    $live[[string]($high * 4294967296 + $low)] = "$high,$low"
  }
}
Get-ChildItem HKLM:\SOFTWARE\Microsoft\DirectX | ForEach-Object {
  $p = Get-ItemProperty $_.PSPath
  if ($p.Description -and $null -ne $p.AdapterLuid) {
    $key = [string]$p.AdapterLuid
    if ($live.ContainsKey($key)) { "$($p.Description)|$($live[$key])" }
  }
}
"#;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum GpuChoice {
    Default,
    Nvidia,
    Amd,
    Intel,
}

impl GpuChoice {
    pub(crate) fn parse(raw: &str) -> Option<GpuChoice> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "default" | "auto" | "" => Some(GpuChoice::Default),
            "nvidia" => Some(GpuChoice::Nvidia),
            "amd" | "radeon" => Some(GpuChoice::Amd),
            "intel" => Some(GpuChoice::Intel),
            _ => None,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            GpuChoice::Default => "default",
            GpuChoice::Nvidia => "nvidia",
            GpuChoice::Amd => "amd",
            GpuChoice::Intel => "intel",
        }
    }

    fn description_matches(self, description: &str) -> bool {
        let d = description.to_ascii_lowercase();
        match self {
            GpuChoice::Default => false,
            GpuChoice::Nvidia => d.contains("nvidia"),
            GpuChoice::Amd => d.contains("amd") || d.contains("radeon"),
            GpuChoice::Intel => d.contains("intel"),
        }
    }
}

fn choice_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("gpu-choice.txt")
}

pub(crate) fn record_choice(profile_dir: &Path, choice: Option<GpuChoice>) {
    let path = choice_sidecar_path(profile_dir);
    match choice {
        Some(c) => {
            let _ = std::fs::create_dir_all(profile_dir);
            let _ = std::fs::write(&path, c.label());
        }
        None => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

pub(crate) fn recorded_choice(profile_dir: &Path) -> Option<GpuChoice> {
    std::fs::read_to_string(choice_sidecar_path(profile_dir)).ok().and_then(|s| GpuChoice::parse(&s))
}

fn uncapped_sidecar_path(profile_dir: &Path) -> PathBuf {
    profile_dir.join("uncapped.txt")
}

pub(crate) fn record_uncapped(profile_dir: &Path, uncapped: bool) {
    let path = uncapped_sidecar_path(profile_dir);
    if uncapped {
        let _ = std::fs::create_dir_all(profile_dir);
        let _ = std::fs::write(&path, "1");
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

pub(crate) fn recorded_uncapped(profile_dir: &Path) -> bool {
    uncapped_sidecar_path(profile_dir).exists()
}

#[derive(Default)]
pub(crate) struct LaunchOptions {
    pub(crate) gpu: Option<GpuChoice>,
    pub(crate) uncapped: bool,
}

fn apply_launch_token(options: &mut LaunchOptions, token: &str) -> Result<(), String> {
    if token == "uncapped" {
        options.uncapped = true;
        return Ok(());
    }
    let value = token.strip_prefix("gpu=").ok_or_else(|| format!("'{token}' is not a launch option (expected gpu=nvidia|amd|intel|default or uncapped)"))?;
    options.gpu = Some(GpuChoice::parse(value).ok_or_else(|| format!("gpu={value} is not one of nvidia|amd|intel|default"))?);
    Ok(())
}

pub(crate) fn split_launch_options(body: &str) -> (LaunchOptions, Option<String>, String) {
    let trimmed = body.trim_start();
    let (first_line, remainder) = match trimmed.find('\n') {
        Some(nl) => (trimmed[..nl].trim_end(), &trimmed[nl + 1..]),
        None => (trimmed.trim_end(), ""),
    };
    let mut options = LaunchOptions::default();
    let session_new_options = first_line.strip_prefix("session new").filter(|rest| rest.starts_with(char::is_whitespace));
    let (tokens, rebuilt) = match session_new_options {
        Some(rest) => (rest, format!("session new\n{remainder}")),
        None if first_line == "uncapped" || first_line.starts_with("gpu=") => (first_line, remainder.to_string()),
        None => return (options, None, body.to_string()),
    };
    for token in tokens.split_whitespace() {
        if let Err(e) = apply_launch_token(&mut options, token) {
            return (LaunchOptions::default(), Some(e), body.to_string());
        }
    }
    (options, None, rebuilt)
}

pub(crate) fn is_gpu_query(body: &str) -> bool {
    body.trim() == "gpu"
}

fn resolve_luid_args(choice: GpuChoice) -> Result<Vec<String>, String> {
    if choice == GpuChoice::Default {
        return Ok(Vec::new());
    }
    if !cfg!(windows) {
        return Err(format!("gpu={} selection is only implemented on Windows (ANGLE d3d11 adapter LUID); on this OS use gpu=default and select via the OS", choice.label()));
    }
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", LUID_RESOLVER_POWERSHELL])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("gpu={}: powershell LUID lookup failed to start: {e}", choice.label()))?;
    let listing = String::from_utf8_lossy(&output.stdout);
    let luid = listing
        .lines()
        .filter_map(|line| line.split_once('|'))
        .filter(|(description, _)| !description.to_ascii_lowercase().contains("basic render"))
        .find(|(description, _)| choice.description_matches(description))
        .map(|(_, luid)| luid.trim().to_string());
    match luid {
        Some(luid) => Ok(vec!["--use-angle=d3d11".to_string(), format!("--use-adapter-luid={luid}")]),
        None => Err(format!(
            "gpu={}: no live {} adapter found among: {}",
            choice.label(),
            choice.label(),
            listing.lines().filter_map(|l| l.split_once('|').map(|(d, _)| d.to_string())).collect::<Vec<_>>().join(", ")
        )),
    }
}

pub(crate) fn launch_args(profile_dir: &Path, configured: Option<&str>, uncapped: bool) -> Result<Vec<String>, String> {
    let mut args: Vec<String> = ANTI_THROTTLE_ARGS.iter().map(|s| s.to_string()).collect();
    if uncapped {
        args.extend(UNCAPPED_ARGS.iter().map(|s| s.to_string()));
        if cfg!(windows) {
            args.push("--use-angle=d3d11".to_string());
        }
    }
    let choice = recorded_choice(profile_dir)
        .or_else(|| std::env::var("GM_BROWSER_GPU").ok().and_then(|v| GpuChoice::parse(&v)))
        .or_else(|| configured.and_then(GpuChoice::parse));
    if let Some(choice) = choice {
        args.extend(resolve_luid_args(choice)?);
    }
    Ok(args)
}

pub(crate) fn report(node: &Path, port: u16, cdp_endpoint: &str, profile_dir: &Path, timeout_ms: u64, uncapped: bool) -> Value {
    let topology = std::thread::spawn(crate::display::query);
    with_display(probe_report(node, port, cdp_endpoint, profile_dir, timeout_ms, uncapped), topology, uncapped)
}

pub(crate) fn with_display_probe(probe: Value, uncapped: bool) -> Value {
    with_display(probe, std::thread::spawn(crate::display::query), uncapped)
}

fn with_display(mut out: Value, topology: std::thread::JoinHandle<Result<crate::display::Topology, String>>, uncapped: bool) -> Value {
    let raf_fps = out.get("fps").and_then(Value::as_u64);
    let display = match topology.join() {
        Ok(Ok(t)) => t.report(raf_fps, uncapped),
        Ok(Err(e)) => json!({"uncapped": uncapped, "display_warn": e}),
        Err(_) => json!({"uncapped": uncapped, "display_warn": "display topology probe panicked"}),
    };
    if let (Some(o), Value::Object(d)) = (out.as_object_mut(), display) {
        o.extend(d);
    }
    out
}

fn probe_report(node: &Path, port: u16, cdp_endpoint: &str, profile_dir: &Path, timeout_ms: u64, uncapped: bool) -> Value {
    let stamp = format!("{}-{}", std::process::id(), port);
    let tmp = std::env::temp_dir();
    let helper_path = tmp.join(format!("agentplug-gpu-eval-{stamp}.mjs"));
    let probe_path = tmp.join(format!("agentplug-gpu-probe-{stamp}.js"));
    let script_path = tmp.join(format!("agentplug-gpu-script-{stamp}.js"));
    let result_path = tmp.join(format!("agentplug-gpu-result-{stamp}.json"));
    let write_ok = std::fs::write(&helper_path, CDP_EVAL_JS).is_ok()
        && std::fs::write(&probe_path, GPU_PROBE_JS).is_ok()
        && std::fs::write(&script_path, "void 0").is_ok();
    let outcome = if write_ok {
        run_helper(node, &helper_path, &probe_path, &script_path, &result_path, port, cdp_endpoint, profile_dir, timeout_ms, uncapped)
    } else {
        json!({"accelerated": false, "warn": "gpu report could not write its temp helper files"})
    };
    for p in [&helper_path, &probe_path, &script_path, &result_path] {
        let _ = std::fs::remove_file(p);
    }
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_helper(node: &Path, helper: &Path, probe: &Path, script: &Path, result: &Path, port: u16, cdp_endpoint: &str, profile_dir: &Path, timeout_ms: u64, uncapped: bool) -> Value {
    let cfg = json!({
        "uncapped": uncapped,
        "port": port,
        "cdpEndpoint": cdp_endpoint,
        "scriptFile": script.to_string_lossy(),
        "resultFile": result.to_string_lossy(),
        "timeoutMs": timeout_ms,
        "mode": "gpu",
        "gpuProbeFile": probe.to_string_lossy(),
        "wantGpu": recorded_choice(profile_dir).filter(|c| *c != GpuChoice::Default).map(GpuChoice::label),
    })
    .to_string();
    let mut cmd = Command::new(node);
    cmd.arg(helper).arg(&cfg).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return json!({"accelerated": false, "warn": format!("gpu report helper failed to start: {e}")}),
    };
    let finished = matches!(child.wait_timeout(Duration::from_millis(timeout_ms + 3000)), Ok(Some(_)));
    if !finished {
        let _ = child.kill();
        let _ = child.wait();
        return json!({"accelerated": false, "warn": "gpu report timed out"});
    }
    let envelope: Value = std::fs::read_to_string(result).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    if let Some(err) = envelope.get("__cdpError").and_then(|v| v.as_str()) {
        return json!({"accelerated": false, "warn": format!("gpu probe failed: {err}")});
    }
    envelope.get("result").cloned().unwrap_or_else(|| json!({"accelerated": false, "warn": "gpu probe returned nothing"}))
}
