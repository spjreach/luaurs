//! Per-process tracing-JIT state: hot counters, compiled traces by loop
//! header, recording status. (Thread-local: Luau states on different OS
//! threads don't share traces.)

use super::config::TraceConfig;
use super::hotness::HotCounters;
use super::trace::{AbortReason, Trace, TraceId};
use crate::lobject::Proto;
use std::cell::RefCell;
use std::collections::HashMap as StdMap;

/// FxHash (rustc's): for the JIT's small integer keys - (proto, pc),
/// trace tags - looked up on every trace exit; SipHash cost more than the
/// lookups (hash-flooding resistance isn't needed for internal keys).
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, v: u8) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u16(&mut self, v: u16) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.add(v as u64);
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.add(v);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.add(v as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

impl FxHasher {
    #[inline]
    fn add(&mut self, w: u64) {
        self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

pub type HashMap<K, V> = StdMap<K, V, std::hash::BuildHasherDefault<FxHasher>>;
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
    /// loops (proto, header) whose integer conversions' entry checks
    /// failed: recorded without them from then on
    pub no_int: HashMap<(usize, u32), u8>,
    /// side traces by tag
    pub sides: HashMap<u16, Trace>,
    /// compiled traces by tag: a root's (proto, header), or a side trace
    pub by_tag: HashMap<u16, TraceKey>,
    /// dropped traces by tag (code linked to them may still be running:
    /// their exits must resolve)
    pub dead: HashMap<u16, Trace>,
    /// cells of traces linking to a loop's root trace, by (proto, header)
    pub link_cells: HashMap<(usize, u32), Vec<usize>>,
    next_tag: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceKey {
    Root(usize, u32),
    Side(u16),
    Dead(u16),
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
            traces: HashMap::default(),
            next_id: 1,
            aborts: Vec::new(),
            no_int: HashMap::default(),
            sides: HashMap::default(),
            by_tag: HashMap::default(),
            dead: HashMap::default(),
            link_cells: HashMap::default(),
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
            TraceKey::Dead(t) => self.dead.get_mut(&t),
        }
    }

    /// The loop a trace (by tag) belongs to, and whether it was dropped.
    pub fn root_key_of(&mut self, tag: u16) -> Option<((usize, u32), bool)> {
        let dead = matches!(self.by_tag.get(&tag)?, TraceKey::Dead(_));
        Some((self.by_tag_mut(tag)?.root_key, dead))
    }

    /// Points every link to loop `key` at `reentry` (0: unlinked).
    pub fn set_links(&mut self, key: (usize, u32), reentry: usize) {
        if let Some(cells) = self.link_cells.get(&key) {
            for &c in cells {
                unsafe { *(c as *mut usize) = reentry };
            }
        }
    }

    /// Drops a root trace and its side traces (their code stays: it's only
    /// reachable through the root).
    pub fn remove_root(&mut self, key: (usize, u32)) {
        if let Some(t) = self.traces.remove(&key) {
            bump_generation();
            self.set_links(key, 0);
            for &st in &t.sides {
                if let Some(side) = self.sides.remove(&st) {
                    self.by_tag.insert(st, TraceKey::Dead(st));
                    self.dead.insert(st, side);
                }
            }
            self.by_tag.insert(t.tag, TraceKey::Dead(t.tag));
            self.dead.insert(t.tag, t);
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

thread_local! {
    static GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(1) };
}

/// Changes whenever a compiled trace is dropped (caches of trace entry
/// points compare it).
pub fn generation() -> u64 {
    GENERATION.with(|g| g.get())
}

fn bump_generation() {
    GENERATION.with(|g| g.set(g.get() + 1));
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
        let keys: Vec<(usize, u32)> = state.traces.keys().copied().collect();
        for k in keys {
            state.remove_root(k);
        }
        bump_generation();
        state.hot = HotCounters::default();
        state.aborts.clear();
    });
}
