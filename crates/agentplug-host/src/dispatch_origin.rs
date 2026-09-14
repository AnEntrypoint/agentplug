use std::cell::RefCell;

use serde_json::Value;

#[derive(Clone, Default)]
pub(crate) struct DispatchOrigin {
    pub gm_session: Option<String>,
    pub named_page_session: Option<String>,
}

pub(crate) const UNATTRIBUTED_DISPATCH_SESSION: &str = "default";

impl DispatchOrigin {
    pub(crate) fn implicit_page_session(&self, guest_resolved_session: &str) -> String {
        self.gm_session
            .clone()
            .or_else(|| Some(guest_resolved_session.trim().to_string()).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| UNATTRIBUTED_DISPATCH_SESSION.to_string())
    }

    pub(crate) fn page_session(&self, explicit_session_line: Option<String>, guest_resolved_session: &str) -> String {
        explicit_session_line
            .or_else(|| self.named_page_session.clone())
            .unwrap_or_else(|| self.implicit_page_session(guest_resolved_session))
    }
}

thread_local! {
    static CURRENT_DISPATCH_ORIGIN: RefCell<Option<DispatchOrigin>> = const { RefCell::new(None) };
}

pub struct DispatchOriginScope {
    previous: Option<DispatchOrigin>,
}

impl Drop for DispatchOriginScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        CURRENT_DISPATCH_ORIGIN.with(|cell| *cell.borrow_mut() = previous);
    }
}

pub fn enter_dispatch_origin_scope(spool_task: &str, body: &str) -> DispatchOriginScope {
    let origin = dispatch_origin_of(spool_task, body);
    let previous = CURRENT_DISPATCH_ORIGIN.with(|cell| cell.replace(Some(origin)));
    DispatchOriginScope { previous }
}

pub(crate) fn current_dispatch_origin() -> DispatchOrigin {
    CURRENT_DISPATCH_ORIGIN.with(|cell| cell.borrow().clone()).unwrap_or_default()
}

fn dispatch_origin_of(spool_task: &str, body: &str) -> DispatchOrigin {
    let envelope = serde_json::from_str::<Value>(body).ok().filter(Value::is_object);
    let envelope_field = |name: &str| {
        envelope
            .as_ref()
            .and_then(|v| v.get(name))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    DispatchOrigin {
        gm_session: envelope_field("session_id").or_else(|| gm_session_from_spool_task(spool_task)),
        named_page_session: envelope_field("sessionId"),
    }
}

const GM_MCP_TASK_UNIX_MS_MIN_DIGITS: usize = 12;

fn is_counter_segment(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
}

fn gm_session_from_spool_task(spool_task: &str) -> Option<String> {
    let segments: Vec<&str> = spool_task.split('-').collect();
    let n = segments.len();
    let has_gm_mcp_pid_ms_counter_tail = n >= 4
        && is_counter_segment(segments[n - 3])
        && is_counter_segment(segments[n - 2])
        && segments[n - 2].len() >= GM_MCP_TASK_UNIX_MS_MIN_DIGITS
        && is_counter_segment(segments[n - 1]);
    let session_segment_count = if has_gm_mcp_pid_ms_counter_tail {
        n - 3
    } else if n >= 2 && is_counter_segment(segments[n - 1]) {
        n - 1
    } else {
        return None;
    };
    let session = segments[..session_segment_count].join("-");
    Some(session).filter(|s| !s.trim().is_empty())
}
