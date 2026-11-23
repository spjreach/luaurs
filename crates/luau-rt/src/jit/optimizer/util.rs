//! Shared helpers for optimizer passes: ref remapping, constant lookup and
//! liveness.  The IR is a linear SSA list whose `Ref`s are instruction
//! indices; passes remove instructions by replacing them with `Op::Nop` and
//! remap uses, so every original `Ref` remains a valid index.

use super::super::ir::instruction::{Cmp, Inst, Op, Ref};
use super::super::ir::types::IrType;
use super::super::trace::Trace;
use std::collections::HashSet;

/// Follow a chain of replacements to the current definition.
pub fn resolve(map: &[Ref], mut r: Ref) -> Ref {
    while map[r.index()] != r {
        r = map[r.index()];
    }
    r
}

pub fn op_operands(op: &Op) -> Vec<Ref> {
    Inst { op: op.clone(), ty: IrType::Nil }.operands()
}

pub fn identity(t: &Trace) -> Vec<Ref> {
    (0..t.insts.len()).map(|i| Ref(i as u32)).collect()
}

/// Clone `op` with every operand resolved through `map`.
pub fn map_op(op: &Op, map: &[Ref]) -> Op {
    let r = |x: Ref| resolve(map, x);
    match op {
        Op::KNil => Op::KNil,
        Op::KBool(b) => Op::KBool(*b),
        Op::KNum(n) => Op::KNum(*n),
        Op::KInt(n) => Op::KInt(*n),
        Op::KPtr(p) => Op::KPtr(*p),
        Op::KI32(n) => Op::KI32(*n),
        Op::SLoad { slot, guarded, snap } => Op::SLoad { slot: *slot, guarded: *guarded, snap: *snap },
        Op::Guard { cmp, a, b, snap } => Op::Guard { cmp: *cmp, a: r(*a), b: r(*b), snap: *snap },
        Op::GuardTruthy { a, want, snap } => Op::GuardTruthy { a: r(*a), want: *want, snap: *snap },
        Op::GuardSame { a, k, snap } => Op::GuardSame { a: r(*a), k: r(*k), snap: *snap },
        Op::Add(a, b) => Op::Add(r(*a), r(*b)),
        Op::Sub(a, b) => Op::Sub(r(*a), r(*b)),
        Op::Mul(a, b) => Op::Mul(r(*a), r(*b)),
        Op::Div(a, b) => Op::Div(r(*a), r(*b)),
        Op::Mod(a, b) => Op::Mod(r(*a), r(*b)),
        Op::Pow(a, b) => Op::Pow(r(*a), r(*b)),
        Op::IDiv(a, b) => Op::IDiv(r(*a), r(*b)),
        Op::Neg(a) => Op::Neg(r(*a)),
        Op::Math1(k, a) => Op::Math1(*k, r(*a)),
        Op::NumToI32 { a, snap } => Op::NumToI32 { a: r(*a), snap: *snap },
        Op::I32ToNum(a) => Op::I32ToNum(r(*a)),
        Op::AddI32(a, b) => Op::AddI32(r(*a), r(*b)),
        Op::ASize(a) => Op::ASize(r(*a)),
        Op::ARef { t, idx } => Op::ARef { t: r(*t), idx: r(*idx) },
        Op::XLoad { p, snap } => Op::XLoad { p: r(*p), snap: *snap },
        Op::XStore { p, v } => Op::XStore { p: r(*p), v: r(*v) },
        Op::GuardPlainTable { t, store, snap } => Op::GuardPlainTable { t: r(*t), store: *store, snap: *snap },
        Op::FrameFunc => Op::FrameFunc,
        Op::Barrier { t, v } => Op::Barrier { t: r(*t), v: r(*v) },
        Op::TLen { t } => Op::TLen { t: r(*t) },
        Op::SLen { s } => Op::SLen { s: r(*s) },
        Op::NewTable { narray, nhash } => Op::NewTable { narray: *narray, nhash: *nhash },
        Op::DupTable { tpl } => Op::DupTable { tpl: r(*tpl) },
        Op::HSlot { t, idx } => Op::HSlot { t: r(*t), idx: *idx },
        Op::HRef { t, k, idx, lsize, store, snap } => Op::HRef { t: r(*t), k: r(*k), idx: *idx, lsize: *lsize, store: *store, snap: *snap },
        Op::ULoad { cl, idx, by_ref, stack, snap } => Op::ULoad { cl: r(*cl), idx: *idx, by_ref: *by_ref, stack: *stack, snap: *snap },
        Op::GuardUvSlot { cl, idx, slot, snap } => Op::GuardUvSlot { cl: r(*cl), idx: *idx, slot: *slot, snap: *snap },
        Op::ClEnv { cl } => Op::ClEnv { cl: r(*cl) },
        Op::GuardProto { f, p, snap } => Op::GuardProto { f: r(*f), p: r(*p), snap: *snap },
        Op::GuardFlag { p, off, snap } => Op::GuardFlag { p: r(*p), off: *off, snap: *snap },
        Op::Loop => Op::Loop,
        Op::Phi { pre, back } => Op::Phi { pre: r(*pre), back: r(*back) },
        Op::Nop => Op::Nop,
    }
}

/// Rewrite every operand and every snapshot value through `map`.
pub fn rewrite(t: &mut Trace, map: &[Ref]) {
    for inst in &mut t.insts {
        inst.op = map_op(&inst.op, map);
    }
    for snap in &mut t.snapshots {
        for entry in &mut snap.entries {
            entry.value = resolve(map, entry.value);
        }
    }
}

/// A cheap use map.  Snapshot entries count as uses even if no current guard
/// names the snapshot: the backend can still emit an exit that needs them,
/// and keeping a few extra values is safer than removing a live one.
pub fn used_refs(t: &Trace) -> Vec<bool> {
    let mut used = vec![false; t.insts.len()];
    let mut mark = |r: Ref| {
        if r.index() < used.len() {
            used[r.index()] = true;
        }
    };
    for inst in &t.insts {
        for operand in inst.operands() {
            mark(operand);
        }
    }
    for snap in &t.snapshots {
        for entry in &snap.entries {
            mark(entry.value);
        }
    }
    used
}

/// Refs used as the `back` operand of a `Phi`.  They must stay in the loop
/// body; lowering one to a pre-loop value would fail `validate`.
pub fn phi_back_refs(t: &Trace) -> HashSet<Ref> {
    let mut refs = HashSet::new();
    for inst in &t.insts {
        if let Op::Phi { back, .. } = inst.op {
            refs.insert(back);
        }
    }
    refs
}

pub fn loop_index(t: &Trace) -> Option<usize> {
    t.insts.iter().position(|inst| matches!(inst.op, Op::Loop))
}

pub fn const_num(t: &Trace, r: Ref) -> Option<f64> {
    match t.inst(r).op {
        Op::KNum(n) => Some(n),
        _ => None,
    }
}

pub fn const_i32(t: &Trace, r: Ref) -> Option<i32> {
    match t.inst(r).op {
        Op::KI32(n) => Some(n),
        _ => None,
    }
}

pub fn const_bool(t: &Trace, r: Ref) -> Option<bool> {
    match t.inst(r).op {
        Op::KBool(b) => Some(b),
        Op::KNil => Some(false),
        Op::KNum(_) | Op::KInt(_) | Op::KPtr(_) | Op::KI32(_) => Some(true),
        _ => None,
    }
}

/// Compare two constant operands for the guard comparison ops.
pub fn compare(cmp: Cmp, a: f64, b: f64) -> bool {
    match cmp {
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        Cmp::Nlt => !(a < b),
        Cmp::Nle => !(a <= b),
    }
}

pub fn compare_i32(cmp: Cmp, a: i32, b: i32) -> bool {
    match cmp {
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        Cmp::Nlt => !(a < b),
        Cmp::Nle => !(a <= b),
    }
}
