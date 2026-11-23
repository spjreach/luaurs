//! Hot-loop detection.
//!
//! Every backward branch (FORNLOOP, FORGLOOP, JUMPBACK) counts executions of
//! its loop header, keyed by (function prototype, header pc). When a header
//! reaches the threshold, the interpreter asks for a trace to be recorded
//! there. A loop whose recording keeps failing waits longer each time
//! (the threshold doubles) and is given up on after `MAX_PENALTY` failures.

use crate::lobject::Proto;
use std::collections::HashMap;

/// LuaJIT's default `hotloop`.
pub const DEFAULT_HOTLOOP: u32 = 56;
/// Failed recordings before a loop is blacklisted.
pub const MAX_PENALTY: u8 = 6;

#[derive(Default, Clone, Copy)]
struct Counter {
    count: u32,
    penalty: u8,
    /// recordings that left the loop (every fourth one counts as a failure:
    /// a loop that usually runs one or two iterations keeps doing that)
    left: u8,
}

#[derive(Default)]
pub struct HotCounters {
    counters: HashMap<(usize, u32), Counter>,
}

impl HotCounters {
    /// Counts one execution of the loop at `header` in `p`; true when the
    /// loop just became hot (the counter restarts).
    pub fn tick(&mut self, p: *mut Proto, header: u32, threshold: u32) -> bool {
        let c = self.counters.entry((p as usize, header)).or_default();
        if c.penalty >= MAX_PENALTY {
            return false;
        }
        c.count += 1;
        if c.count >= threshold << c.penalty {
            c.count = 0;
            true
        } else {
            false
        }
    }

    /// A recording at this loop failed: wait twice as long next time.
    pub fn penalize(&mut self, p: *mut Proto, header: u32) {
        let c = self.counters.entry((p as usize, header)).or_default();
        c.penalty = c.penalty.saturating_add(1);
        c.count = 0;
    }

    /// A recording left the loop during its iteration.
    pub fn left_loop(&mut self, p: *mut Proto, header: u32) {
        let c = self.counters.entry((p as usize, header)).or_default();
        c.left = c.left.wrapping_add(1);
        if c.left % 4 == 0 {
            c.penalty = c.penalty.saturating_add(1);
        }
        c.count = 0;
    }

    pub fn is_blacklisted(&self, p: *mut Proto, header: u32) -> bool {
        self.counters.get(&(p as usize, header)).is_some_and(|c| c.penalty >= MAX_PENALTY)
    }

    /// Forgets a freed function's loops.
    pub fn forget_proto(&mut self, p: *mut Proto) {
        self.counters.retain(|&(q, _), _| q != p as usize);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_then_backoff_then_blacklist() {
        let mut h = HotCounters::default();
        let p = 0x1000 as *mut Proto;
        assert!((0..9).all(|_| !h.tick(p, 3, 10)));
        assert!(h.tick(p, 3, 10));
        h.penalize(p, 3);
        assert!((0..19).all(|_| !h.tick(p, 3, 10)));
        assert!(h.tick(p, 3, 10));
        for _ in 0..MAX_PENALTY {
            h.penalize(p, 3);
        }
        assert!(h.is_blacklisted(p, 3));
        assert!((0..10_000).all(|_| !h.tick(p, 3, 10)));
    }
}
