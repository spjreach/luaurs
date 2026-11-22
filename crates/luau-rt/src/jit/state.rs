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
    /// side traces by tag
    pub sides: HashMap<u16, Trace>,
    /// compiled traces by tag: a root's (proto, header), or a side trace
    pub by_tag: HashMap<u16, TraceKey>,
    next_tag: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceKey {
    Root(usize, u32),
    Side(u16),
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
            sides: HashMap::new(),
            by_tag: HashMap::new(),
            next_tag: 1,
        }
    }

    /// A tag for a trace about to be compiled (None: all 32767 used).
    pub fn new_tag(&mut self) -> Option<u16> {
        if self.next_tag >= 0x7fff {
            return None;
        }
        let t = self.next_tag;
        self.next_tag += 1;
        Some(t)
    }

    /// Remembers a recent abort (the last 256).
    pub fn note_abort(&mut self, a: (TraceId, u32, AbortReason)) {
        if self.aborts.len() >= 256 {
            self.aborts.drain(..128);
        }
        self.aborts.push(a);
    }

    pub fn by_tag_mut(&mut self, tag: u16) -> Option<&mut Trace> {
        match *self.by_tag.get(&tag)? {
            TraceKey::Root(p, pc) => self.traces.get_mut(&(p, pc)),
            TraceKey::Side(t) => self.sides.get_mut(&t),
        }
    }

    /// Drops a root trace and its side traces (their code stays: it's only
    /// reachable through the root).
    pub fn remove_root(&mut self, key: (usize, u32)) {
        if let Some(t) = self.traces.remove(&key) {
            self.by_tag.remove(&t.tag);
            for st in t.sides {
                self.sides.remove(&st);
                self.by_tag.remove(&st);
            }
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
        state.sides.clear();
        state.by_tag.clear();
        state.hot = HotCounters::default();
        state.aborts.clear();
    });
}
