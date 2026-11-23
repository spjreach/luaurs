//! Loop-invariant code motion.
//!
//! The IR deliberately keeps `Ref`s stable, so a pass cannot insert a new
//! instruction and shift later indices.  Instead, an invariant body
//! instruction is copied into an unused `Nop` slot before `Loop`; that slot
//! is a real pre-loop definition, and all uses are remapped to it.
//!
//! Values used as a `Phi back` operand are not moved: `validate` requires
//! them to stay in the loop body.  If DCE left no suitable pre-loop slot the
//! instruction is left where it is.

use super::super::ir::instruction::{Op, Ref};
use super::super::trace::Trace;
use super::util::{identity, loop_index, map_op, op_operands, phi_back_refs, rewrite, used_refs};

pub fn run(t: &mut Trace) -> bool {
    let Some(loop_at) = loop_index(t) else { return false };
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    let mut used = used_refs(t);
    let mut changed = false;

    for i in (loop_at + 1)..t.insts.len() {
        let inst = &t.insts[i];
        if !inst.is_pure() || inst.is_const() || matches!(inst.op, Op::Nop | Op::Phi { .. }) {
            continue;
        }
        if t.inserts && matches!(inst.op, Op::ARef { .. } | Op::HSlot { .. } | Op::ABase(_)) {
            continue;
        }
        if backs.contains(&Ref(i as u32)) {
            continue;
        }

        let op = map_op(&inst.op, &map);
        let operands = op_operands(&op);
        if operands.iter().any(|r| r.index() >= loop_at) {
            continue;
        }
        let after = operands.iter().map(|r| r.index()).max().map_or(0, |n| n + 1);

        let Some(slot) = (after..loop_at).find(|&j| matches!(t.insts[j].op, Op::Nop) && !used[j]) else {
            continue;
        };
        let ty = t.insts[i].ty;
        t.insts[slot].op = op;
        t.insts[slot].ty = ty;
        used[slot] = true;
        map[i] = Ref(slot as u32);
        t.insts[i].op = Op::Nop;
        changed = true;
    }

    if changed {
        rewrite(t, &map);
    }
    changed
}
