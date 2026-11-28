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
        Op::KI64(n) => Op::KI64(*n),
        Op::SLoad { slot, guarded, snap } => Op::SLoad { slot: *slot, guarded: *guarded, snap: *snap },
        Op::Guard { cmp, a, b, snap } => Op::Guard { cmp: *cmp, a: r(*a), b: r(*b), snap: *snap },
        Op::NumToI64 { a, lim, shift, snap } => Op::NumToI64 { a: r(*a), lim: *lim, shift: *shift, snap: *snap },
        Op::I32ToI64(a) => Op::I32ToI64(r(*a)),
        Op::NegI64(a) => Op::NegI64(r(*a)),
        Op::I64ToNum(a, s) => Op::I64ToNum(r(*a), *s),
        Op::ModI64K(a, k) => Op::ModI64K(r(*a), *k),
        Op::AddI64(a, b) => Op::AddI64(r(*a), r(*b)),
        Op::SubI64(a, b) => Op::SubI64(r(*a), r(*b)),
        Op::MulI64(a, b) => Op::MulI64(r(*a), r(*b)),
        Op::Select { cmp, a, b, t, f } => Op::Select { cmp: *cmp, a: r(*a), b: r(*b), t: r(*t), f: r(*f) },
        Op::GuardTruthy { a, want, snap } => Op::GuardTruthy { a: r(*a), want: *want, snap: *snap },
        Op::GuardSame { a, k, snap } => Op::GuardSame { a: r(*a), k: r(*k), snap: *snap },
        Op::GuardDiff { a, k, snap } => Op::GuardDiff { a: r(*a), k: r(*k), snap: *snap },
        Op::StrCmp { a, b } => Op::StrCmp { a: r(*a), b: r(*b) },
        Op::Add(a, b) => Op::Add(r(*a), r(*b)),
        Op::Sub(a, b) => Op::Sub(r(*a), r(*b)),
        Op::Mul(a, b) => Op::Mul(r(*a), r(*b)),
        Op::Div(a, b) => Op::Div(r(*a), r(*b)),
        Op::Mod(a, b) => Op::Mod(r(*a), r(*b)),
        Op::Pow(a, b) => Op::Pow(r(*a), r(*b)),
        Op::IDiv(a, b) => Op::IDiv(r(*a), r(*b)),
        Op::Neg(a) => Op::Neg(r(*a)),
        Op::Math1(k, a) => Op::Math1(*k, r(*a)),
        Op::NumToStr(a) => Op::NumToStr(r(*a)),
        Op::NumToI32 { a, snap } => Op::NumToI32 { a: r(*a), snap: *snap },
        Op::I32ToNum(a) => Op::I32ToNum(r(*a)),
        Op::AddI32(a, b) => Op::AddI32(r(*a), r(*b)),
        Op::AndI32(a, b) => Op::AndI32(r(*a), r(*b)),
        Op::ASize(a) => Op::ASize(r(*a)),
        Op::ARef { t, idx } => Op::ARef { t: r(*t), idx: r(*idx) },
        Op::ABase(t) => Op::ABase(r(*t)),
        Op::TruncI32(a) => Op::TruncI32(r(*a)),
        Op::XLoad { p, snap } => Op::XLoad { p: r(*p), snap: *snap },
        Op::XStore { p, v } => Op::XStore { p: r(*p), v: r(*v) },
        Op::GuardPlainTable { t, store, snap } => Op::GuardPlainTable { t: r(*t), store: *store, snap: *snap },
        Op::FrameFunc => Op::FrameFunc,
        Op::Random { state } => Op::Random { state: *state },
        Op::Barrier { t, v } => Op::Barrier { t: r(*t), v: r(*v) },
        Op::TMeta { t, snap } => Op::TMeta { t: r(*t), snap: *snap },
        Op::MakeClosure { cl, site, vals, nvals, buf } => {
            let mut v = *vals;
            for x in v.iter_mut().take(*nvals as usize) {
                *x = r(*x);
            }
            Op::MakeClosure { cl: r(*cl), site: *site, vals: v, nvals: *nvals, buf: *buf }
        }
        Op::CCall { slot, nargs, nres, savedpc, frames, off, snap } => Op::CCall { slot: *slot, nargs: *nargs, nres: *nres, savedpc: *savedpc, frames: *frames, off: *off, snap: *snap },
        Op::SReload { slot, snap } => Op::SReload { slot: *slot, snap: *snap },
        Op::Concat { a, b, c, savedpc, frames, off, snap } => Op::Concat { a: *a, b: *b, c: *c, savedpc: *savedpc, frames: *frames, off: *off, snap: *snap },
        Op::UStore { cl, idx, v, buf, snap } => Op::UStore { cl: r(*cl), idx: *idx, v: r(*v), buf: *buf, snap: *snap },
        Op::Not(a) => Op::Not(r(*a)),
        Op::VArith { op, a, b, floor } => Op::VArith { op: *op, a: r(*a), b: r(*b), floor: *floor },
        Op::VNeg(a) => Op::VNeg(r(*a)),
        Op::VargLoad { idx, snap } => Op::VargLoad { idx: r(*idx), snap: *snap },
        Op::CloseUpvals { slot, snap } => Op::CloseUpvals { slot: *slot, snap: *snap },
        Op::GuardTop { top, snap } => Op::GuardTop { top: *top, snap: *snap },
        Op::PopFrame { ret, snap } => Op::PopFrame { ret: *ret, snap: *snap },
        Op::Sunk => Op::Sunk,
        Op::CallC { slot, nargs, nres, savedpc, frames, off, snap } => Op::CallC { slot: *slot, nargs: *nargs, nres: *nres, savedpc: *savedpc, frames: *frames, off: *off, snap: *snap },
        Op::VGet { v, lane } => Op::VGet { v: r(*v), lane: *lane },
        Op::VMake { x, y, z } => Op::VMake { x: r(*x), y: r(*y), z: r(*z) },
        Op::GuardVecEq { a, b, want, snap } => Op::GuardVecEq { a: r(*a), b: r(*b), want: *want, snap: *snap },
        Op::LudToI32(a) => Op::LudToI32(r(*a)),
        Op::I32ToLud(a) => Op::I32ToLud(r(*a)),
        Op::TGet { t, k, buf } => Op::TGet { t: r(*t), k: r(*k), buf: *buf },
        Op::TSet { t, k, v, buf, snap } => Op::TSet { t: r(*t), k: r(*k), v: r(*v), buf: *buf, snap: *snap },
        Op::HMiss { t, k, snap } => Op::HMiss { t: r(*t), k: r(*k), snap: *snap },
        Op::GuardCFunc { f, fp, snap } => Op::GuardCFunc { f: r(*f), fp: *fp, snap: *snap },
        Op::TLen { t } => Op::TLen { t: r(*t) },
        Op::SLen { s } => Op::SLen { s: r(*s) },
        Op::NewTable { narray, nhash } => Op::NewTable { narray: *narray, nhash: *nhash },
        Op::DupTable { tpl } => Op::DupTable { tpl: r(*tpl) },
        Op::Builtin { bfid, args, nargs, nres, buf, snap } => {
            let mut a = *args;
            for x in a.iter_mut().take(*nargs as usize) {
                *x = r(*x);
            }
            Op::Builtin { bfid: *bfid, args: a, nargs: *nargs, nres: *nres, buf: *buf, snap: *snap }
        }
        Op::HSlot { t, idx } => Op::HSlot { t: r(*t), idx: *idx },
        Op::HRef { t, k, idx, lsize, store, snap } => Op::HRef { t: r(*t), k: r(*k), idx: *idx, lsize: *lsize, store: *store, snap: *snap },
        Op::ULoad { cl, idx, by_ref, stack, snap } => Op::ULoad { cl: r(*cl), idx: *idx, by_ref: *by_ref, stack: *stack, snap: *snap },
        Op::GuardUvSlot { cl, idx, slot, snap } => Op::GuardUvSlot { cl: r(*cl), idx: *idx, slot: *slot, snap: *snap },
        Op::ClEnv { cl } => Op::ClEnv { cl: r(*cl) },
        Op::UPtr { cl, idx, stack, snap } => Op::UPtr { cl: r(*cl), idx: *idx, stack: *stack, snap: *snap },
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
        Cmp::ULt => a >= 0.0 && a < b,
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
        Cmp::ULt => (a as u32) < (b as u32),
    }
}
