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
        // the step's sign (FORNLOOP guards it before the loop): the loop's
        // own test can then run on the twin
        let zero_k = |r: Ref| matches!(t.inst(r).op, Op::KNum(z) if z == 0.0);
        let sign = (0..l).find_map(|j| match t.insts[j].op {
            Op::Guard { cmp: Cmp::Lt, a, b, .. } if zero_k(a) && b == step => Some(true),
            Op::Guard { cmp: Cmp::Lt, a, b, .. } if a == step && zero_k(b) => Some(false),
            _ => None,
        });
        let back = match t.insts[p].op {
            Op::Phi { back, .. } => back,
            _ => continue,
        };
        let loop_guard = (l + 1..n).find(|&j| match t.insts[j].op {
            Op::Guard { cmp: Cmp::Le, a, b, .. } => (sign == Some(true) && a == back && b == limit) || (sign == Some(false) && a == limit && b == back),
            _ => false,
        });
        // (not in loops with data-dependent branches: iterations through
        // side traces hand the counter back in its stack slot, and the
        // conversions both ways would sit on that path)
        let branchy = (l + 1..n).any(|j| match t.insts[j].op {
            Op::Guard { cmp, a, b, .. } => Some(j) != loop_guard && cmp != Cmp::ULt && (t.ty(a) == IrType::Number || t.ty(b) == IrType::Number),
            _ => false,
        });
        let control = sign.is_some() && loop_guard.is_some() && !branchy && std::env::var_os("LUAU_TRACE_NOICTL").is_none();
        if users.is_empty() && !control {
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
        if repl.is_empty() && !control {
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
        // the loop's test on the twin: i <= limit is i <= floor(limit) for
        // an integer i (>= / ceil going down); the counter's float values
        // are the twin's converted (the float counter then dies)
        let mut after_back: Vec<usize> = Vec::new();
        if control {
            use luau_common::bytecode::BuiltinFunction as B;
            let up = sign == Some(true);
            let lf = push(t, Op::Math1(if up { B::MathFloor as u8 } else { B::MathCeil as u8 }, limit), IrType::Number);
            let ilim = push(t, Op::NumToI32 { a: Ref(lf as u32), snap }, IrType::I32);
            pre_new.extend([lf, ilim]);
            let g = loop_guard.unwrap();
            if let Op::Guard { snap: gs, .. } = t.insts[g].op {
                t.insts[g].op = if up {
                    Op::Guard { cmp: Cmp::Le, a: Ref(ib as u32), b: Ref(ilim as u32), snap: gs }
                } else {
                    Op::Guard { cmp: Cmp::Le, a: Ref(ilim as u32), b: Ref(ib as u32), snap: gs }
                };
            }
            let fp = push(t, Op::I32ToNum(Ref(ip as u32)), IrType::Number);
            head_new.push(fp);
            let fb = push(t, Op::I32ToNum(Ref(ib as u32)), IrType::Number);
            body_new.retain(|&x| x != ib);
            after_back.extend([ib, fb]);
            map.resize(t.insts.len(), Ref(0));
            for (k, m) in map.iter_mut().enumerate() {
                if k >= n && m.0 == 0 {
                    *m = Ref(k as u32);
                }
            }
            map[p] = Ref(fp as u32);
            map[back.index()] = Ref(fb as u32);
        }
        changed = true;
        super::util::rewrite(t, &map);
        // order: pre-loop, new pre-loop, Loop, PHIs, the twin, its uses,
        // body, its back value
        let total = t.insts.len();
        let orig: Vec<usize> = (0..total).filter(|i| !pre_new.contains(i) && !head_new.contains(i) && !body_new.contains(i) && !after_back.contains(i) && *i != ip).collect();
        let mut order: Vec<usize> = orig.iter().copied().filter(|&i| i < l).collect();
        order.extend(pre_new.iter().copied());
        order.push(l);
        let body: Vec<usize> = orig.iter().copied().filter(|&i| i > l).collect();
        let lead = body.iter().take_while(|&&i| matches!(t.insts[i].op, Op::Phi { .. } | Op::Nop)).count();
        order.extend(body[..lead].iter().copied());
        order.push(ip);
        order.extend(head_new.iter().copied());
        for &i in &body[lead..] {
            order.push(i);
            if i == back.index() {
                // (right after the float increment: exits from here on
                // name its value)
                order.extend(after_back.iter().copied());
            }
        }
        order.extend(body_new.iter().copied());
        super::hoist_consts::reorder(t, &order);
        // (one counter per pass: indices moved)
        return changed;
    }
    changed
}

/// Doubles hold every integer below this exactly.
const EXACT: f64 = 4503599627370496.0; // 2^52

/// Integer loop values kept by a modulus: a Number PHI whose back value is
/// `e % c` (c an integer constant, 2 <= c < 2^31) and whose `e` is built
/// from the PHI, integer constants, converted I32s, + - * and negation,
/// with every intermediate below 2^52 in magnitude given |PHI| < c, is
/// computed with 64-bit integers: those doubles are exact integers, so
/// float + - * are the integer ones, and `a - floor(a / c) * c` (Lua's %)
/// is the integer remainder rounded down. The entry value is checked to
/// be an integer (not -0) below c in magnitude before the loop; the float
/// values (for exits, other users) are the integers converted.
pub fn int_mod_phi(t: &mut Trace) -> bool {
    if std::env::var_os("LUAU_TRACE_NOMODPHI").is_some() {
        return false;
    }
    let Some(loop_ref) = t.loop_ref else { return false };
    let l = loop_ref.index();
    let n = t.insts.len();
    for p in l + 1..n {
        let Op::Phi { pre, back } = t.insts[p].op else {
            if matches!(t.insts[p].op, Op::Nop) {
                continue;
            }
            break;
        };
        if t.insts[p].ty != IrType::Number || pre.index() >= l || back.index() <= l {
            continue;
        }
        if let Some((nodes, guards)) = accumulator(t, p, pre, back, l) {
            let pre_guards = guards(t);
            convert_mod_phi(t, p, pre, back, back, None, pre_guards, &nodes);
            return true;
        }
        let Op::Mod(e, k) = t.insts[back.index()].op else { continue };
        let Op::KNum(c) = t.inst(k).op else { continue };
        if !(2.0..2147483648.0).contains(&c) || c.fract() != 0.0 {
            continue;
        }
        let mut nodes = Vec::new();
        let Some(b) = mod_bound(t, e, p, c, l, &mut nodes, 0) else { continue };
        if b >= EXACT {
            continue;
        }
        convert_mod_phi(t, p, pre, back, e, Some(c as i64), Vec::new(), &nodes);
        return true;
    }
    false
}

/// Magnitude bound of `v` (see `int_mod_phi`), its body nodes into
/// `nodes` (operands first).
fn mod_bound(t: &Trace, v: Ref, p: usize, c: f64, l: usize, nodes: &mut Vec<usize>, depth: u32) -> Option<f64> {
    if depth > 10 {
        return None;
    }
    if v.index() == p {
        return Some(c);
    }
    let b = match t.inst(v).op {
        Op::KNum(k) if k.fract() == 0.0 && k.abs() <= 2147483648.0 => return Some(k.abs()),
        Op::I32ToNum(_) => 2147483648.0,
        Op::Add(a, b2) | Op::Sub(a, b2) => mod_bound(t, a, p, c, l, nodes, depth + 1)? + mod_bound(t, b2, p, c, l, nodes, depth + 1)?,
        Op::Mul(a, b2) => mod_bound(t, a, p, c, l, nodes, depth + 1)? * mod_bound(t, b2, p, c, l, nodes, depth + 1)?,
        Op::Neg(a) => mod_bound(t, a, p, c, l, nodes, depth + 1)?,
        _ => return None,
    };
    if v.index() <= l || b >= EXACT {
        return None;
    }
    if !nodes.contains(&v.index()) {
        nodes.push(v.index());
    }
    Some(b)
}

/// An integer accumulator: PHI `p` with back value `p + e` / `e + p` /
/// `p - e`, `e` linear in the loop counter (a * i + b, integer constants)
/// of a loop whose test runs on the counter's I32 twin. Returns `e` and a
/// builder of the pre-loop guards proving every partial sum stays below
/// 2^52: |pre| < 2^51 and, for s = |start| and s = |limit|, the sum of at
/// most 2s + 1 terms each at most A s + B (A = |a|, B = |b|) - at most
/// (3A + 2B) s^2 + A + 3B - below 2^51 (checked on s^2: no abs needed).
#[allow(clippy::type_complexity)]
fn accumulator(t: &Trace, p: usize, pre: Ref, back: Ref, l: usize) -> Option<(Vec<usize>, Box<dyn Fn(&mut Trace) -> Vec<usize>>)> {
    // the back value: + - tree of the PHI (once, added) and terms linear
    // in the counter; per iteration the terms add at most A s + B
    struct W<'a> {
        t: &'a Trace,
        p: usize,
        l: usize,
        ip: Option<usize>,
        nodes: Vec<usize>,
        pcount: u32,
    }
    impl W<'_> {
        /// (coefficient of the PHI, sum of |counter coefficients|, sum
        /// of |constants|) of `v` with sign `sg`
        fn walk(&mut self, v: Ref, sg: f64, depth: u32) -> Option<(f64, f64, f64)> {
            if depth > 10 {
                return None;
            }
            if v.index() == self.p {
                self.pcount += 1;
                return Some((sg, 0.0, 0.0));
            }
            let t = self.t;
            let k = |r: Ref| match t.inst(r).op {
                Op::KNum(c) if c.fract() == 0.0 && c.abs() < 65536.0 => Some(c),
                _ => None,
            };
            let r = match t.inst(v).op {
                Op::KNum(c) if c.fract() == 0.0 && c.abs() < 65536.0 => return Some((0.0, 0.0, c.abs())),
                Op::I32ToNum(w) if matches!(t.inst(w).op, Op::Phi { .. }) => {
                    if self.ip.is_some_and(|x| x != w.index()) {
                        return None;
                    }
                    self.ip = Some(w.index());
                    (0.0, 1.0, 0.0)
                }
                Op::Add(x, y) | Op::Sub(x, y) => {
                    let s2 = if matches!(t.inst(v).op, Op::Sub(..)) { -sg } else { sg };
                    let (p1, a1, b1) = self.walk(x, sg, depth + 1)?;
                    let (p2, a2, b2) = self.walk(y, s2, depth + 1)?;
                    (p1 + p2, a1 + a2, b1 + b2)
                }
                Op::Neg(x) => self.walk(x, -sg, depth + 1)?,
                Op::Mul(x, y) | Op::Mul(y, x) if k(y).is_some() => {
                    let before = self.pcount;
                    let (pc, a, b) = self.walk(x, sg, depth + 1)?;
                    if self.pcount != before || pc != 0.0 {
                        return None;
                    }
                    let c = k(y)?.abs();
                    (0.0, a * c, b * c)
                }
                _ => return None,
            };
            if v.index() <= self.l {
                return None;
            }
            if !self.nodes.contains(&v.index()) {
                self.nodes.push(v.index());
            }
            Some(r)
        }
    }
    let mut w = W { t, p, l, ip: None, nodes: Vec::new(), pcount: 0 };
    let (pc, ca, cb) = w.walk(back, 1.0, 0)?;
    if pc != 1.0 || w.pcount != 1 || w.nodes.last() != Some(&back.index()) {
        return None;
    }
    let (ipo, nodes) = (w.ip, w.nodes);
    if ca > 65536.0 || cb > 65536.0 * 65536.0 {
        return None;
    }
    // the counter: its twin's start and the loop test's limit
    let ip = match ipo {
        Some(ip) => ip,
        // (a constant step: the loop's counter bounds the trip count)
        None => (l + 1..t.insts.len()).take_while(|&j| matches!(t.insts[j].op, Op::Phi { .. } | Op::Nop)).find(|&j| t.insts[j].ty == IrType::I32)?,
    };
    let Op::Phi { pre: ipre, back: ib } = t.insts[ip].op else { return None };
    let Op::NumToI32 { a: start, .. } = t.inst(ipre).op else { return None };
    let limit = (l + 1..t.insts.len()).find_map(|j| match t.insts[j].op {
        Op::Guard { cmp: Cmp::Le, a, b, .. } if a == ib || b == ib => {
            let lim = if a == ib { b } else { a };
            match t.inst(lim).op {
                Op::NumToI32 { a: f, .. } => match t.inst(f).op {
                    Op::Math1(_, x) => Some(x),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    })?;
    if start.index() >= l || limit.index() >= l {
        return None;
    }
    let coef = 3.0 * ca + 2.0 * cb;
    let cst = ca + 3.0 * cb;
    let half = 2251799813685248.0; // 2^51
    Some((
        nodes,
        Box::new(move |t: &mut Trace| {
            let push = |t: &mut Trace, op: Op, ty: IrType| -> usize {
                t.insts.push(Inst { op, ty });
                t.insts.len() - 1
            };
            let snap = SnapId(0);
            let mut out = Vec::new();
            let khi = push(t, Op::KNum(half), IrType::Number);
            let klo = push(t, Op::KNum(-half), IrType::Number);
            let kc = push(t, Op::KNum(coef), IrType::Number);
            let kd = push(t, Op::KNum(cst), IrType::Number);
            out.extend([khi, klo, kc, kd]);
            for g in [
                Op::Guard { cmp: Cmp::Lt, a: pre, b: Ref(khi as u32), snap },
                Op::Guard { cmp: Cmp::Lt, a: Ref(klo as u32), b: pre, snap },
            ] {
                out.push(push(t, g, IrType::Nil));
            }
            for s in [start, limit] {
                let sq = push(t, Op::Mul(s, s), IrType::Number);
                let m = push(t, Op::Mul(Ref(sq as u32), Ref(kc as u32)), IrType::Number);
                let v = push(t, Op::Add(Ref(m as u32), Ref(kd as u32)), IrType::Number);
                let g = push(t, Op::Guard { cmp: Cmp::Lt, a: Ref(v as u32), b: Ref(khi as u32), snap }, IrType::Nil);
                out.extend([sq, m, v, g]);
            }
            out
        }),
    ))
}

/// `c` Some: the modulus PHI (see `int_mod_phi`); None: an accumulator
/// whose back value is the last of `nodes`, `pre_guards` its pre-loop
/// checks (see `accumulator`).
#[allow(clippy::too_many_arguments)]
fn convert_mod_phi(t: &mut Trace, p: usize, pre: Ref, back: Ref, e: Ref, c: Option<i64>, pre_guards: Vec<usize>, nodes: &[usize]) {
    use std::collections::HashMap;
    let l = t.loop_ref.unwrap().index();
    // (the instructions before this conversion: the guards were just added)
    let n = t.insts.len() - pre_guards.len();
    debug_assert!(pre_guards.iter().all(|&g| g >= n));
    let push = |t: &mut Trace, op: Op, ty: IrType| -> usize {
        t.insts.push(Inst { op, ty });
        t.insts.len() - 1
    };
    let snap = SnapId(0);
    // before the loop: (modulus) |pre| < c, an integer
    let mut pre_new = pre_guards;
    if let Some(c) = c {
        let khi = push(t, Op::KNum(c as f64), IrType::Number);
        let klo = push(t, Op::KNum(-(c as f64)), IrType::Number);
        let g1 = push(t, Op::Guard { cmp: Cmp::Lt, a: pre, b: Ref(khi as u32), snap }, IrType::Nil);
        let g2 = push(t, Op::Guard { cmp: Cmp::Lt, a: Ref(klo as u32), b: pre, snap }, IrType::Nil);
        pre_new.extend([khi, klo, g1, g2]);
    }
    // (also the bound body re-entry checks)
    let lim = c.unwrap_or(2251799813685248); // c / 2^51
    let x0 = push(t, Op::NumToI64 { a: pre, lim, snap }, IrType::I64);
    pre_new.push(x0);
    let xp = push(t, Op::Phi { pre: Ref(x0 as u32), back: Ref(x0 as u32) }, IrType::I64);
    // new instructions placed after an original one
    let mut after: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut int_of: HashMap<usize, Ref> = HashMap::new();
    int_of.insert(p, Ref(xp as u32));
    let mut operand = |t: &mut Trace, v: Ref, int_of: &HashMap<usize, Ref>, pre_new: &mut Vec<usize>| -> Ref {
        if let Some(&r) = int_of.get(&v.index()) {
            return r;
        }
        match t.inst(v).op {
            Op::KNum(k) => {
                let r = push(t, Op::KI64(k as i64), IrType::I64);
                pre_new.push(r);
                Ref(r as u32)
            }
            _ => unreachable!("operand without an integer twin"),
        }
    };
    let mut convs: Vec<(usize, Ref)> = Vec::new();
    for &v in nodes {
        let op = match t.insts[v].op {
            Op::I32ToNum(w) => Op::I32ToI64(w),
            Op::Add(a, b) => Op::AddI64(operand(t, a, &int_of, &mut pre_new), operand(t, b, &int_of, &mut pre_new)),
            Op::Sub(a, b) => Op::SubI64(operand(t, a, &int_of, &mut pre_new), operand(t, b, &int_of, &mut pre_new)),
            Op::Mul(a, b) => Op::MulI64(operand(t, a, &int_of, &mut pre_new), operand(t, b, &int_of, &mut pre_new)),
            Op::Neg(a) => Op::NegI64(operand(t, a, &int_of, &mut pre_new)),
            _ => unreachable!(),
        };
        let r = push(t, op, IrType::I64);
        let cv = push(t, Op::I64ToNum(Ref(r as u32)), IrType::Number);
        after.entry(v).or_default().extend([r, cv]);
        int_of.insert(v, Ref(r as u32));
        convs.push((v, Ref(cv as u32)));
    }
    let (ri, rv) = match c {
        Some(c) => {
            let ei = operand(t, e, &int_of, &mut pre_new);
            let ri = push(t, Op::ModI64K(ei, c), IrType::I64);
            let rv = push(t, Op::I64ToNum(Ref(ri as u32)), IrType::Number);
            after.entry(back.index()).or_default().extend([ri, rv]);
            (Ref(ri as u32), Some(rv))
        }
        // (the back value is one of the nodes)
        None => (int_of[&back.index()], None),
    };
    if let Op::Phi { back: b, .. } = &mut t.insts[xp].op {
        *b = ri;
    }
    let pv = push(t, Op::I64ToNum(Ref(xp as u32)), IrType::Number);
    // every use of the floats: the integers converted
    let mut map: Vec<Ref> = (0..t.insts.len() as u32).map(Ref).collect();
    map[p] = Ref(pv as u32);
    if let Some(rv) = rv {
        map[back.index()] = Ref(rv as u32);
    }
    for &(v, cv) in &convs {
        map[v] = cv;
    }
    t.insts[p].op = Op::Nop;
    t.insts[back.index()].op = Op::Nop;
    for &(v, _) in &convs {
        t.insts[v].op = Op::Nop;
    }
    super::util::rewrite(t, &map);
    // order: pre-loop, new pre-loop, Loop, PHIs, the new PHI and its
    // conversion, body with each new instruction after its float one
    let last_phi = (l + 1..n).take_while(|&j| matches!(t.insts[j].op, Op::Phi { .. } | Op::Nop)).last().unwrap_or(l);
    let mut order = Vec::with_capacity(t.insts.len());
    for j in 0..n {
        if j == l {
            order.extend(pre_new.iter().copied());
        }
        order.push(j);
        if j == last_phi {
            order.push(xp);
            order.push(pv);
        }
        if let Some(v) = after.get(&j) {
            order.extend(v.iter().copied());
        }
    }
    // (the PHIs together at the loop head: new instructions anchored to
    // ones among them go after them all)
    let at = order.iter().position(|&j| j == l).unwrap() + 1;
    let (phis, rest): (Vec<usize>, Vec<usize>) = order[at..].iter().partition(|&&j| matches!(t.insts[j].op, Op::Phi { .. }));
    order.truncate(at);
    order.extend(phis);
    order.extend(rest);
    super::hoist_consts::reorder(t, &order);
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
