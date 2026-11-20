//! Copy propagation and algebraic identities.
//!
//! There is no explicit `Copy` op in this IR, so an identity is handled by
//! remapping all uses to an existing ref and leaving a `Nop` behind.  A
//! `Phi { pre, back }` with `pre == back` is a copy too.

use super::super::ir::instruction::{Op, Ref};
use super::super::trace::Trace;
use super::util::{const_i32, const_num, identity, phi_back_refs, resolve, rewrite};

fn target(t: &Trace, i: usize, map: &[Ref]) -> Option<Ref> {
    let inst = &t.insts[i];
    let r = |x: Ref| resolve(map, x);
    let target = match &inst.op {
        Op::Phi { pre, back } if r(*pre) == r(*back) => r(*pre),
        Op::Mul(a, b) => {
            let (a, b) = (r(*a), r(*b));
            if const_num(t, b) == Some(1.0) {
                a
            } else if const_num(t, a) == Some(1.0) {
                b
            } else {
                return None;
            }
        }
        Op::Div(a, b) if const_num(t, r(*b)) == Some(1.0) => r(*a),
        Op::Sub(a, b) if const_num(t, r(*b)).is_some_and(|n| n.to_bits() == 0.0f64.to_bits()) => r(*a),
        Op::AddI32(a, b) => {
            let (a, b) = (r(*a), r(*b));
            if const_i32(t, b) == Some(0) {
                a
            } else if const_i32(t, a) == Some(0) {
                b
            } else {
                return None;
            }
        }
        _ => return None,
    };
    (target.index() < i).then_some(target)
}

pub fn run(t: &mut Trace) -> bool {
    let backs = phi_back_refs(t);
    let mut map = identity(t);
    let mut changed = false;
    for i in 0..t.insts.len() {
        if backs.contains(&Ref(i as u32)) {
            continue;
        }
        if let Some(target) = target(t, i, &map) {
            map[i] = target;
            t.insts[i].op = Op::Nop;
            changed = true;
        }
    }
    if changed {
        rewrite(t, &map);
    }
    changed
}
