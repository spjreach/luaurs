//! Strength reduction and cheap algebraic rewrites.
//!
//! Only transformations that are exact for IEEE-754 are applied.  In
//! particular, `x * 0` is not folded to zero because `inf * 0` is NaN, and
//! `x + 0` is skipped because it changes `-0`.
//!
//! Instructions are rewritten in place, so `Ref`s stay stable.

use super::super::ir::instruction::{Op, Ref};
use super::super::ir::types::IrType;
use super::super::trace::Trace;
use super::util::const_num;

fn two(t: &Trace, r: Ref) -> bool {
    const_num(t, r).is_some_and(|n| n.to_bits() == 2.0f64.to_bits())
}

pub fn run(t: &mut Trace) -> bool {
    let mut changed = false;
    for i in 0..t.insts.len() {
        let op = t.insts[i].op.clone();
        if let Op::Mul(a, b) = op {
            // x * 2 == x + x, including NaN and infinities.
            if two(t, b) {
                t.insts[i].op = Op::Add(a, a);
                t.insts[i].ty = IrType::Number;
                changed = true;
            } else if two(t, a) {
                t.insts[i].op = Op::Add(b, b);
                t.insts[i].ty = IrType::Number;
                changed = true;
            }
        }
    }
    changed
}
