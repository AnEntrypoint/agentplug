//! Action recorder: a per-session log of user actions and network activity
//! captured from the local headed browser, for skill generation.
//!
//! Limits and sanitising follow docs/browser-parity.md section 1.4. The
//! capture hook calls `record_event` with each raw browser event; this module
//! sanitises it, enforces the limits and serves `events` and `frame` reads.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

pub const DEFAULT_RECORDING_MAX_DURATION_MS: u64 = 20 * 60 * 1_000;
pub const RECORDING_MAX_BYTES: usize = 500 * 1024 * 1024;
pub const MAX_ACTIVE_RECORDINGS: usize = 10;
pub const RECORDING_ENABLE_TIMEOUT_MS: u64 = 15_000;
pub const RECORDING_DISABLE_TIMEOUT_MS: u64 = 2_000;

const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_SESSION_LEN: usize = 64;
const REDACTED: &str = "[REDACTED]";

// Hosts and path fragments whose requests are telemetry and never recorded.
const TELEMETRY_HOSTS: &[&str] = &[
    "google-analytics.com",
    "googletagmanager.com",
    "doubleclick.net",
    "segment.io",
    "segment.com",
    "mixpanel.com",
    "sentry.io",
    "datadoghq.com",
];
const TELEMETRY_PATHS: &[&str] = &["/collect", "/beacon", "/telemetry"];

// Body keys whose values are replaced before an event is stored.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "authorization",
    "cookie",
    "api_key",
    "apikey",
    "session",
];

/// Filters for `events`: `kind` is the event type, `errors` keeps only
/// problem events, `exclude` drops events whose URL contains the text.
#[derive(Clone, Debug, Default)]
pub struct EventFilter {
    pub kind: Option<String>,
    pub errors: bool,
    pub exclude: Option<String>,
}

#[derive(Clone)]
struct RecordedEvent {
    event_id: String,
    kind: String,
    at_ms: u64,
    url: Option<String>,
    text: Option<String>,
    body: Option<String>,
    is_error: bool,
    repeat: u64,
    bytes: usize,
}

struct Recording {
    started_ms: u64,
    stopped: bool,
    bytes: usize,
    next_id: u64,
    events: Vec<RecordedEvent>,
}

static RECORDINGS: OnceLock<Mutex<HashMap<String, Recording>>> = OnceLock::new();

fn with_registry<T>(f: impl FnOnce(&mut HashMap<String, Recording>) -> T) -> T {
    let cell = RECORDINGS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

fn validate_session(session: &str) -> Result<(), String> {
    let ok = !session.is_empty()
        && session.len() <= MAX_SESSION_LEN
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err("recording_invalid_session".to_string())
    }
}

// Marks a recording stopped once its maximum duration has passed.
fn expire(recording: &mut Recording, now: u64) {
    if !recording.stopped && now.saturating_sub(recording.started_ms) > DEFAULT_RECORDING_MAX_DURATION_MS {
        recording.stopped = true;
    }
}

/// Starts a recording for `session`. Replaces a previous stopped recording of
/// the same name; fails when `MAX_ACTIVE_RECORDINGS` are already active.
pub fn start(session: &str) -> Result<Value, String> {
    validate_session(session)?;
    let now = crate::now_ms();
    with_registry(|map| {
        for recording in map.values_mut() {
            expire(recording, now);
        }
        if let Some(existing) = map.get(session) {
            if !existing.stopped {
                return Err("recording_already_active".to_string());
            }
        }
        let active = map.values().filter(|r| !r.stopped).count();
        if active >= MAX_ACTIVE_RECORDINGS {
            return Err("recording_limit_active".to_string());
        }
        map.insert(
            session.to_string(),
            Recording {
                started_ms: now,
                stopped: false,
                bytes: 0,
                next_id: 1,
                events: Vec::new(),
            },
        );
        Ok(json!({
            "session": session,
            "startedAtMs": now,
            "maxDurationMs": DEFAULT_RECORDING_MAX_DURATION_MS,
            "maxBytes": RECORDING_MAX_BYTES,
            "maxActive": MAX_ACTIVE_RECORDINGS,
        }))
    })
}

/// Stops the recording for `session`. Its events stay readable until it is started again.
pub fn stop(session: &str) -> Result<Value, String> {
    validate_session(session)?;
    let now = crate::now_ms();
    with_registry(|map| {
        let recording = map
            .get_mut(session)
            .ok_or_else(|| "recording_not_found".to_string())?;
        recording.stopped = true;
        Ok(json!({
            "session": session,
            "stoppedAtMs": now,
            "eventCount": recording.events.len(),
            "bytes": recording.bytes,
        }))
    })
}

/// Records one raw browser event into the active recording of `session`.
/// Telemetry requests are dropped silently; repeated identical network events
/// are merged into the previous one by raising its `repeat` count.
pub fn record_event(session: &str, raw: &Value) -> Result<(), String> {
    validate_session(session)?;
    let now = crate::now_ms();
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let url = match raw.get("url").and_then(Value::as_str) {
        Some(u) => {
            if is_telemetry_url(u) {
                return Ok(());
            }
            Some(sanitize_recorded_url(u))
        }
        None => None,
    };
    let body = raw.get("body").map(|b| match b {
        Value::String(s) => sanitize_recorded_body(s),
        other => sanitize_recorded_body(&other.to_string()),
    });
    let text = raw
        .get("text")
        .and_then(Value::as_str)
        .map(|t| truncate(t, MAX_TEXT_BYTES));
    let is_error = raw.get("errors").and_then(Value::as_bool).unwrap_or(false)
        || raw.get("error").and_then(Value::as_bool).unwrap_or(false)
        || raw.get("failed").and_then(Value::as_bool).unwrap_or(false)
        || kind == "pageerror"
        || kind == "error"
        || raw.get("status").and_then(Value::as_u64).map_or(false, |s| s >= 400);

    with_registry(|map| {
        let recording = map
            .get_mut(session)
            .ok_or_else(|| "recording_not_active".to_string())?;
        expire(recording, now);
        if recording.stopped {
            return Err("recording_not_active".to_string());
        }
        if kind == "network" {
            if let Some(last) = recording.events.last_mut() {
                if last.kind == "network"
                    && last.url == url
                    && last.text == text
                    && last.body == body
                    && last.is_error == is_error
                {
                    last.repeat += 1;
                    return Ok(());
                }
            }
        }
        let event_id = format!("e{}", recording.next_id);
        let mut event = RecordedEvent {
            event_id,
            kind,
            at_ms: now,
            url,
            text,
            body,
            is_error,
            repeat: 1,
            bytes: 0,
        };
        event.bytes = event_to_json(&event).to_string().len();
        if recording.bytes + event.bytes > RECORDING_MAX_BYTES {
            recording.stopped = true;
            return Err("recording_size_limit".to_string());
        }
        recording.bytes += event.bytes;
        recording.next_id += 1;
        recording.events.push(event);
        Ok(())
    })
}

/// Lists the events of `session` that pass `filter`, in capture order.
pub fn events(session: &str, filter: &EventFilter) -> Result<Value, String> {
    validate_session(session)?;
    let now = crate::now_ms();
    with_registry(|map| {
        let recording = map
            .get_mut(session)
            .ok_or_else(|| "recording_not_found".to_string())?;
        expire(recording, now);
        let exclude = filter.exclude.as_deref().filter(|s| !s.is_empty());
        let list: Vec<Value> = recording
            .events
            .iter()
            .filter(|e| filter.kind.as_deref().map_or(true, |k| e.kind == k))
            .filter(|e| !filter.errors || e.is_error)
            .filter(|e| match (exclude, e.url.as_deref()) {
                (Some(pat), Some(url)) => !url.contains(pat),
                _ => true,
            })
            .map(event_to_json)
            .collect();
        Ok(json!({
            "session": session,
            "count": list.len(),
            "events": list,
        }))
    })
}

/// Returns one recorded event by `event_id`, with its sanitised body.
pub fn frame(session: &str, event_id: &str) -> Result<Value, String> {
    validate_session(session)?;
    with_registry(|map| {
        let recording = map
            .get(session)
            .ok_or_else(|| "recording_not_found".to_string())?;
        recording
            .events
            .iter()
            .find(|e| e.event_id == event_id)
            .map(event_to_json)
            .ok_or_else(|| "recording_event_not_found".to_string())
    })
}

fn event_to_json(e: &RecordedEvent) -> Value {
    json!({
        "eventId": e.event_id,
        "type": e.kind,
        "atMs": e.at_ms,
        "url": e.url,
        "text": e.text,
        "body": e.body,
        "errors": e.is_error,
        "repeat": e.repeat,
    })
}

fn host_of(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    host.split(':').next().unwrap_or("").to_ascii_lowercase()
}

/// True when the URL is a telemetry request (`TELEMETRY_HOSTS`, `TELEMETRY_PATHS`).
pub fn is_telemetry_url(url: &str) -> bool {
    let host = host_of(url);
    let lower = url.to_ascii_lowercase();
    TELEMETRY_HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
        || TELEMETRY_PATHS.iter().any(|p| lower.contains(p))
}

/// Removes URL userinfo and replaces every query value with `[REDACTED]`.
pub fn sanitize_recorded_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let (rest, fragment) = match rest.split_once('#') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let (path_part, query) = match rest.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (rest, None),
    };
    let (authority, path) = match path_part.find('/') {
        Some(i) => (&path_part[..i], &path_part[i..]),
        None => (path_part, ""),
    };
    let authority = authority.rsplit('@').next().unwrap_or("");
    let mut out = String::new();
    if let Some(s) = scheme {
        out.push_str(s);
        out.push_str("://");
    }
    out.push_str(authority);
    out.push_str(path);
    if let Some(q) = query {
        let pairs: Vec<String> = q
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| match p.split_once('=') {
                Some((k, v)) if !v.is_empty() => format!("{k}={REDACTED}"),
                Some((k, _)) => format!("{k}="),
                None => p.to_string(),
            })
            .collect();
        if !pairs.is_empty() {
            out.push('?');
            out.push_str(&pairs.join("&"));
        }
    }
    if fragment.is_some() {
        out.push('#');
    }
    out
}

/// Redacts secret-named values in a JSON body and bounds its size. A body that
/// is not JSON is only bounded.
pub fn sanitize_recorded_body(body: &str) -> String {
    match serde_json::from_str::<Value>(body) {
        Ok(mut value) => {
            redact_value(&mut value);
            truncate(&value.to_string(), MAX_TEXT_BYTES)
        }
        Err(_) => truncate(body, MAX_TEXT_BYTES),
    }
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                let lower = key.to_ascii_lowercase();
                if SECRET_KEYS.iter().any(|k| lower.contains(k)) {
                    *val = Value::String(REDACTED.to_string());
                } else {
                    redact_value(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        _ => {}
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}
