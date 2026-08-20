use std::path::PathBuf;

fn main() {
    let scratch = std::env::temp_dir().join(format!("agentplug-browser-engine-witness-{}", std::process::id()));
    let _ = std::fs::create_dir_all(scratch.join(".gm"));
    let cwd: PathBuf = scratch.clone();

    println!("=== witness 1: cdp-shaped envelope (engine=chrome), session list on empty state ===");
    let cdp_envelope = serde_json::json!({"body": "session list", "timeoutMs": 5000, "engine": "chrome"}).to_string();
    let v = agentplug_host::browser_run(&cdp_envelope, &cwd, "witness-cdp");
    println!("{}", serde_json::to_string_pretty(&v).unwrap());
    assert_eq!(v.get("ok").and_then(|b| b.as_bool()), Some(true), "cdp session list must report ok:true even with zero sessions");
    assert!(v.get("sessions").and_then(|s| s.as_array()).map(|a| a.is_empty()).unwrap_or(false), "fresh cwd must have zero tracked sessions");

    println!("\n=== witness 2: new browser-verb envelope (engine=lightpanda), expect a real actionable error on this platform ===");
    let browser_envelope = serde_json::json!({"body": "session new", "timeoutMs": 5000, "engine": "lightpanda"}).to_string();
    let v2 = agentplug_host::browser_run(&browser_envelope, &cwd, "witness-lightpanda");
    println!("{}", serde_json::to_string_pretty(&v2).unwrap());
    let ok2 = v2.get("ok").and_then(|b| b.as_bool()).unwrap_or(true);
    let stderr2 = v2.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
    if cfg!(windows) {
        assert_eq!(ok2, false, "lightpanda session new must fail on Windows (no native binary)");
        assert!(stderr2.contains("no native Windows binary"), "error must name the real platform gap, got: {stderr2}");
        assert!(stderr2.contains("WSL2") || stderr2.contains("Docker"), "error must name a real remediation path, got: {stderr2}");
        println!("PASS: lightpanda correctly reports the actionable Windows-unsupported error, no silent Chrome fallback");
    } else {
        println!("non-Windows platform: lightpanda path depends on real binary availability on PATH, not asserted here");
    }

    println!("\n=== witness 3: engine field absent entirely -> must default to Chrome (cdp backward-compat) ===");
    let no_engine_envelope = serde_json::json!({"body": "session list", "timeoutMs": 5000}).to_string();
    let v3 = agentplug_host::browser_run(&no_engine_envelope, &cwd, "witness-no-engine");
    println!("{}", serde_json::to_string_pretty(&v3).unwrap());
    assert_eq!(v3.get("ok").and_then(|b| b.as_bool()), Some(true), "missing-engine session list must still succeed (defaults to chrome engine selection, not an error)");

    println!("\n=== witness 4: steel_endpoint configured -> steel wins regardless of requested engine, and a down endpoint reports a real dial error ===");
    let cfg_path = cwd.join(".gm").join("browser-config.json");
    std::fs::write(&cfg_path, serde_json::json!({"steel_endpoint": "127.0.0.1:1"}).to_string()).unwrap();
    let steel_envelope = serde_json::json!({"body": "session new", "timeoutMs": 3000, "engine": "chrome"}).to_string();
    let v4 = agentplug_host::browser_run(&steel_envelope, &cwd, "witness-steel");
    println!("{}", serde_json::to_string_pretty(&v4).unwrap());
    let ok4 = v4.get("ok").and_then(|b| b.as_bool()).unwrap_or(true);
    let stderr4 = v4.get("stderr").and_then(|s| s.as_str()).unwrap_or("");
    assert_eq!(ok4, false, "dialing an unreachable steel endpoint must fail, not silently fall through to chrome");
    assert!(stderr4.contains("steel-browser"), "error must name steel-browser as the configured target, got: {stderr4}");
    println!("PASS: configured steel_endpoint takes CDP-target priority over an explicit chrome hint, and reports a real dial failure when unreachable");
    let _ = std::fs::remove_file(&cfg_path);

    let _ = std::fs::remove_dir_all(&scratch);
    println!("\nALL WITNESSES PASSED");
}
