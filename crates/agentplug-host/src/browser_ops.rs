// Host operations behind the guest's browser verbs (rows 2, 13 to 18, 63 to 65). One entry point, reply(),
// serves both the component host (component.rs) and the core-module host (imports.rs), so each op has one body.

use std::path::Path;

use serde_json::{json, Value};

use crate::action_recorder::{self, EventFilter};
use crate::{browser, browser_lease};

const RESET_TTL_MS: u64 = 60_000;
const RESET_WAIT_MS: u64 = 30_000;

fn parse_body(body: &str) -> Result<Value, String> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(body).map_err(|e| format!("body must be JSON: {e}"))
}

fn text<'a>(req: &'a Value, key: &str) -> &'a str {
    req.get(key).and_then(Value::as_str).unwrap_or("")
}

fn lift(op: &str, result: Result<Value, String>) -> Value {
    match result {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "op": op, "error": e }),
    }
}

// op: session-list | session-delete | session-update | session-reset | browser-reset | browser-list
//     | recorder-start | recorder-stop | recorder-events | recorder-frame
// body: JSON. session-* take {"name"}; session-update also {"title"?, "color"?}; recorder-* take {"session"};
// recorder-events takes {"type"?, "errors"?, "exclude"?}; recorder-frame takes {"eventId"}.
pub fn reply(root: &Path, op: &str, body: &str) -> Value {
    let req = match parse_body(body) {
        Ok(req) => req,
        Err(e) => return json!({ "ok": false, "op": op, "error": e }),
    };
    match op {
        "session-list" => browser::session_list(root),
        "session-delete" => browser::session_delete(root, text(&req, "name")),
        "session-update" => browser::session_update(
            root,
            text(&req, "name"),
            req.get("title").and_then(Value::as_str),
            req.get("color").and_then(Value::as_str),
        ),
        "session-reset" => browser::session_reset(root, text(&req, "name")),
        "browser-reset" => {
            let agent = browser_lease::next_agent_id();
            match browser_lease::reset_session(root, &agent, RESET_TTL_MS, RESET_WAIT_MS) {
                Ok(()) => json!({ "ok": true, "op": op, "reset": true }),
                Err(e) => json!({ "ok": false, "op": op, "error": e }),
            }
        }
        "browser-list" => browser_lease::list_browsers(),
        "recorder-start" => lift(op, action_recorder::start(text(&req, "session"))),
        "recorder-stop" => lift(op, action_recorder::stop(text(&req, "session"))),
        "recorder-events" => {
            let filter = EventFilter {
                kind: req.get("type").and_then(Value::as_str).map(str::to_string),
                errors: req.get("errors").and_then(Value::as_bool).unwrap_or(false),
                exclude: req.get("exclude").and_then(Value::as_str).map(str::to_string),
            };
            lift(op, action_recorder::events(text(&req, "session"), &filter))
        }
        "recorder-frame" => lift(op, action_recorder::frame(text(&req, "session"), text(&req, "eventId"))),
        other => json!({ "ok": false, "op": other, "error": format!("unknown browser op {other}") }),
    }
}
