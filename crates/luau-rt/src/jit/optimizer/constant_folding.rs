//! Constant folding and propagation.
//!
//! Constants are instructions, so folding is a forward pass: once an
//! instruction's operands are constants, replace the instruction in place
//! with the corresponding constant.  `Ref` indices and snapshot ids stay
//! unchanged.

use super::super::ir::instruction::{Op, Ref};
use super::super::ir::types::IrType;
use super::super::trace::Trace;
use super::util::{compare, compare_i32, const_bool, const_i32, const_num};

fn constant(t: &Trace, r: Ref) -> Option<(Op, IrType)> {
    let inst = t.inst(r);
    match inst.op {
        Op::KNil | Op::KBool(_) | Op::KNum(_) | Op::KInt(_) | Op::KPtr(_) | Op::KI32(_) => Some((inst.op.clone(), inst.ty)),
        _ => None,
    }
}

fn same_constant(a: &Op, b: &Op) -> bool {
    match (a, b) {
        (Op::KNil, Op::KNil) => true,
        (Op::KBool(x), Op::KBool(y)) => x == y,
        (Op::KNum(x), Op::KNum(y)) => x.to_bits() == y.to_bits(),
        (Op::KInt(x), Op::KInt(y)) => x == y,
        (Op::KPtr(x), Op::KPtr(y)) => x == y,
        (Op::KI32(x), Op::KI32(y)) => x == y,
        _ => false,
    }
}

fn fold(t: &Trace, i: usize) -> Option<(Op, IrType)> {
    let inst = &t.insts[i];
    let op = match &inst.op {
        Op::Add(a, b) => Op::KNum(const_num(t, *a)? + const_num(t, *b)?),
        Op::Sub(a, b) => Op::KNum(const_num(t, *a)? - const_num(t, *b)?),
        Op::Mul(a, b) => Op::KNum(const_num(t, *a)? * const_num(t, *b)?),
        Op::Div(a, b) => Op::KNum(const_num(t, *a)? / const_num(t, *b)?),
        Op::Mod(a, b) => {
            let a = const_num(t, *a)?;
            let b = const_num(t, *b)?;
            Op::KNum(a - (a / b).floor() * b)
        }
        Op::IDiv(a, b) => Op::KNum((const_num(t, *a)? / const_num(t, *b)?).floor()),
        Op::Pow(a, b) => Op::KNum(const_num(t, *a)?.powf(const_num(t, *b)?)),
        Op::Neg(a) => Op::KNum(-const_num(t, *a)?),
        Op::AddI32(a, b) => Op::KI32(const_i32(t, *a)?.wrapping_add(const_i32(t, *b)?)),
        Op::I32ToNum(a) => Op::KNum(const_i32(t, *a)? as f64),
        Op::NumToI32 { a, .. } => {
            let n = const_num(t, *a)?;
            if !n.is_finite() || n.fract() != 0.0 {
                return None;
            }
            if n < i32::MIN as f64 || n > i32::MAX as f64 {
                return None;
            }
            Op::KI32(n as i32)
        }
        Op::Guard { cmp, a, b, .. } => {
            let result = if let (Some(a), Some(b)) = (const_num(t, *a), const_num(t, *b)) {
                compare(*cmp, a, b)
            } else if let (Some(a), Some(b)) = (const_i32(t, *a), const_i32(t, *b)) {
                compare_i32(*cmp, a, b)
            } else {
                return None;
            };
            if result {
                Op::Nop
            } else {
                return None;
            }
        }
        Op::GuardTruthy { a, want, .. } => {
            if const_bool(t, *a)? == *want {
                Op::Nop
            } else {
                return None;
            }
        }
        Op::GuardSame { a, k, .. } => match (constant(t, *a), constant(t, *k)) {
            (Some((a, aty)), Some((k, kty))) if aty == kty && same_constant(&a, &k) => Op::Nop,
            _ => return None,
        },
        Op::Phi { pre, back } => match (constant(t, *pre), constant(t, *back)) {
            (Some((a, aty)), Some((b, bty))) if aty == bty && same_constant(&a, &b) => return Some((a, aty)),
            _ => return None,
        },
        _ => return None,
    };
    let ty = if matches!(&op, Op::Nop) { inst.ty } else { op_type(&op) };
    Some((op, ty))
}

fn op_type(op: &Op) -> IrType {
    match op {
        Op::KNil => IrType::Nil,
        Op::KBool(_) => IrType::Boolean,
        Op::KNum(_) => IrType::Number,
        Op::KInt(_) => IrType::Integer,
        Op::KPtr(_) => IrType::Ptr,
        Op::KI32(_) => IrType::I32,
        _ => IrType::Nil,
    }
}

pub fn run(t: &mut Trace) -> bool {
    let mut changed = false;
    for i in 0..t.insts.len() {
        if let Some((op, ty)) = fold(t, i) {
            t.insts[i].op = op;
            t.insts[i].ty = ty;
            changed = true;
        }
    }
    changed
}
