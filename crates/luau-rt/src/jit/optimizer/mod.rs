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
pub mod hoist_guards;
pub mod licm;
pub mod narrow;
pub mod redundant;
pub mod sink;
pub mod strength;
pub mod util;

#[cfg(test)]
mod tests;

/// Runs the pass pipeline to a fixed point, validating after each pass.
/// Validates the IR between passes in debug builds (or with
/// `LUAU_TRACE_VALIDATE=1`): a full walk per pass, too slow to pay on every
/// trace in release.
fn check(t: &Trace) -> Result<(), String> {
    if cfg!(debug_assertions) || crate::trace_flag!("LUAU_TRACE_VALIDATE") {
        validate(t)
    } else {
        Ok(())
    }
}

pub fn optimize_checked(t: &mut Trace) -> Result<(), String> {
    for _ in 0..5 {
        let mut changed = false;

        changed |= constant_folding::run(t);
        check(t)?;
        changed |= copy_propagation::run(t);
        check(t)?;
        changed |= strength::run(t);
        check(t)?;
        changed |= cse::run(t);
        check(t)?;
        changed |= redundant::run(t);
        check(t)?;
        changed |= forwarding::run(t);
        check(t)?;
        changed |= dce::run(t);
        check(t)?;
        changed |= licm::run(t);
        check(t)?;

        if !changed {
            break;
        }
    }
    check(t)?;
    if narrow::run(t) | narrow::int_induction(t) | narrow::int_mod_phi(t) {
        check(t)?;
        // (the conversions may now be loop-invariant or common)
        for _ in 0..3 {
            let c = cse::run(t) | redundant::run(t) | dce::run(t) | licm::run(t);
            check(t)?;
            if !c {
                break;
            }
        }
    }
    hoist_consts::run(t);
    check(t)?;
    sink::run(t);
    check(t)?;
    // (sinking drops stores that kept loads in the loop)
    if hoist_guards::run(t) {
        check(t)?;
        // (loop values carried in memory become PHIs there: integers too)
        if narrow::int_mod_phi(t) {
            check(t)?;
            let _ = cse::run(t) | dce::run(t);
            check(t)?;
        }
    }
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
