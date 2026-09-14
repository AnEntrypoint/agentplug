use serde_json::{json, Value};
use std::path::Path;

type SiblingPools = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<crate::registry::SharedPluginPool>>>>;

const PAGE_FIELD: &str = "page";

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

fn strip_extract_markdown_prefix(body: &str) -> (bool, &str) {
    let trimmed = body.trim_start();
    if trimmed == "extract-markdown" {
        return (true, "");
    }
    if let Some(remainder) = trimmed.strip_prefix("extract-markdown\n") {
        return (true, remainder);
    }
    (false, body)
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

enum SessionCommand<'a> {
    New,
    List,
    Close(&'a str),
    Reset(&'a str),
    None,
}

fn parse_session_command(body: &str) -> (SessionCommand<'_>, &str) {
    let trimmed = body.trim_start();
    let (first_line, remainder) = match trimmed.find('\n') {
        Some(nl) => (&trimmed[..nl], &trimmed[nl + 1..]),
        None => (trimmed, ""),
    };
    let first_line = first_line.trim_end();
    if first_line == "session new" || first_line.starts_with("session new ") {
        return (SessionCommand::New, remainder);
    }
    if first_line == "session list" {
        return (SessionCommand::List, remainder);
    }
    if let Some(id) = first_line.strip_prefix("session close ") {
        return (SessionCommand::Close(id.trim()), remainder);
    }
    if let Some(id) = first_line.strip_prefix("session reset ") {
        return (SessionCommand::Reset(id.trim()), remainder);
    }
    (SessionCommand::None, body)
}

const UNSUPPORTED_MODES: &[&str] = &["screenshot", "capture", "profile", "trace"];

fn rejects_unsupported_mode(body: &str) -> Option<Value> {
    let trimmed = body.trim_start();
    for mode in UNSUPPORTED_MODES {
        if trimmed == *mode
            || trimmed.starts_with(&format!("{mode}\n"))
            || (*mode == "screenshot" && trimmed.starts_with("screenshot="))
        {
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

struct PageTarget<'a> {
    cwd: &'a Path,
    siblings: SiblingPools,
    page: String,
    gm_session: Option<String>,
}

impl PageTarget<'_> {
    fn call(&self, verb: &str, mut body: Value) -> Value {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(PAGE_FIELD.to_string(), json!(self.page));
        }
        let reply = call_oxibrowser(self.cwd, self.siblings.clone(), verb, &body)
            .unwrap_or_else(|e| json!({"ok": false, "error": e.to_string()}));
        self.attribute(reply)
    }

    fn attribute(&self, mut reply: Value) -> Value {
        let Some(obj) = reply.as_object_mut() else { return reply };
        if !obj.contains_key(PAGE_FIELD) {
            obj.insert("page_partition_unsupported".to_string(), json!(true));
            obj.insert(
                "page_partition_note".to_string(),
                json!("this oxibrowser plugin predates per-page partitioning, so every gm session shares one page -- update the oxibrowser plugin"),
            );
        }
        obj.insert("session_id".to_string(), json!(self.page));
        obj.insert("gm_session".to_string(), json!(self.gm_session));
        reply
    }

    fn list(&self, caller_implicit_session: &str) -> Value {
        let mut reply = self.call("list-pages", json!({}));
        let pages = reply
            .pointer_mut("/data/pages")
            .and_then(Value::as_array_mut)
            .map(std::mem::take)
            .unwrap_or_default();
        let sessions: Vec<Value> = pages
            .into_iter()
            .map(|mut entry| {
                let page = entry.get(PAGE_FIELD).and_then(Value::as_str).unwrap_or_default().to_string();
                if let Some(obj) = entry.as_object_mut() {
                    obj.insert("session_id".to_string(), json!(page));
                    obj.insert("alive".to_string(), json!(true));
                    obj.insert("is_caller_implicit_session".to_string(), json!(page == caller_implicit_session));
                }
                entry
            })
            .collect();
        if let Some(obj) = reply.as_object_mut() {
            obj.insert("sessions".to_string(), json!(sessions));
            obj.insert("caller_gm_session".to_string(), json!(self.gm_session));
            obj.insert("caller_implicit_session".to_string(), json!(caller_implicit_session));
        }
        reply
    }

    fn close(&self) -> Value {
        self.call("close-page", json!({}))
    }
}

fn call_oxibrowser(
    caller_root: &Path,
    caller_siblings: SiblingPools,
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

fn redirect_to_steel(body: &str, opts: &str, cwd: &Path, session_id: &str) -> Value {
    let mut opts_v: Value = serde_json::from_str(opts).unwrap_or_else(|_| json!({}));
    if let Some(obj) = opts_v.as_object_mut() {
        obj.insert("engine".to_string(), json!("steel"));
    } else {
        opts_v = json!({"engine": "steel"});
    }
    crate::browser::run(body, &opts_v.to_string(), cwd, session_id)
}

pub fn run(body: &str, opts: &str, cwd: &Path, session_id: &str, siblings: SiblingPools) -> Value {
    if crate::browser_engine::steel_endpoint_override(cwd).is_some() {
        return redirect_to_steel(body, opts, cwd, session_id);
    }

    let (explicit_sid, after_sid) = strip_session_id_prefix(body);
    let origin = crate::dispatch_origin::current_dispatch_origin();
    let caller_implicit_session = origin.implicit_page_session(session_id);
    let named_page = |page: String| PageTarget {
        cwd,
        siblings: siblings.clone(),
        page,
        gm_session: origin.gm_session.clone(),
    };
    let own_page = named_page(origin.page_session(explicit_sid, session_id));

    let (session_command, after_session_command) = parse_session_command(after_sid);
    let trailing_body_present = !after_session_command.trim().is_empty();
    let terminal_refusal = |command: &str| {
        json!({"ok": false, "error": format!("'{command}' is a terminal session command and the lines after it were not evaluated -- send them as their own dispatch, or stack them under 'session new'/'session reset <id>'")})
    };
    let (target, script) = match session_command {
        SessionCommand::New if !trailing_body_present => {
            return own_page.attribute(json!({"ok": true, "page": own_page.page, "note": "oxibrowser opens a page on its first navigate/evaluate; session new reports the page this gm session owns"}));
        }
        SessionCommand::New => (own_page, after_session_command),
        SessionCommand::List if trailing_body_present => return terminal_refusal("session list"),
        SessionCommand::List => return own_page.list(&caller_implicit_session),
        SessionCommand::Close(id) | SessionCommand::Reset(id) if id.is_empty() => {
            return json!({"ok": false, "error": format!("session close/reset requires an explicit id, e.g. 'session close {caller_implicit_session}' for this gm session's own page")});
        }
        SessionCommand::Close(_) if trailing_body_present => return terminal_refusal("session close <id>"),
        SessionCommand::Close(id) => return named_page(id.to_string()).close(),
        SessionCommand::Reset(id) => {
            let target = named_page(id.to_string());
            let closed = target.close();
            if !trailing_body_present {
                return closed;
            }
            (target, after_session_command)
        }
        SessionCommand::None => (own_page, after_sid),
    };

    let mut rest = script;
    let mut dom_selector = None;
    let mut url = None;
    let mut extract_markdown = false;
    loop {
        if let Some(rejection) = rejects_unsupported_mode(rest) {
            return rejection;
        }
        let (timeout_override, after_timeout) = strip_timeout_prefix(rest);
        if timeout_override.is_some() {
            rest = after_timeout;
            continue;
        }
        let (selector, after_dom) = strip_dom_prefix(rest);
        if let Some(selector) = selector {
            dom_selector = Some(selector);
            rest = after_dom;
            continue;
        }
        let (is_extract_markdown, after_extract_markdown) = strip_extract_markdown_prefix(rest);
        if is_extract_markdown {
            extract_markdown = true;
            rest = after_extract_markdown;
            continue;
        }
        let (parsed_url, after_url) = strip_url_prefix(rest);
        if let Some(parsed_url) = parsed_url {
            url = Some(parsed_url);
            rest = after_url;
            continue;
        }
        break;
    }

    if let Some(url) = url {
        let nav = target.call("navigate", json!({"url": url}));
        if nav.get("ok").and_then(Value::as_bool) != Some(true) {
            return nav;
        }
        if dom_selector.is_none() && !extract_markdown && rest.trim().is_empty() {
            return target.attribute(json!({"ok": true, "navigated": true, "page": target.page, "url": nav}));
        }
    }
    if let Some(selector) = dom_selector {
        return target.call("dom-query", json!({"selector": selector}));
    }
    if extract_markdown {
        return target.call("extract-markdown", json!({}));
    }
    if rest.trim().is_empty() {
        return json!({
            "ok": false,
            "error": "browser body resolved to an empty script after prefix parsing -- nothing would be evaluated",
        });
    }
    target.call("evaluate", json!({"expression": rest}))
}
