//! Per-process tracing-JIT state: hot counters, compiled traces by loop
//! header, recording status. (Thread-local: Luau states on different OS
//! threads don't share traces.)

use super::config::TraceConfig;
use super::hotness::HotCounters;
use super::trace::{AbortReason, Trace, TraceId};
use crate::lobject::Proto;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI8, Ordering};

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
        let config = TraceConfig::global();
        JitState {
            enabled: match ENABLED_OVERRIDE.load(Ordering::Relaxed) {
                0 => false,
                1 => true,
                _ => config.enabled,
            },
            hotloop: config.hotloop,
            max_trace_len: config.max_trace_len,
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

static ENABLED_OVERRIDE: AtomicI8 = AtomicI8::new(-1);

thread_local! {
    static STATE: RefCell<JitState> = RefCell::new(JitState::new());
}

/// Runs `f` with this thread's JIT state.
pub fn with<R>(f: impl FnOnce(&mut JitState) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

/// Like `with`, but does nothing while the thread's state is being torn
/// down or is in use (functions freed during thread exit).
pub fn try_with(f: impl FnOnce(&mut JitState)) {
    let _ = STATE.try_with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            f(&mut s)
        }
    });
}

pub fn enabled() -> bool {
    with(|s| s.enabled)
}

/// Cheap process-level check for interpreter hot paths. Unlike `enabled()`,
/// this avoids touching the thread-local `RefCell` on every interpreter entry.
pub fn trace_enabled() -> bool {
    match ENABLED_OVERRIDE.load(Ordering::Relaxed) {
        0 => false,
        1 => true,
        _ => TraceConfig::global().enabled,
    }
}

/// Enable or disable tracing for this thread and subsequent interpreter
/// entries. The override is process-wide so the cheap interpreter check does
/// not need to borrow thread-local state.
pub fn set_enabled(enabled: bool) {
    ENABLED_OVERRIDE.store(enabled as i8, Ordering::Relaxed);
    with(|state| state.enabled = enabled);
}

/// Discard compiled traces, hot-loop history, and diagnostic aborts.
pub fn flush() {
    with(|state| {
        state.traces.clear();
        state.hot = HotCounters::default();
        state.aborts.clear();
    });
}
