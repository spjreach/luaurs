//! Integer-valued numbers index arrays without the exactness check.
//!
//! `NumToI32` converts and checks that the number was an integer (exit if
//! not). For a value that is integer-valued by construction - an integer
//! constant, a converted I32, floor/ceil/`//`, a sum, difference or product
//! of such values (doubles stay integer-valued; overflow gives inf), or a
//! loop PHI whose entry value and back value are - one truncating
//! conversion (`TruncI32`) gives the same I32. Entry values the proof needs
//! (stack loads before the loop) are checked once, before the loop.
//!
//! A value beyond the I32 range truncates to i32::MIN where the checked
//! conversion would have exited; every `NumToI32` user is a bounds check
//! that then fails (array index: an unsigned compare against the size;
//! `select`: a signed compare of index - 1), so the trace still exits.

use super::super::ir::instruction::{Inst, Op, Ref, SnapId};
use super::super::ir::types::IrType;
use super::super::trace::Trace;
use super::hoist_consts::move_before_loop;
use std::collections::HashSet;

pub fn run(t: &mut Trace) -> bool {
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    // optimistic: everything a proof may use is integral, then knock out
    // what isn't until nothing changes (PHIs need it: they're circular)
    let mut integral = vec![false; n];
    for i in 0..n {
        integral[i] = match t.insts[i].op {
            Op::KNum(v) => v.is_finite() && v.fract() == 0.0,
            Op::I32ToNum(_) | Op::IDiv(..) | Op::Add(..) | Op::Sub(..) | Op::Mul(..) | Op::Phi { .. } => t.insts[i].ty == IrType::Number,
            Op::Math1(b, _) => {
                use luau_common::bytecode::BuiltinFunction as B;
                b == B::MathFloor as u8 || b == B::MathCeil as u8
            }
            // (checked before the loop when a proof uses it)
            Op::SLoad { .. } => i < l && t.insts[i].ty == IrType::Number,
            _ => false,
        };
    }
    loop {
        let mut changed = false;
        for i in 0..n {
            if !integral[i] {
                continue;
            }
            let ok = match t.insts[i].op {
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) => integral[a.index()] && integral[b.index()],
                Op::Phi { pre, back } => integral[pre.index()] && integral[back.index()],
                _ => true,
            };
            if !ok {
                integral[i] = false;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // the checked conversions that can go, and the loads their proofs use
    let mut need: HashSet<usize> = HashSet::new();
    let mut changed = false;
    for i in 0..n {
        let Op::NumToI32 { a, .. } = t.insts[i].op else { continue };
        if !integral[a.index()] {
            continue;
        }
        let mut stack = vec![a.index()];
        let mut seen = HashSet::new();
        while let Some(v) = stack.pop() {
            if !seen.insert(v) {
                continue;
            }
            match t.insts[v].op {
                Op::SLoad { .. } => {
                    need.insert(v);
                }
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) => stack.extend([a.index(), b.index()]),
                Op::Phi { pre, back } => stack.extend([pre.index(), back.index()]),
                _ => {}
            }
        }
        t.insts[i].op = Op::TruncI32(a);
        changed = true;
    }
    if !changed {
        return false;
    }
    // the entry checks (exit through the entry snapshot: nothing done yet)
    let mut need: Vec<usize> = need.into_iter().collect();
    need.sort_unstable();
    let mut added = Vec::new();
    for s in need {
        added.push(t.insts.len());
        t.insts.push(Inst { op: Op::NumToI32 { a: Ref(s as u32), snap: SnapId(0) }, ty: IrType::I32 });
    }
    if !added.is_empty() {
        move_before_loop(t, &added);
    }
    true
}
