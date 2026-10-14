use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard};

pub const EXEC_WORKERS: usize = 4;
pub const BACKGROUND_AFTER_MS: u64 = 15_000;

struct Gate {
    running: usize,
    next_ticket: u64,
    queue: VecDeque<u64>,
}

static GATE: Mutex<Gate> = Mutex::new(Gate {
    running: 0,
    next_ticket: 0,
    queue: VecDeque::new(),
});
static RELEASED: Condvar = Condvar::new();

fn gate() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|error| error.into_inner())
}

pub struct WorkerLease;

pub fn acquire() -> WorkerLease {
    let mut state = gate();
    let ticket = state.next_ticket;
    state.next_ticket += 1;
    state.queue.push_back(ticket);
    while !(state.running < EXEC_WORKERS && state.queue.front() == Some(&ticket)) {
        state = RELEASED
            .wait(state)
            .unwrap_or_else(|error| error.into_inner());
    }
    state.queue.pop_front();
    state.running += 1;
    RELEASED.notify_all();
    WorkerLease
}

impl Drop for WorkerLease {
    fn drop(&mut self) {
        let mut state = gate();
        state.running -= 1;
        RELEASED.notify_all();
    }
}
