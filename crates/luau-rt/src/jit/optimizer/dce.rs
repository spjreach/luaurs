//! Dead-code elimination.
//!
//! Removes pure instructions (and unused Phis) whose result is never read.
//! `Nop` is used instead of deleting an entry so refs stay valid.  Snapshot
//! entries are treated as uses; an exit can still need them after a guard is
//! rewritten, and keeping a value is cheaper than a dangling ref.

use super::super::ir::instruction::Op;
use super::super::trace::Trace;
use super::util::used_refs;

pub fn run(t: &mut Trace) -> bool {
    let mut any = false;
    loop {
        let used = used_refs(t);
        let mut changed = false;
        for i in 0..t.insts.len() {
            if used[i] {
                continue;
            }
            let removable = t.insts[i].is_pure() || matches!(t.insts[i].op, Op::Phi { .. });
            if removable && !matches!(t.insts[i].op, Op::Nop) {
                t.insts[i].op = Op::Nop;
                changed = true;
                any = true;
            }
        }
        if !changed {
            break;
        }
    }
    any
}
