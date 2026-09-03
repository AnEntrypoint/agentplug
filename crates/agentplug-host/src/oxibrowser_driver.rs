//! Translates the `serp` verb's plain-text-body dispatch shape into calls
//! against the sideloaded `oxibrowser` plugin (its own `plugkit_alloc`/
//! `plugin_call` instance, driven via the same sibling-pool machinery
//! `host_plugin_call` uses). oxibrowser's verb surface (navigate/evaluate/
//! dom-query/extract-markdown/capabilities) is a genuine subset of what the
//! `cdp`/`browser` verbs (real Chrome, lightpanda, steel) support --
//! anything outside that subset (screenshot/capture/profile/trace/viewport,
//! multi-tab session pooling) returns a clear "not supported here, use
//! cdp/browser" error rather than silently mishandling it. When
//! steel-browser is configured (`.gm/browser-config.json`'s
//! `steel_endpoint` or `GM_STEEL_BROWSER_URL`), `run` redirects the whole
//! dispatch to `browser::run` instead, since Steel takes over `serp` too
//! (explicit user decision: steel-browser overrides every one of
//! serp/browser/cdp uniformly, not just the CDP-capable pair).

use serde_json::{json, Value};
use std::path::Path;

fn strip_session_id_prefix(body: &str) -> (Option<String>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("sessionId=") else { return (None, body) };
    let Some(nl) = rest.find('\n') else { return (None, body) };
    let (id, remainder) = (&rest[..nl], &rest[nl + 1..]);
    let id = id.trim();
    if id.is_empty() { (None, remainder) } else { (Some(id.to_string()), remainder) }
}

fn strip_timeout_prefix(body: &str) -> (Option<u64>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("timeout=") else { return (None, body) };
    let Some(nl) = rest.find('\n') else { return (None, body) };
    let (num_str, remainder) = (&rest[..nl], &rest[nl + 1..]);
    match num_str.trim().parse::<u64>() {
        Ok(ms) => (Some(ms), remainder),
        Err(_) => (None, body),
    }
}

fn strip_dom_prefix(body: &str) -> (Option<String>, &str) {
    let trimmed = body.trim_start();
    let Some(rest) = trimmed.strip_prefix("dom=") else { return (None, body) };
    let (selector, remainder) = match rest.find('\n') {
        Some(nl) => (rest[..nl].trim().to_string(), &rest[nl + 1..]),
        None => (rest.trim().to_string(), ""),
    };
    (Some(selector), remainder)
}

fn strip_url_prefix(body: &str) -> (Option<String>, &str) {
    let trimmed = body.trim_start();
    if let Some(rest) = trimmed.strip_prefix("url=") {
        return match rest.find('\n') {
            Some(nl) => (Some(rest[..nl].trim().to_string()), &rest[nl + 1..]),
            None => (Some(rest.trim().to_string()), ""),
        };
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return match trimmed.find('\n') {
            Some(nl) => (Some(trimmed[..nl].trim().to_string()), &trimmed[nl + 1..]),
            None => (Some(trimmed.trim().to_string()), ""),
        };
    }
    (None, body)
}

enum SessionCommand {
    New,
    List,
    Close,
    Reset,
    None,
}

fn parse_session_command(body: &str) -> SessionCommand {
    let trimmed = body.trim();
    if trimmed == "session new" || trimmed.starts_with("session new\n") {
        return SessionCommand::New;
    }
    if trimmed == "session list" || trimmed.starts_with("session list\n") {
        return SessionCommand::List;
    }
    if trimmed.starts_with("session close ") {
        return SessionCommand::Close;
    }
    if trimmed.starts_with("session reset ") {
        return SessionCommand::Reset;
    }
    SessionCommand::None
}

const UNSUPPORTED_MODES: &[&str] = &["screenshot", "capture", "profile", "trace"];

fn rejects_unsupported_mode(body: &str) -> Option<Value> {
    let trimmed = body.trim_start();
    for mode in UNSUPPORTED_MODES {
        if trimmed.starts_with(&format!("{mode}\n")) || trimmed == *mode {
            return Some(json!({
                "ok": false,
                "error": format!("browser (oxibrowser) does not support the '{mode}' mode yet"),
                "note": "use the cdp verb for real-Chrome/playwright-style capabilities like screenshots, CPU profiling, and tracing",
            }));
        }
    }
    if trimmed.starts_with("viewport=") {
        return Some(json!({
            "ok": false,
            "error": "browser (oxibrowser) does not support viewport overrides yet",
            "note": "use the cdp verb for real-Chrome/playwright-style viewport control",
        }));
    }
    None
}

fn call_oxibrowser(
    caller_root: &Path,
    caller_siblings: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<crate::registry::SharedPluginPool>>>>,
    verb: &str,
    body: &Value,
) -> anyhow::Result<Value> {
    let sibling_pool = {
        let guard = caller_siblings.lock().unwrap();
        guard.get("oxibrowser").cloned()
    };
    let Some(sibling_pool) = sibling_pool else {
        return Ok(json!({
            "ok": false,
            "error": "oxibrowser plugin not registered for this project (check .agentplug/plugins.txt)",
        }));
    };
    let mut guard = sibling_pool
        .acquire()
        .expect("acquire() always returns Some -- FIFO wait never denies");
    let body_s = body.to_string();
    let result = match guard.as_mut() {
        None => Err(anyhow::anyhow!("plugin_not_loaded_yet")),
        Some(handle) => crate::registry::dispatch_on(
            &mut handle.store,
            handle.instance,
            verb,
            &body_s,
            caller_root,
            caller_siblings.clone(),
        ),
    };
    sibling_pool.evict_if_swap_pending(&mut guard);
    drop(guard);
    match result {
        Ok(s) if !s.is_empty() => Ok(serde_json::from_str(&s).unwrap_or(Value::String(s))),
        Ok(_) => Ok(json!({"ok": true})),
        Err(e) => Ok(json!({"ok": false, "error": e.to_string()})),
    }
}

/// Entry point mirroring `browser::run`'s `(body, opts, cwd, session_id)`
/// shape, called from `host_oxi_exec`. `body` is the caller's raw
/// plain-text verbatim, never JSON-wrapped; `opts` is the small separate
/// metadata payload (`{"timeoutMs": <n>}`) rs-plugkit's `serp` verb sends
/// via host_oxi_exec's own opts_ptr/opts_len param.
pub fn run(
    body: &str,
    opts: &str,
    cwd: &Path,
    session_id: &str,
    siblings: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<crate::registry::SharedPluginPool>>>>,
) -> Value {
    // A configured steel-browser endpoint takes over serp too (explicit
    // user decision: steel overrides serp/browser/cdp uniformly, not just
    // the two CDP-capable verbs). oxibrowser's own verb surface has no wire
    // compatibility with a CDP session, so this redirects the whole
    // dispatch to browser::run (the same CDP-over-port driver cdp/browser
    // use) with `engine: "steel"` forced into a fresh opts payload, rather
    // than trying to translate oxibrowser's narrower body grammar onto a
    // CDP session.
    if crate::browser_engine::steel_endpoint_override(cwd).is_some() {
        let mut opts_v: Value = serde_json::from_str(opts).unwrap_or_else(|_| json!({}));
        if let Some(obj) = opts_v.as_object_mut() {
            obj.insert("engine".to_string(), json!("steel"));
        } else {
            opts_v = json!({"engine": "steel"});
        }
        return crate::browser::run(body, &opts_v.to_string(), cwd, session_id);
    }

    let inner_body = body.to_string();

    let (explicit_sid, after_sid) = strip_session_id_prefix(&inner_body);
    let session_id = explicit_sid.as_deref().filter(|s| !s.is_empty()).unwrap_or(session_id);
    let session_id = if session_id.is_empty() { "default" } else { session_id };

    match parse_session_command(after_sid) {
        SessionCommand::New => {
            // oxibrowser keeps one implicit session per wasm instance
            // (thread_local SESSION in wasm_dispatch.rs) rather than a
            // multi-session pool like real Chrome -- "new" is a no-op that
            // reports success so session-lifecycle scripts written against
            // the cdp verb's contract do not need a special case.
            return json!({"ok": true, "session_id": session_id, "note": "oxibrowser keeps one implicit session per plugin instance; session new/close/reset are accepted but no-ops"});
        }
        SessionCommand::List => {
            return json!({"ok": true, "sessions": [{"session_id": session_id, "alive": true}]});
        }
        SessionCommand::Close | SessionCommand::Reset => {
            return json!({"ok": true, "closed": true, "session_id": session_id});
        }
        SessionCommand::None => {}
    }

    if let Some(rejection) = rejects_unsupported_mode(after_sid) {
        return rejection;
    }

    let (_timeout_override, after_timeout) = strip_timeout_prefix(after_sid);
    let (dom_selector, after_dom) = strip_dom_prefix(after_timeout);
    let (url, after_url) = strip_url_prefix(after_dom);

    if let Some(url) = url {
        let result = call_oxibrowser(cwd, siblings.clone(), "navigate", &json!({"url": url}));
        let nav = match result {
            Ok(v) => v,
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        };
        if nav.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            return nav;
        }
        if after_url.trim().is_empty() {
            return json!({"ok": true, "navigated": true, "url": nav});
        }
        return match call_oxibrowser(cwd, siblings, "evaluate", &json!({"expression": after_url})) {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
    }

    if let Some(selector) = dom_selector {
        return match call_oxibrowser(cwd, siblings, "dom-query", &json!({"selector": selector})) {
            Ok(v) => v,
            Err(e) => json!({"ok": false, "error": e.to_string()}),
        };
    }

    if after_dom.trim().is_empty() {
        return json!({
            "ok": false,
            "error": "browser body resolved to an empty script after prefix parsing -- nothing would be evaluated",
        });
    }

    match call_oxibrowser(cwd, siblings, "evaluate", &json!({"expression": after_dom})) {
        Ok(v) => v,
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}
