//! Per-process tracing-JIT state: hot counters, compiled traces by loop
//! header, recording status. (Thread-local: Luau states on different OS
//! threads don't share traces.)

use super::hotness::{HotCounters, DEFAULT_HOTLOOP};
use super::trace::{AbortReason, Trace, TraceId};
use crate::lobject::Proto;
use std::cell::RefCell;
use std::collections::HashMap;

pub struct JitState {
    pub enabled: bool,
    pub hotloop: u32,
    /// longest trace (IR instructions) before recording aborts
    pub max_trace_len: usize,
    pub max_traces: usize,
    pub hot: HotCounters,
    /// finished traces by (proto, header pc)
    pub traces: HashMap<(usize, u32), Trace>,
    next_id: u32,
    /// recent aborts, for diagnostics
    pub aborts: Vec<(TraceId, u32, AbortReason)>,
}

impl JitState {
    fn new() -> JitState {
        let env = |k: &str| std::env::var(k).ok();
        JitState {
            enabled: env("LUAU_TRACE").is_some_and(|v| v != "0"),
            hotloop: env("LUAU_TRACE_HOTLOOP").and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_HOTLOOP),
            max_trace_len: env("LUAU_TRACE_MAXLEN").and_then(|v| v.parse().ok()).unwrap_or(4000),
            max_traces: 1000,
            hot: HotCounters::default(),
            traces: HashMap::new(),
            next_id: 1,
            aborts: Vec::new(),
        }
    }

    pub fn new_id(&mut self) -> TraceId {
        let id = TraceId(self.next_id);
        self.next_id += 1;
        id
    }

    pub fn trace_at(&self, p: *mut Proto, pc: u32) -> Option<&Trace> {
        self.traces.get(&(p as usize, pc))
    }
}

thread_local! {
    static STATE: RefCell<JitState> = RefCell::new(JitState::new());
}

/// Runs `f` with this thread's JIT state.
pub fn with<R>(f: impl FnOnce(&mut JitState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub fn enabled() -> bool {
    with(|s| s.enabled)
}
