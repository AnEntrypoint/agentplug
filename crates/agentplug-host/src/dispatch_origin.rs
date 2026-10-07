use std::cell::RefCell;

#[derive(Clone, Default)]
pub(crate) struct DispatchOrigin {
    pub spool_task: Option<String>,
    pub submitted_at_ms: Option<u64>,
}

const DISPATCH_ENV_SUBMITTED_AT_MS: &str = "AGENTPLUG_DISPATCH_SUBMITTED_AT_MS";
const DISPATCH_ENV_TASK: &str = "AGENTPLUG_DISPATCH_TASK";
const DISPATCH_ENV_PREFIX: &str = "AGENTPLUG_DISPATCH_";

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

pub fn enter_dispatch_origin_scope(
    spool_task: &str,
    submitted_at_ms: Option<u64>,
) -> DispatchOriginScope {
    let origin = DispatchOrigin {
        spool_task: Some(spool_task.to_string()).filter(|task| !task.is_empty()),
        submitted_at_ms,
    };
    let previous = CURRENT_DISPATCH_ORIGIN.with(|cell| cell.replace(Some(origin)));
    DispatchOriginScope { previous }
}

pub(crate) fn dispatch_env_value(key: &str) -> Option<Option<String>> {
    if !key.starts_with(DISPATCH_ENV_PREFIX) {
        return None;
    }
    let origin = CURRENT_DISPATCH_ORIGIN
        .with(|cell| cell.borrow().clone())
        .unwrap_or_default();
    Some(match key {
        DISPATCH_ENV_SUBMITTED_AT_MS => origin.submitted_at_ms.map(|ms| ms.to_string()),
        DISPATCH_ENV_TASK => origin.spool_task,
        _ => None,
    })
}
