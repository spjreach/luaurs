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

use super::super::ir::instruction::{Cmp, Inst, Op, Ref, SnapId};
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
            Op::I32ToNum(_) | Op::IDiv(..) | Op::Add(..) | Op::Sub(..) | Op::Mul(..) | Op::Mod(..) | Op::Neg(_) | Op::Phi { .. } => t.insts[i].ty == IrType::Number,
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
                // (x - floor(x/y)*y of integers is one; NaN for y = 0
                // truncates to i32::MIN, which bounds checks reject)
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Mod(a, b) => integral[a.index()] && integral[b.index()],
                Op::Neg(a) => integral[a.index()],
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
                Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Mod(a, b) => stack.extend([a.index(), b.index()]),
                Op::Neg(a) => stack.push(a.index()),
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

/// Bound on a loop counter's magnitude for its I32 twin (start and limit
/// are checked to be below it: with a step below 2^16, the counter and a
/// few small constants added to it stay inside i32).
const IND_BOUND: f64 = 1073741824.0; // 2^30
const STEP_BOUND: i32 = 65536;

/// Integer loop counters: a numeric for loop's counter PHI (back value
/// `p + step`, the loop guard comparing that with a limit; start, step and
/// limit from before the loop) gets an I32 twin counting along (start and
/// step converted before the loop, both and the limit checked to keep it
/// in range). Array indices computed from the counter - `TruncI32` of the
/// counter plus/minus small integer constants, modulo powers of two - are
/// then computed on the twin with integer instructions, provably the same
/// values.
pub fn int_induction(t: &mut Trace) -> bool {
    static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *OFF.get_or_init(|| std::env::var_os("LUAU_TRACE_NOINTIND").is_some()) {
        return false;
    }
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    let mut changed = false;
    // the counters
    let mut counters: Vec<(usize, Ref, Ref, Ref)> = Vec::new(); // (phi, start, step, limit)
    for i in l + 1..n {
        let Op::Phi { pre, back } = t.insts[i].op else {
            if matches!(t.insts[i].op, Op::Nop) {
                continue;
            }
            break;
        };
        if t.insts[i].ty != IrType::Number || pre.index() >= l {
            continue;
        }
        let step = match t.insts[back.index()].op {
            Op::Add(a, b) if a.index() == i && b.index() < l => b,
            Op::Add(a, b) if b.index() == i && a.index() < l => a,
            _ => continue,
        };
        let limit = (l + 1..n).find_map(|j| match t.insts[j].op {
            Op::Guard { cmp: Cmp::Le, a, b, .. } if a == back && b.index() < l => Some(b),
            Op::Guard { cmp: Cmp::Le, a, b, .. } if b == back && a.index() < l => Some(a),
            _ => None,
        });
        let Some(limit) = limit else { continue };
        counters.push((i, pre, step, limit));
    }
    for (p, start, step, limit) in counters {
        // which conversions it would serve
        let users: Vec<usize> = (l + 1..t.insts.len()).filter(|&j| matches!(t.insts[j].op, Op::TruncI32(a) if int_expr(t, a, p, 0).is_some())).collect();
        if users.is_empty() {
            continue;
        }
        let mut pre_new = Vec::new();
        let mut body_new = Vec::new();
        let push = |t: &mut Trace, op: Op, ty: IrType| -> usize {
            t.insts.push(Inst { op, ty });
            t.insts.len() - 1
        };
        let snap = SnapId(0);
        let khi = push(t, Op::KNum(IND_BOUND), IrType::Number);
        let klo = push(t, Op::KNum(-IND_BOUND), IrType::Number);
        let shi = push(t, Op::KI32(STEP_BOUND), IrType::I32);
        let slo = push(t, Op::KI32(-STEP_BOUND), IrType::I32);
        pre_new.extend([khi, klo, shi, slo]);
        for v in [start, limit] {
            let g1 = push(t, Op::Guard { cmp: Cmp::Lt, a: v, b: Ref(khi as u32), snap }, IrType::Nil);
            let g2 = push(t, Op::Guard { cmp: Cmp::Lt, a: Ref(klo as u32), b: v, snap }, IrType::Nil);
            pre_new.extend([g1, g2]);
        }
        let i0 = push(t, Op::NumToI32 { a: start, snap }, IrType::I32);
        let is = push(t, Op::NumToI32 { a: step, snap }, IrType::I32);
        let g3 = push(t, Op::Guard { cmp: Cmp::Lt, a: Ref(is as u32), b: Ref(shi as u32), snap }, IrType::Nil);
        let g4 = push(t, Op::Guard { cmp: Cmp::Lt, a: Ref(slo as u32), b: Ref(is as u32), snap }, IrType::Nil);
        pre_new.extend([i0, is, g3, g4]);
        // the twin: placeholder back value fixed below
        let ip = push(t, Op::Phi { pre: Ref(i0 as u32), back: Ref(i0 as u32) }, IrType::I32);
        let ib = push(t, Op::AddI32(Ref(ip as u32), Ref(is as u32)), IrType::I32);
        body_new.push(ib);
        if let Op::Phi { back, .. } = &mut t.insts[ip].op {
            *back = Ref(ib as u32);
        }
        // the conversions, rebuilt on the twin (right after the PHIs)
        let mut head_new = Vec::new();
        let mut repl: Vec<(usize, Ref)> = Vec::new();
        let mut pairs: Vec<(Ref, Ref)> = Vec::new();
        for u in users {
            let Op::TruncI32(a) = t.insts[u].op else { continue };
            if let Some(r) = build_int(t, a, p, ip, &mut head_new, &mut pre_new, &mut pairs) {
                repl.push((u, r));
            }
        }
        if repl.is_empty() {
            continue;
        }
        // the float values themselves (in slots, read by exits): the
        // integer ones converted back - the same numbers (integers, never
        // -0: sums with small constants and powers-of-two remainders)
        let mut floats: Vec<(Ref, usize)> = Vec::new();
        for &(a, r) in &pairs {
            let c = push(t, Op::I32ToNum(r), IrType::Number);
            head_new.push(c);
            floats.push((a, c));
        }
        let mut map: Vec<Ref> = (0..t.insts.len() as u32).map(Ref).collect();
        for &(u, r) in &repl {
            map[u] = r;
            t.insts[u].op = Op::Nop;
        }
        for &(a, c) in &floats {
            map[a.index()] = Ref(c as u32);
        }
        changed = true;
        super::util::rewrite(t, &map);
        // order: pre-loop, new pre-loop, Loop, PHIs, the twin, its uses,
        // body, its back value
        let total = t.insts.len();
        let orig: Vec<usize> = (0..total).filter(|i| !pre_new.contains(i) && !head_new.contains(i) && !body_new.contains(i) && *i != ip).collect();
        let mut order: Vec<usize> = orig.iter().copied().filter(|&i| i < l).collect();
        order.extend(pre_new.iter().copied());
        order.push(l);
        let body: Vec<usize> = orig.iter().copied().filter(|&i| i > l).collect();
        let lead = body.iter().take_while(|&&i| matches!(t.insts[i].op, Op::Phi { .. } | Op::Nop)).count();
        order.extend(body[..lead].iter().copied());
        order.push(ip);
        order.extend(head_new.iter().copied());
        order.extend(body[lead..].iter().copied());
        order.extend(body_new.iter().copied());
        super::hoist_consts::reorder(t, &order);
        // (one counter per pass: indices moved)
        return changed;
    }
    changed
}

/// Bound on the magnitude of `v` as an integer expression of counter
/// `p` (None: not one, or possibly out of i32).
fn int_expr(t: &Trace, v: Ref, p: usize, depth: u32) -> Option<f64> {
    if depth > 6 {
        return None;
    }
    let small = |k: Ref| match t.inst(k).op {
        Op::KNum(c) if c.fract() == 0.0 && c.abs() < STEP_BOUND as f64 => Some(c),
        _ => None,
    };
    let b = match t.inst(v).op {
        _ if v.index() == p => IND_BOUND + STEP_BOUND as f64,
        Op::Add(a, k) | Op::Add(k, a) if small(k).is_some() => int_expr(t, a, p, depth + 1)? + small(k).unwrap().abs(),
        Op::Sub(a, k) if small(k).is_some() => int_expr(t, a, p, depth + 1)? + small(k).unwrap().abs(),
        Op::Mod(a, k) => match t.inst(k).op {
            Op::KNum(c) if c >= 1.0 && c <= 1073741824.0 && (c as u64).is_power_of_two() && c.fract() == 0.0 => {
                int_expr(t, a, p, depth + 1)?;
                c - 1.0
            }
            _ => return None,
        },
        _ => return None,
    };
    (b < 2147483647.0).then_some(b)
}

/// Emits `v` (an `int_expr` of counter `p`) on the twin `ip`: body
/// instructions into `body`, constants into `pre`.
fn build_int(t: &mut Trace, v: Ref, p: usize, ip: usize, body: &mut Vec<usize>, pre: &mut Vec<usize>, pairs: &mut Vec<(Ref, Ref)>) -> Option<Ref> {
    let push = |t: &mut Trace, op: Op, ty: IrType| -> usize {
        t.insts.push(Inst { op, ty });
        t.insts.len() - 1
    };
    let konst = |t: &Trace, k: Ref| match t.inst(k).op {
        Op::KNum(c) => Some(c),
        _ => None,
    };
    if v.index() == p {
        return Some(Ref(ip as u32));
    }
    let (x, c, neg, modk) = match t.inst(v).op {
        Op::Add(a, k) if konst(t, k).is_some() && int_expr(t, a, p, 0).is_some() => (a, konst(t, k)?, false, false),
        Op::Add(k, a) if konst(t, k).is_some() && int_expr(t, a, p, 0).is_some() => (a, konst(t, k)?, false, false),
        Op::Sub(a, k) if konst(t, k).is_some() => (a, konst(t, k)?, true, false),
        Op::Mod(a, k) if konst(t, k).is_some() => (a, konst(t, k)?, false, true),
        _ => return None,
    };
    if let Some(&(_, r)) = pairs.iter().find(|q| q.0 == v) {
        return Some(r);
    }
    let xi = build_int(t, x, p, ip, body, pre, pairs)?;
    let (op_c, op) = if modk { ((c as i64 - 1) as i32, 1) } else { (if neg { -(c as i32) } else { c as i32 }, 0) };
    let kc = push(t, Op::KI32(op_c), IrType::I32);
    pre.push(kc);
    let r = if op == 1 { push(t, Op::AndI32(xi, Ref(kc as u32)), IrType::I32) } else { push(t, Op::AddI32(xi, Ref(kc as u32)), IrType::I32) };
    body.push(r);
    pairs.push((v, Ref(r as u32)));
    Some(Ref(r as u32))
}
