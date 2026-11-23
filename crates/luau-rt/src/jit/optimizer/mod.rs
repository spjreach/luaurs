//! IR optimization passes (DeepSeek).
//!
//! Each pass is `fn(&mut Trace) -> bool` (`true` when it changed the trace).
//! Passes rewrite instructions in place and turn removed ones into `Op::Nop`,
//! so every `Ref` index stays stable (snapshots and Phis point at them).
//! `optimize_checked` validates after every pass and after the fixpoint.

use super::ir::validate;
use super::trace::Trace;

pub mod constant_folding;
pub mod copy_propagation;
pub mod cse;
pub mod dce;
pub mod forwarding;
pub mod hoist_consts;
pub mod licm;
pub mod strength;
pub mod util;

#[cfg(test)]
mod tests;

/// Runs the pass pipeline to a fixed point, validating after each pass.
pub fn optimize_checked(t: &mut Trace) -> Result<(), String> {
    for _ in 0..5 {
        let mut changed = false;

        changed |= constant_folding::run(t);
        validate(t)?;
        changed |= copy_propagation::run(t);
        validate(t)?;
        changed |= strength::run(t);
        validate(t)?;
        changed |= cse::run(t);
        validate(t)?;
        changed |= forwarding::run(t);
        validate(t)?;
        changed |= dce::run(t);
        validate(t)?;
        changed |= licm::run(t);
        validate(t)?;

        if !changed {
            break;
        }
    }
    validate(t)?;
    hoist_consts::run(t);
    validate(t)
}

/// Convenience entry point for callers that already checked the trace.
/// Debug builds assert on a broken result; release builds leave it to the
/// final `optimize_checked` caller/backend.
pub fn optimize(t: &mut Trace) {
    if let Err(error) = optimize_checked(t) {
        debug_assert!(false, "trace optimizer produced invalid IR: {error}");
    }
}
