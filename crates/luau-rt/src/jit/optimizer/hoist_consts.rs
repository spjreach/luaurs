//! Constants used in the loop body move in front of the `Loop` marker.
//!
//! The recorder leaves them in the body (each iteration rematerializes
//! them, keeping their registers free in between). A `movabs` + `movq` per
//! use per iteration costs more than holding them: the backend spills the
//! values live to the loop's end first when registers run out, and a
//! spilled constant is one load. Constants have no operands, so moving one
//! earlier never breaks a definition-before-use order; this pass renumbers
//! the trace (a permutation) and remaps every reference.

use super::super::ir::instruction::{Op, Ref};
use super::super::recorder::remap_operands;
use super::super::trace::Trace;

pub fn run(t: &mut Trace) {
    let Some(loop_ref) = t.loop_ref else { return };
    let l = loop_ref.index();
    let n = t.insts.len();
    let moved: Vec<usize> = (l + 1..n).filter(|&i| t.insts[i].is_const()).collect();
    if !moved.is_empty() {
        move_before_loop(t, &moved);
    }
}

/// Moves instructions `moved` (after the Loop marker, their operands all
/// defined before it) to just before it, in that order: renumbers the
/// trace and remaps every reference.
pub fn move_before_loop(t: &mut Trace, moved: &[usize]) {
    let Some(loop_ref) = t.loop_ref else { return };
    let l = loop_ref.index();
    let n = t.insts.len();
    let set: std::collections::HashSet<usize> = moved.iter().copied().collect();
    // new order: pre-loop, the moved ones, Loop, the rest
    let mut order: Vec<usize> = (0..l).collect();
    order.extend(moved.iter().copied());
    order.push(l);
    order.extend((l + 1..n).filter(|i| !set.contains(i)));
    let mut new_of = vec![0u32; n];
    for (new, &old) in order.iter().enumerate() {
        new_of[old] = new as u32;
    }
    let rm = |r: Ref| Ref(new_of[r.index()]);
    let old = std::mem::take(&mut t.insts);
    t.insts = order
        .iter()
        .map(|&i| {
            let mut inst = old[i].clone();
            remap_operands(&mut inst.op, &rm);
            inst
        })
        .collect();
    for sn in &mut t.snapshots {
        for e in &mut sn.entries {
            e.value = rm(e.value);
        }
        sn.ref_limit = Ref(u32::MAX);
    }
    t.loop_ref = Some(rm(loop_ref));
    // each snapshot covers what is defined before its first user (or
    // everything it names)
    for (i, inst) in t.insts.iter().enumerate() {
        if let Some(s) = inst.snapshot() {
            let sn = &mut t.snapshots[s.0 as usize];
            sn.ref_limit = Ref(sn.ref_limit.0.min(i as u32));
        }
    }
    for sn in &mut t.snapshots {
        if sn.ref_limit.0 == u32::MAX {
            sn.ref_limit = Ref(sn.entries.iter().map(|e| e.value.0 + 1).max().unwrap_or(0));
        }
    }
    let _ = Op::Nop;
}
