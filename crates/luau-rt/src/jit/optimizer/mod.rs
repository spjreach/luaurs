//! IR optimization passes (DeepSeek).
//!
//! Each pass is `fn(&mut Trace)`. Passes may rewrite instructions in place
//! and turn removed ones into `Op::Nop`, but must keep every `Ref` index
//! stable (snapshots and Phis point at them) and leave the trace passing
//! `ir::validate`.

use super::trace::Trace;

/// Runs the pass pipeline. (Empty until the passes land.)
pub fn optimize(t: &mut Trace) {
    debug_assert!(super::ir::validate(t).is_ok());
}
