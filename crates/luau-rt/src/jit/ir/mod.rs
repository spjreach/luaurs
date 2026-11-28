//! The tracing JIT's intermediate representation: linear SSA (see
//! `instruction`), built by `builder` while recording, checked by
//! `validate` before optimization and compilation.

pub mod builder;
pub mod instruction;
pub mod types;

use crate::jit::trace::Trace;
use instruction::{Op, Ref};
use types::IrType;

/// Checks the structural invariants every pass and the backend rely on:
/// operands defined before use (Phi `back` operands excepted: they come from
/// the loop body), at most one `Loop`, Phis right after it, snapshots that
/// only name values defined before them, and operand types that fit.
pub fn validate(t: &Trace) -> Result<(), String> {
    let n = t.insts.len();
    let mut loop_at: Option<usize> = None;
    for (i, inst) in t.insts.iter().enumerate() {
        let here = Ref(i as u32);
        match &inst.op {
            Op::Loop => {
                if loop_at.is_some() {
                    return Err(format!("{}: second Loop", i));
                }
                loop_at = Some(i);
            }
            Op::Phi { pre, back } => {
                let Some(l) = loop_at else { return Err(format!("{}: Phi before Loop", i)) };
                // (the backend takes the PHIs as one block at the loop head)
                if t.insts[l + 1..i].iter().any(|x| !matches!(x.op, Op::Phi { .. } | Op::Nop)) {
                    return Err(format!("{}: Phi after a body instruction", i));
                }
                if pre.index() >= l {
                    return Err(format!("{}: Phi pre {} not before the loop", i, pre.0));
                }
                if back.index() >= n || back.index() <= l {
                    return Err(format!("{}: Phi back {} not in the loop body", i, back.0));
                }
                if t.ty(*pre) != t.ty(*back) || t.ty(*pre) != inst.ty {
                    return Err(format!("{}: Phi types differ", i));
                }
            }
            _ => {
                for o in inst.operands() {
                    if o < here && matches!(t.insts[o.index()].op, Op::Nop) {
                        return Err(format!("{}: operand {} was removed", i, o.0));
                    }
                    if o >= here {
                        return Err(format!("{}: operand {} used before its definition", i, o.0));
                    }
                }
            }
        }
        if let Some(s) = inst.snapshot() {
            let Some(snap) = t.snapshots.get(s.0 as usize) else { return Err(format!("{}: snapshot {} missing", i, s.0)) };
            for e in &snap.entries {
                if e.value >= snap.ref_limit || e.value.index() >= n {
                    return Err(format!("{}: snapshot {} names a later value {}", i, s.0, e.value.0));
                }
                if !t.ty(e.value).is_lua() {
                    return Err(format!("{}: snapshot {} stores a non-Lua value {}", i, s.0, e.value.0));
                }
            }
        }
        let num = |r: &Ref| t.ty(*r) == IrType::Number;
        let ok = match &inst.op {
            Op::Add(a, b) | Op::Sub(a, b) | Op::Mul(a, b) | Op::Div(a, b) | Op::Mod(a, b) | Op::Pow(a, b) | Op::IDiv(a, b) => {
                num(a) && num(b) && inst.ty == IrType::Number
            }
            Op::Neg(a) | Op::Math1(_, a) => num(a) && inst.ty == IrType::Number,
            Op::AddI32(a, b) => t.ty(*a) == IrType::I32 && t.ty(*b) == IrType::I32,
            Op::ASize(tab) | Op::GuardPlainTable { t: tab, .. } => t.ty(*tab) == IrType::Table,
            Op::ARef { t: tab, idx } => matches!(t.ty(*tab), IrType::Table | IrType::Ptr) && t.ty(*idx) == IrType::I32 && inst.ty == IrType::Ptr,
            Op::ABase(tab) => t.ty(*tab) == IrType::Table && inst.ty == IrType::Ptr,
            Op::XLoad { p, .. } | Op::XStore { p, .. } | Op::GuardFlag { p, .. } => t.ty(*p) == IrType::Ptr,
            Op::ULoad { cl, .. } | Op::GuardUvSlot { cl, .. } => t.ty(*cl) == IrType::Function,
            Op::HSlot { t: tab, .. } => matches!(t.ty(*tab), IrType::Table | IrType::Ptr) && inst.ty == IrType::Ptr,
            Op::HRef { t: tab, k, .. } => matches!(t.ty(*tab), IrType::Table | IrType::Ptr) && t.ty(*k) == IrType::String && inst.ty == IrType::Ptr,
            Op::ClEnv { cl } => t.ty(*cl) == IrType::Function && inst.ty == IrType::Ptr,
            Op::GuardProto { f, p, .. } => t.ty(*f) == IrType::Function && t.ty(*p) == IrType::Ptr,
            _ => true,
        };
        if !ok {
            return Err(format!("{}: operand types don't fit {:?}", i, inst.op));
        }
    }
    if let Some(l) = t.loop_ref {
        if loop_at != Some(l.index()) {
            return Err("loop_ref doesn't point at the Loop".into());
        }
    }
    Ok(())
}
